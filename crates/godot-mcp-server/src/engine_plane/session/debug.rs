//! Receipt-bound language-server and debug-adapter commands to a managed
//! editor session.
//!
//! An editor session is launched with the `--lsp-port` and `--dap-port` this
//! server allocated, so its committed launch record is exactly the receipt
//! [`godot_mcp_engine::adapters::receipt`] requires. An endpoint is dialled
//! only when that receipt still holds — the editor's PID and start time, the
//! binary it runs, the project root and its generation — and when the
//! loopback listener on the port is held by that same process. Nothing else
//! is ever dialled.
//!
//! Each protocol of each editor session has one worker thread that owns the
//! verified connection and its adapter session, so a debug session keeps its
//! launch state and its queued events between commands. The first command of
//! a protocol connects it under that command's own permit; every command
//! reaches the worker only after the gateway journaled it. A connection the
//! editor dropped ends its worker, and the next command connects again — and
//! verifies the receipt again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use godot_mcp_bridge::protocol::mutation::MutationStatus;
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_engine::adapters::admission::ClientAdmission;
use godot_mcp_engine::adapters::connect::{ConnectedEndpoint, EndpointRegistry};
use godot_mcp_engine::adapters::error::AdapterError;
use godot_mcp_engine::adapters::receipt::{EditorEndpoint, EndpointKind, ObservedEndpoint};
use godot_mcp_engine::adapters::tools::{EffectBoundary, EffectGate, EffectRequest, ToolPermit};
use godot_mcp_engine::adapters::uri::{self, PathStyle};
use godot_mcp_engine::supervision::journal::hex_sha256;
use godot_mcp_engine::supervision::procfs;
use serde_json::{Map, Value, json};

use super::{SessionKind, SessionManager, SessionState};
use crate::editor::boundary::receipt;
use crate::effect::boundary::{
    BoundaryUnavailable, EditorEffectOutcome, EditorMutation, EffectTicket, LspDapEffectBoundary,
    LspDapExchange,
};
use crate::engine_plane::journal::EngineJournal;
use crate::journal::{EffectBoundaryKind, TypedProof};

/// How long one connection attempt or one adapter request may take. The
/// debug adapter clamps it to Godot's own 5 s per-message timeout.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest `stopped` wait one command may ask for.
pub(crate) const MAX_STOP_WAIT_MS: u64 = 60_000;
/// How many recent debug events a session's state keeps.
const RECENT_EVENTS: usize = 32;

/// The language-server commands.
pub(crate) const LANGUAGE_COMMANDS: [&str; 2] = ["diagnostics", "symbols"];
/// The debug-adapter commands.
pub(crate) const DEBUG_COMMANDS: [&str; 9] = [
    "breakpoints",
    "launch",
    "stopped",
    "stack",
    "scopes",
    "variables",
    "continue",
    "pause",
    "stop",
];
/// The action that stops an editor session itself.
pub(crate) const SESSION_STOP: &str = "editor.session.stop";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn outcome_error(kind: ErrorKind, message: impl Into<String>) -> OutcomeError {
    OutcomeError::new(kind, message, false, BTreeMap::new())
}

/// Which protocol serves `command`.
pub(crate) fn endpoint_of(command: &str) -> Option<EndpointKind> {
    if LANGUAGE_COMMANDS.contains(&command) {
        Some(EndpointKind::LanguageServer)
    } else if DEBUG_COMMANDS.contains(&command) {
        Some(EndpointKind::DebugAdapter)
    } else {
        None
    }
}

/// Why a command did not answer, and whether it may have reached the editor.
#[derive(Debug, Clone)]
pub(crate) struct CommandFailure {
    error: OutcomeError,
    /// The request was written and no usable answer came back.
    unresolved: bool,
    /// The connection is unusable; its worker ends.
    channel_lost: bool,
}

impl CommandFailure {
    fn refused(error: OutcomeError) -> Self {
        Self {
            error,
            unresolved: false,
            channel_lost: false,
        }
    }

    /// Classify an adapter failure: a refusal before anything was written, an
    /// answer that is an error, or a request with no usable answer.
    fn adapter(error: &AdapterError, localize: impl Fn(&str) -> String) -> Self {
        let (unresolved, channel_lost) = match error {
            AdapterError::Codec(_) => (true, true),
            AdapterError::Timeout { .. } | AdapterError::MalformedResponse { .. } => (true, false),
            _ => (false, false),
        };
        Self {
            error: outcome_error(error.error_kind(), localize(&error.to_string())),
            unresolved,
            channel_lost,
        }
    }
}

/// The permit gate of one journaled command: the engine permit is derived
/// from the transaction the gateway journaled for it, and authorises an
/// LSP/DAP exchange and nothing else.
struct TicketGate {
    transaction_id: String,
}

impl EffectGate for TicketGate {
    fn authorize(&self, request: &EffectRequest) -> Result<ToolPermit, String> {
        if request.boundary != EffectBoundary::LspDap {
            return Err(format!(
                "a debug command's permit covers an lsp_dap exchange, not {}",
                request.boundary.as_str()
            ));
        }
        Ok(ToolPermit::issue(&self.transaction_id, request))
    }
}

/// One command for a protocol worker.
struct Job {
    command: String,
    arguments: Map<String, Value>,
    transaction_id: String,
    reply: mpsc::SyncSender<Result<Value, CommandFailure>>,
}

/// What `debug.session.get` reports of one editor session's protocols.
#[derive(Debug, Default, Clone)]
struct DebugState {
    lsp_connected: bool,
    dap_connected: bool,
    launch_state: String,
    last_stop: Option<Value>,
    breakpoints: BTreeMap<String, Vec<u64>>,
    events: Vec<String>,
}

#[derive(Default)]
struct Workers {
    lsp: Option<mpsc::Sender<Job>>,
    dap: Option<mpsc::Sender<Job>>,
}

/// Every editor session's protocol workers.
pub(crate) struct DebugManager {
    sessions: Arc<SessionManager>,
    journal: Arc<EngineJournal>,
    admission: ClientAdmission,
    project_object_identity: String,
    generation: u64,
    workers: Mutex<BTreeMap<String, Workers>>,
    states: Mutex<BTreeMap<String, Arc<Mutex<DebugState>>>>,
}

impl std::fmt::Debug for DebugManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DebugManager")
            .field("sessions", &lock(&self.workers).len())
            .finish_non_exhaustive()
    }
}

impl DebugManager {
    pub(crate) fn new(
        sessions: Arc<SessionManager>,
        store: Arc<crate::journal::TransactionStore>,
        project_object_identity: String,
        generation: u64,
    ) -> Self {
        Self {
            sessions,
            journal: Arc::new(EngineJournal::new(store)),
            admission: ClientAdmission::new(),
            project_object_identity,
            generation,
            workers: Mutex::new(BTreeMap::new()),
            states: Mutex::new(BTreeMap::new()),
        }
    }

    /// The session manager editor sessions belong to.
    pub(crate) fn sessions(&self) -> &SessionManager {
        &self.sessions
    }

    fn project_root(&self) -> &Path {
        self.sessions.project_root()
    }

    fn root_uri(&self) -> String {
        file_uri(self.project_root())
    }

    fn state(&self, session: &str) -> Arc<Mutex<DebugState>> {
        Arc::clone(lock(&self.states).entry(session.to_owned()).or_default())
    }

    /// Editor session `id`, refused when it is a game.
    fn editor(&self, id: &str) -> Result<Arc<super::ManagedSession>, OutcomeError> {
        let session = self.sessions.session(id)?;
        if session.kind() != SessionKind::Editor {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                format!("{id} is a game session; debugging needs an editor session"),
            ));
        }
        Ok(session)
    }

    /// What `debug.session.get` answers for editor session `id`, or for every
    /// editor session without one.
    pub(crate) fn describe(&self, id: Option<&str>) -> Result<Value, OutcomeError> {
        let Some(id) = id else {
            let described: Vec<Value> = self
                .sessions
                .list()
                .into_iter()
                .filter(|session| session["kind"] == json!(SessionKind::Editor.as_str()))
                .filter_map(|session| {
                    session["session"]
                        .as_str()
                        .and_then(|id| self.describe(Some(id)).ok())
                })
                .collect();
            return Ok(json!({ "sessions": described }));
        };
        let session = self.editor(id)?;
        let state = lock(&self.state(id)).clone();
        let (lsp, dap) = session.editor_ports();
        let running = session.state() == SessionState::Running;
        let root_uri = self.root_uri();
        let registry = EndpointRegistry::new(
            self.journal.as_ref(),
            self.admission.clone(),
            &self.project_object_identity,
            &root_uri,
        );
        let owned = |port: Option<u16>, kind: EndpointKind| -> Value {
            let Some(port) = port else {
                return json!({ "requested": false });
            };
            if !running {
                return json!({
                    "requested": true,
                    "owned": false,
                    "reason": "the editor session is not running",
                });
            }
            match registry.owned(kind, &EditorEndpoint::loopback(kind, port)) {
                Ok(_) => json!({ "requested": true, "owned": true }),
                Err(error) => json!({
                    "requested": true,
                    "owned": false,
                    "reason": localize(self.project_root(), &error.to_string()),
                }),
            }
        };
        let launch_state = if state.launch_state.is_empty() {
            "idle"
        } else {
            state.launch_state.as_str()
        };
        Ok(json!({
            "session": session.public(),
            "language_server": {
                "receipt": owned(lsp, EndpointKind::LanguageServer),
                "connected": state.lsp_connected,
            },
            "debug_adapter": {
                "receipt": owned(dap, EndpointKind::DebugAdapter),
                "connected": state.dap_connected,
                "launch_state": launch_state,
                "last_stop": state.last_stop,
                "breakpoints": state.breakpoints,
                "recent_events": state.events,
            },
        }))
    }

    /// Carry out one journaled command on its protocol's worker.
    fn command(
        &self,
        session: &str,
        command: &str,
        arguments: Map<String, Value>,
        transaction_id: &str,
        timeout: Duration,
    ) -> Result<Value, CommandFailure> {
        let kind = endpoint_of(command).ok_or_else(|| {
            CommandFailure::refused(outcome_error(
                ErrorKind::InvalidInput,
                format!("{command} is not a debug command"),
            ))
        })?;
        let editor = self.editor(session).map_err(CommandFailure::refused)?;
        if editor.state() != SessionState::Running {
            return Err(CommandFailure::refused(outcome_error(
                ErrorKind::Unavailable,
                format!("{session} is {}, not running", editor.state().as_str()),
            )));
        }
        let (lsp, dap) = editor.editor_ports();
        let port = match kind {
            EndpointKind::LanguageServer => lsp,
            EndpointKind::DebugAdapter => dap,
        }
        .ok_or_else(|| {
            CommandFailure::refused(outcome_error(
                ErrorKind::Unsupported,
                format!("{session} was started without its {}", kind.as_str()),
            ))
        })?;
        let sender = self.worker(session, kind, port)?;
        let (reply, answer) = mpsc::sync_channel(1);
        sender
            .send(Job {
                command: command.to_owned(),
                arguments,
                transaction_id: transaction_id.to_owned(),
                reply,
            })
            .map_err(|_| {
                self.forget(session, kind);
                CommandFailure::refused(outcome_error(
                    ErrorKind::Unavailable,
                    format!("the {} connection ended; retry the command", kind.as_str()),
                ))
            })?;
        match answer.recv_timeout(timeout + EXCHANGE_TIMEOUT) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(failure)) => {
                if failure.channel_lost {
                    self.forget(session, kind);
                }
                Err(failure)
            }
            Err(_) => {
                // The worker is still waiting on the editor: the connection
                // is no longer trusted for the next command.
                self.forget(session, kind);
                Err(CommandFailure {
                    error: outcome_error(
                        ErrorKind::Uncertain,
                        format!("the {} did not answer {command} in time", kind.as_str()),
                    ),
                    unresolved: true,
                    channel_lost: true,
                })
            }
        }
    }

    fn worker(
        &self,
        session: &str,
        kind: EndpointKind,
        port: u16,
    ) -> Result<mpsc::Sender<Job>, CommandFailure> {
        let mut workers = lock(&self.workers);
        let entry = workers.entry(session.to_owned()).or_default();
        let slot = match kind {
            EndpointKind::LanguageServer => &mut entry.lsp,
            EndpointKind::DebugAdapter => &mut entry.dap,
        };
        if let Some(sender) = slot {
            return Ok(sender.clone());
        }
        let (sender, jobs) = mpsc::channel::<Job>();
        let context = WorkerContext {
            journal: Arc::clone(&self.journal),
            admission: self.admission.clone(),
            project_object_identity: self.project_object_identity.clone(),
            project_root: self.project_root().to_path_buf(),
            root_uri: self.root_uri(),
            generation: self.generation,
            endpoint: EditorEndpoint::loopback(kind, port),
            state: self.state(session),
        };
        std::thread::Builder::new()
            .name(format!("godot-mcp-{}-{session}", kind.as_str()))
            .spawn(move || match kind {
                EndpointKind::LanguageServer => lsp_worker(&context, &jobs),
                EndpointKind::DebugAdapter => dap_worker(&context, &jobs),
            })
            .map_err(|error| {
                CommandFailure::refused(outcome_error(ErrorKind::Internal, error.to_string()))
            })?;
        *slot = Some(sender.clone());
        Ok(sender)
    }

    /// Forget a protocol worker, so the next command connects again and
    /// verifies the receipt again.
    fn forget(&self, session: &str, kind: EndpointKind) {
        if let Some(entry) = lock(&self.workers).get_mut(session) {
            match kind {
                EndpointKind::LanguageServer => entry.lsp = None,
                EndpointKind::DebugAdapter => entry.dap = None,
            }
        }
    }

    /// End both protocol workers of `session`: its editor is going away.
    pub(crate) fn close(&self, session: &str) {
        lock(&self.workers).remove(session);
    }
}

/// The `file://` URI of `path`, spelled the way both Godot servers spell it.
pub(crate) fn file_uri(path: &Path) -> String {
    uri::file_uri(&path.display().to_string(), PathStyle::host())
}

/// `text` with every spelling of the project's absolute root replaced by
/// `res://`, so no answer names a private path.
fn localize(root: &Path, text: &str) -> String {
    localize_in(&root.display().to_string(), text, PathStyle::host())
}

fn localize_in(root: &str, text: &str, style: PathStyle) -> String {
    let spellings = uri::root_spellings(root, style);
    let separators: &[char] = match style {
        PathStyle::Unix => &['/'],
        PathStyle::Windows => &['/', '\\'],
    };
    let mut text = text.to_owned();
    for spelling in &spellings {
        for separator in separators {
            text = text.replace(&format!("{spelling}{separator}"), "res://");
        }
    }
    for spelling in &spellings {
        text = text.replace(spelling.as_str(), "res://");
    }
    text
}

/// A project-relative `res://` spelling of an absolute source path or
/// `file://` URI, or `None` for one outside the project.
fn res_path(root: &Path, source: &str) -> Option<String> {
    res_path_in(&root.display().to_string(), source, PathStyle::host())
}

fn res_path_in(root: &str, source: &str, style: PathStyle) -> Option<String> {
    if source.starts_with("res://") {
        return Some(source.to_owned());
    }
    uri::relative_to(root, &uri::path_of(source), style).map(|relative| format!("res://{relative}"))
}

struct WorkerContext {
    journal: Arc<EngineJournal>,
    admission: ClientAdmission,
    project_object_identity: String,
    project_root: PathBuf,
    root_uri: String,
    generation: u64,
    endpoint: EditorEndpoint,
    state: Arc<Mutex<DebugState>>,
}

impl WorkerContext {
    fn localize(&self, text: &str) -> String {
        localize(&self.project_root, text)
    }

    fn failure(&self, error: &AdapterError) -> CommandFailure {
        CommandFailure::adapter(error, |text| self.localize(text))
    }

    /// What is observable about the launch holding the endpoint right now, for
    /// the receipt's tuple check: the identity of the receipt's process `pid`,
    /// and the binary `program` runs, read through the path the OS names for
    /// its executable. They are one process wherever the barrier execs the
    /// engine in place.
    fn observe(&self, pid: u32, program: u32) -> ObservedEndpoint {
        let identity = procfs::process_identity(pid).ok().flatten();
        let binary_sha256 = procfs::executable(program)
            .ok()
            .flatten()
            .and_then(|path| std::fs::read(path).ok())
            .map(|bytes| hex_sha256(&bytes))
            .unwrap_or_default();
        ObservedEndpoint {
            endpoint: self.endpoint.clone(),
            pid: identity.as_ref().map(|identity| identity.pid),
            start_time: identity.and_then(|identity| identity.start_time),
            binary_sha256,
            project_root_uri: self.root_uri.clone(),
            project_generation: self.generation,
        }
    }

    /// Verify the receipt and the listener, then admit, permit and dial.
    fn connect(&self, transaction_id: &str) -> Result<ConnectedEndpoint, CommandFailure> {
        let registry = EndpointRegistry::new(
            self.journal.as_ref(),
            self.admission.clone(),
            &self.project_object_identity,
            &self.root_uri,
        );
        let kind = self.endpoint.kind;
        let receipt = registry
            .owned(kind, &self.endpoint)
            .map_err(|error| self.failure(&error))?;
        let Some(program) = self.listener_program(receipt.pid, &receipt.start_time) else {
            return Err(CommandFailure::refused(outcome_error(
                ErrorKind::Unavailable,
                format!(
                    "the {} listener on {} is not held by the editor this server launched",
                    kind.as_str(),
                    self.endpoint.authority()
                ),
            )));
        };
        registry
            .connect(
                &TicketGate {
                    transaction_id: transaction_id.to_owned(),
                },
                kind,
                &self.endpoint,
                &self.observe(receipt.pid, program),
                0,
                EXCHANGE_TIMEOUT,
            )
            .map_err(|error| self.failure(&error))
    }

    /// The process running the program the launch ran, when the endpoint's
    /// listener is held by that launch.
    fn listener_program(&self, pid: u32, start_time: &str) -> Option<u32> {
        match procfs::launch_listener(self.endpoint.port, pid, Some(start_time)) {
            Ok(procfs::LaunchListener::Launch { program }) => Some(program),
            Ok(procfs::LaunchListener::Absent | procfs::LaunchListener::Foreign) | Err(_) => None,
        }
    }
}

/// Answer `first` and everything already queued behind it with `failure`.
fn reply_all(jobs: &mpsc::Receiver<Job>, first: &Job, failure: &CommandFailure) {
    let _ = first.reply.send(Err(failure.clone()));
    while let Ok(job) = jobs.try_recv() {
        let _ = job.reply.send(Err(failure.clone()));
    }
}

fn text_argument<'a>(arguments: &'a Map<String, Value>, key: &str) -> &'a str {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn lsp_worker(context: &WorkerContext, jobs: &mpsc::Receiver<Job>) {
    let Ok(first) = jobs.recv() else {
        return;
    };
    let mut connected = match context.connect(&first.transaction_id) {
        Ok(connected) => connected,
        Err(failure) => return reply_all(jobs, &first, &failure),
    };
    let mut session = match connected.lsp(EXCHANGE_TIMEOUT) {
        Ok(session) => session,
        Err(error) => return reply_all(jobs, &first, &context.failure(&error)),
    };
    if let Err(error) = session.initialize() {
        return reply_all(jobs, &first, &context.failure(&error));
    }
    lock(&context.state).lsp_connected = true;
    let mut next = Some(first);
    while let Some(job) = next.take().or_else(|| jobs.recv().ok()) {
        let path = text_argument(&job.arguments, "path").to_owned();
        let uri = text_argument(&job.arguments, "uri").to_owned();
        let answer = if job.command == "diagnostics" {
            let text = text_argument(&job.arguments, "text");
            session
                .did_open(&uri, text)
                .and_then(|()| session.diagnostics_for(&uri))
                .map(|diagnostics| {
                    let _ = session.did_close(&uri);
                    let found: Vec<Value> = diagnostics
                        .iter()
                        // A clean file is reported as one empty sentinel.
                        .filter(|diagnostic| {
                            diagnostic.severity.is_some() || !diagnostic.message.is_empty()
                        })
                        .map(|diagnostic| {
                            json!({
                                "severity": match diagnostic.severity {
                                    Some(1) => "error",
                                    Some(2) => "warning",
                                    Some(3) => "information",
                                    _ => "hint",
                                },
                                "line": diagnostic.line.map(|line| line + 1),
                                "column": diagnostic.character.map(|column| column + 1),
                                "message": context.localize(&diagnostic.message),
                            })
                        })
                        .collect();
                    json!({ "path": path, "diagnostics": found })
                })
                .map_err(|error| context.failure(&error))
        } else {
            session
                .document_symbols(&uri)
                .map(|symbols| {
                    let found: Vec<Value> = symbols
                        .iter()
                        .map(|symbol| {
                            json!({
                                "name": symbol.name,
                                "kind": symbol.kind,
                                "detail": symbol
                                    .detail
                                    .as_deref()
                                    .map(|detail| context.localize(detail)),
                                "line": symbol.line.map(|line| line + 1),
                                "container": symbol.container,
                            })
                        })
                        .collect();
                    json!({ "path": path, "symbols": found })
                })
                .map_err(|error| context.failure(&error))
        };
        let lost = matches!(&answer, Err(failure) if failure.channel_lost);
        let _ = job.reply.send(answer);
        if lost {
            break;
        }
    }
    lock(&context.state).lsp_connected = false;
}

fn thread_of(arguments: &Map<String, Value>) -> i64 {
    arguments.get("thread").and_then(Value::as_i64).unwrap_or(1)
}

/// Only the fields of a `stopped` event body this server reports.
fn stop_facts(body: &Value) -> Value {
    let mut facts = Map::new();
    for key in [
        "reason",
        "threadId",
        "description",
        "text",
        "allThreadsStopped",
    ] {
        if let Some(value) = body.get(key) {
            facts.insert(key.to_owned(), value.clone());
        }
    }
    Value::Object(facts)
}

/// Carry out one debug-adapter command on a connected session.
fn dap_command(
    context: &WorkerContext,
    session: &mut godot_mcp_engine::adapters::dap::DapSession<'_>,
    command: &str,
    arguments: &Map<String, Value>,
) -> Result<Value, AdapterError> {
    match command {
        "breakpoints" => {
            let path = text_argument(arguments, "path").to_owned();
            let lines: Vec<u64> = arguments
                .get("lines")
                .and_then(Value::as_array)
                .map(|lines| lines.iter().filter_map(Value::as_u64).collect())
                .unwrap_or_default();
            let tool = arguments
                .get("tool")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let acknowledged =
                session.set_breakpoints(text_argument(arguments, "absolute"), &lines, tool)?;
            lock(&context.state).breakpoints.insert(path.clone(), lines);
            let acknowledged: Vec<Value> = acknowledged
                .iter()
                .map(|breakpoint| {
                    json!({
                        "verified": breakpoint.verified,
                        "line": breakpoint.line,
                        "message": breakpoint
                            .message
                            .as_deref()
                            .map(|message| context.localize(message)),
                    })
                })
                .collect();
            Ok(json!({ "path": path, "breakpoints": acknowledged }))
        }
        "launch" => {
            session.launch(false)?;
            let state = session.configuration_done()?;
            Ok(json!({ "launch_state": state.as_str() }))
        }
        "stopped" => {
            let wait = arguments
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(10_000)
                .min(MAX_STOP_WAIT_MS);
            let event = session.wait_for_event("stopped", Duration::from_millis(wait))?;
            let facts = stop_facts(event.get("body").unwrap_or(&Value::Null));
            lock(&context.state).last_stop = Some(facts.clone());
            Ok(json!({ "stopped": facts }))
        }
        "stack" => {
            let frames: Vec<Value> = session
                .stack_trace(thread_of(arguments))?
                .iter()
                .map(|frame| {
                    json!({
                        "id": frame.id,
                        "name": context.localize(&frame.name),
                        "source": frame
                            .source
                            .as_deref()
                            .and_then(|source| res_path(&context.project_root, source)),
                        "line": frame.line,
                    })
                })
                .collect();
            Ok(json!({ "frames": frames }))
        }
        "scopes" => {
            let frame = arguments.get("frame").and_then(Value::as_i64).unwrap_or(0);
            let scopes: Vec<Value> = session
                .scopes(frame)?
                .iter()
                .map(|(name, reference)| json!({ "name": name, "reference": reference }))
                .collect();
            Ok(json!({ "scopes": scopes }))
        }
        "variables" => {
            let reference = arguments
                .get("reference")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let variables: Vec<Value> = session
                .variables(reference)?
                .iter()
                .map(|variable| {
                    json!({
                        "name": variable.name,
                        "value": context.localize(&variable.value),
                        "type": variable.type_name,
                        "reference": variable.variables_reference,
                    })
                })
                .collect();
            Ok(json!({ "variables": variables }))
        }
        "continue" => {
            session.continue_execution(thread_of(arguments))?;
            Ok(json!({ "continued": true }))
        }
        "pause" => {
            session.pause(thread_of(arguments))?;
            Ok(json!({ "paused": true }))
        }
        _ => {
            let detail = session.disconnect(true)?;
            Ok(json!({ "stopped": true, "detail": detail }))
        }
    }
}

fn dap_worker(context: &WorkerContext, jobs: &mpsc::Receiver<Job>) {
    let Ok(first) = jobs.recv() else {
        return;
    };
    let mut connected = match context.connect(&first.transaction_id) {
        Ok(connected) => connected,
        Err(failure) => return reply_all(jobs, &first, &failure),
    };
    let mut session = match connected.dap(EXCHANGE_TIMEOUT) {
        Ok(session) => session,
        Err(error) => return reply_all(jobs, &first, &context.failure(&error)),
    };
    if let Err(error) = session.initialize() {
        return reply_all(jobs, &first, &context.failure(&error));
    }
    lock(&context.state).dap_connected = true;
    let mut next = Some(first);
    while let Some(job) = next.take().or_else(|| jobs.recv().ok()) {
        let answer = dap_command(context, &mut session, &job.command, &job.arguments)
            .map_err(|error| context.failure(&error));
        {
            let mut state = lock(&context.state);
            state.launch_state = session.launch_state().as_str().to_owned();
            let events = session.events();
            state.events = events[events.len().saturating_sub(RECENT_EVENTS)..].to_vec();
        }
        let ended =
            job.command == "stop" || matches!(&answer, Err(failure) if failure.channel_lost);
        let _ = job.reply.send(answer);
        if ended {
            break;
        }
    }
    let mut state = lock(&context.state);
    state.dap_connected = false;
    state.launch_state = "idle".to_owned();
}

/// The LSP/DAP effect boundary `serve` installs for managed editor sessions.
pub(crate) struct SessionDebugBoundary {
    debug: Arc<DebugManager>,
}

impl SessionDebugBoundary {
    pub(crate) const fn new(debug: Arc<DebugManager>) -> Self {
        Self { debug }
    }
}

fn unbound() -> TypedProof {
    TypedProof::EditorRuntime {
        bridge_reachable: false,
        receipt_transaction_id: None,
        receipt_action_id: None,
        live_state_probe: None,
    }
}

fn settled(status: MutationStatus, error: &OutcomeError) -> EditorEffectOutcome {
    EditorEffectOutcome {
        status,
        payload: json!({}),
        error: Some((error.kind(), error.message().to_owned())),
        proof: unbound(),
    }
}

impl LspDapEffectBoundary for SessionDebugBoundary {
    fn available(&self) -> bool {
        true
    }

    fn connected(&self) -> bool {
        self.debug.sessions.knows_a_session_of(SessionKind::Editor)
    }

    fn running(&self) -> bool {
        self.debug.sessions.runs_a_session_of(SessionKind::Editor)
    }

    fn exchange(
        &self,
        _ticket: &EffectTicket,
        _exchange: &LspDapExchange,
    ) -> Result<String, BoundaryUnavailable> {
        // Every exchange is a typed debug command carried out by `apply`.
        Err(BoundaryUnavailable {
            boundary: "lsp_dap",
        })
    }

    fn apply(
        &self,
        ticket: &EffectTicket,
        command: &EditorMutation,
    ) -> Result<EditorEffectOutcome, BoundaryUnavailable> {
        if ticket.boundary() != EffectBoundaryKind::LspDap || ticket.transaction_id().is_empty() {
            return Err(BoundaryUnavailable {
                boundary: "lsp_dap",
            });
        }
        let mut arguments: Map<String, Value> = match serde_json::from_str(&command.payload) {
            Ok(Value::Object(object)) => object,
            _ => Map::new(),
        };
        let Some(Value::String(session)) = arguments.remove("session") else {
            return Ok(settled(
                MutationStatus::NotStarted,
                &outcome_error(ErrorKind::InvalidInput, "a debug command names its session"),
            ));
        };
        if command.action == SESSION_STOP {
            // The protocol connections end before the editor does.
            self.debug.close(&session);
            return Ok(
                match self.debug.sessions.stop(&session, &command.idempotency_id) {
                    Ok(value) => EditorEffectOutcome {
                        status: MutationStatus::Committed,
                        proof: receipt(
                            ticket,
                            &command.action,
                            &json!({
                                "digest": value.get("state").and_then(Value::as_str).unwrap_or("stopped")
                            }),
                        ),
                        payload: value,
                        error: None,
                    },
                    Err(error) if error.kind() == ErrorKind::Uncertain => {
                        settled(MutationStatus::Unresolved, &error)
                    }
                    Err(error) => settled(MutationStatus::NotStarted, &error),
                },
            );
        }
        Ok(
            match self.debug.command(
                &session,
                &command.action,
                arguments,
                ticket.transaction_id(),
                command.timeout,
            ) {
                Ok(payload) => EditorEffectOutcome {
                    status: MutationStatus::Committed,
                    proof: receipt(
                        ticket,
                        &command.action,
                        &json!({ "digest": hex_sha256(payload.to_string().as_bytes()) }),
                    ),
                    payload,
                    error: None,
                },
                Err(failure) if failure.unresolved => {
                    settled(MutationStatus::Unresolved, &failure.error)
                }
                Err(failure) => settled(MutationStatus::NotStarted, &failure.error),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_belongs_to_exactly_one_protocol() {
        for command in LANGUAGE_COMMANDS {
            assert_eq!(endpoint_of(command), Some(EndpointKind::LanguageServer));
        }
        for command in DEBUG_COMMANDS {
            assert_eq!(endpoint_of(command), Some(EndpointKind::DebugAdapter));
        }
        assert_eq!(endpoint_of("evaluate"), None, "no expression evaluation");
        assert_eq!(endpoint_of(SESSION_STOP), None);
    }

    #[test]
    fn answers_never_name_the_private_project_root() {
        let root = Path::new("/private/state/project");
        assert_eq!(
            localize(root, "Parse error in /private/state/project/player.gd:3"),
            "Parse error in res://player.gd:3"
        );
        assert_eq!(
            localize(root, "file:///private/state/project/a.gd"),
            "res://a.gd"
        );
        assert_eq!(
            res_path(root, "/private/state/project/levels/one.gd"),
            Some("res://levels/one.gd".to_owned())
        );
        assert_eq!(
            res_path(root, "res://already.gd"),
            Some("res://already.gd".to_owned())
        );
        assert_eq!(res_path(root, "/elsewhere/engine.gd"), None);
    }

    /// Godot 4.5+ writes `file:///C%3A/...` on Windows, 4.4 echoes this
    /// server's root URI, the debug adapter reports `C:/...` frames, and the
    /// server's own messages carry the native `C:\...` path: none of them
    /// may reach an answer.
    #[test]
    fn a_windows_project_root_is_never_named_in_any_of_its_spellings() {
        let root = r"C:\Users\qa\AppData\Local\Temp\m5\game";
        let style = PathStyle::Windows;
        for (text, expected) in [
            (
                "file:///C%3A/Users/qa/AppData/Local/Temp/m5/game/broken.gd",
                "res://broken.gd",
            ),
            (
                "file:///C:/Users/qa/AppData/Local/Temp/m5/game/a.gd",
                "res://a.gd",
            ),
            (
                "Parse error in C:/Users/qa/AppData/Local/Temp/m5/game/player.gd:3",
                "Parse error in res://player.gd:3",
            ),
            (
                r"cannot open C:\Users\qa\AppData\Local\Temp\m5\game\x.gd",
                "cannot open res://x.gd",
            ),
            (
                r"root C:\Users\qa\AppData\Local\Temp\m5\game",
                "root res://",
            ),
        ] {
            assert_eq!(localize_in(root, text, style), expected, "{text}");
        }
        for (source, expected) in [
            (
                "C:/Users/qa/AppData/Local/Temp/m5/game/player.gd",
                Some("res://player.gd"),
            ),
            (
                "file:///C%3A/Users/qa/AppData/Local/Temp/m5/game/levels/one.gd",
                Some("res://levels/one.gd"),
            ),
            (
                "c:/users/qa/appdata/local/temp/m5/game/a.gd",
                Some("res://a.gd"),
            ),
            ("C:/godot/modules/gdscript/x.gd", None),
        ] {
            assert_eq!(
                res_path_in(root, source, style).as_deref(),
                expected,
                "{source}"
            );
        }
        assert_eq!(
            uri::file_uri(&format!(r"{root}\player.gd"), style),
            "file:///C%3A/Users/qa/AppData/Local/Temp/m5/game/player.gd"
        );
    }

    #[test]
    fn a_ticket_gate_authorises_an_lsp_dap_exchange_only() {
        let gate = TicketGate {
            transaction_id: "transaction-1".to_owned(),
        };
        let mut request = godot_mcp_engine::adapters::connect::exchange_request(
            EndpointKind::DebugAdapter,
            "project_abc",
            3,
            "stackTrace",
        );
        let permit = gate.authorize(&request).expect("an lsp_dap exchange");
        assert_eq!(permit.transaction_id(), "transaction-1");
        request.boundary = EffectBoundary::Process;
        assert!(gate.authorize(&request).is_err());
    }

    #[test]
    fn an_adapter_failure_says_whether_the_request_may_have_reached_the_editor() {
        let identity = |text: &str| text.to_owned();
        let timeout = CommandFailure::adapter(
            &AdapterError::Timeout {
                method: "continue".to_owned(),
                millis: 5000,
            },
            identity,
        );
        assert!(timeout.unresolved && !timeout.channel_lost);
        let refused = CommandFailure::adapter(
            &AdapterError::unavailable("stack needs a running debuggee"),
            identity,
        );
        assert!(!refused.unresolved && !refused.channel_lost);
        let lost = CommandFailure::adapter(
            &AdapterError::Codec(godot_mcp_engine::adapters::codec::CodecError::Eof {
                read: 0,
                expected: 1,
            }),
            identity,
        );
        assert!(lost.unresolved && lost.channel_lost);
    }
}
