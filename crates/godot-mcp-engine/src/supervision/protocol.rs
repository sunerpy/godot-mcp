//! The gapless launch protocol.
//!
//! There is **no window** between process creation and the durable
//! `effect_started_fsynced` receipt: the boundary (or the weak path's inert
//! locator) is pre-created and its identity is fsynced as `intent_fsynced`
//! *before* anything is created, every created process is barrier-gated so it
//! cannot run user code, and the gate is opened only after the complete identity
//! receipt is durable.
//!
//! The normative order, identical for both platform paths:
//!
//! | step | meaning |
//! | --- | --- |
//! | pre-create the boundary/locator | a stable named object, or inert private locator state; no process |
//! | `intent_fsynced` | durably binds that already-created boundary identity |
//! | create the anchor | strong: the barrier-gated child inside the boundary; weak: the blocked minimal supervisor |
//! | `staged_fsynced` | confirmed created, provably not running user code |
//! | create + confirm the gated child | strong: the anchor itself; weak: created behind a second gate |
//! | `effect_started_fsynced` | containment proven and the full receipt fsynced, strictly before any release |
//! | release | open the gate |
//! | `committed_fsynced` | the release/resume completed and was confirmed |
//!
//! Fault hooks ([`FaultInjection`]) can inject an ordinary failure, a simulated
//! crash or a supervisor `SIGKILL` at every one of the six [`CrashStage`] points,
//! which is what makes the crash-recovery state machine testable rather than
//! merely documented.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use godot_mcp_domain::ErrorKind;
use serde_json::{Value, json};

use super::boundary::{
    Anchor, BoundaryError, BoundaryIdentity, BoundaryProvider, BoundaryStrength,
    ContainmentBoundary, ContainmentState, GatedChild, SpawnPlan, TrustLevel, WaitOutcome,
};
use super::gate::{self, CapturedStream, LaunchPaths};
use super::journal::{
    BoundaryMembership, Disposition, JournalError, LAUNCH_ABANDONED_PRE_RELEASE, LaunchJournal,
    LaunchPayload, LaunchPhase, LaunchRecord, ProcessLaunchProof, ReleaseStatus,
    TransactionEnvelope, hex_sha256,
};
use super::procfs;

/// Durable launch-payload fact keys. Reconciliation reads exactly these.
pub mod facts {
    /// The gate artifact's path, derived deterministically from the work
    /// directory and transaction ID, so it is durable from `intent_fsynced` on.
    pub const GATE_PATH: &str = "gate_path";
    /// SHA-256 of the revalidated executable.
    pub const BINARY_SHA256: &str = "binary_sha256";
    /// The project generation the launch revalidated.
    pub const PROJECT_GENERATION: &str = "project_generation";
    /// The held project root's OS object identity.
    pub const PROJECT_ROOT_OBJECT: &str = "project_root_object";
    /// The held `project.godot`'s OS object identity.
    pub const PROJECT_FILE_OBJECT: &str = "project_file_object";
    /// The planned endpoint claim, recorded before anything is created.
    pub const PLANNED_ENDPOINT: &str = "planned_control_endpoint";
    /// The observed endpoint claim, recorded once the anchor exists.
    pub const ACTUAL_ENDPOINT: &str = "actual_control_endpoint";
    /// The complete argument array, never a shell command string.
    pub const ARGV: &str = "argv";
    /// The anchor's PID.
    pub const ANCHOR_PID: &str = "supervisor_pid";
    /// The anchor's process start time. Paired with the PID and PGID; never used
    /// alone, and a bare PGID is never used alone either.
    pub const ANCHOR_START_TIME: &str = "supervisor_start_time";
    /// The anchor's process group, weak path only.
    pub const ANCHOR_PROCESS_GROUP: &str = "supervisor_process_group";
    /// The gated child's PID.
    pub const CHILD_PID: &str = "child_pid";
    /// The gated child's process start time.
    pub const CHILD_START_TIME: &str = "child_start_time";
    /// Whether boundary membership was confirmed by live inspection.
    pub const MEMBERSHIP_CONFIRMED: &str = "boundary_membership_confirmed";
}

/// The exact environment a supervised process may see. Nothing else is inherited.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnvironmentAllowlist {
    /// Names copied from the current environment when present.
    pub allow: BTreeSet<String>,
    /// Additional name/value pairs set explicitly for this launch.
    pub extra: BTreeMap<String, String>,
}

impl EnvironmentAllowlist {
    /// The resolved environment, sorted so a launch is byte-reproducible.
    #[must_use]
    pub fn resolve(&self) -> Vec<(String, String)> {
        let mut resolved: BTreeMap<String, String> = BTreeMap::new();
        for name in &self.allow {
            if let Ok(value) = std::env::var(name) {
                resolved.insert(name.clone(), value);
            }
        }
        for (name, value) in &self.extra {
            resolved.insert(name.clone(), value.clone());
        }
        resolved.into_iter().collect()
    }
}

/// Byte budgets for a supervised process's captured output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputCaps {
    /// Maximum retained stdout bytes.
    pub stdout_bytes: usize,
    /// Maximum retained stderr bytes.
    pub stderr_bytes: usize,
}

/// One supervised launch request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRequest {
    /// The canonical transaction envelope this launch is journaled under.
    pub envelope: TransactionEnvelope,
    /// Whether the code being launched is trusted.
    pub trust: TrustLevel,
    /// The executable, already discovered by Todo 13 and revalidated by hash here.
    pub program: PathBuf,
    /// The argument array. Never a shell command string.
    pub arguments: Vec<String>,
    /// The expected SHA-256 of `program`.
    pub binary_sha256: String,
    /// The bundled `godot-mcp-supervisor` binary that provides the exec barrier and
    /// the weak path's minimal supervisor. Passed explicitly rather than discovered
    /// from the environment, so two launches in one process can never disagree
    /// about which barrier they gated behind.
    pub supervisor_binary: PathBuf,
    /// The held project root's display path, for the child's working directory.
    pub project_root_display: PathBuf,
    /// Exactly what the child's environment may contain.
    pub environment: EnvironmentAllowlist,
    /// Output byte caps.
    pub caps: OutputCaps,
    /// Wall-clock budget for the released child.
    pub deadline: Duration,
    /// Scratch directory for this launch's transient artifacts.
    pub work_dir: PathBuf,
    /// A path the caller expects the released child to produce, used only by
    /// observers to check that nothing ran before its gate.
    pub evidence_path: Option<PathBuf>,
}

/// How a supervised process ended, as structured diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminationKind {
    /// Exited with status 0.
    ExitedClean,
    /// Exited with a nonzero status.
    NonzeroExit {
        /// The exit status.
        code: i32,
    },
    /// Terminated by a signal.
    Signaled {
        /// The signal's stable spelling.
        signal: String,
    },
    /// The supervisor's deadline, not the child, ended the run.
    TimedOut,
}

impl TerminationKind {
    fn of(outcome: &WaitOutcome) -> Self {
        if outcome.timed_out {
            return Self::TimedOut;
        }
        match (outcome.exit_code, &outcome.signal) {
            (Some(0), None) => Self::ExitedClean,
            (Some(code), None) => Self::NonzeroExit { code },
            (_, Some(signal)) => Self::Signaled {
                signal: signal.clone(),
            },
            (None, None) => Self::Signaled {
                signal: "unknown".to_owned(),
            },
        }
    }

    const fn release_status(&self) -> ReleaseStatus {
        match self {
            Self::ExitedClean => ReleaseStatus::ExitedClean,
            Self::NonzeroExit { .. } => ReleaseStatus::NonzeroExit,
            Self::Signaled { .. } | Self::TimedOut => ReleaseStatus::Signaled,
        }
    }
}

/// Structured exit/signal/timeout/truncation diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchDiagnostics {
    /// How the process ended.
    pub termination: TerminationKind,
    /// Exit status, when it exited normally.
    pub exit_code: Option<i32>,
    /// Signal spelling, when it was signalled.
    pub signal: Option<String>,
    /// Captured stdout, capped.
    pub stdout: CapturedStream,
    /// Captured stderr, capped.
    pub stderr: CapturedStream,
}

/// One completed launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOutcome {
    /// The transaction this launch was journaled under.
    pub transaction_id: String,
    /// The boundary the launch used, for reopening or proving empty.
    pub boundary: BoundaryIdentity,
    /// The boundary's concrete identity string, as bound into the proof.
    pub boundary_identity: String,
    /// How strongly the launch was contained.
    pub strength: BoundaryStrength,
    /// The supervised child's PID.
    pub child_pid: u32,
    /// The revalidated executable hash.
    pub binary_sha256: String,
    /// The argument array actually issued.
    pub argv: Vec<String>,
    /// The typed durable proof written for this launch.
    pub proof: ProcessLaunchProof,
    /// Structured diagnostics.
    pub diagnostics: LaunchDiagnostics,
}

/// The six points at which a fault can be injected. These are exactly the
/// windows the crash-recovery state machine must resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CrashStage {
    /// The boundary/locator exists; nothing is durable and no process exists.
    BeforeIntentFsync,
    /// The intent is durable; nothing has been created yet.
    AfterIntentBeforeChildCreation,
    /// The anchor exists; `staged_fsynced` is not durable yet.
    AfterChildCreationBeforeStagedFsync,
    /// The anchor is durable and the gated child exists, still blocked; the
    /// receipt is not durable yet.
    AfterStagedBeforeReceiptFsync,
    /// The receipt is durable; the gate has not been opened.
    AfterReceiptBeforeRelease,
    /// The gate is open; `committed_fsynced` is not durable yet.
    AfterReleaseBeforeCommitFsync,
}

impl CrashStage {
    /// Every stage, in protocol order.
    pub const ALL: [Self; 6] = [
        Self::BeforeIntentFsync,
        Self::AfterIntentBeforeChildCreation,
        Self::AfterChildCreationBeforeStagedFsync,
        Self::AfterStagedBeforeReceiptFsync,
        Self::AfterReceiptBeforeRelease,
        Self::AfterReleaseBeforeCommitFsync,
    ];

    /// Stable spelling for diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BeforeIntentFsync => "before_intent_fsync",
            Self::AfterIntentBeforeChildCreation => "after_intent_before_child_creation",
            Self::AfterChildCreationBeforeStagedFsync => "after_child_creation_before_staged_fsync",
            Self::AfterStagedBeforeReceiptFsync => "after_staged_before_receipt_fsync",
            Self::AfterReceiptBeforeRelease => "after_receipt_before_release",
            Self::AfterReleaseBeforeCommitFsync => "after_release_before_commit_fsync",
        }
    }
}

/// What an injected fault does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultMode {
    /// Terminate the launch the way a process death does: nothing is cleaned up
    /// and no disposition is recorded, so restart reconciliation must resolve it.
    Crash,
    /// An ordinary failure: the launch's own error handling runs.
    Error,
    /// `SIGKILL` the anchor/supervisor and let the launch continue, which is the
    /// weak mode's documented residual risk.
    SigkillSupervisor,
}

impl FaultMode {
    /// Every mode. There are three.
    pub const ALL: [Self; 3] = [Self::Crash, Self::Error, Self::SigkillSupervisor];
}

/// One injected fault, or none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FaultInjection {
    point: Option<(CrashStage, FaultMode)>,
}

impl FaultInjection {
    /// No fault.
    #[must_use]
    pub const fn none() -> Self {
        Self { point: None }
    }

    /// Simulate a process death at `stage`.
    #[must_use]
    pub const fn crash_at(stage: CrashStage) -> Self {
        Self {
            point: Some((stage, FaultMode::Crash)),
        }
    }

    /// Inject an ordinary failure at `stage`.
    #[must_use]
    pub const fn error_at(stage: CrashStage) -> Self {
        Self {
            point: Some((stage, FaultMode::Error)),
        }
    }

    /// `SIGKILL` the supervisor at `stage`.
    #[must_use]
    pub const fn sigkill_supervisor_at(stage: CrashStage) -> Self {
        Self {
            point: Some((stage, FaultMode::SigkillSupervisor)),
        }
    }

    /// The mode injected at `stage`, if any.
    #[must_use]
    pub fn at(&self, stage: CrashStage) -> Option<FaultMode> {
        self.point
            .and_then(|(point, mode)| (point == stage).then_some(mode))
    }
}

/// One observable protocol event, in protocol order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchEvent {
    /// The boundary/locator was created before anything else.
    BoundaryPrecreated,
    /// One canonical phase became durable.
    Phase(LaunchPhase),
    /// The anchor process was created.
    MemberCreated,
    /// Boundary membership was confirmed by live inspection.
    MembershipConfirmed,
    /// The gate was opened.
    GateReleased,
    /// The supervised child ended.
    ChildExited,
    /// The boundary was proven empty and closed.
    BoundaryClosed,
}

impl LaunchEvent {
    /// Stable label, used by tests to assert the whole ordering at once.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::BoundaryPrecreated => "boundary_precreated",
            Self::Phase(LaunchPhase::IntentFsynced) => "phase:intent_fsynced",
            Self::Phase(LaunchPhase::StagedFsynced) => "phase:staged_fsynced",
            Self::Phase(LaunchPhase::EffectStartedFsynced) => "phase:effect_started_fsynced",
            Self::Phase(LaunchPhase::CommittedFsynced) => "phase:committed_fsynced",
            Self::MemberCreated => "member_created",
            Self::MembershipConfirmed => "membership_confirmed",
            Self::GateReleased => "gate_released",
            Self::ChildExited => "child_exited",
            Self::BoundaryClosed => "boundary_closed",
        }
    }
}

/// What the world looks like at one [`CrashStage`].
#[derive(Debug, Clone, Copy)]
pub struct LaunchView<'a> {
    /// The transaction being launched.
    pub transaction_id: &'a str,
    /// The last durable phase, if any.
    pub durable_phase: Option<LaunchPhase>,
    /// The anchor's PID, once it exists.
    pub anchor_pid: Option<u32>,
    /// The gated child's PID, once it exists.
    pub child_pid: Option<u32>,
    /// The gate artifact.
    pub gate_path: &'a Path,
    /// A path the released child is expected to produce.
    pub evidence_path: Option<&'a Path>,
}

/// How far the launch has progressed, owned separately from the session so an
/// observer hook can inspect the world while the protocol still mutates it.
#[derive(Debug, Clone)]
struct Progress {
    transaction_id: String,
    gate_path: PathBuf,
    evidence_path: Option<PathBuf>,
    durable_phase: Option<LaunchPhase>,
    anchor_pid: Option<u32>,
    child_pid: Option<u32>,
}

impl Progress {
    fn view(&self) -> LaunchView<'_> {
        LaunchView {
            transaction_id: &self.transaction_id,
            durable_phase: self.durable_phase,
            anchor_pid: self.anchor_pid,
            child_pid: self.child_pid,
            gate_path: &self.gate_path,
            evidence_path: self.evidence_path.as_deref(),
        }
    }
}

/// Observes the launch protocol. The default implementation observes nothing, so
/// production callers pay nothing for the hooks the tests need.
pub trait LaunchObserver {
    /// One protocol event happened.
    fn event(&self, event: &LaunchEvent) {
        let _ = event;
    }

    /// The protocol reached one crash stage.
    fn stage(&self, stage: CrashStage, view: &LaunchView<'_>) {
        let _ = (stage, view);
    }
}

/// An observer that ignores everything.
#[derive(Debug, Clone, Copy, Default)]
pub struct SilentObserver;

impl LaunchObserver for SilentObserver {}

/// A launch failure.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    /// An untrusted launch had no strong boundary available. It never falls back
    /// to the weak anchor protocol.
    #[error("untrusted launch refused: no strong containment boundary is available ({refusals})")]
    StrongBoundaryUnavailable {
        /// Why each candidate provider was refused.
        refusals: String,
    },
    /// No boundary at all was available.
    #[error("launch refused: no containment boundary is available ({refusals})")]
    NoBoundary {
        /// Why each candidate provider was refused.
        refusals: String,
    },
    /// The executable no longer matches its revalidated hash.
    #[error("executable {path} no longer matches its revalidated hash")]
    BinaryChanged {
        /// The display path of the executable.
        path: String,
    },
    /// Containment could not be confirmed, so the gate was never opened.
    #[error("containment could not be confirmed for pid {pid} in boundary {boundary}")]
    ContainmentUnconfirmed {
        /// The child's PID.
        pid: u32,
        /// The boundary's identity.
        boundary: String,
    },
    /// The weak path's supervisor was lost. The documented residual risk: after
    /// release its process group offers no further guarantee.
    #[error("supervisor was lost after release; containment is no longer guaranteed: {reason}")]
    SupervisorLost {
        /// What was observed.
        reason: String,
    },
    /// A boundary operation failed.
    #[error(transparent)]
    Boundary(#[from] BoundaryError),
    /// A durable journal write failed. The launch never proceeds past a phase
    /// whose mark is not durable.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// A deliberately injected fault.
    #[error("injected {mode:?} fault at {stage}")]
    InjectedFault {
        /// Where the fault was injected.
        stage: &'static str,
        /// What the fault did.
        mode: FaultMode,
    },
}

impl LaunchError {
    /// Map to the stable domain error taxonomy.
    #[must_use]
    pub fn error_kind(&self) -> ErrorKind {
        match self {
            Self::StrongBoundaryUnavailable { .. } | Self::NoBoundary { .. } => {
                ErrorKind::Unsupported
            }
            Self::BinaryChanged { .. } => ErrorKind::Conflict,
            Self::ContainmentUnconfirmed { .. } | Self::SupervisorLost { .. } => {
                ErrorKind::Uncertain
            }
            Self::Boundary(error) => error.error_kind(),
            Self::Journal(_) | Self::InjectedFault { .. } => ErrorKind::Internal,
        }
    }
}

/// What an abort must do with the state it already created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AbortPolicy {
    /// Leave everything exactly as a process death would: no cleanup, no
    /// disposition. Restart reconciliation resolves it.
    Leak,
    /// Run the launch's own cleanup: terminate, prove empty, close, compensate.
    Compensate,
    /// Neither prove nor clean: leave the transaction open so reconciliation must
    /// resolve it, because containment can no longer be proven.
    LeaveOpen,
}

struct Abort {
    error: LaunchError,
    policy: AbortPolicy,
}

impl Abort {
    const fn compensate(error: LaunchError) -> Self {
        Self {
            error,
            policy: AbortPolicy::Compensate,
        }
    }
}

/// One launch's mutable state, so an abort can act on exactly what exists.
struct Session {
    paths: LaunchPaths,
    boundary: Option<Box<dyn ContainmentBoundary>>,
    anchor: Option<Anchor>,
    record: Option<LaunchRecord>,
    payloads: Vec<LaunchPayload>,
}

/// What the shared steps leave once the gate is open.
struct Released {
    transaction_id: String,
    boundary_identity: BoundaryIdentity,
    gated: GatedChild,
    argv: Vec<String>,
}

/// A launch released into a session that outlives the call that started it.
///
/// [`LaunchProtocol::start`] returns one once `committed_fsynced` records the
/// child as resumed and contained. It owns the boundary and the anchor; the
/// launch journal is not written again. [`Self::wait`] ends the session: the
/// child's own exit, or its lifetime, after which the whole boundary is
/// terminated. Something holding only the durable [`Self::boundary`] identity
/// can stop it at any time through [`BoundaryProvider::reopen`], exactly as
/// restart reconciliation does.
pub struct RunningLaunch {
    transaction_id: String,
    paths: LaunchPaths,
    boundary: Box<dyn ContainmentBoundary>,
    anchor: Anchor,
    gated: GatedChild,
    identity: BoundaryIdentity,
    binary_sha256: String,
    argv: Vec<String>,
    caps: OutputCaps,
}

impl std::fmt::Debug for RunningLaunch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunningLaunch")
            .field("transaction_id", &self.transaction_id)
            .field("child_pid", &self.gated.pid)
            .finish_non_exhaustive()
    }
}

/// How a running launch ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchExit {
    /// The terminal proof: exit facts and whether the boundary was proven
    /// empty. The launch journal already holds the running proof; this one
    /// belongs to whoever records the session.
    pub proof: ProcessLaunchProof,
    /// Structured diagnostics, with both streams capped.
    pub diagnostics: LaunchDiagnostics,
    /// Whether the boundary was closed after it was proven empty.
    pub boundary_closed: bool,
}

impl RunningLaunch {
    /// The launch transaction.
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    /// The boundary's durable identity, enough to reopen and stop it.
    #[must_use]
    pub const fn boundary(&self) -> &BoundaryIdentity {
        &self.identity
    }

    /// How strongly the launch is contained.
    #[must_use]
    pub fn strength(&self) -> BoundaryStrength {
        self.identity.strength()
    }

    /// The released child's PID.
    #[must_use]
    pub const fn child_pid(&self) -> u32 {
        self.gated.pid
    }

    /// The released child's process start time, when observable.
    #[must_use]
    pub fn child_start_time(&self) -> Option<&str> {
        self.gated.start_time.as_deref()
    }

    /// The revalidated executable hash.
    #[must_use]
    pub fn binary_sha256(&self) -> &str {
        &self.binary_sha256
    }

    /// The argument array actually issued, program first.
    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// Where the child's stdout is being captured, for reading while it runs.
    #[must_use]
    pub fn stdout_path(&self) -> &Path {
        &self.paths.stdout
    }

    /// Where the child's stderr is being captured, for reading while it runs.
    #[must_use]
    pub fn stderr_path(&self) -> &Path {
        &self.paths.stderr
    }

    /// Wait for the session to end, at most `lifetime`.
    ///
    /// The child ending on its own, or being stopped from its recorded
    /// boundary identity, ends the wait. Past `lifetime` the whole boundary is
    /// terminated. Either way the boundary is then inspected, and closed once
    /// proven empty.
    ///
    /// # Errors
    /// Returns [`LaunchError::SupervisorLost`] when the weak path's supervisor
    /// vanished and its process group cannot be proven empty, and
    /// [`LaunchError::Boundary`] when waiting or inspection fails.
    pub fn wait(&mut self, lifetime: Duration) -> Result<LaunchExit, LaunchError> {
        let waited = match self.boundary.wait(&mut self.anchor, lifetime) {
            Ok(waited) => waited,
            // A stop through the recorded identity ends the weak path's
            // supervisor together with its group, so its channel closes
            // without an exit report: the session ended, its status unknown.
            Err(BoundaryError::SupervisorLost(reason)) => {
                if let Some(group) = self.anchor.process_group {
                    let _ = self.boundary.terminate_recorded_group(group);
                }
                match self.boundary.state().map_err(LaunchError::Boundary)? {
                    ContainmentState::Empty => WaitOutcome {
                        exit_code: None,
                        signal: None,
                        timed_out: false,
                    },
                    _ => return Err(LaunchError::SupervisorLost { reason }),
                }
            }
            Err(error) => return Err(LaunchError::Boundary(error)),
        };
        if waited.timed_out {
            self.boundary
                .terminate_all()
                .map_err(LaunchError::Boundary)?;
        }
        let termination = TerminationKind::of(&waited);
        let empty = matches!(
            self.boundary.state().map_err(LaunchError::Boundary)?,
            ContainmentState::Empty
        );
        let boundary_closed = empty && self.boundary.close().is_ok();
        Ok(LaunchExit {
            proof: ProcessLaunchProof {
                boundary_identity: self.identity.identity.clone(),
                pid: Some(self.gated.pid),
                start_time: self.gated.start_time.clone(),
                boundary_membership: BoundaryMembership::Confirmed,
                release_status: termination.release_status(),
                exit_code: waited.exit_code.map(i64::from),
                signal: waited.signal.clone(),
                boundary_empty_after_exit: Some(empty),
            },
            diagnostics: LaunchDiagnostics {
                termination,
                exit_code: waited.exit_code,
                signal: waited.signal,
                stdout: gate::capture(&self.paths.stdout, self.caps.stdout_bytes),
                stderr: gate::capture(&self.paths.stderr, self.caps.stderr_bytes),
            },
            boundary_closed,
        })
    }

    /// Remove the launch's scratch: its gate and both captured streams. Call it
    /// once whatever must outlive the session was copied out of them.
    ///
    /// # Errors
    /// Returns the filesystem error when the scratch directory cannot be removed.
    pub fn remove_scratch(&self) -> std::io::Result<()> {
        self.paths.remove_scratch()
    }
}

/// Drives the gapless launch protocol against one durable journal.
pub struct LaunchProtocol<'j> {
    journal: &'j dyn LaunchJournal,
}

impl<'j> LaunchProtocol<'j> {
    /// Bind the protocol to Todo 6's durable journal.
    #[must_use]
    pub const fn new(journal: &'j dyn LaunchJournal) -> Self {
        Self { journal }
    }

    /// Run one launch end to end.
    ///
    /// # Errors
    /// Returns [`LaunchError`] when the launch is refused before spawn, when a
    /// durable write fails, when containment cannot be confirmed, or when a fault
    /// is injected. In every failure case either the boundary is proven empty and
    /// the transaction is compensated, or the transaction is left open for
    /// reconciliation — never a released child with no receipt.
    pub fn launch(
        &self,
        request: &LaunchRequest,
        provider: &dyn BoundaryProvider,
        fault: &FaultInjection,
        observer: &dyn LaunchObserver,
    ) -> Result<LaunchOutcome, LaunchError> {
        let mut session = Session {
            paths: LaunchPaths::for_transaction(
                &request.work_dir,
                &request.envelope.transaction_id,
            ),
            boundary: None,
            anchor: None,
            record: None,
            payloads: Vec::new(),
        };
        match self.run(&mut session, request, provider, fault, observer) {
            Ok(outcome) => Ok(outcome),
            Err(abort) => Err(self.abort(session, abort)),
        }
    }

    /// Start one launch that outlives this call: a managed session.
    ///
    /// The protocol runs exactly as [`Self::launch`] does up to and including
    /// the release, then records `committed_fsynced` with the child
    /// `resumed-contained` — the durable proof that a released, contained
    /// process is running, which is what an owned editor endpoint's receipt
    /// requires — and returns without waiting for the child. The returned
    /// [`RunningLaunch`] owns the boundary; [`RunningLaunch::wait`] ends it.
    ///
    /// # Errors
    /// Returns [`LaunchError`] exactly as [`Self::launch`] does for every step up
    /// to the release, and when `committed_fsynced` cannot be made durable, in
    /// which case the released child is terminated and the launch compensated.
    pub fn start(
        &self,
        request: &LaunchRequest,
        provider: &dyn BoundaryProvider,
        fault: &FaultInjection,
        observer: &dyn LaunchObserver,
    ) -> Result<RunningLaunch, LaunchError> {
        let mut session = Session {
            paths: LaunchPaths::for_transaction(
                &request.work_dir,
                &request.envelope.transaction_id,
            ),
            boundary: None,
            anchor: None,
            record: None,
            payloads: Vec::new(),
        };
        let released = match self.run_to_release(&mut session, request, provider, fault, observer) {
            Ok(released) => released,
            Err(abort) => return Err(self.abort(session, abort)),
        };
        let running = ProcessLaunchProof {
            boundary_identity: released.boundary_identity.identity.clone(),
            pid: Some(released.gated.pid),
            start_time: released.gated.start_time.clone(),
            boundary_membership: BoundaryMembership::Confirmed,
            release_status: ReleaseStatus::ResumedContained,
            exit_code: None,
            signal: None,
            boundary_empty_after_exit: None,
        };
        if let Err(abort) = self.write(
            &mut session,
            &request.envelope,
            LaunchPhase::CommittedFsynced,
            Some(running),
        ) {
            return Err(self.abort(session, abort));
        }
        observer.event(&LaunchEvent::Phase(LaunchPhase::CommittedFsynced));
        let Session {
            paths,
            boundary,
            anchor,
            ..
        } = session;
        Ok(RunningLaunch {
            transaction_id: released.transaction_id,
            paths,
            boundary: boundary.expect("the boundary was created before the release"),
            anchor: anchor.expect("the anchor was created before the release"),
            gated: released.gated,
            identity: released.boundary_identity,
            binary_sha256: request.binary_sha256.clone(),
            argv: released.argv,
            caps: request.caps,
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the normative launch ordering is one sequence; splitting it would hide the very \
                  ordering this todo exists to guarantee"
    )]
    fn run_to_release(
        &self,
        session: &mut Session,
        request: &LaunchRequest,
        provider: &dyn BoundaryProvider,
        fault: &FaultInjection,
        observer: &dyn LaunchObserver,
    ) -> Result<Released, Abort> {
        let transaction_id = request.envelope.transaction_id.clone();

        // Untrusted code never uses the weak anchor protocol, and never falls back
        // to it when no strong boundary is available.
        if request.trust == TrustLevel::Untrusted && provider.strength() != BoundaryStrength::Strong
        {
            return Err(Abort {
                error: LaunchError::StrongBoundaryUnavailable {
                    refusals: format!(
                        "{}: weak containment is never used for untrusted code",
                        provider.kind().as_str()
                    ),
                },
                policy: AbortPolicy::LeaveOpen,
            });
        }
        provider.available().map_err(|error| Abort {
            error: if request.trust == TrustLevel::Untrusted {
                LaunchError::StrongBoundaryUnavailable {
                    refusals: format!("{}: {error}", provider.kind().as_str()),
                }
            } else {
                LaunchError::NoBoundary {
                    refusals: format!("{}: {error}", provider.kind().as_str()),
                }
            },
            policy: AbortPolicy::LeaveOpen,
        })?;

        // The executable's identity is revalidated immediately before use, so a
        // swapped binary is refused rather than launched.
        let observed = std::fs::read(&request.program)
            .map(|bytes| hex_sha256(&bytes))
            .unwrap_or_default();
        if observed != request.binary_sha256 {
            return Err(Abort {
                error: LaunchError::BinaryChanged {
                    path: request.program.display().to_string(),
                },
                policy: AbortPolicy::LeaveOpen,
            });
        }

        // ---- Step 1: pre-create the boundary, before any fsync and before any
        //      process of any kind exists. ---------------------------------------
        let boundary = provider.create(&transaction_id).map_err(|error| Abort {
            error: LaunchError::Boundary(error),
            policy: AbortPolicy::LeaveOpen,
        })?;
        let boundary_identity = boundary.identity().clone();
        session.boundary = Some(boundary);
        observer.event(&LaunchEvent::BoundaryPrecreated);

        let mut progress = Progress {
            transaction_id: transaction_id.clone(),
            gate_path: session.paths.gate.clone(),
            evidence_path: request.evidence_path.clone(),
            durable_phase: None,
            anchor_pid: None,
            child_pid: None,
        };

        self.checkpoint(
            CrashStage::BeforeIntentFsync,
            fault,
            observer,
            &progress,
            session,
        )?;

        // ---- Step 2: `intent_fsynced`, binding the already-created boundary. ---
        let mut intent = boundary_identity.to_facts();
        intent.insert(
            facts::GATE_PATH.to_owned(),
            json!(session.paths.gate.display().to_string()),
        );
        intent.insert(
            facts::BINARY_SHA256.to_owned(),
            json!(request.binary_sha256),
        );
        intent.insert(
            facts::PROJECT_GENERATION.to_owned(),
            json!(request.envelope.project.generation),
        );
        intent.insert(
            facts::PROJECT_ROOT_OBJECT.to_owned(),
            json!(request.envelope.project.root_object),
        );
        intent.insert(
            facts::PROJECT_FILE_OBJECT.to_owned(),
            json!(request.envelope.project.file_object),
        );
        intent.insert(
            facts::PLANNED_ENDPOINT.to_owned(),
            boundary_identity
                .control_endpoint
                .as_ref()
                .map_or(Value::Null, |endpoint| json!(endpoint)),
        );
        let argv = {
            let mut argv = vec![request.program.display().to_string()];
            argv.extend(request.arguments.iter().cloned());
            argv
        };
        intent.insert(facts::ARGV.to_owned(), json!(argv));
        session.payloads.push(LaunchPayload::new(
            LaunchRecord::INTENT_PAYLOAD,
            &intent,
            vec![
                (
                    "project.godot".to_owned(),
                    Some(request.envelope.project.file_object.clone()),
                ),
                (
                    "project_root".to_owned(),
                    Some(request.envelope.project.root_object.clone()),
                ),
            ],
        ));
        self.write(session, &request.envelope, LaunchPhase::IntentFsynced, None)?;
        observer.event(&LaunchEvent::Phase(LaunchPhase::IntentFsynced));
        progress.durable_phase = Some(LaunchPhase::IntentFsynced);

        self.checkpoint(
            CrashStage::AfterIntentBeforeChildCreation,
            fault,
            observer,
            &progress,
            session,
        )?;

        // ---- Step 3: create the anchor, barrier-gated. -------------------------
        let plan = SpawnPlan {
            program: request.program.clone(),
            arguments: request.arguments.clone(),
            environment: request.environment.resolve(),
            work_dir: request.project_root_display.clone(),
            stdout_path: session.paths.stdout.clone(),
            stderr_path: session.paths.stderr.clone(),
            gate_path: session.paths.gate.clone(),
            barrier_binary: request.supervisor_binary.clone(),
            join_cgroup: boundary_identity.cgroup_dir(),
        };
        let mut anchor = session
            .boundary
            .as_ref()
            .expect("the boundary was created above")
            .create_anchor(&plan)
            .map_err(|error| Abort::compensate(LaunchError::Boundary(error)))?;
        if anchor.start_time.is_none() {
            anchor.start_time = procfs::process_identity(anchor.pid)
                .ok()
                .flatten()
                .and_then(|identity| identity.start_time);
        }
        let anchor_pid = anchor.pid;
        let anchor_start = anchor.start_time.clone();
        let anchor_group = anchor.process_group;
        session.anchor = Some(anchor);
        observer.event(&LaunchEvent::MemberCreated);
        progress.anchor_pid = Some(anchor_pid);

        self.checkpoint(
            CrashStage::AfterChildCreationBeforeStagedFsync,
            fault,
            observer,
            &progress,
            session,
        )?;

        // ---- Step 4: `staged_fsynced` — created, provably not running user code.
        let mut staged = BTreeMap::new();
        staged.insert(facts::ANCHOR_PID.to_owned(), json!(anchor_pid));
        staged.insert(
            facts::ANCHOR_START_TIME.to_owned(),
            anchor_start
                .as_ref()
                .map_or(Value::Null, |start| json!(start)),
        );
        staged.insert(
            facts::ANCHOR_PROCESS_GROUP.to_owned(),
            anchor_group.map_or(Value::Null, |group| json!(group)),
        );
        staged.insert(
            facts::ACTUAL_ENDPOINT.to_owned(),
            boundary_identity
                .control_endpoint
                .as_ref()
                .map_or(Value::Null, |endpoint| json!(endpoint)),
        );
        staged.insert(
            facts::GATE_PATH.to_owned(),
            json!(session.paths.gate.display().to_string()),
        );
        session.payloads.push(LaunchPayload::new(
            LaunchRecord::STAGED_PAYLOAD,
            &staged,
            Vec::new(),
        ));
        self.write(session, &request.envelope, LaunchPhase::StagedFsynced, None)?;
        observer.event(&LaunchEvent::Phase(LaunchPhase::StagedFsynced));
        progress.durable_phase = Some(LaunchPhase::StagedFsynced);

        // ---- Step 5: create the gated child and confirm containment. -----------
        let gated = {
            let boundary = session
                .boundary
                .as_ref()
                .expect("the boundary was created above");
            let anchor = session
                .anchor
                .as_mut()
                .expect("the anchor was created above");
            boundary
                .create_gated_child(anchor)
                .map_err(|error| Abort::compensate(LaunchError::Boundary(error)))?
        };
        let confirmed = session
            .boundary
            .as_ref()
            .expect("the boundary was created above")
            .confirm_membership(gated.pid)
            .map_err(|error| Abort::compensate(LaunchError::Boundary(error)))?;
        if !confirmed {
            return Err(Abort::compensate(LaunchError::ContainmentUnconfirmed {
                pid: gated.pid,
                boundary: boundary_identity.identity.clone(),
            }));
        }
        observer.event(&LaunchEvent::MembershipConfirmed);
        progress.child_pid = Some(gated.pid);

        self.checkpoint(
            CrashStage::AfterStagedBeforeReceiptFsync,
            fault,
            observer,
            &progress,
            session,
        )?;

        // ---- Step 6: the receipt, strictly before any release. -----------------
        let receipt = receipt_facts(&gated, &boundary_identity, request);
        session.payloads.push(LaunchPayload::new(
            LaunchRecord::RECEIPT_PAYLOAD,
            &receipt,
            Vec::new(),
        ));
        let suspended = ProcessLaunchProof {
            boundary_identity: boundary_identity.identity.clone(),
            pid: Some(gated.pid),
            start_time: gated.start_time.clone(),
            boundary_membership: BoundaryMembership::Confirmed,
            release_status: ReleaseStatus::Suspended,
            exit_code: None,
            signal: None,
            boundary_empty_after_exit: None,
        };
        self.write(
            session,
            &request.envelope,
            LaunchPhase::EffectStartedFsynced,
            Some(suspended),
        )?;
        observer.event(&LaunchEvent::Phase(LaunchPhase::EffectStartedFsynced));
        progress.durable_phase = Some(LaunchPhase::EffectStartedFsynced);

        self.checkpoint(
            CrashStage::AfterReceiptBeforeRelease,
            fault,
            observer,
            &progress,
            session,
        )?;

        // ---- Step 7: release. -------------------------------------------------
        {
            let boundary = session
                .boundary
                .as_ref()
                .expect("the boundary was created above");
            let anchor = session
                .anchor
                .as_mut()
                .expect("the anchor was created above");
            boundary
                .release(anchor)
                .map_err(|error| Abort::compensate(LaunchError::Boundary(error)))?;
        }
        observer.event(&LaunchEvent::GateReleased);

        self.checkpoint(
            CrashStage::AfterReleaseBeforeCommitFsync,
            fault,
            observer,
            &progress,
            session,
        )?;

        Ok(Released {
            transaction_id,
            boundary_identity,
            gated,
            argv,
        })
    }

    fn run(
        &self,
        session: &mut Session,
        request: &LaunchRequest,
        provider: &dyn BoundaryProvider,
        fault: &FaultInjection,
        observer: &dyn LaunchObserver,
    ) -> Result<LaunchOutcome, Abort> {
        let Released {
            transaction_id,
            boundary_identity,
            gated,
            argv,
        } = self.run_to_release(session, request, provider, fault, observer)?;

        // ---- Step 8: bounded wait, then the terminal proof. -------------------
        let waited = {
            let boundary = session
                .boundary
                .as_ref()
                .expect("the boundary was created above");
            let anchor = session
                .anchor
                .as_mut()
                .expect("the anchor was created above");
            boundary.wait(anchor, request.deadline)
        };
        let waited = match waited {
            Ok(waited) => waited,
            Err(BoundaryError::SupervisorLost(reason)) => {
                return Err(Abort {
                    error: LaunchError::SupervisorLost { reason },
                    policy: AbortPolicy::LeaveOpen,
                });
            }
            Err(error) => return Err(Abort::compensate(LaunchError::Boundary(error))),
        };
        observer.event(&LaunchEvent::ChildExited);

        let termination = TerminationKind::of(&waited);
        let boundary = session
            .boundary
            .as_ref()
            .expect("the boundary was created above");
        if waited.timed_out {
            boundary
                .terminate_all()
                .map_err(|error| Abort::compensate(LaunchError::Boundary(error)))?;
        }
        let empty = matches!(
            boundary
                .state()
                .map_err(|error| Abort::compensate(LaunchError::Boundary(error)))?,
            ContainmentState::Empty
        );
        let proof = ProcessLaunchProof {
            boundary_identity: boundary_identity.identity.clone(),
            pid: Some(gated.pid),
            start_time: gated.start_time.clone(),
            boundary_membership: BoundaryMembership::Confirmed,
            release_status: termination.release_status(),
            exit_code: waited.exit_code.map(i64::from),
            signal: waited.signal.clone(),
            boundary_empty_after_exit: Some(empty),
        };
        let diagnostics = LaunchDiagnostics {
            termination: termination.clone(),
            exit_code: waited.exit_code,
            signal: waited.signal.clone(),
            stdout: gate::capture(&session.paths.stdout, request.caps.stdout_bytes),
            stderr: gate::capture(&session.paths.stderr, request.caps.stderr_bytes),
        };

        if termination == TerminationKind::TimedOut {
            // A deadline is not a completed effect: the launch is compensated, and
            // `committed_fsynced` is never written.
            let record = self.record_of(
                session,
                &request.envelope,
                LaunchPhase::EffectStartedFsynced,
            );
            self.journal
                .settle(
                    &record,
                    Disposition::Compensated,
                    "launch exceeded its deadline and was terminated inside its boundary",
                    Some(&proof),
                )
                .map_err(|error| Abort {
                    error: LaunchError::Journal(error),
                    policy: AbortPolicy::LeaveOpen,
                })?;
            let boundary_closed = boundary.close().is_ok();
            observer.event(&LaunchEvent::BoundaryClosed);
            if boundary_closed {
                let _ = session.paths.remove_scratch();
            }
        } else {
            self.write(
                session,
                &request.envelope,
                LaunchPhase::CommittedFsynced,
                Some(proof.clone()),
            )?;
            observer.event(&LaunchEvent::Phase(LaunchPhase::CommittedFsynced));
            session
                .boundary
                .as_ref()
                .expect("the boundary was created above")
                .close()
                .map_err(|error| Abort {
                    error: LaunchError::Boundary(error),
                    policy: AbortPolicy::LeaveOpen,
                })?;
            observer.event(&LaunchEvent::BoundaryClosed);
            let _ = session.paths.remove_scratch();
        }

        Ok(LaunchOutcome {
            transaction_id,
            boundary_identity: boundary_identity.identity.clone(),
            strength: boundary_identity.strength(),
            boundary: boundary_identity,
            child_pid: gated.pid,
            binary_sha256: request.binary_sha256.clone(),
            argv,
            proof,
            diagnostics,
        })
    }

    fn checkpoint(
        &self,
        stage: CrashStage,
        fault: &FaultInjection,
        observer: &dyn LaunchObserver,
        progress: &Progress,
        session: &mut Session,
    ) -> Result<(), Abort> {
        observer.stage(stage, &progress.view());
        match fault.at(stage) {
            None => Ok(()),
            Some(FaultMode::Crash) => Err(Abort {
                error: LaunchError::InjectedFault {
                    stage: stage.as_str(),
                    mode: FaultMode::Crash,
                },
                policy: AbortPolicy::Leak,
            }),
            Some(FaultMode::Error) => Err(Abort {
                error: LaunchError::InjectedFault {
                    stage: stage.as_str(),
                    mode: FaultMode::Error,
                },
                policy: AbortPolicy::Compensate,
            }),
            Some(FaultMode::SigkillSupervisor) => {
                // The residual risk this models is "the supervisor is SIGKILLed
                // *after* release", so the kill waits until the release it follows is
                // provably complete. Killing during the release window would instead
                // model case 4(i), where resume provably never happened.
                if stage >= CrashStage::AfterReleaseBeforeCommitFsync {
                    gate::wait_for_open(&progress.gate_path, Duration::from_secs(10)).map_err(
                        |reason| Abort {
                            error: LaunchError::SupervisorLost { reason },
                            policy: AbortPolicy::LeaveOpen,
                        },
                    )?;
                }
                if let Some(anchor) = session.anchor.as_mut() {
                    match anchor.link.as_mut() {
                        Some(link) => link.sigkill_supervisor(),
                        None => {
                            let _ = procfs::kill_pid(anchor.pid);
                        }
                    }
                }
                Ok(())
            }
        }
    }

    fn record_of(
        &self,
        session: &Session,
        envelope: &TransactionEnvelope,
        phase: LaunchPhase,
    ) -> LaunchRecord {
        LaunchRecord {
            envelope: envelope.clone(),
            phase,
            payloads: session.payloads.clone(),
            proof: session
                .record
                .as_ref()
                .and_then(|record| record.proof.clone()),
        }
    }

    fn write(
        &self,
        session: &mut Session,
        envelope: &TransactionEnvelope,
        phase: LaunchPhase,
        proof: Option<ProcessLaunchProof>,
    ) -> Result<(), Abort> {
        let record = LaunchRecord {
            envelope: envelope.clone(),
            phase,
            payloads: session.payloads.clone(),
            proof: proof.or_else(|| session.record.as_ref().and_then(|r| r.proof.clone())),
        };
        self.journal.write_phase(&record).map_err(|error| Abort {
            error: LaunchError::Journal(error),
            // A phase whose mark is not durable must never be treated as durable:
            // the launch stops here and the previous durable state is what
            // reconciliation resolves.
            policy: if phase == LaunchPhase::IntentFsynced {
                AbortPolicy::LeaveOpen
            } else {
                AbortPolicy::Compensate
            },
        })?;
        session.record = Some(record);
        Ok(())
    }

    fn abort(&self, session: Session, abort: Abort) -> LaunchError {
        let Session {
            paths,
            boundary,
            anchor,
            record,
            ..
        } = session;
        match abort.policy {
            AbortPolicy::Leak => {
                // Exactly what a process death leaves behind: no cleanup, no
                // disposition, every descriptor still held so a blocked supervisor
                // stays blocked. Restart reconciliation is what resolves it.
                if let Some(anchor) = anchor {
                    std::mem::forget(anchor);
                }
                if let Some(boundary) = boundary {
                    std::mem::forget(boundary);
                }
            }
            AbortPolicy::LeaveOpen => {
                drop(anchor);
                drop(boundary);
            }
            AbortPolicy::Compensate => {
                let cleaned = boundary.as_ref().is_none_or(|boundary| {
                    boundary.terminate_all().is_ok() && boundary.close().is_ok()
                });
                if cleaned && let Some(record) = &record {
                    let settled = self.journal.settle(
                        record,
                        Disposition::Compensated,
                        LAUNCH_ABANDONED_PRE_RELEASE,
                        None,
                    );
                    if settled.is_ok() {
                        let _ = paths.remove_scratch();
                    }
                }
                drop(anchor);
                drop(boundary);
            }
        }
        abort.error
    }
}

fn receipt_facts(
    gated: &GatedChild,
    boundary: &BoundaryIdentity,
    request: &LaunchRequest,
) -> BTreeMap<String, Value> {
    let mut receipt = BTreeMap::new();
    receipt.insert(facts::CHILD_PID.to_owned(), json!(gated.pid));
    receipt.insert(
        facts::CHILD_START_TIME.to_owned(),
        gated
            .start_time
            .as_ref()
            .map_or(Value::Null, |start| json!(start)),
    );
    receipt.insert(facts::MEMBERSHIP_CONFIRMED.to_owned(), json!(true));
    receipt.insert(
        facts::BINARY_SHA256.to_owned(),
        json!(request.binary_sha256),
    );
    receipt.insert(
        facts::PROJECT_ROOT_OBJECT.to_owned(),
        json!(request.envelope.project.root_object),
    );
    receipt.insert(
        facts::ACTUAL_ENDPOINT.to_owned(),
        boundary
            .control_endpoint
            .as_ref()
            .map_or(Value::Null, |endpoint| json!(endpoint)),
    );
    receipt
}

/// Where the bundled `godot-mcp-supervisor` binary sits in a deployed build:
/// beside the running executable. A caller that already knows the path (a test
/// against a freshly built binary, or a packaged layout that differs) sets
/// [`LaunchRequest::supervisor_binary`] directly instead.
#[must_use]
pub fn bundled_supervisor() -> PathBuf {
    std::env::current_exe()
        .map_or_else(|_| PathBuf::new(), |executable| supervisor_for(&executable))
}

fn supervisor_for(executable: &Path) -> PathBuf {
    let executable_name = if cfg!(windows) {
        "godot-mcp-supervisor.exe"
    } else {
        "godot-mcp-supervisor"
    };
    let candidate = executable
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join(executable_name);
    if candidate.is_file() {
        return candidate;
    }
    let fallback = candidate
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "deps"))
        .and_then(Path::parent)
        .map(|target_dir| target_dir.join(executable_name));
    fallback.unwrap_or(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn there_are_exactly_six_crash_stages_and_three_fault_modes() {
        assert_eq!(CrashStage::ALL.len(), 6);
        assert_eq!(FaultMode::ALL.len(), 3);
        let mut sorted = CrashStage::ALL;
        sorted.sort_unstable();
        assert_eq!(sorted, CrashStage::ALL, "the stages are in protocol order");
    }

    #[test]
    fn a_fault_fires_only_at_its_own_stage() {
        let fault = FaultInjection::crash_at(CrashStage::AfterReceiptBeforeRelease);
        for stage in CrashStage::ALL {
            let expected =
                (stage == CrashStage::AfterReceiptBeforeRelease).then_some(FaultMode::Crash);
            assert_eq!(fault.at(stage), expected);
        }
        assert!(
            CrashStage::ALL
                .iter()
                .all(|stage| FaultInjection::none().at(*stage).is_none())
        );
    }

    #[test]
    fn the_event_labels_spell_the_normative_ordering() {
        let labels: Vec<&str> = [
            LaunchEvent::BoundaryPrecreated,
            LaunchEvent::Phase(LaunchPhase::IntentFsynced),
            LaunchEvent::MemberCreated,
            LaunchEvent::Phase(LaunchPhase::StagedFsynced),
            LaunchEvent::MembershipConfirmed,
            LaunchEvent::Phase(LaunchPhase::EffectStartedFsynced),
            LaunchEvent::GateReleased,
            LaunchEvent::ChildExited,
            LaunchEvent::Phase(LaunchPhase::CommittedFsynced),
            LaunchEvent::BoundaryClosed,
        ]
        .iter()
        .map(LaunchEvent::label)
        .collect();
        assert_eq!(
            labels,
            vec![
                "boundary_precreated",
                "phase:intent_fsynced",
                "member_created",
                "phase:staged_fsynced",
                "membership_confirmed",
                "phase:effect_started_fsynced",
                "gate_released",
                "child_exited",
                "phase:committed_fsynced",
                "boundary_closed"
            ]
        );
    }

    #[test]
    fn only_allowlisted_environment_variables_resolve() {
        let mut allow = BTreeSet::new();
        allow.insert("PATH".to_owned());
        allow.insert("GODOT_MCP_DEFINITELY_ABSENT".to_owned());
        let mut extra = BTreeMap::new();
        extra.insert("GODOT_MCP_SCOPED".to_owned(), "yes".to_owned());
        let resolved = EnvironmentAllowlist { allow, extra }.resolve();
        let names: Vec<&str> = resolved.iter().map(|(name, _)| name.as_str()).collect();
        assert!(names.contains(&"GODOT_MCP_SCOPED"));
        assert!(!names.contains(&"GODOT_MCP_DEFINITELY_ABSENT"));
    }

    #[test]
    fn a_timeout_is_never_a_clean_termination() {
        let timed_out = TerminationKind::of(&WaitOutcome {
            exit_code: Some(0),
            signal: None,
            timed_out: true,
        });
        assert_eq!(timed_out, TerminationKind::TimedOut);
        assert_eq!(timed_out.release_status(), ReleaseStatus::Signaled);
    }

    #[test]
    fn packaged_supervisor_resolves_beside_the_server_binary() {
        let temp = tempfile::tempdir().expect("temp directory");
        let bin = temp.path().join("package").join("bin");
        std::fs::create_dir_all(&bin).expect("create package bin");
        let server = bin.join(if cfg!(windows) {
            "godot-mcp.exe"
        } else {
            "godot-mcp"
        });
        let supervisor = bin.join(if cfg!(windows) {
            "godot-mcp-supervisor.exe"
        } else {
            "godot-mcp-supervisor"
        });
        std::fs::write(&supervisor, b"supervisor").expect("write supervisor");

        let resolved = supervisor_for(&server);

        assert_eq!(resolved, supervisor);
    }
}
