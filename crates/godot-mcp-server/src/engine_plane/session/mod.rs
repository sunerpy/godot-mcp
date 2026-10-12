//! Managed Godot sessions: a game or an editor this server started and owns.
//!
//! A session is one detached supervised launch
//! ([`LaunchProtocol::start`](godot_mcp_engine::supervision::protocol::LaunchProtocol::start)):
//! the boundary, the process identity and the release are durable before the
//! start answers, and the call returns once the session is ready — a game's
//! runtime probe connected and described itself, an editor's language server
//! and debug adapter accept connections — rather than when the process exits.
//!
//! Each session has one supervising thread. It performs the launch (the
//! running launch owns a containment boundary that never leaves that thread),
//! reports the launch facts, waits for the process to end or its lifetime to
//! pass, archives both output streams into the session's private directory
//! and records the exit. A stop asks a game's probe to release its input and
//! quit, and terminates the recorded boundary from its durable identity when
//! the process does not end by itself.
//!
//! Records are durable under `<state>/sessions/<id>/`; a server that starts
//! after another stopped terminates every session the other left behind
//! before it serves anything ([`SessionManager::reconcile`]).

mod apng;
pub(crate) mod boundary;
pub(crate) mod debug;
pub(crate) mod export;
mod record;
pub(crate) mod tools;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use godot_mcp_bridge::protocol::envelope::{OperationClass, ProjectClaim};
use godot_mcp_bridge::runtime as probe_files;
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_engine::adapters::receipt::{EndpointKind, EndpointReceipt};
use godot_mcp_engine::supervision::boundary::BoundaryIdentity;
use godot_mcp_engine::supervision::journal::{
    BoundaryMembership, LaunchJournal as _, ProcessLaunchProof, ReleaseStatus, hex_sha256,
};
use godot_mcp_engine::supervision::procfs;
use godot_mcp_engine::supervision::protocol::{
    FaultInjection, LaunchExit, LaunchProtocol, SilentObserver, TerminationKind,
};
use serde_json::{Value, json};

pub(crate) use record::{SessionKind, SessionRecord, SessionState};

use super::journal::EngineJournal;
use crate::editor::bridge::{BridgeConfig, BridgeFailure, EditorBridge};
use crate::effect::boundary::{SessionStartOutcome, SessionStartRequest};

/// The rendezvous file of a game session's probe bridge.
const RENDEZVOUS_FILE: &str = "runtime.rendezvous";
/// The probe scripts' directory inside a session directory.
const PROBE_DIRECTORY: &str = "probe";
/// An export's private output directory inside its session directory.
const OUTPUT_DIRECTORY: &str = "out";
/// Where an export's app bundle is unpacked to be smoke-run.
const BUNDLE_DIRECTORY: &str = "bundle";
/// Most sessions that may run at once.
const MAX_RUNNING: usize = 4;
/// How long the launch itself may take before the start gives up on it.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a stopped game may take to quit by itself before its boundary is
/// terminated.
const STOP_GRACE: Duration = Duration::from_secs(10);
/// How long termination may take to be observed.
const TERMINATE_WAIT: Duration = Duration::from_secs(20);
/// The most archived bytes kept of one output stream.
const MAX_ARCHIVED_BYTES: u64 = 32 * 1024 * 1024;
/// The most bytes one log read returns.
pub(crate) const MAX_LOG_PAGE: u64 = 64 * 1024;
/// How many times port allocation looks for two loopback ports no committed
/// launch names.
const PORT_ATTEMPTS: usize = 16;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

fn outcome_error(kind: ErrorKind, message: impl Into<String>) -> OutcomeError {
    OutcomeError::new(kind, message, false, BTreeMap::new())
}

/// A fresh opaque session ID of `kind`.
pub(crate) fn new_session_id(kind: SessionKind) -> String {
    let mut seed = Vec::with_capacity(48);
    seed.extend_from_slice(&now_unix_ms().to_le_bytes());
    seed.extend_from_slice(&std::process::id().to_le_bytes());
    seed.extend_from_slice(format!("{:?}", Instant::now()).as_bytes());
    seed.extend_from_slice(&rand_bytes());
    let digest = hex_sha256(&seed);
    format!("{}{}", kind.prefix(), &digest[..32])
}

fn rand_bytes() -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    // The time, process and instant already make the ID unique; the OS
    // CSPRNG makes it unguessable.
    let _ = getrandom::fill(&mut bytes);
    bytes
}

/// Whether `value` is a well-formed session ID.
pub(crate) fn is_session_id(value: &str) -> bool {
    [SessionKind::Game, SessionKind::Editor, SessionKind::Export]
        .iter()
        .any(|kind| {
            value.strip_prefix(kind.prefix()).is_some_and(|hex| {
                hex.len() == 32
                    && hex
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            })
        })
}

/// The private paths of one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionPaths {
    pub(crate) directory: PathBuf,
    pub(crate) launcher: PathBuf,
    pub(crate) rendezvous: PathBuf,
    /// Where an export writes its output.
    pub(crate) output: PathBuf,
}

/// The launch facts the supervising thread reports once the launch is
/// committed as running.
#[derive(Debug, Clone)]
struct LaunchFacts {
    transaction_id: String,
    boundary: BoundaryIdentity,
    child_pid: u32,
    child_start_time: Option<String>,
    binary_sha256: String,
    argv: Vec<String>,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl LaunchFacts {
    fn running_proof(&self) -> ProcessLaunchProof {
        ProcessLaunchProof {
            boundary_identity: self.boundary.identity.clone(),
            pid: Some(self.child_pid),
            start_time: self.child_start_time.clone(),
            boundary_membership: BoundaryMembership::Confirmed,
            release_status: ReleaseStatus::ResumedContained,
            exit_code: None,
            signal: None,
            boundary_empty_after_exit: None,
        }
    }
}

struct Live {
    record: SessionRecord,
    launch: Option<LaunchFacts>,
    /// The supervising thread has recorded the end of the session.
    finished: bool,
}

/// One session this server owns.
pub(crate) struct ManagedSession {
    id: String,
    kind: SessionKind,
    directory: PathBuf,
    bridge: Option<Arc<EditorBridge>>,
    live: Mutex<Live>,
    changed: Condvar,
}

impl std::fmt::Debug for ManagedSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedSession")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl ManagedSession {
    fn record(&self) -> SessionRecord {
        lock(&self.live).record.clone()
    }

    /// Change the record and make the change durable.
    fn update(&self, change: impl FnOnce(&mut SessionRecord)) {
        let mut live = lock(&self.live);
        change(&mut live.record);
        if let Err(error) = record::write(&self.directory, &live.record) {
            tracing::warn!(
                event = "session_record_unwritable",
                session = %self.id,
                reason = %error,
                "a session record could not be made durable"
            );
        }
        drop(live);
        self.changed.notify_all();
    }

    fn finished(&self) -> bool {
        lock(&self.live).finished
    }

    /// Wait until the supervising thread recorded the end, at most `limit`.
    fn wait_finished(&self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        let mut live = lock(&self.live);
        while !live.finished {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            live = self
                .changed
                .wait_timeout(live, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    pub(crate) fn state(&self) -> SessionState {
        lock(&self.live).record.state
    }

    pub(crate) const fn kind(&self) -> SessionKind {
        self.kind
    }

    /// An export session's facts, with each output's artifact checked
    /// against `store`; `None` for any other session.
    pub(crate) fn export_public(
        &self,
        store: Option<&crate::artifacts::ArtifactStore>,
        now_unix_ms: u64,
    ) -> Option<Value> {
        lock(&self.live)
            .record
            .export
            .as_ref()
            .map(|export| export.public(store, now_unix_ms))
    }

    /// Whether `principal` may see and cancel this session: an export belongs
    /// to the principal that started it, every other session to its server.
    pub(crate) fn belongs_to(&self, principal: &str) -> bool {
        lock(&self.live)
            .record
            .export
            .as_ref()
            .is_none_or(|export| export.belongs_to(principal))
    }

    /// The ports an editor session hosts its language server and debug
    /// adapter on, as its launch argument array names them.
    pub(crate) fn editor_ports(&self) -> (Option<u16>, Option<u16>) {
        let live = lock(&self.live);
        (live.record.lsp_port, live.record.dap_port)
    }

    /// The public projection, with what is live right now.
    pub(crate) fn public(&self) -> Value {
        let record = self.record();
        let mut value = record.public();
        if let (Some(bridge), Value::Object(object)) = (&self.bridge, &mut value) {
            let status = bridge.status();
            object.insert(
                "probe".to_owned(),
                json!({
                    "connected": status.connected,
                    "described": status.description,
                    "sessions_opened": status.sessions_opened,
                }),
            );
        }
        value
    }
}

/// Every managed session of one bound project.
pub(crate) struct SessionManager {
    root: PathBuf,
    work_dir: PathBuf,
    project_root: PathBuf,
    project: ProjectClaim,
    journal: Arc<EngineJournal>,
    providers: Arc<super::providers::LaunchProviders>,
    sessions: Mutex<BTreeMap<String, Arc<ManagedSession>>>,
    /// Where a finished export registers its outputs.
    export_artifacts: Mutex<Option<export::ExportArtifacts>>,
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionManager")
            .field("sessions", &lock(&self.sessions).len())
            .finish_non_exhaustive()
    }
}

impl SessionManager {
    /// The manager of the project whose private state root is `state_dir`.
    pub(crate) fn new(
        store: Arc<crate::journal::TransactionStore>,
        state_dir: &Path,
        project_root: &Path,
        project: ProjectClaim,
        providers: Arc<super::providers::LaunchProviders>,
    ) -> Self {
        Self {
            root: state_dir.join("sessions"),
            work_dir: state_dir.join("engine-launches"),
            project_root: project_root.to_path_buf(),
            project,
            journal: Arc::new(EngineJournal::new(store)),
            providers,
            sessions: Mutex::new(BTreeMap::new()),
            export_artifacts: Mutex::new(None),
        }
    }

    /// Let finished exports register their outputs in `artifacts`.
    pub(crate) fn attach_export_artifacts(&self, artifacts: export::ExportArtifacts) {
        *lock(&self.export_artifacts) = Some(artifacts);
    }

    /// The artifact store finished exports register in, when attached.
    pub(crate) fn export_store(&self) -> Option<Arc<crate::artifacts::ArtifactStore>> {
        lock(&self.export_artifacts)
            .as_ref()
            .map(|artifacts| Arc::clone(&artifacts.store))
    }

    /// The private paths of session `id`.
    pub(crate) fn paths(&self, id: &str) -> SessionPaths {
        let directory = self.root.join(id);
        SessionPaths {
            launcher: directory.join(PROBE_DIRECTORY).join(probe_files::LAUNCHER),
            rendezvous: directory.join(RENDEZVOUS_FILE),
            output: directory.join(OUTPUT_DIRECTORY),
            directory,
        }
    }

    /// The scratch directory every supervised launch's streams are captured in.
    pub(crate) fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    /// The project root a launch runs against.
    pub(crate) fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// One session this server knows, running or not.
    pub(crate) fn session(&self, id: &str) -> Result<Arc<ManagedSession>, OutcomeError> {
        if !is_session_id(id) {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                "`session` must be a session ID that project.run.start or editor.session.start \
                 returned",
            ));
        }
        lock(&self.sessions).get(id).cloned().ok_or_else(|| {
            outcome_error(
                ErrorKind::NotFound,
                format!("{id} is not a session of this server"),
            )
        })
    }

    /// The probe bridge of running game session `id`.
    pub(crate) fn running_game(&self, id: &str) -> Result<Arc<EditorBridge>, OutcomeError> {
        let session = self.session(id)?;
        if session.kind != SessionKind::Game {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                format!("{id} is an editor session, not a game"),
            ));
        }
        let state = session.state();
        if state != SessionState::Running {
            return Err(OutcomeError::new(
                ErrorKind::Unavailable,
                format!("{id} is {}, not running", state.as_str()),
                false,
                BTreeMap::new(),
            ));
        }
        session
            .bridge
            .clone()
            .ok_or_else(|| outcome_error(ErrorKind::Internal, "a game session has no probe bridge"))
    }

    /// Whether this server knows a session of `kind`, running or ended: one a
    /// command, a stop or a read can be answered for.
    pub(crate) fn knows_a_session_of(&self, kind: SessionKind) -> bool {
        lock(&self.sessions)
            .values()
            .any(|session| session.kind == kind)
    }

    /// Every session of `kind` this server knows, by ID.
    pub(crate) fn sessions_of(&self, kind: SessionKind) -> Vec<Arc<ManagedSession>> {
        lock(&self.sessions)
            .values()
            .filter(|session| session.kind == kind)
            .cloned()
            .collect()
    }

    /// The last progress percentage a running export printed.
    pub(crate) fn export_progress(&self, id: &str) -> Option<u64> {
        let first = self.logs(id, "stdout", 0, 1).ok()?;
        let total = first.get("total_bytes").and_then(Value::as_u64)?;
        let tail = self
            .logs(
                id,
                "stdout",
                total.saturating_sub(MAX_LOG_PAGE),
                MAX_LOG_PAGE,
            )
            .ok()?;
        last_progress(tail.get("text").and_then(Value::as_str)?)
    }

    /// Whether a session of `kind` is running now.
    pub(crate) fn runs_a_session_of(&self, kind: SessionKind) -> bool {
        lock(&self.sessions)
            .values()
            .any(|session| session.kind == kind && session.state() == SessionState::Running)
    }

    /// Two free loopback ports for an editor session's language server and
    /// debug adapter, neither of them named by any launch this server
    /// committed: an endpoint is found by its port, so a port an earlier
    /// editor launch named would make the new editor's receipt ambiguous.
    ///
    /// # Errors
    /// Returns `internal` when the launch journal cannot be read and
    /// `unavailable` when no such pair turns up.
    pub(crate) fn allocate_editor_ports(&self) -> Result<(u16, u16), OutcomeError> {
        let committed = self
            .journal
            .committed_launches()
            .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
        let named: BTreeSet<u16> = committed
            .iter()
            .flat_map(|record| {
                EndpointKind::ALL.into_iter().filter_map(move |kind| {
                    EndpointReceipt::from_launch(record, kind, "")
                        .ok()
                        .map(|receipt| receipt.endpoint.port)
                })
            })
            .collect();
        for _ in 0..PORT_ATTEMPTS {
            // Both listeners are held at once, so the two ports differ.
            let bound = std::net::TcpListener::bind(("127.0.0.1", 0)).and_then(|first| {
                std::net::TcpListener::bind(("127.0.0.1", 0)).map(|second| (first, second))
            });
            let (first, second) =
                bound.map_err(|error| outcome_error(ErrorKind::Unavailable, error.to_string()))?;
            let ports = first
                .local_addr()
                .and_then(|first| {
                    second
                        .local_addr()
                        .map(|second| (first.port(), second.port()))
                })
                .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
            if !named.contains(&ports.0) && !named.contains(&ports.1) {
                return Ok(ports);
            }
        }
        Err(outcome_error(
            ErrorKind::Unavailable,
            "no pair of free loopback ports that no earlier editor launch named was found",
        ))
    }

    /// The session a start transaction created, for an idempotent replay.
    pub(crate) fn started_by(&self, start_transaction_id: &str) -> Option<Arc<ManagedSession>> {
        lock(&self.sessions)
            .values()
            .find(|session| {
                session.record().start_transaction_id.as_deref() == Some(start_transaction_id)
            })
            .cloned()
    }

    /// The public projection of every session this server knows.
    pub(crate) fn list(&self) -> Vec<Value> {
        lock(&self.sessions)
            .values()
            .map(|session| session.public())
            .collect()
    }

    /// Start one session and return once it is ready.
    ///
    /// # Errors
    /// Returns the refusal when too many sessions run, the launch fails, or the
    /// session does not become ready in time; whatever was launched has then
    /// been terminated and the session is recorded `failed`.
    pub(crate) fn start(
        &self,
        request: &SessionStartRequest,
        start_transaction_id: &str,
    ) -> Result<SessionStartOutcome, OutcomeError> {
        if !is_session_id(&request.session_id) {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                "malformed session ID",
            ));
        }
        {
            let sessions = lock(&self.sessions);
            let running = sessions
                .values()
                .filter(|session| !session.state().is_terminal())
                .count();
            if running >= MAX_RUNNING {
                return Err(OutcomeError::new(
                    ErrorKind::Unavailable,
                    format!(
                        "{running} sessions are already running; stop one before starting another"
                    ),
                    true,
                    BTreeMap::new(),
                ));
            }
            if sessions.contains_key(&request.session_id) {
                return Err(outcome_error(
                    ErrorKind::Conflict,
                    "the session ID is taken",
                ));
            }
        }
        let paths = self.paths(&request.session_id);
        godot_mcp_domain::sysfs::open_private_state_dir(&self.root)
            .and_then(|_| godot_mcp_domain::sysfs::open_private_state_dir(&paths.directory))
            .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
        let mut record = SessionRecord::new(
            request.session_id.clone(),
            request.kind,
            request.scene.clone(),
            request.display.clone(),
            u64::try_from(request.lifetime.as_millis()).unwrap_or(u64::MAX),
            now_unix_ms(),
        );
        record.start_transaction_id = Some(start_transaction_id.to_owned());
        record.godot_version.clone_from(&request.godot_version);
        record.lsp_port = request.lsp_port;
        record.dap_port = request.dap_port;
        record.export = request.export.as_ref().map(export::ExportRecord::planned);
        record.argv_sha256 = Some(hex_sha256(
            serde_json::to_string(&request.launch.arguments)
                .unwrap_or_default()
                .as_bytes(),
        ));
        record::write(&paths.directory, &record)
            .map_err(|error| outcome_error(ErrorKind::Internal, error))?;

        let bridge = match request.kind {
            SessionKind::Game => {
                write_probe(&paths).map_err(|error| outcome_error(ErrorKind::Internal, error))?;
                Some(
                    EditorBridge::start_with(
                        BridgeConfig::runtime(paths.rendezvous.clone(), probe_files::METHODS),
                        self.project.clone(),
                    )
                    .map_err(|error| outcome_error(ErrorKind::Internal, error))?,
                )
            }
            SessionKind::Editor => None,
            SessionKind::Export => {
                godot_mcp_domain::sysfs::open_private_state_dir(&paths.output)
                    .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
                None
            }
        };
        let session = Arc::new(ManagedSession {
            id: request.session_id.clone(),
            kind: request.kind,
            directory: paths.directory.clone(),
            bridge,
            live: Mutex::new(Live {
                record,
                launch: None,
                finished: false,
            }),
            changed: Condvar::new(),
        });
        lock(&self.sessions).insert(request.session_id.clone(), Arc::clone(&session));

        let (started, launched) = mpsc::sync_channel(1);
        let supervising = Arc::clone(&session);
        let journal = Arc::clone(&self.journal);
        let provider = match self.providers.selected() {
            Ok(provider) => provider,
            Err(error) => {
                self.fail(&session, error.message());
                return Err(error);
            }
        };
        let launch = request.launch.clone();
        let lifetime = request.lifetime;
        let export_artifacts = lock(&self.export_artifacts).clone();
        let spawned = std::thread::Builder::new()
            .name(format!("godot-mcp-session-{}", &request.session_id))
            .spawn(move || {
                supervise(
                    &supervising,
                    &journal,
                    provider.as_ref(),
                    &launch,
                    lifetime,
                    &started,
                    export_artifacts.as_ref(),
                )
            });
        if let Err(error) = spawned {
            self.fail(
                &session,
                &format!("the session could not be supervised: {error}"),
            );
            return Err(outcome_error(ErrorKind::Internal, error.to_string()));
        }
        let facts = match launched.recv_timeout(LAUNCH_TIMEOUT) {
            Ok(Ok(facts)) => facts,
            Ok(Err(error)) => {
                self.fail(&session, &error);
                return Err(OutcomeError::new(
                    ErrorKind::Unavailable,
                    format!("the session could not be launched: {error}"),
                    false,
                    BTreeMap::new(),
                ));
            }
            Err(_) => {
                self.fail(&session, "the launch did not complete in time");
                return Err(outcome_error(
                    ErrorKind::Timeout,
                    "the session's launch did not complete in time",
                ));
            }
        };
        let proof = super::process_proof(facts.running_proof());
        let kind = facts.boundary.kind.as_str();
        let strength = facts.boundary.strength().as_str();
        session.update(|record| {
            record.launch_transaction_id = Some(facts.transaction_id.clone());
            record.boundary = Some(facts.boundary.to_facts());
            record.boundary_kind = Some(kind.to_owned());
            record.boundary_strength = Some(strength.to_owned());
            record.binary_sha256 = Some(facts.binary_sha256.clone());
            record.argv_sha256 = Some(hex_sha256(
                serde_json::to_string(&facts.argv)
                    .unwrap_or_default()
                    .as_bytes(),
            ));
        });
        lock(&session.live).launch = Some(facts);

        if let Err(error) = self.await_ready(&session, request) {
            self.terminate(&session);
            self.fail(&session, &error.to_string());
            return Err(error);
        }
        let reported_version = session
            .bridge
            .as_ref()
            .and_then(|bridge| bridge.status().description)
            .and_then(|description| {
                description
                    .get("godot_version")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        session.update(|record| {
            record.state = SessionState::Running;
            record.started_at_unix_ms = Some(now_unix_ms());
            if reported_version.is_some() {
                record.godot_version = reported_version;
            }
        });
        tracing::info!(
            event = "session_started",
            session = %session.id,
            kind = session.kind.as_str(),
            "a managed Godot session started"
        );
        Ok(SessionStartOutcome {
            session: session.public(),
            proof,
            boundary_kind: kind,
            boundary_strength: strength,
        })
    }

    /// Wait until the session can serve: a game's probe connected and
    /// described itself; an editor's requested endpoints accept connections.
    fn await_ready(
        &self,
        session: &ManagedSession,
        request: &SessionStartRequest,
    ) -> Result<(), OutcomeError> {
        // An export is ready once it is launched: it ends by itself.
        if session.kind == SessionKind::Export {
            return Ok(());
        }
        let deadline = Instant::now() + request.readiness;
        loop {
            if session.finished() {
                let exit = early_exit_detail(&session.record());
                return Err(OutcomeError::new(
                    ErrorKind::Unavailable,
                    format!(
                        "the {} exited before it became ready{exit}; read its output with \
                         project.run.logs",
                        session.kind.as_str()
                    ),
                    false,
                    BTreeMap::new(),
                ));
            }
            let ready = match session.kind {
                SessionKind::Game => session
                    .bridge
                    .as_ref()
                    .is_some_and(|bridge| bridge.status().connected),
                SessionKind::Editor => {
                    let launch = lock(&session.live)
                        .launch
                        .as_ref()
                        .map(|launch| (launch.child_pid, launch.child_start_time.clone()));
                    match launch {
                        Some((pid, start_time)) => {
                            editor_listening(pid, start_time.as_deref(), request)?
                        }
                        None => false,
                    }
                }
                SessionKind::Export => true,
            };
            if ready {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(OutcomeError::new(
                    ErrorKind::Timeout,
                    format!(
                        "the {} did not become ready within {} s",
                        session.kind.as_str(),
                        request.readiness.as_secs()
                    ),
                    false,
                    BTreeMap::new(),
                ));
            }
            // A bounded condition poll: neither the probe connection nor a
            // listening port announces itself.
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn fail(&self, session: &ManagedSession, reason: &str) {
        session.update(|record| {
            if !record.state.is_terminal() {
                record.state = SessionState::Failed;
            }
            record.reason = Some(reason.to_owned());
        });
        if let Some(bridge) = &session.bridge {
            bridge.stop();
        }
    }

    /// Terminate a session's boundary from its recorded identity and wait for
    /// the supervising thread to record the end.
    fn terminate(&self, session: &ManagedSession) -> bool {
        let identity = lock(&session.live)
            .launch
            .as_ref()
            .map(|launch| launch.boundary.clone());
        if let Some(identity) = identity {
            match self.providers.reopen(&identity) {
                Ok(Some(boundary)) => {
                    if let Err(error) = boundary.terminate_all() {
                        tracing::warn!(
                            event = "session_termination_unproven",
                            session = %session.id,
                            reason = %error,
                            "a session's boundary could not be proven empty"
                        );
                    }
                }
                // Nothing of the launch is left; the supervising thread
                // records the end.
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    event = "session_boundary_unreachable",
                    session = %session.id,
                    reason = %error,
                    "a session's boundary could not be reopened to end it"
                ),
            }
        }
        session.wait_finished(TERMINATE_WAIT)
    }

    /// Stop session `id`: a game's probe releases its input and quits; a
    /// session that does not end by itself is terminated. Stopping a session
    /// that already ended answers its record.
    ///
    /// # Errors
    /// Returns `not_found` for an unknown session and `uncertain` when the end
    /// cannot be observed.
    pub(crate) fn stop(&self, id: &str, idempotency_id: &str) -> Result<Value, OutcomeError> {
        let session = self.session(id)?;
        if session.finished() {
            let mut value = session.public();
            if let Value::Object(object) = &mut value {
                object.insert("already_ended".to_owned(), Value::Bool(true));
            }
            return Ok(value);
        }
        session.update(|record| {
            if !record.state.is_terminal() {
                record.state = SessionState::Stopping;
            }
        });
        let mut released = Value::Null;
        if let Some(bridge) = &session.bridge {
            match bridge.call(
                "runtime.session.stop",
                OperationClass::Mutation,
                &json!({}),
                idempotency_id,
                Duration::from_secs(5),
            ) {
                Ok(crate::editor::bridge::BridgeAnswer::Result { payload, .. }) => {
                    released = payload.get("released").cloned().unwrap_or(Value::Null);
                }
                Ok(_) | Err(BridgeFailure::NotConnected | BridgeFailure::Unsupported(_)) => {}
                Err(failure) => {
                    tracing::info!(
                        event = "session_probe_stop_unanswered",
                        session = %session.id,
                        reason = %failure.message(),
                        "a game probe did not answer the stop; its boundary is terminated"
                    );
                }
            }
        }
        let ended = if session.bridge.is_some() {
            session.wait_finished(STOP_GRACE) || self.terminate(&session)
        } else {
            // An editor never quits by itself: its boundary is terminated
            // at once.
            self.terminate(&session)
        };
        if !ended {
            return Err(OutcomeError::new(
                ErrorKind::Uncertain,
                format!("{id} could not be proven stopped"),
                false,
                BTreeMap::new(),
            ));
        }
        let mut value = session.public();
        if let Value::Object(object) = &mut value {
            object.insert("released_input".to_owned(), released);
        }
        Ok(value)
    }

    /// Stop every running session: the server is going away.
    pub(crate) fn stop_all(&self) {
        let running: Vec<Arc<ManagedSession>> = lock(&self.sessions)
            .values()
            .filter(|session| !session.finished())
            .cloned()
            .collect();
        for session in running {
            let _ = self.stop(&session.id, &format!("shutdown-{}", session.id));
        }
    }

    /// One page of a session's captured output.
    ///
    /// # Errors
    /// Returns `not_found` for an unknown session or stream.
    pub(crate) fn logs(
        &self,
        id: &str,
        stream: &str,
        offset: u64,
        limit: u64,
    ) -> Result<Value, OutcomeError> {
        let session = self.session(id)?;
        let (path, finished) = {
            let live = lock(&session.live);
            let archived = session.directory.join(match stream {
                "stdout" => record::STDOUT_LOG,
                "stderr" => record::STDERR_LOG,
                _ => {
                    return Err(outcome_error(
                        ErrorKind::InvalidInput,
                        "`stream` must be stdout or stderr",
                    ));
                }
            });
            let path = match (&live.launch, live.finished) {
                (Some(launch), false) => {
                    if stream == "stdout" {
                        launch.stdout.clone()
                    } else {
                        launch.stderr.clone()
                    }
                }
                _ => archived,
            };
            (path, live.finished)
        };
        let limit = limit.clamp(1, MAX_LOG_PAGE);
        let (bytes, total) = read_page(&path, offset, limit);
        let next = offset + bytes.len() as u64;
        Ok(json!({
            "session": id,
            "stream": stream,
            "offset": offset,
            "next_offset": next,
            "total_bytes": total,
            "text": String::from_utf8_lossy(&bytes),
            "running": !finished,
            "eof": finished && next >= total,
        }))
    }

    /// Terminate every session a previous server left behind and record how
    /// each ended. Runs before anything is served.
    ///
    /// Returns the session IDs and the state each was reconciled to.
    pub(crate) fn reconcile(&self) -> Vec<(String, SessionState)> {
        let mut settled = Vec::new();
        for (directory, mut record) in record::load_all(&self.root) {
            if record.state.is_terminal() {
                self.remember(directory, record);
                continue;
            }
            let identity = record
                .boundary
                .as_ref()
                .and_then(BoundaryIdentity::from_facts);
            let proven_empty = match &identity {
                // Nothing was ever created inside a boundary for it.
                None => true,
                Some(identity) => match self.providers.reopen(identity) {
                    Ok(None) => true,
                    Ok(Some(boundary)) => match boundary.terminate_all() {
                        Ok(()) => {
                            let _ = boundary.close();
                            true
                        }
                        Err(error) => {
                            tracing::warn!(
                                event = "session_termination_unproven",
                                session = %record.session_id,
                                reason = %error,
                                "a session a previous server left could not be proven ended"
                            );
                            false
                        }
                    },
                    Err(error) => {
                        tracing::warn!(
                            event = "session_boundary_unreopenable",
                            session = %record.session_id,
                            reason = %error,
                            "a session a previous server left names a boundary this server \
                             cannot reopen"
                        );
                        false
                    }
                },
            };
            if let Some(launch) = &record.launch_transaction_id {
                let scratch = godot_mcp_engine::supervision::gate::LaunchPaths::for_transaction(
                    &self.work_dir,
                    launch,
                );
                archive(&scratch.stdout, &directory.join(record::STDOUT_LOG));
                archive(&scratch.stderr, &directory.join(record::STDERR_LOG));
            }
            record.state = if proven_empty {
                SessionState::Reconciled
            } else {
                SessionState::Quarantined
            };
            // An export's partial output stays private, as evidence of what
            // the interrupted export had written; it is never registered.
            if let Some(export) = record.export.as_mut() {
                let bytes = export::evidence_bytes(&directory.join(OUTPUT_DIRECTORY));
                export.outcome = Some("interrupted".to_owned());
                export.recovery_evidence_bytes = (bytes > 0).then_some(bytes);
            }
            record.ended_at_unix_ms = Some(now_unix_ms());
            record.reason = Some(if proven_empty {
                "the server that owned this session stopped; its boundary was terminated and \
                 proven empty when the next server started"
                    .to_owned()
            } else {
                "the server that owned this session stopped and its boundary could not be \
                 proven empty"
                    .to_owned()
            });
            let _ = record::write(&directory, &record);
            settled.push((record.session_id.clone(), record.state));
            self.remember(directory, record);
        }
        settled
    }

    /// Know an ended session from its record, so its status and archived
    /// output stay readable.
    fn remember(&self, directory: PathBuf, record: SessionRecord) {
        let session = Arc::new(ManagedSession {
            id: record.session_id.clone(),
            kind: record.kind,
            directory,
            bridge: None,
            live: Mutex::new(Live {
                record,
                launch: None,
                finished: true,
            }),
            changed: Condvar::new(),
        });
        lock(&self.sessions).insert(session.id.clone(), session);
    }
}

impl Drop for SessionManager {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// The last `[ NN% ]` progress mark in Godot's output.
fn last_progress(text: &str) -> Option<u64> {
    text.match_indices("% ]")
        .filter_map(|(at, _)| {
            let digits: String = text[..at]
                .chars()
                .rev()
                .take_while(char::is_ascii_digit)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            digits.parse::<u64>().ok().filter(|percent| *percent <= 100)
        })
        .last()
}

/// Who holds the loopback listener on one port.
enum Listener {
    /// Nothing listens there yet.
    Absent,
    /// The editor process this server launched.
    Editor,
    /// Some other process: the editor can never bind the port.
    Foreign,
}

/// Who holds the listener on `127.0.0.1:<port>`, attributed to the editor's
/// launch by the operating system rather than by a connect, which any
/// listener would accept.
fn listener_on(pid: u32, start_time: Option<&str>, port: u16) -> Result<Listener, OutcomeError> {
    match procfs::launch_listener(port, pid, start_time) {
        Ok(procfs::LaunchListener::Absent) => Ok(Listener::Absent),
        Ok(procfs::LaunchListener::Launch { .. }) => Ok(Listener::Editor),
        Ok(procfs::LaunchListener::Foreign) => Ok(Listener::Foreign),
        Err(error) => Err(outcome_error(
            ErrorKind::Unavailable,
            format!(
                "this platform cannot attribute a loopback listener to the editor process \
                 ({error}), so an editor session cannot prove it owns its endpoints"
            ),
        )),
    }
}

/// Whether the editor launched as `pid` (created at `start_time`) listens on
/// every port `request` asked for.
///
/// # Errors
/// Returns `unavailable` once a port is held by a process that is not the
/// editor, which the editor can then never bind.
fn editor_listening(
    pid: u32,
    start_time: Option<&str>,
    request: &SessionStartRequest,
) -> Result<bool, OutcomeError> {
    let mut ready = true;
    for (port, kind) in [
        (request.lsp_port, EndpointKind::LanguageServer),
        (request.dap_port, EndpointKind::DebugAdapter),
    ] {
        let Some(port) = port else {
            continue;
        };
        match listener_on(pid, start_time, port)? {
            Listener::Editor => {}
            Listener::Absent => ready = false,
            Listener::Foreign => {
                return Err(outcome_error(
                    ErrorKind::Unavailable,
                    format!(
                        "the {} listener on 127.0.0.1:{port} is not held by the editor this \
                         server launched; another process holds the port",
                        kind.as_str()
                    ),
                ));
            }
        }
    }
    Ok(ready)
}

/// The supervising thread of one session.
fn supervise(
    session: &ManagedSession,
    journal: &EngineJournal,
    provider: &dyn godot_mcp_engine::supervision::boundary::BoundaryProvider,
    launch: &godot_mcp_engine::supervision::protocol::LaunchRequest,
    lifetime: Duration,
    started: &mpsc::SyncSender<Result<LaunchFacts, String>>,
    export_artifacts: Option<&export::ExportArtifacts>,
) {
    let protocol = LaunchProtocol::new(journal);
    let mut running =
        match protocol.start(launch, provider, &FaultInjection::none(), &SilentObserver) {
            Ok(running) => running,
            Err(error) => {
                let _ = started.send(Err(error.to_string()));
                mark_finished(session, None, Some(error.to_string()));
                return;
            }
        };
    let facts = LaunchFacts {
        transaction_id: running.transaction_id().to_owned(),
        boundary: running.boundary().clone(),
        child_pid: running.child_pid(),
        child_start_time: running.child_start_time().map(str::to_owned),
        binary_sha256: running.binary_sha256().to_owned(),
        argv: running.argv().to_vec(),
        stdout: running.stdout_path().to_path_buf(),
        stderr: running.stderr_path().to_path_buf(),
    };
    let _ = started.send(Ok(facts));
    let exit = running.wait(lifetime);
    archive(
        running.stdout_path(),
        &session.directory.join(record::STDOUT_LOG),
    );
    archive(
        running.stderr_path(),
        &session.directory.join(record::STDERR_LOG),
    );
    let _ = running.remove_scratch();
    if session.kind == SessionKind::Export {
        // A stop that ended the export is its cancellation.
        let cancelled = session.state() == SessionState::Stopping;
        let output = session.directory.join(OUTPUT_DIRECTORY);
        let bundle = session.directory.join(BUNDLE_DIRECTORY);
        let completion = export::Completion {
            session_id: &session.id,
            output_dir: &output,
            bundle_dir: &bundle,
            artifacts: export_artifacts,
            journal,
            provider,
            launch,
            now_unix_ms: now_unix_ms(),
        };
        let mut record = session.record();
        completion.settle(
            &mut record,
            exit.as_ref().ok().map(|exit| &exit.diagnostics),
            cancelled,
        );
        session.update(|current| current.export = record.export.take());
    }
    match exit {
        Ok(exit) => mark_finished(session, Some(&exit), None),
        Err(error) => mark_finished(session, None, Some(error.to_string())),
    }
}

/// How a session that ended before it was ready ended, for its start's
/// refusal: its termination, or, when its exit could not be observed, the
/// reason the supervising thread recorded.
fn early_exit_detail(record: &record::SessionRecord) -> String {
    match (&record.exit, &record.reason) {
        (Some(exit), _) => format!(" ({})", exit.termination),
        (None, Some(reason)) => format!(" (its exit could not be observed: {reason})"),
        (None, None) => String::new(),
    }
}

fn mark_finished(session: &ManagedSession, exit: Option<&LaunchExit>, error: Option<String>) {
    if let Some(bridge) = &session.bridge {
        bridge.stop();
    }
    {
        let mut live = lock(&session.live);
        let record = &mut live.record;
        record.ended_at_unix_ms = Some(now_unix_ms());
        record.state = match (record.state, exit) {
            (SessionState::Starting, _) | (_, None) => SessionState::Failed,
            (SessionState::Stopping, Some(_)) if session.kind == SessionKind::Export => {
                SessionState::Cancelled
            }
            _ => SessionState::Exited,
        };
        if let Some(exit) = exit {
            record.exit = Some(record::ExitFacts {
                termination: match &exit.diagnostics.termination {
                    TerminationKind::ExitedClean => "exited_clean".to_owned(),
                    TerminationKind::NonzeroExit { code } => format!("nonzero_exit({code})"),
                    TerminationKind::Signaled { signal } => format!("signaled({signal})"),
                    TerminationKind::TimedOut => "lifetime_ended".to_owned(),
                },
                exit_code: exit.diagnostics.exit_code,
                signal: exit.diagnostics.signal.clone(),
                timed_out: exit.diagnostics.termination == TerminationKind::TimedOut,
                boundary_empty: exit.proof.boundary_empty_after_exit,
            });
        }
        if let Some(error) = error {
            record.reason.get_or_insert(error);
        }
        live.finished = true;
        let _ = record::write(&session.directory, &live.record);
    }
    session.changed.notify_all();
    tracing::info!(
        event = "session_ended",
        session = %session.id,
        "a managed Godot session ended"
    );
}

/// Write the probe scripts into the session's private probe directory.
fn write_probe(paths: &SessionPaths) -> Result<(), String> {
    let directory = paths.directory.join(PROBE_DIRECTORY);
    godot_mcp_domain::sysfs::open_private_state_dir(&directory)
        .map_err(|error| error.to_string())?;
    for file in probe_files::FILES {
        let target = directory.join(file.name);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut handle = options.open(&target).map_err(|error| error.to_string())?;
        std::io::Write::write_all(&mut handle, file.bytes).map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Copy at most [`MAX_ARCHIVED_BYTES`] of a captured stream into the session
/// directory.
fn archive(from: &Path, to: &Path) {
    let Ok(mut source) = std::fs::File::open(from) else {
        return;
    };
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let Ok(mut target) = options.open(to) else {
        return;
    };
    let _ = std::io::copy(&mut (&mut source).take(MAX_ARCHIVED_BYTES), &mut target);
}

/// Read at most `limit` bytes of `path` from `offset`, with the file's length.
fn read_page(path: &Path, offset: u64, limit: u64) -> (Vec<u8>, u64) {
    let Ok(mut file) = std::fs::File::open(path) else {
        return (Vec::new(), 0);
    };
    let total = file.metadata().map_or(0, |metadata| metadata.len());
    if offset >= total || file.seek(SeekFrom::Start(offset)).is_err() {
        return (Vec::new(), total);
    }
    let mut bytes = Vec::new();
    let _ = file.take(limit).read_to_end(&mut bytes);
    (bytes, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session that ends before it is ready says how it exited, or, when its
    /// exit could not be observed, why not.
    #[test]
    fn an_early_exit_names_how_the_session_ended_or_why_that_is_unknown() {
        let mut record = record::SessionRecord::new(
            "run_0123456789abcdef0123456789abcdef".to_owned(),
            SessionKind::Game,
            None,
            "headless".to_owned(),
            1_000,
            1,
        );
        assert_eq!(early_exit_detail(&record), "");
        record.reason = Some("the control channel reached end of file".to_owned());
        assert_eq!(
            early_exit_detail(&record),
            " (its exit could not be observed: the control channel reached end of file)"
        );
        record.exit = Some(record::ExitFacts {
            termination: "nonzero_exit(3)".to_owned(),
            exit_code: Some(3),
            signal: None,
            timed_out: false,
            boundary_empty: Some(true),
        });
        assert_eq!(early_exit_detail(&record), " (nonzero_exit(3))");
    }

    #[test]
    fn session_ids_are_opaque_and_typed() {
        let game = new_session_id(SessionKind::Game);
        let editor = new_session_id(SessionKind::Editor);
        assert!(game.starts_with("run_") && game.len() == 36, "{game}");
        assert!(
            editor.starts_with("editor_") && editor.len() == 39,
            "{editor}"
        );
        assert!(is_session_id(&game) && is_session_id(&editor));
        assert_ne!(game, new_session_id(SessionKind::Game));
        for malformed in [
            "run_",
            "run_XYZ",
            "session_0123",
            &game.to_uppercase(),
            "../run_0",
        ] {
            assert!(!is_session_id(malformed), "{malformed}");
        }
    }

    #[test]
    fn a_log_page_is_bounded_and_reports_the_total() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("stdout");
        std::fs::write(&path, b"0123456789").expect("write");
        assert_eq!(read_page(&path, 2, 3), (b"234".to_vec(), 10));
        assert_eq!(read_page(&path, 10, 3), (Vec::new(), 10));
        assert_eq!(
            read_page(&directory.path().join("absent"), 0, 3),
            (Vec::new(), 0)
        );
    }
}
