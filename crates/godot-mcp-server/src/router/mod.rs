//! The cross-plane outcome router.
//!
//! One entry point decides, for every public outcome, which of the five planes
//! answers it and how; drives that answer through the capability policy permit and
//! the private effect gateway; and reports the required plane, the plane actually
//! used, any declared degradation and every effect that happened.
//!
//! ## The order of the gates is normative
//!
//! For a mutation:
//!
//! 1. **plan** — the canonical matrix row plus the environment observation;
//! 2. **write block** — a quarantined transaction or an unresolved process
//!    survivor refuses *every* mutation, before anything else is considered;
//! 3. **precedence** — a live editor buffer that diverges from disk makes a raw
//!    byte write an actionable conflict, not an overwrite;
//! 4. **idempotency** — an exact same-context replay returns its recorded result;
//!    any other reuse is the single byte-stable conflict;
//! 5. **availability** — every required plane must have a compiled-in boundary
//!    *and* a present environment;
//! 6. **admission** — the scheduler serializes the effect lane and applies the
//!    deadline, fairness and cancellation rules;
//! 7. **policy permit** — Todo 26 evaluates its six gates and mints the permit;
//! 8. **gateway** — Todo 6 journals, executes and derives exactly one terminal
//!    record from a re-derived typed proof; and
//! 9. **refresh** — affected snapshots advance only after a *proven committed*
//!    write.
//!
//! Steps 2, 3 and 5 happen before anything is queued, so an outcome that can never
//! run never occupies queue capacity.
//!
//! ## What this layer refuses to do
//!
//! * It never falls back from an editor-safe mutation to a raw text write. An
//!   outcome whose required plane cannot run is refused with `unavailable`; there
//!   is no code path that rewrites it into a different, weaker outcome.
//! * It never returns the bytes on disk as live editor state. A read that requires
//!   a live plane is answered live or refused; a read that answers from disk while
//!   a buffer diverges says so through [`Degradation`].
//! * It never infers success from dispatch. Completion is derived from the durable
//!   recorded status, and only `committed` maps to
//!   [`RouteCompletion::Completed`].
//! * It never hides a partial failure. A composed outcome whose steps disagree
//!   reports [`RouteCompletion::Partial`] with the failed steps named.
//! * It creates no second gateway, scheduler, journal or schema. Every durable
//!   fact it reports is read from Todo 6's store through the existing types.
//!
//! ## The editor and runtime planes in this release
//!
//! Their bridges are not part of this release. Their branches stay here, and are
//! refused honestly with `unavailable` after the availability gate re-derives the
//! boundary set from the effect gateway. A caller-supplied observation can widen a
//! *plan*, so the decision table stays testable, but it can never widen what
//! actually executes.

mod conflict;
mod health;
mod plan;
mod recovery;

pub use conflict::{PlaneConflict, Remediation, SnapshotAuthority, SnapshotPrecedence};
pub use health::{PlaneHealth, RouterHealth};
pub use plan::{
    Degradation, DegradationReason, EnginePresence, ExecutionStrategy, PlaneObservation,
    ProtocolFidelity, RoutePlan, RuntimePresence, WorkspaceFreshness,
};
pub use recovery::{RepairIntent, RepairReport};

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use godot_mcp_bridge::protocol::mutation::MutationLedger;
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::catalog::ToolDescriptor;
use godot_mcp_domain::contracts::{
    GodotValue, OutcomeError, PrincipalId, ProjectIdentity, SessionId,
};
use godot_mcp_engine::supervision::journal::{JournalError, TransactionEnvelope, hex_sha256};
use tokio_util::sync::CancellationToken;

use crate::effect::boundary::{EditorEffectOutcome, EditorMutation};
use crate::effect::gateway::EditorEffectIntent;
use crate::effect::{
    EffectGateway, EffectOutcome, FileEffectIntent, PlannedWrite, ProcessEffectIntent,
    ProcessEffectOutcome, ProcessLaunchRequest, WritePrecondition,
};
use crate::idempotency::gate::IdempotencyGate;
use crate::idempotency::{
    ConflictEnvelope, ConnectionSession, IdempotencyId, ModePrincipal, MutationIdentity,
    ProjectBinding, ProjectObjectIdentity, RecordedStatus, RecordedVerdict, ReplayDecision,
    TransportAuthMode, TransportBinding, canonical_request_hash, family_of_descriptor,
};
use crate::journal::{FileOperation, HeldProject, TransactionStore, TransportMode};
use crate::policy::{DispatchRequest, PolicyAuthority, PolicyPermit, PolicyRefusal, SurvivorProbe};
use crate::scheduler::{
    AdmissionError, AdmissionRequest, EffectPlane, Lane, PlaneAvailability, QueuePriority,
    Scheduler, priority_for,
};

use conflict::precedence_conflict;
use health::health_report;
use plan::plan_for;
pub(crate) use plan::plane_of_boundary;
#[cfg(any(test, feature = "test-hooks"))]
use plan::unknown_operation;

use crate::audit::{AuditBoundary, AuditDigest, AuditPhase, RequestAudit, RequestEffectFacts};
use crate::workspace::{CommitScope, WorkspaceCoherence, WorkspaceCoordinator};

/// The transport-established identity every routed request is attributed to.
///
/// All three facts travel together because the eight-element mutation identity
/// tuple needs all three at once, and because the policy authority is bound to the
/// same principal and session. Splitting them would let a request be journaled
/// under one identity and authorized under another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterIdentity {
    /// Transport/auth mode that established the identity.
    pub transport_mode: TransportAuthMode,
    /// Mode-specific principal, already in the shape that mode requires.
    pub principal: String,
    /// The MCP connection this request arrived on.
    pub session: String,
}

impl RouterIdentity {
    const fn auth_mode(&self) -> TransportAuthMode {
        self.transport_mode
    }

    /// The journal's own spelling of the same mode.
    const fn journal_mode(&self) -> TransportMode {
        match self.transport_mode {
            TransportAuthMode::Stdio => TransportMode::Stdio,
            TransportAuthMode::TrustedLocal => TransportMode::TrustedLocal,
            TransportAuthMode::ProxiedOidc => TransportMode::ProxiedOidc,
        }
    }

    /// Rebuild the mode-specific principal.
    ///
    /// An OIDC principal is a length-prefixed `(issuer, subject)` pair, so the
    /// journal round trip is injective; the other two modes carry a single opaque
    /// local identifier.
    fn mode_principal(&self) -> Result<ModePrincipal, OutcomeError> {
        ModePrincipal::from_journal_string(self.auth_mode(), &self.principal)
            .map_err(|error| internal(&error.to_string()))
    }
}

/// One survivor probe shared by the policy authority and the scheduler.
///
/// Both gates must consult the *same* probe. Two independent probes could disagree
/// about whether a survivor blocks writes, and the weaker answer would win by
/// accident.
pub(crate) struct SharedSurvivors(pub(crate) Arc<dyn SurvivorProbe>);

impl SurvivorProbe for SharedSurvivors {
    fn unresolved_survivor(&self) -> Result<Option<String>, String> {
        self.0.unresolved_survivor()
    }
}

/// Everything the router needs, supplied together so it cannot be assembled with
/// a component silently omitted.
pub(crate) struct RouterParts {
    pub(crate) project: ProjectIdentity,
    pub(crate) held: HeldProject,
    pub(crate) store: Arc<TransactionStore>,
    pub(crate) gateway: Arc<EffectGateway>,
    pub(crate) scheduler: Arc<Scheduler>,
    pub(crate) policy: Arc<PolicyAuthority>,
    pub(crate) survivors: Arc<dyn SurvivorProbe>,
    pub(crate) identity: RouterIdentity,
    pub(crate) principal: Option<PrincipalId>,
    pub(crate) session: Option<SessionId>,
}

/// One request to route.
#[derive(Debug, Clone)]
pub struct RouteRequest {
    /// Canonical tool name.
    pub tool: String,
    /// Caller-supplied idempotency identifier.
    pub idempotency_id: String,
    /// The transaction a mutating request will execute. Reads carry none.
    pub transaction_id: Option<String>,
    /// Descriptor-relative resource the request addresses.
    pub path: String,
    /// Bytes a mutating request publishes.
    pub bytes: Vec<u8>,
    /// Complete request arguments; hashed canonically, never truncated.
    pub arguments: serde_json::Value,
    /// What the caller observed about the environment.
    pub observation: PlaneObservation,
    /// Cancellation token the caller may trip before or during execution.
    pub cancellation: CancellationToken,
    /// When the request entered the queue.
    pub queued_at_unix_ms: u64,
    pub(crate) identity: Option<RouterIdentity>,
    pub(crate) effect: Option<RouteEffect>,
    pub(crate) audit: Option<RequestAudit>,
}

#[derive(Debug, Clone)]
pub(crate) enum RouteEffect {
    File(FileEffectIntent),
    Process {
        intent: ProcessEffectIntent,
        request: Box<ProcessLaunchRequest>,
    },
    /// A mutation the connected live editor carries out.
    Editor {
        intent: EditorEffectIntent,
        mutation: Box<EditorMutation>,
    },
    /// A managed Godot session to start, linked to its supervised launch.
    SessionStart {
        intent: ProcessEffectIntent,
        request: Box<crate::effect::boundary::SessionStartRequest>,
    },
    /// A command to a running managed game session.
    Runtime {
        intent: EditorEffectIntent,
        command: Box<EditorMutation>,
    },
    /// A command to a managed editor session's language server or debug
    /// adapter, or the stop of that session.
    Debug {
        intent: EditorEffectIntent,
        command: Box<EditorMutation>,
    },
    /// The stop of a managed session that is a process effect: an export's
    /// cancellation.
    SessionStop {
        intent: EditorEffectIntent,
        command: Box<EditorMutation>,
    },
}

impl RouteEffect {
    const fn plane(&self) -> EffectPlane {
        match self {
            Self::File(_) => EffectPlane::File,
            Self::Process { .. } | Self::SessionStart { .. } | Self::SessionStop { .. } => {
                EffectPlane::Process
            }
            Self::Editor { .. } => EffectPlane::Editor,
            Self::Runtime { .. } => EffectPlane::Runtime,
            Self::Debug { .. } => EffectPlane::LspDap,
        }
    }

    fn intent(&self) -> (&str, &str, &str, &serde_json::Value) {
        match self {
            Self::File(intent) => (
                &intent.transaction_id,
                &intent.tool_name,
                &intent.idempotency_id,
                &intent.arguments,
            ),
            Self::Process { intent, .. } => (
                &intent.transaction_id,
                &intent.tool_name,
                &intent.idempotency_id,
                &intent.arguments,
            ),
            Self::SessionStart { intent, .. } => (
                &intent.transaction_id,
                &intent.tool_name,
                &intent.idempotency_id,
                &intent.arguments,
            ),
            Self::Editor { intent, .. }
            | Self::Runtime { intent, .. }
            | Self::Debug { intent, .. }
            | Self::SessionStop { intent, .. } => (
                &intent.transaction_id,
                &intent.tool_name,
                &intent.idempotency_id,
                &intent.arguments,
            ),
        }
    }
}

/// One project path the connected live editor holds open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeldPath {
    /// Project-relative path.
    pub(crate) path: String,
    /// Whether the editor holds changes the file does not; `None` when the
    /// editor cannot tell.
    pub(crate) unsaved: Option<bool>,
}

/// What the connected live editor holds open, asked before a headless writer
/// may touch a path.
pub(crate) trait LiveEditorView: Send + Sync {
    /// `Ok(None)` when no editor is connected; the open paths otherwise.
    ///
    /// # Errors
    /// Returns a description when an editor is connected but did not say.
    fn held_paths(&self) -> Result<Option<Vec<HeldPath>>, String>;
}

/// What one step of a routed outcome did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// The step ran and its result is proven.
    Succeeded,
    /// The step ran or was attempted and did not succeed.
    Failed(String),
    /// The step was not applicable, with the reason it was not.
    ///
    /// A skip is *not* a failure and *not* a silent omission: it is recorded so a
    /// reader can see that a plane in the plan produced nothing, and why.
    Skipped(&'static str),
}

impl StepOutcome {
    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed(_) => "failed",
            Self::Skipped(_) => "skipped",
        }
    }
}

/// One step of a routed outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteStep {
    plane: Option<EffectPlane>,
    action: &'static str,
    outcome: StepOutcome,
}

impl RouteStep {
    /// Which plane the step ran on, if any.
    #[must_use]
    pub const fn plane(&self) -> Option<EffectPlane> {
        self.plane
    }

    /// Stable name of what the step did.
    #[must_use]
    pub const fn action(&self) -> &'static str {
        self.action
    }

    /// What the step produced.
    #[must_use]
    pub const fn outcome(&self) -> &StepOutcome {
        &self.outcome
    }
}

/// How completely a routed outcome finished.
///
/// The four states exist because "it worked", "some of it worked", "nobody knows"
/// and "nothing happened" are four different truths and collapsing any two of them
/// is how a caller ends up acting on a state that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RouteCompletion {
    /// Every step is proven done.
    Completed,
    /// Some steps are proven done and at least one is not. Never reported as a
    /// success, and never reported as a clean failure either.
    Partial,
    /// The outcome was dispatched and its result is not decidable from here.
    Uncertain,
    /// Nothing happened; the pre-state is proven.
    Refused,
}

impl RouteCompletion {
    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Partial => "partial",
            Self::Uncertain => "uncertain",
            Self::Refused => "refused",
        }
    }

    /// Derive completion from one durable recorded status.
    ///
    /// Only `committed` completes. `dispatched` and `unresolved` are *uncertain*,
    /// never completed and never refused: the boundary was already touched, so
    /// "no answer came back" is not "it did not happen".
    #[must_use]
    pub const fn of_recorded(status: RecordedStatus) -> Self {
        match status {
            RecordedStatus::Committed => Self::Completed,
            RecordedStatus::NotStarted | RecordedStatus::Compensated => Self::Refused,
            RecordedStatus::Dispatched | RecordedStatus::Unresolved => Self::Uncertain,
        }
    }

    /// Aggregate the steps of a composed outcome.
    ///
    /// A mixed result is `Partial`. It is deliberately impossible for this function
    /// to return `Completed` while any step failed.
    #[must_use]
    pub fn of_steps(steps: &[StepOutcome]) -> Self {
        let failed = steps
            .iter()
            .any(|step| matches!(step, StepOutcome::Failed(_)));
        let succeeded = steps.contains(&StepOutcome::Succeeded);
        match (failed, succeeded) {
            (true, true) => Self::Partial,
            (true, false) => Self::Refused,
            (false, _) => Self::Completed,
        }
    }
}

/// The complete report for one routed outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteReport {
    plan: RoutePlan,
    completion: RouteCompletion,
    steps: Vec<RouteStep>,
    used: Vec<EffectPlane>,
    effects: Vec<EffectPlane>,
    refreshed: Vec<EffectPlane>,
    transaction_id: Option<String>,
    recorded_status: Option<RecordedStatus>,
    recorded_result: Option<serde_json::Value>,
    recorded_verdict: Option<RecordedVerdict>,
    replayed: bool,
    effect_outcome: Option<EffectOutcome>,
    process_outcome: Option<ProcessEffectOutcome>,
    editor_outcome: Option<EditorEffectOutcome>,
    session_outcome: Option<crate::effect::boundary::SessionStartOutcome>,
    workspace: Option<serde_json::Value>,
}

impl RouteReport {
    /// The public description of the workspace snapshot published by this
    /// route's read-your-writes barrier: present exactly when a committed
    /// mutation was observed before the route returned.
    #[must_use]
    pub fn workspace(&self) -> Option<&serde_json::Value> {
        self.workspace.as_ref()
    }

    /// The plan that was selected.
    #[must_use]
    pub const fn plan(&self) -> &RoutePlan {
        &self.plan
    }

    /// Planes the outcome required.
    #[must_use]
    pub fn required_planes(&self) -> Vec<EffectPlane> {
        self.plan.required_planes()
    }

    /// Planes that actually ran.
    #[must_use]
    pub fn used_planes(&self) -> Vec<EffectPlane> {
        self.used.clone()
    }

    /// Effects that actually happened.
    #[must_use]
    pub fn effects(&self) -> Vec<EffectPlane> {
        self.effects.clone()
    }

    /// Snapshots refreshed as part of this outcome.
    #[must_use]
    pub fn refreshed_snapshots(&self) -> Vec<EffectPlane> {
        self.refreshed.clone()
    }

    /// Whether the answer is full fidelity or a declared reduction.
    #[must_use]
    pub const fn degradation(&self) -> Degradation {
        self.plan.degradation()
    }

    /// How completely the outcome finished.
    #[must_use]
    pub const fn completion(&self) -> RouteCompletion {
        self.completion
    }

    /// Every step, in execution order.
    #[must_use]
    pub fn steps(&self) -> Vec<RouteStep> {
        self.steps.clone()
    }

    /// The reasons of every step that did not succeed.
    #[must_use]
    pub fn failed_steps(&self) -> Vec<String> {
        self.steps
            .iter()
            .filter_map(|step| match &step.outcome {
                StepOutcome::Failed(reason) => Some(reason.clone()),
                StepOutcome::Succeeded | StepOutcome::Skipped(_) => None,
            })
            .collect()
    }

    /// The journaled transaction, for a mutation.
    #[must_use]
    pub fn transaction_id(&self) -> Option<&str> {
        self.transaction_id.as_deref()
    }

    /// The durable recorded status the completion was derived from.
    #[must_use]
    pub const fn recorded_status(&self) -> Option<RecordedStatus> {
        self.recorded_status
    }

    /// The result read from the existing durable journal for an exact replay.
    #[must_use]
    pub fn recorded_result(&self) -> Option<&serde_json::Value> {
        self.recorded_result.as_ref()
    }

    #[must_use]
    pub const fn recorded_verdict(&self) -> Option<RecordedVerdict> {
        self.recorded_verdict
    }

    /// Whether the answer came from an exact same-context recorded result.
    #[must_use]
    pub const fn replayed(&self) -> bool {
        self.replayed
    }

    pub(crate) const fn effect_outcome(&self) -> Option<&EffectOutcome> {
        self.effect_outcome.as_ref()
    }

    pub(crate) const fn process_outcome(&self) -> Option<&ProcessEffectOutcome> {
        self.process_outcome.as_ref()
    }

    /// The three-state answer of a bridge peer: the live editor, or a game
    /// session for a runtime command.
    pub(crate) const fn editor_outcome(&self) -> Option<&EditorEffectOutcome> {
        self.editor_outcome.as_ref()
    }

    /// The started session, for a session start.
    pub(crate) const fn session_outcome(
        &self,
    ) -> Option<&crate::effect::boundary::SessionStartOutcome> {
        self.session_outcome.as_ref()
    }
}

/// Run a blocking effect without stalling the async runtime's other work: on a
/// multi-threaded runtime the worker hands its queue to another thread first.
fn blocking<T>(effect: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(effect)
        }
        _ => effect(),
    }
}

/// Why a route was refused. Each variant maps to exactly one stable error class.
#[derive(Debug)]
pub enum RouteRefusal {
    /// The tool is not a canonical capability-matrix operation.
    NotRoutable(OutcomeError),
    /// A required plane has no boundary in this build, or the environment does not
    /// provide it.
    Unavailable(OutcomeError),
    /// A quarantined transaction or an unresolved survivor blocks every mutation.
    WriteBlocked(OutcomeError),
    /// A live editor buffer holds the authoritative state for this resource.
    Conflict(PlaneConflict),
    /// The idempotency identifier was reused from a different context.
    Replay(ConflictEnvelope),
    /// The capability policy refused.
    Denied(OutcomeError),
    /// The outcome was dispatched and its result is not decidable from here.
    Uncertain(OutcomeError),
    /// A precondition the request was built against no longer holds.
    Precondition(OutcomeError),
}

impl RouteRefusal {
    /// The caller-facing refusal.
    #[must_use]
    pub fn to_outcome(&self) -> OutcomeError {
        match self {
            Self::NotRoutable(error)
            | Self::Unavailable(error)
            | Self::WriteBlocked(error)
            | Self::Denied(error)
            | Self::Uncertain(error)
            | Self::Precondition(error) => error.clone(),
            Self::Conflict(conflict) => conflict.to_outcome(),
            Self::Replay(envelope) => envelope.to_outcome(),
        }
    }
}

/// What sets one router's transaction IDs apart from every other router's. A
/// launch's containment boundary is named after its transaction ID, and some of
/// those names, a Windows Job Object's or a container's, are shared by every
/// process on the host: two servers that start a launch in the same millisecond
/// must still mint different IDs.
fn transaction_salt() -> String {
    let mut seed = std::process::id().to_le_bytes().to_vec();
    let mut entropy = [0_u8; 16];
    // The process ID already separates live servers; the OS CSPRNG also
    // separates two routers in one process and a reused process ID.
    let _ = getrandom::fill(&mut entropy);
    seed.extend_from_slice(&entropy);
    hex_sha256(&seed)
}

/// The per-project outcome router.
pub struct OutcomeRouter {
    project: ProjectIdentity,
    held: HeldProject,
    store: Arc<TransactionStore>,
    gateway: Arc<EffectGateway>,
    scheduler: Arc<Scheduler>,
    policy: Arc<PolicyAuthority>,
    identity: RouterIdentity,
    principal: Option<PrincipalId>,
    session: Option<SessionId>,
    snapshots: Mutex<BTreeMap<EffectPlane, u64>>,
    transaction_salt: String,
    transaction_sequence: AtomicU64,
    workspace: OnceLock<Arc<WorkspaceCoordinator>>,
    editor: OnceLock<Arc<dyn LiveEditorView>>,
}

impl OutcomeRouter {
    /// Assemble the router and bind the durable machinery to the scheduler.
    ///
    /// This is the single production assembly point, and binding happens *here*
    /// rather than at a transport so no transport can forget it: every transport
    /// that wants to route must build a router, and building a router binds the
    /// cross-plane idempotency gate, the durable store and the survivor probe.
    pub(crate) fn assemble(parts: RouterParts) -> Arc<Self> {
        let availability = boundary_planes(parts.gateway.as_ref());
        parts.scheduler.set_plane_availability(availability);
        parts.scheduler.bind_journal(
            Arc::new(IdempotencyGate::new(
                Arc::clone(&parts.store),
                parts.held.object_identity.clone(),
                parts.held.generation,
            )),
            Arc::clone(&parts.store),
            Arc::clone(&parts.survivors),
        );
        Arc::new(Self {
            project: parts.project,
            held: parts.held,
            store: parts.store,
            gateway: parts.gateway,
            scheduler: parts.scheduler,
            policy: parts.policy,
            identity: parts.identity,
            principal: parts.principal,
            session: parts.session,
            snapshots: Mutex::new(
                EffectPlane::ALL
                    .into_iter()
                    .map(|plane| (plane, 0_u64))
                    .collect(),
            ),
            transaction_salt: transaction_salt(),
            transaction_sequence: AtomicU64::new(1),
            workspace: OnceLock::new(),
            editor: OnceLock::new(),
        })
    }

    /// Bind the live editor whose open paths no headless writer may touch.
    /// Binding twice is ignored.
    pub(crate) fn attach_editor(&self, editor: Arc<dyn LiveEditorView>) {
        let _ = self.editor.set(editor);
    }

    /// Refuse a headless write to a path the connected live editor holds
    /// open: its buffer is the scene or script the user sees, and writing the
    /// file underneath it would either be overwritten by the editor's next
    /// save or silently diverge from it.
    fn live_editor_conflict(&self, effect: &RouteEffect) -> Result<(), RouteRefusal> {
        let RouteEffect::File(intent) = effect else {
            return Ok(());
        };
        let Some(editor) = self.editor.get() else {
            return Ok(());
        };
        let held = match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| editor.held_paths())
            }
            _ => editor.held_paths(),
        };
        let held = match held {
            Ok(Some(held)) => held,
            Ok(None) => return Ok(()),
            Err(reason) => {
                return Err(RouteRefusal::Unavailable(OutcomeError::new(
                    ErrorKind::Unavailable,
                    format!(
                        "the connected editor did not say which files it holds open ({reason}); \
                         nothing was written"
                    ),
                    true,
                    BTreeMap::new(),
                )));
            }
        };
        for write in &intent.writes {
            if let Some(open) = held.iter().find(|open| open.path == write.path) {
                let mut details = BTreeMap::new();
                details.insert("path".to_owned(), GodotValue::String(write.path.clone()));
                details.insert(
                    "unsaved".to_owned(),
                    open.unsaved.map_or(GodotValue::Null, GodotValue::Bool),
                );
                return Err(RouteRefusal::Precondition(OutcomeError::new(
                    ErrorKind::Conflict,
                    format!(
                        "{} is open in the connected editor; change a scene through \
                         scene.batch.apply, which edits it there, or close it in the editor \
                         first. Nothing was written.",
                        write.path
                    ),
                    false,
                    details,
                )));
            }
        }
        Ok(())
    }

    /// Bind the workspace coordinator whose snapshot every committed mutation
    /// must refresh before it answers. Binding twice is ignored.
    pub(crate) fn attach_workspace(&self, workspace: Arc<WorkspaceCoordinator>) {
        let _ = self.workspace.set(workspace);
    }

    pub(crate) fn next_transaction_id(&self, scope: &str, observed_at_unix_ms: u64) -> String {
        let sequence = self.transaction_sequence.fetch_add(1, Ordering::Relaxed);
        let digest = hex_sha256(
            format!(
                "{}:{scope}:{observed_at_unix_ms}:{sequence}",
                self.transaction_salt
            )
            .as_bytes(),
        );
        format!(
            "{}-{}-4{}-8{}-{}",
            &digest[0..8],
            &digest[8..12],
            &digest[13..16],
            &digest[17..20],
            &digest[20..32]
        )
    }

    /// Which planes have a real boundary compiled into this build.
    ///
    /// This is re-derived from the effect gateway on every call, so the router and
    /// the gateway can never disagree about what exists.
    #[must_use]
    pub fn plane_availability(&self) -> PlaneAvailability {
        planes_of(self.gateway.live_boundary_availability())
    }

    /// The current refresh generation of one plane's snapshot.
    #[must_use]
    pub fn snapshot_generation(&self, plane: EffectPlane) -> u64 {
        self.snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&plane)
            .copied()
            .unwrap_or_default()
    }

    /// The routing decision for one tool, without executing anything.
    ///
    /// # Errors
    /// Returns `not_found` for a tool the canonical matrix does not name.
    pub fn plan(
        &self,
        tool: &str,
        observation: &PlaneObservation,
    ) -> Result<RoutePlan, RouteRefusal> {
        plan_for(tool, observation).map_err(RouteRefusal::NotRoutable)
    }

    /// Health and remediation for every plane, plus the live write block.
    #[must_use]
    pub fn health(&self, observed_at_unix_ms: u64) -> RouterHealth {
        let blocked = match self.scheduler.write_block() {
            Ok(Some(block)) => Some(block.to_outcome().message().to_owned()),
            Ok(None) => None,
            // An unreadable block state is surfaced, never reported as "clear".
            Err(error) => Some(error.message().to_owned()),
        };
        // Which boundaries this server attaches is one fact; what each has to
        // serve right now — a connected editor plugin, a running game or
        // editor session — is the other. A plane that is attached but idle is
        // reported as such, never as absent from the build.
        let present = planes_of(self.gateway.present_boundaries());
        let observation = PlaneObservation::new(
            boundary_planes(self.gateway.as_ref()),
            if present.of(EffectPlane::Editor) {
                EnginePresence::EditorConnected
            } else {
                EnginePresence::Absent
            },
            if present.of(EffectPlane::Runtime) {
                RuntimePresence::GameRunning
            } else {
                RuntimePresence::NoInstance
            },
            ProtocolFidelity::Full,
            WorkspaceFreshness::Fresh,
        );
        // A managed editor session hosts the language server and debug
        // adapter whether or not an editor plugin is connected.
        health_report(&observation, blocked, observed_at_unix_ms)
            .with_environment(EffectPlane::LspDap, present.of(EffectPlane::LspDap))
            .with_unavailable_reason(
                EffectPlane::Process,
                self.gateway.process_unavailable_reason(),
            )
    }

    /// Health and remediation against a caller's own environment observation.
    #[must_use]
    pub fn health_for(
        &self,
        observation: &PlaneObservation,
        observed_at_unix_ms: u64,
    ) -> RouterHealth {
        let blocked = match self.scheduler.write_block() {
            Ok(Some(block)) => Some(block.to_outcome().message().to_owned()),
            Ok(None) => None,
            Err(error) => Some(error.message().to_owned()),
        };
        let observation = observation.with_boundaries(self.plane_availability());
        health_report(&observation, blocked, observed_at_unix_ms)
    }

    /// Apply one proof-driven operator repair.
    ///
    /// # Errors
    /// Returns the recovery refusal. Nothing here can force or clear a block.
    pub fn repair(
        &self,
        transaction_id: &str,
        intent: RepairIntent,
    ) -> Result<RepairReport, RouteRefusal> {
        recovery::apply_repair(
            &self.store,
            self.gateway.as_ref(),
            self.scheduler.as_ref(),
            &self.held,
            transaction_id,
            intent,
        )
        .map_err(RouteRefusal::Denied)
    }

    /// Persist every unresolved editor-bridge mutation through Todo 6's store.
    ///
    /// # Errors
    /// Returns the first canonical-record or durable-write failure.
    pub fn quarantine_bridge_mutations(
        &self,
        ledger: &MutationLedger,
        envelope: &TransactionEnvelope,
    ) -> Result<usize, JournalError> {
        ledger.drain_quarantine(envelope, self.store.as_ref())
    }

    /// Route one outcome end to end.
    ///
    /// # Errors
    /// Returns the refusal. Every refusal is taken before the boundary it would
    /// have reached, except [`RouteRefusal::Uncertain`], which is exactly the case
    /// where the boundary was already touched and no answer came back.
    pub async fn route(&self, request: &RouteRequest) -> Result<RouteReport, RouteRefusal> {
        // The caller's observation may widen a plan, but never what executes: the
        // compiled-in boundary set is replaced with the gateway's own report.
        let observation = request
            .observation
            .with_boundaries(self.plane_availability());
        let plan = plan_for(&request.tool, &observation).map_err(RouteRefusal::NotRoutable)?;
        self.dispatch(request, &observation, plan).await
    }

    /// Route one request through an already-built plan. Every later gate reads
    /// the plan's own catalog descriptor; no tool name is looked up again.
    async fn dispatch(
        &self,
        request: &RouteRequest,
        observation: &PlaneObservation,
        plan: RoutePlan,
    ) -> Result<RouteReport, RouteRefusal> {
        let priority = priority_for(family_of_descriptor(plan.descriptor()));
        if plan.is_mutation() {
            self.route_mutation(request, observation, plan, priority)
                .await
        } else {
            self.route_read(request, observation, plan, priority).await
        }
    }

    /// Plan one frozen version 0.1 planned operation (test builds only).
    ///
    /// The shape comes from
    /// [`historical_v0_1_descriptor`](godot_mcp_domain::catalog::historical_v0_1_descriptor),
    /// never from the catalog, so production routing cannot reach it.
    ///
    /// # Errors
    /// Returns `not_found` for a name that was not a version 0.1 planned operation.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn plan_historical(
        &self,
        tool: &str,
        observation: &PlaneObservation,
    ) -> Result<RoutePlan, RouteRefusal> {
        let descriptor = godot_mcp_domain::catalog::historical_v0_1_descriptor(tool)
            .ok_or_else(|| RouteRefusal::NotRoutable(unknown_operation(tool)))?;
        Ok(plan::plan_for_descriptor(descriptor, observation))
    }

    /// Route one frozen version 0.1 planned operation through the production
    /// gates (test builds only), so plane refusals for shapes later milestones
    /// implement stay exercised.
    ///
    /// # Errors
    /// Returns the same refusals [`Self::route`] would.
    #[cfg(any(test, feature = "test-hooks"))]
    pub async fn route_historical(
        &self,
        request: &RouteRequest,
    ) -> Result<RouteReport, RouteRefusal> {
        let observation = request
            .observation
            .with_boundaries(self.plane_availability());
        let plan = self.plan_historical(&request.tool, &observation)?;
        self.dispatch(request, &observation, plan).await
    }

    /// Return an exact durable replay before an engine helper or process is
    /// resolved. A fresh request returns `None` and continues through the normal
    /// route, which repeats the same gate after admission to close the race with a
    /// concurrent first attempt.
    pub(crate) fn recorded_replay(
        &self,
        request: &RouteRequest,
    ) -> Result<Option<RouteReport>, RouteRefusal> {
        let observation = request
            .observation
            .with_boundaries(self.plane_availability());
        let plan = plan_for(&request.tool, &observation).map_err(RouteRefusal::NotRoutable)?;
        if !plan.is_mutation() {
            return Err(RouteRefusal::Denied(internal(
                "a replay preflight requires a mutating route",
            )));
        }
        self.replay_report(request, &observation, plan)
    }

    async fn route_read(
        &self,
        request: &RouteRequest,
        observation: &PlaneObservation,
        plan: RoutePlan,
        priority: QueuePriority,
    ) -> Result<RouteReport, RouteRefusal> {
        self.require_planes(&plan, observation)?;
        let admission = self
            .scheduler
            .admit_read_classed(&AdmissionRequest::new(
                Lane::Read,
                priority,
                request.cancellation.clone(),
                request.queued_at_unix_ms,
            ))
            .await
            .map_err(admission_refusal)?;
        self.authorize(plan.descriptor(), None, request.audit.as_ref())?;
        drop(admission);

        // A lagging derived view is refreshed rather than answered from. This is
        // the only pre-answer refresh: it repairs a view the router owns, and it
        // never substitutes one plane's state for another's.
        let refreshed = if plan.requires_snapshot_refresh() {
            self.refresh(&[EffectPlane::File])
        } else {
            Vec::new()
        };
        let steps = plan
            .required_planes()
            .into_iter()
            .map(|plane| RouteStep {
                plane: Some(plane),
                action: "observe",
                outcome: StepOutcome::Succeeded,
            })
            .collect::<Vec<_>>();
        let completion = RouteCompletion::of_steps(
            &steps
                .iter()
                .map(|step| step.outcome.clone())
                .collect::<Vec<_>>(),
        );
        let used = plan.required_planes();
        Ok(RouteReport {
            plan,
            completion,
            steps,
            used,
            effects: Vec::new(),
            refreshed,
            transaction_id: None,
            recorded_status: None,
            recorded_result: None,
            recorded_verdict: None,
            replayed: false,
            effect_outcome: None,
            process_outcome: None,
            editor_outcome: None,
            session_outcome: None,
            workspace: None,
        })
    }

    async fn route_mutation(
        &self,
        request: &RouteRequest,
        observation: &PlaneObservation,
        plan: RoutePlan,
        priority: QueuePriority,
    ) -> Result<RouteReport, RouteRefusal> {
        if let Some(replay) = self.replay_report(request, observation, plan.clone())? {
            return Ok(replay);
        }

        // 5. Every required plane must exist and be provided.
        self.require_planes(&plan, observation)?;

        let mut effect_plane = plan
            .primary_effect_plane()
            .ok_or_else(|| RouteRefusal::Denied(internal("a mutation named no effect plane")))?;
        // A tool the live editor can carry out unchanged is authorized and
        // journaled at the editor boundary when its caller chose that backend.
        let in_editor = matches!(request.effect, Some(RouteEffect::Editor { .. }))
            && effect_plane != EffectPlane::Editor;
        let permit_descriptor = if in_editor {
            effect_plane = EffectPlane::Editor;
            godot_mcp_domain::catalog::in_editor_descriptor(&request.tool).ok_or_else(|| {
                RouteRefusal::Denied(internal(
                    "this tool cannot be carried out by the live editor",
                ))
            })?
        } else {
            plan.descriptor()
        };
        let transaction_id = request.transaction_id.clone().ok_or_else(|| {
            RouteRefusal::Denied(internal(
                "a mutating route must name its transaction identifier",
            ))
        })?;

        // 6. Admission. The effect lane is serialized per project.
        let mut admission = self
            .scheduler
            .admit_mutation(
                &AdmissionRequest::new(
                    Lane::Effect,
                    priority,
                    request.cancellation.clone(),
                    request.queued_at_unix_ms,
                ),
                effect_plane,
            )
            .await
            .map_err(admission_refusal)?;

        let effect = match request.effect.clone() {
            Some(effect) => effect,
            None => {
                RouteEffect::File(self.file_intent(plan.descriptor(), request, &transaction_id)?)
            }
        };
        self.live_editor_conflict(&effect)?;
        let (effect_transaction, effect_tool, effect_idempotency, effect_arguments) =
            effect.intent();
        if effect.plane() != effect_plane
            || effect_transaction != transaction_id
            || effect_tool != request.tool
            || effect_idempotency != request.idempotency_id
            || effect_arguments != &request.arguments
        {
            return Err(RouteRefusal::Denied(internal(
                "the routed effect does not match its route request",
            )));
        }
        // A request built against a named workspace revision is refused here,
        // inside the serialized effect lane, unless the workspace is still at
        // exactly that revision. A stale snapshot proves no revision at all.
        if let RouteEffect::File(intent) = &effect
            && let Some(expected) = intent.expected_revision
            && let Some(coordinator) = self.workspace.get()
        {
            let snapshot = coordinator.snapshot();
            let current = snapshot.revision();
            if let WorkspaceCoherence::Stale { reason } = snapshot.coherence() {
                let mut details = BTreeMap::new();
                details.insert(
                    "expected_revision".to_owned(),
                    GodotValue::Integer(i64::try_from(expected).unwrap_or(i64::MAX)),
                );
                details.insert(
                    "stale_reason".to_owned(),
                    GodotValue::String(reason.clone()),
                );
                return Err(RouteRefusal::Precondition(OutcomeError::new(
                    ErrorKind::Conflict,
                    format!(
                        "the workspace is stale ({reason}), so expected revision {expected} \
                         cannot be proven; retry once it is coherent again"
                    ),
                    false,
                    details,
                )));
            }
            if current != expected {
                let mut details = BTreeMap::new();
                details.insert(
                    "expected_revision".to_owned(),
                    GodotValue::Integer(i64::try_from(expected).unwrap_or(i64::MAX)),
                );
                details.insert(
                    "current_revision".to_owned(),
                    GodotValue::Integer(i64::try_from(current).unwrap_or(i64::MAX)),
                );
                return Err(RouteRefusal::Precondition(OutcomeError::new(
                    ErrorKind::Conflict,
                    format!(
                        "the workspace is at revision {current}, not the expected revision \
                         {expected}; re-read and retry"
                    ),
                    false,
                    details,
                )));
            }
        }
        match &effect {
            RouteEffect::File(intent) => {
                self.gateway
                    .open_file_transaction(intent)
                    .and_then(|_| self.gateway.stage_file_transaction(intent))
                    .map_err(|error| RouteRefusal::Denied(error.to_outcome()))?;
            }
            RouteEffect::Process { intent, .. } | RouteEffect::SessionStart { intent, .. } => {
                self.gateway
                    .open_process_transaction(intent)
                    .map_err(|error| RouteRefusal::Denied(error.to_outcome()))?;
            }
            RouteEffect::Runtime { intent, .. } => {
                self.gateway
                    .open_runtime_transaction(intent)
                    .map_err(|error| RouteRefusal::Denied(error.to_outcome()))?;
            }
            RouteEffect::Debug { intent, .. } => {
                self.gateway
                    .open_debug_transaction(intent)
                    .map_err(|error| RouteRefusal::Denied(error.to_outcome()))?;
            }
            RouteEffect::SessionStop { intent, .. } => {
                self.gateway
                    .open_session_stop_transaction(intent)
                    .map_err(|error| RouteRefusal::Denied(error.to_outcome()))?;
            }
            RouteEffect::Editor { intent, .. } => {
                self.gateway
                    .open_editor_transaction(intent)
                    .map_err(|error| match error {
                        error @ crate::effect::EffectError::Precondition { .. } => {
                            RouteRefusal::Precondition(error.to_outcome())
                        }
                        error => RouteRefusal::Denied(error.to_outcome()),
                    })?;
            }
        }

        // 7. The policy permit. Nothing before this point could mint one.
        let grant = self.authorize(
            permit_descriptor,
            Some(transaction_id.clone()),
            request.audit.as_ref(),
        )?;
        let permit = grant.ok_or_else(|| {
            RouteRefusal::Denied(internal("a mutating dispatch produced no permit"))
        })?;

        admission.mark_started();
        // 8. The gateway journals `effect_started_fsynced`, executes, and derives
        //    exactly one terminal record from a re-derived typed proof.
        // What the committed effect touched, for the read-your-writes barrier:
        // file effects name their paths; an import rewrites engine metadata
        // whose paths are not known in advance; a run or probe changes no
        // project source and is left to the watcher.
        let commit_scope = match &effect {
            RouteEffect::File(intent) => Some(CommitScope::Paths(
                intent
                    .writes
                    .iter()
                    .map(|write| write.path.clone())
                    .collect(),
            )),
            RouteEffect::Process { .. }
                if plan.descriptor().identity_family
                    == godot_mcp_domain::catalog::IdentityFamily::ProjectImport =>
            {
                Some(CommitScope::Unknown)
            }
            RouteEffect::Process { .. }
            | RouteEffect::SessionStart { .. }
            | RouteEffect::Runtime { .. }
            | RouteEffect::Debug { .. }
            | RouteEffect::SessionStop { .. } => None,
            RouteEffect::Editor { intent, .. } if !intent.files.is_empty() => Some(
                CommitScope::Paths(intent.files.iter().map(|(path, _)| path.clone()).collect()),
            ),
            RouteEffect::Editor { .. } => None,
        };
        let mut session_outcome = None;
        let (effect_outcome, process_outcome, editor_outcome) = match effect {
            RouteEffect::File(intent) => {
                // Only a delete whose request proved the removed bytes is
                // executed; see `EffectGateway::execute_file_effect_removing`.
                let removals: Vec<String> = intent
                    .writes
                    .iter()
                    .filter(|write| {
                        write.operation == FileOperation::Delete
                            && matches!(write.precondition, WritePrecondition::Sha256(_))
                    })
                    .map(|write| write.path.clone())
                    .collect();
                let outcome = self
                    .policy
                    .dispatch_file_effect_removing(self.gateway.as_ref(), &permit, &removals)
                    .map_err(|error| {
                        RouteRefusal::Denied(OutcomeError::new(
                            error.error_kind(),
                            error.to_string(),
                            false,
                            BTreeMap::new(),
                        ))
                    })?;
                (Some(outcome), None, None)
            }
            RouteEffect::Process { request, .. } => {
                let outcome = self
                    .policy
                    .dispatch_process_effect(self.gateway.as_ref(), &permit, &request)
                    .map_err(process_dispatch_refusal)?;
                (None, Some(outcome), None)
            }
            RouteEffect::Editor { mutation, .. } => {
                let dispatched = match tokio::runtime::Handle::try_current() {
                    Ok(handle)
                        if handle.runtime_flavor()
                            == tokio::runtime::RuntimeFlavor::MultiThread =>
                    {
                        tokio::task::block_in_place(|| {
                            self.policy.dispatch_editor_effect(
                                self.gateway.as_ref(),
                                &permit,
                                &mutation,
                            )
                        })
                    }
                    _ => self.policy.dispatch_editor_effect(
                        self.gateway.as_ref(),
                        &permit,
                        &mutation,
                    ),
                };
                let outcome = dispatched.map_err(process_dispatch_refusal)?;
                (None, None, Some(outcome))
            }
            RouteEffect::SessionStart { request, .. } => {
                let started = blocking(|| {
                    self.policy
                        .dispatch_session_start(self.gateway.as_ref(), &permit, &request)
                });
                session_outcome = Some(started.map_err(process_dispatch_refusal)?);
                (None, None, None)
            }
            RouteEffect::Runtime { command, .. } => {
                let dispatched = blocking(|| {
                    self.policy
                        .dispatch_runtime_effect(self.gateway.as_ref(), &permit, &command)
                });
                let outcome = dispatched.map_err(process_dispatch_refusal)?;
                (None, None, Some(outcome))
            }
            RouteEffect::Debug { command, .. } => {
                let dispatched = blocking(|| {
                    self.policy
                        .dispatch_debug_effect(self.gateway.as_ref(), &permit, &command)
                });
                let outcome = dispatched.map_err(process_dispatch_refusal)?;
                (None, None, Some(outcome))
            }
            RouteEffect::SessionStop { command, .. } => {
                let dispatched = blocking(|| {
                    self.policy
                        .dispatch_session_stop(self.gateway.as_ref(), &permit, &command)
                });
                let outcome = dispatched.map_err(process_dispatch_refusal)?;
                (None, None, Some(outcome))
            }
        };

        // The recorded status is re-read from the durable journal rather than
        // inferred from the call returning: a dispatched-but-unanswered effect must
        // not be reported as done.
        let recorded = self.recorded_status(&transaction_id)?;
        if let Some(audit) = &request.audit {
            audit.record_effect_facts(RequestEffectFacts {
                boundary: audit_boundary(effect_plane),
                phase: audit_phase(recorded),
                content_digest: (effect_plane == EffectPlane::File)
                    .then(|| AuditDigest::of_bytes("effect-content", &request.bytes)),
            });
        }
        let completion = RouteCompletion::of_recorded(recorded);
        let succeeded = completion == RouteCompletion::Completed;
        let steps = vec![RouteStep {
            plane: Some(effect_plane),
            action: match effect_plane {
                EffectPlane::File => "publish_file_effect",
                EffectPlane::Process => "launch_process_effect",
                EffectPlane::LspDap => "publish_lsp_dap_effect",
                EffectPlane::Editor => "publish_editor_effect",
                EffectPlane::Runtime => "publish_runtime_effect",
            },
            outcome: if succeeded {
                StepOutcome::Succeeded
            } else {
                StepOutcome::Failed(effect_outcome.as_ref().map_or_else(
                    || recorded.as_str().to_owned(),
                    |outcome| outcome.reason.clone(),
                ))
            },
        }];

        // 9. Read-your-writes: a proven commit is observed by the workspace
        //    coordinator, and its new revision is published, before the route
        //    answers.
        let observed = match (succeeded, &commit_scope, self.workspace.get()) {
            (true, Some(scope), Some(coordinator)) => Some(coordinator.observe_commit(scope)),
            _ => None,
        };
        let workspace = observed.as_ref().map(|snapshot| snapshot.to_value());
        // The lane is released only once the revision containing this effect is
        // published, so the next mutation's revision guard sees it.
        drop(admission);

        // Snapshots advance only after a *proven committed* write.
        let refreshed = if succeeded {
            let mut planes = plan.effects();
            if !planes.contains(&effect_plane) {
                planes.push(effect_plane);
            }
            self.refresh(&planes)
        } else {
            Vec::new()
        };

        if completion == RouteCompletion::Uncertain {
            return Err(RouteRefusal::Uncertain(OutcomeError::new(
                ErrorKind::Uncertain,
                format!(
                    "transaction {} is recorded as {}; whether the effect landed is not decidable \
                     from here",
                    transaction_id,
                    recorded.as_str()
                ),
                false,
                BTreeMap::new(),
            )));
        }

        // 10. A committed mutation answers success only from a coherent snapshot
        //     that contains it (plan section 5.4). When the refresh failed the
        //     effect is still durable, so the caller is told exactly that.
        if let Some(snapshot) = &observed
            && let WorkspaceCoherence::Stale { reason } = snapshot.coherence()
        {
            let mut details = BTreeMap::new();
            details.insert(
                "transaction_id".to_owned(),
                GodotValue::String(transaction_id.clone()),
            );
            details.insert("committed".to_owned(), GodotValue::Bool(true));
            details.insert(
                "workspace_revision".to_owned(),
                GodotValue::Integer(i64::try_from(snapshot.revision()).unwrap_or(i64::MAX)),
            );
            details.insert(
                "stale_reason".to_owned(),
                GodotValue::String(reason.clone()),
            );
            return Err(RouteRefusal::Unavailable(OutcomeError::new(
                ErrorKind::Unavailable,
                format!(
                    "transaction {transaction_id} is committed, but the workspace could not be \
                     refreshed to a coherent revision that contains it ({reason}); read it \
                     again once the project can be indexed"
                ),
                false,
                details,
            )));
        }

        let effects = if succeeded {
            plan.effects()
        } else {
            Vec::new()
        };
        Ok(RouteReport {
            plan,
            completion,
            steps,
            used: vec![effect_plane],
            effects,
            refreshed,
            transaction_id: Some(transaction_id),
            recorded_status: Some(recorded),
            recorded_result: None,
            recorded_verdict: None,
            replayed: false,
            effect_outcome,
            process_outcome,
            editor_outcome,
            session_outcome,
            workspace,
        })
    }

    fn replay_report(
        &self,
        request: &RouteRequest,
        observation: &PlaneObservation,
        plan: RoutePlan,
    ) -> Result<Option<RouteReport>, RouteRefusal> {
        // 2. A quarantined transaction or an unresolved survivor refuses every
        //    mutation, on every plane, before anything else is considered.
        match self.scheduler.write_block() {
            Ok(None) => {}
            Ok(Some(block)) => return Err(RouteRefusal::WriteBlocked(block.to_outcome())),
            Err(error) => return Err(RouteRefusal::WriteBlocked(error)),
        }

        // 3. A live editor buffer that diverges from disk makes a raw byte write an
        //    actionable conflict. There is no overwrite branch and no fallback.
        if let Some(conflict) = precedence_conflict(&plan, observation.freshness(), &request.path) {
            return Err(RouteRefusal::Conflict(conflict));
        }

        // 4. Idempotency, over the durable journal.
        let route_identity = request.identity.as_ref().unwrap_or(&self.identity);
        let identity = self.identity_of(
            plan.descriptor(),
            &request.idempotency_id,
            &request.arguments,
            route_identity,
        )?;
        match self
            .scheduler
            .replay_decision(&identity)
            .map_err(RouteRefusal::Unavailable)?
        {
            ReplayDecision::Fresh => {}
            ReplayDecision::RecordedResult(recorded) => {
                let status = recorded.status();
                return Ok(Some(RouteReport {
                    completion: RouteCompletion::of_recorded(status),
                    steps: vec![RouteStep {
                        plane: plan.primary_effect_plane(),
                        action: "replay_recorded_result",
                        outcome: StepOutcome::Skipped(
                            "an exact same-context identity already has a durable record",
                        ),
                    }],
                    plan,
                    used: Vec::new(),
                    effects: Vec::new(),
                    refreshed: Vec::new(),
                    transaction_id: Some(recorded.transaction_id().to_owned()),
                    recorded_status: Some(status),
                    recorded_result: recorded.result().cloned(),
                    recorded_verdict: Some(recorded.verdict()),
                    replayed: true,
                    effect_outcome: None,
                    process_outcome: None,
                    editor_outcome: None,
                    session_outcome: None,
                    workspace: None,
                }));
            }
            ReplayDecision::Conflict(envelope) => return Err(RouteRefusal::Replay(envelope)),
        }
        Ok(None)
    }

    /// Every required plane must have a compiled-in boundary and be provided.
    ///
    /// This is the honest refusal for the editor and runtime planes in this
    /// release, and it is also where an optimistic caller observation stops
    /// mattering.
    fn require_planes(
        &self,
        plan: &RoutePlan,
        observation: &PlaneObservation,
    ) -> Result<(), RouteRefusal> {
        for plane in plan.required_planes() {
            if !observation.effective(plane) {
                return Err(RouteRefusal::Unavailable(
                    PlaneAvailability::unavailable_error(plane),
                ));
            }
        }
        Ok(())
    }

    /// Evaluate the capability policy and, for a mutation, mint the permit.
    fn authorize(
        &self,
        descriptor: &'static ToolDescriptor,
        transaction_id: Option<String>,
        audit: Option<&RequestAudit>,
    ) -> Result<Option<PolicyPermit>, RouteRefusal> {
        let tool = descriptor.name;
        let grant = match self.policy.authorize_descriptor(
            &DispatchRequest {
                tool: tool.to_owned(),
                project: self.project.clone(),
                principal: self.principal.clone(),
                session: self.session.clone(),
                transaction_id,
            },
            descriptor,
        ) {
            Ok(grant) => grant,
            Err(refusal) => {
                if let Some(audit) = audit {
                    let capability = descriptor.capability;
                    audit.record_policy_decision(
                        self.policy
                            .refusal_audit_decision(tool, capability, &refusal),
                    );
                }
                return Err(policy_refusal(refusal));
            }
        };
        if let Some(audit) = audit {
            audit.record_policy_decision(grant.audit_decision.clone());
        }
        Ok(grant.permit)
    }

    /// Build the eight-element cross-plane mutation identity for one request.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn identity_for(
        &self,
        tool: &str,
        idempotency_id: &str,
        arguments: &serde_json::Value,
    ) -> Result<MutationIdentity, RouteRefusal> {
        let descriptor = godot_mcp_domain::catalog::descriptor(tool)
            .or_else(|| godot_mcp_domain::catalog::historical_v0_1_descriptor(tool))
            .ok_or_else(|| RouteRefusal::NotRoutable(unknown_operation(tool)))?;
        self.identity_of(descriptor, idempotency_id, arguments, &self.identity)
    }

    /// Build the eight-element cross-plane mutation identity.
    fn identity_of(
        &self,
        descriptor: &'static ToolDescriptor,
        idempotency_id: &str,
        arguments: &serde_json::Value,
        identity: &RouterIdentity,
    ) -> Result<MutationIdentity, RouteRefusal> {
        let tool = descriptor.name;
        let transport = TransportBinding::new(
            identity.auth_mode(),
            identity.mode_principal().map_err(RouteRefusal::Denied)?,
            ConnectionSession::new(identity.session.clone())
                .map_err(|error| RouteRefusal::Denied(internal(&error.to_string())))?,
        )
        .map_err(|error| RouteRefusal::Denied(internal(&error.to_string())))?;
        Ok(MutationIdentity::new(
            ProjectBinding::new(
                ProjectObjectIdentity::new(self.held.object_identity.clone())
                    .map_err(|error| RouteRefusal::Denied(internal(&error.to_string())))?,
                self.held.generation,
            ),
            transport,
            family_of_descriptor(descriptor),
            IdempotencyId::new(idempotency_id.to_owned())
                .map_err(|error| RouteRefusal::Denied(internal(&error.to_string())))?,
            canonical_request_hash(tool, arguments)
                .map_err(|error| RouteRefusal::Denied(internal(&error.to_string())))?,
        ))
    }

    /// Build the complete journaled intent for one `file`-plane mutation.
    fn file_intent(
        &self,
        descriptor: &'static ToolDescriptor,
        request: &RouteRequest,
        transaction_id: &str,
    ) -> Result<FileEffectIntent, RouteRefusal> {
        let row = crate::policy::matrix::row_of(descriptor);
        let family = row.family.ok_or_else(|| {
            RouteRefusal::Denied(internal("a read row cannot open an effect transaction"))
        })?;
        let identity = request.identity.as_ref().unwrap_or(&self.identity);
        Ok(FileEffectIntent {
            transaction_id: transaction_id.to_owned(),
            tool_name: request.tool.clone(),
            operation_family: family,
            idempotency_id: request.idempotency_id.clone(),
            transport_mode: identity.journal_mode(),
            // The principal and session are journaled at the earliest fsync
            // anchor, which is what makes the recorded identity comparable to a
            // later request's tuple.
            principal: Some(identity.principal.clone()),
            session: Some(identity.session.clone()),
            arguments: request.arguments.clone(),
            writes: vec![PlannedWrite {
                path: request.path.clone(),
                operation: FileOperation::Create,
                bytes: Some(request.bytes.clone()),
                precondition: WritePrecondition::Observed,
            }],
            required_process_transaction: None,
            opened_at_unix_ms: request.queued_at_unix_ms,
            expected_revision: None,
        })
    }

    fn recorded_status(&self, transaction_id: &str) -> Result<RecordedStatus, RouteRefusal> {
        let gate = IdempotencyGate::new(
            Arc::clone(&self.store),
            self.held.object_identity.clone(),
            self.held.generation,
        );
        gate.recorded(transaction_id)
            .map(|recorded| recorded.status())
            .map_err(RouteRefusal::Unavailable)
    }

    /// Advance the refresh generation of every named plane.
    fn refresh(&self, planes: &[EffectPlane]) -> Vec<EffectPlane> {
        let mut snapshots = self
            .snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut refreshed = Vec::with_capacity(planes.len());
        for plane in planes {
            let generation = snapshots.entry(*plane).or_insert(0);
            *generation = generation.saturating_add(1);
            refreshed.push(*plane);
        }
        refreshed
    }
}

impl crate::kernel::PlanePreflight for OutcomeRouter {
    fn missing_plane(&self, tool: &str) -> Option<EffectPlane> {
        let available = self.plane_availability();
        let observation = PlaneObservation::new(
            available,
            EnginePresence::EditorConnected,
            RuntimePresence::GameRunning,
            ProtocolFidelity::Full,
            WorkspaceFreshness::Fresh,
        );
        let plan = plan_for(tool, &observation).ok()?;
        plan.required_planes()
            .into_iter()
            .find(|plane| !available.of(*plane))
    }

    fn unavailable_error(&self, plane: EffectPlane) -> OutcomeError {
        match (plane, self.gateway.process_unavailable_reason()) {
            (EffectPlane::Process, Some(reason)) => {
                crate::scheduler::PlaneAvailability::unavailable_error_because(plane, &reason)
            }
            _ => crate::scheduler::PlaneAvailability::unavailable_error(plane),
        }
    }
}

/// Which planes have a real boundary, read from the gateway's own report.
fn boundary_planes(gateway: &EffectGateway) -> PlaneAvailability {
    planes_of(gateway.boundary_availability())
}

fn planes_of(boundaries: [(crate::journal::EffectBoundaryKind, bool); 5]) -> PlaneAvailability {
    let mut file = false;
    let mut process = false;
    let mut lsp_dap = false;
    let mut editor = false;
    let mut runtime = false;
    for (boundary, available) in boundaries {
        match plane_of_boundary(boundary) {
            EffectPlane::File => file = available,
            EffectPlane::Process => process = available,
            EffectPlane::LspDap => lsp_dap = available,
            EffectPlane::Editor => editor = available,
            EffectPlane::Runtime => runtime = available,
        }
    }
    PlaneAvailability::new(file, process, lsp_dap, editor, runtime)
}

fn admission_refusal(error: AdmissionError) -> RouteRefusal {
    match error {
        AdmissionError::WriteBlocked(_) => RouteRefusal::WriteBlocked(error.to_outcome()),
        AdmissionError::PlaneUnavailable(_) => RouteRefusal::Unavailable(error.to_outcome()),
        other => RouteRefusal::Denied(other.to_outcome()),
    }
}

fn process_dispatch_refusal(error: crate::policy::authority::PolicyDispatchError) -> RouteRefusal {
    RouteRefusal::Denied(error.to_outcome())
}

const fn audit_boundary(plane: EffectPlane) -> AuditBoundary {
    match plane {
        EffectPlane::File => AuditBoundary::File,
        EffectPlane::Process => AuditBoundary::Process,
        EffectPlane::LspDap => AuditBoundary::LspDap,
        EffectPlane::Editor => AuditBoundary::Editor,
        EffectPlane::Runtime => AuditBoundary::Runtime,
    }
}

const fn audit_phase(status: RecordedStatus) -> AuditPhase {
    match status {
        RecordedStatus::NotStarted => AuditPhase::Opened,
        RecordedStatus::Dispatched => AuditPhase::EffectStarted,
        RecordedStatus::Committed => AuditPhase::Committed,
        RecordedStatus::Compensated => AuditPhase::Compensated,
        RecordedStatus::Unresolved => AuditPhase::Quarantined,
    }
}

fn policy_refusal(refusal: PolicyRefusal) -> RouteRefusal {
    let error = OutcomeError::new(
        refusal.error_kind(),
        refusal.to_string(),
        false,
        BTreeMap::new(),
    );
    match refusal {
        PolicyRefusal::UnknownOperation { .. } => RouteRefusal::NotRoutable(error),
        PolicyRefusal::BoundaryUnavailable { .. } => RouteRefusal::Unavailable(error),
        PolicyRefusal::Quarantined(_) | PolicyRefusal::UnresolvedSurvivor(_) => {
            RouteRefusal::WriteBlocked(error)
        }
        _ => RouteRefusal::Denied(error),
    }
}

fn internal(message: &str) -> OutcomeError {
    OutcomeError::new(ErrorKind::Internal, message, false, BTreeMap::new())
}

#[cfg(test)]
mod self_audit {
    //! Source-scanning self-audit: the four bans this layer must keep are asserted
    //! against its own bytes, so a future edit that reintroduces one fails a test
    //! rather than passing review.
    //!
    //! The scan covers the *code* region only — from the first `use` to the test
    //! module — because this module's own normative prose legitimately names the
    //! very patterns the bans forbid.

    use super::*;

    const ROUTER_SOURCE: &str = include_str!("mod.rs");
    const PLAN_SOURCE: &str = include_str!("plan.rs");
    const CONFLICT_SOURCE: &str = include_str!("conflict.rs");
    const HEALTH_SOURCE: &str = include_str!("health.rs");
    const RECOVERY_SOURCE: &str = include_str!("recovery.rs");

    /// The code region: after the module documentation, before the tests.
    fn code_region(source: &str) -> &str {
        let start = source.find("\nuse ").map_or(0, |index| index + 1);
        let end = source[start..]
            .find("#[cfg(test)]")
            .map_or(source.len(), |index| start + index);
        &source[start..end]
    }

    fn all_sources() -> [(&'static str, &'static str); 5] {
        [
            ("router/mod.rs", ROUTER_SOURCE),
            ("router/plan.rs", PLAN_SOURCE),
            ("router/conflict.rs", CONFLICT_SOURCE),
            ("router/health.rs", HEALTH_SOURCE),
            ("router/recovery.rs", RECOVERY_SOURCE),
        ]
    }

    /// The router defines no second gateway, scheduler, journal or schema.
    #[test]
    fn the_router_defines_no_second_authority_or_persistence() {
        for (name, source) in all_sources() {
            let source = code_region(source);
            for banned in [
                "struct EffectGateway",
                "struct Scheduler",
                "struct TransactionStore",
                "struct EffectPermit",
                "rusqlite",
                "Connection::open",
                "CREATE TABLE",
                "include_str!(\"../../../../schemas",
            ] {
                assert!(
                    !source.contains(banned),
                    "{name} must reuse the existing machinery, never define another ({banned})"
                );
            }
        }
    }

    #[test]
    fn process_dispatch_timeout_is_not_reclassified_as_internal() {
        let refusal =
            process_dispatch_refusal(crate::policy::authority::PolicyDispatchError::Effect(
                crate::effect::EffectError::OperationDeadline(
                    crate::scheduler::OperationDeadline::expired_error(),
                ),
            ));

        let outcome = refusal.to_outcome();

        assert_eq!(outcome.code(), "timeout");
        assert!(!outcome.retryable());
    }

    /// The router never reaches the filesystem or spawns a process itself. Every
    /// byte it publishes goes through the gateway's `file` boundary, and every
    /// process goes through the supervised `process` boundary.
    #[test]
    fn the_router_never_touches_the_filesystem_or_spawns_a_process() {
        for (name, source) in all_sources() {
            let source = code_region(source);
            for banned in [
                "std::fs::",
                "File::open",
                "File::create",
                "Path::new",
                "PathBuf::from",
                "canonicalize",
                "Command::new",
                ".spawn(",
            ] {
                assert!(
                    !source.contains(banned),
                    "{name} must route through a boundary, never act directly ({banned})"
                );
            }
        }
    }

    /// There is no force, clear or override path, and no way to widen a plane.
    #[test]
    fn the_router_offers_no_force_clear_or_widen_path() {
        for (name, source) in all_sources() {
            let source = code_region(source);
            for banned in [
                "fn force_",
                "fn clear_write_block",
                "fn override_",
                "force: bool",
                "allow_raw_text",
                "fn fallback_to_",
                "unwrap_or(true)",
            ] {
                assert!(
                    !source.contains(banned),
                    "{name} must offer no escape hatch ({banned})"
                );
            }
        }
    }

    /// The plan's degradation type cannot express a substituted plane.
    ///
    /// This is asserted structurally rather than by naming: `Degradation` has
    /// exactly one non-`None` variant, it carries the plane that answered, and the
    /// router only ever constructs it with the plane the outcome required.
    #[test]
    fn degradation_cannot_express_a_substituted_plane() {
        use godot_mcp_domain::CapabilityProfile;
        use godot_mcp_domain::manifests::canonical_manifest;

        let source = code_region(PLAN_SOURCE);
        let declaration = source
            .split_once("pub enum Degradation {")
            .expect("the enum exists")
            .1
            .split_once("\n}")
            .expect("the enum closes")
            .0;
        assert_eq!(
            declaration.matches("ReducedFidelity {").count(),
            1,
            "exactly one non-full-fidelity variant may exist"
        );
        for banned in [
            "SubstitutedPlane",
            "FellBackTo",
            "RawTextFallback",
            "DowngradedToDisk",
        ] {
            assert!(
                !source.contains(banned),
                "a substituted plane must remain unrepresentable ({banned})"
            );
        }

        // The behavioural half: whatever the environment looks like, a degraded
        // answer always names a plane the outcome itself required. A plan that
        // reported a plane outside its own requirement set would be a substitution.
        for engine in [
            EnginePresence::Absent,
            EnginePresence::HeadlessOnly,
            EnginePresence::EditorConnected,
        ] {
            for runtime in [RuntimePresence::NoInstance, RuntimePresence::GameRunning] {
                for protocol in [ProtocolFidelity::Full, ProtocolFidelity::Degraded] {
                    for freshness in [
                        WorkspaceFreshness::Fresh,
                        WorkspaceFreshness::StaleIndex,
                        WorkspaceFreshness::UnsavedEditorBuffer,
                    ] {
                        let observation = PlaneObservation::new(
                            PlaneAvailability::new(true, true, true, true, true),
                            engine,
                            runtime,
                            protocol,
                            freshness,
                        );
                        for tool in &canonical_manifest(CapabilityProfile::Full).tools {
                            let plan =
                                plan::plan_for(tool, &observation).expect("a canonical tool");
                            if let Degradation::ReducedFidelity { plane, .. } = plan.degradation() {
                                let required = plan.required_planes();
                                assert!(
                                    required.contains(&plane) || required.is_empty(),
                                    "{tool} degraded onto {}, which it never required",
                                    plane.as_str()
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Completion is derived from the durable record and from the steps, and the
    /// two derivations are the only ones that exist.
    #[test]
    fn completion_is_derived_and_never_assumed() {
        let router = code_region(ROUTER_SOURCE);
        assert_eq!(router.matches("fn of_recorded").count(), 1);
        assert_eq!(router.matches("fn of_steps").count(), 1);
        assert_eq!(
            router
                .matches("completion: RouteCompletion::Completed")
                .count(),
            0,
            "no site may hard-code a completed outcome"
        );
        assert!(
            router.contains("let recorded = self.recorded_status(&transaction_id)?;"),
            "the recorded status must be re-read from the journal after the effect"
        );
    }

    /// Only `committed` completes, on every representable status.
    #[test]
    fn only_a_committed_record_completes() {
        for status in RecordedStatus::ALL {
            let completion = RouteCompletion::of_recorded(status);
            assert_eq!(
                completion == RouteCompletion::Completed,
                status == RecordedStatus::Committed,
                "{} must not complete",
                status.as_str()
            );
            if matches!(
                status,
                RecordedStatus::Dispatched | RecordedStatus::Unresolved
            ) {
                assert_eq!(
                    completion,
                    RouteCompletion::Uncertain,
                    "{} is not decidable and must not be refused either",
                    status.as_str()
                );
            }
        }
    }

    /// A mixed step set is never aggregated into a success.
    #[test]
    fn a_partial_failure_is_never_aggregated_into_a_success() {
        let mixed = [
            StepOutcome::Succeeded,
            StepOutcome::Failed("the second plane refused".to_owned()),
        ];
        assert_eq!(RouteCompletion::of_steps(&mixed), RouteCompletion::Partial);
        assert_eq!(
            RouteCompletion::of_steps(&[StepOutcome::Failed("nothing ran".to_owned())]),
            RouteCompletion::Refused
        );
        assert_eq!(
            RouteCompletion::of_steps(&[
                StepOutcome::Succeeded,
                StepOutcome::Skipped("not applicable"),
            ]),
            RouteCompletion::Completed
        );
        assert_eq!(RouteCompletion::of_steps(&[]), RouteCompletion::Completed);
    }

    /// The write block is consulted before anything else a mutation does.
    #[test]
    fn the_write_block_precedes_every_other_mutation_gate() {
        let router = code_region(ROUTER_SOURCE);
        let mutation_and_helpers = router
            .split_once("async fn route_mutation")
            .expect("the mutation path exists")
            .1;
        let (mutation, replay_helper) = mutation_and_helpers
            .split_once("fn replay_report")
            .expect("the mutation path delegates its replay gate");
        let replay_helper = replay_helper
            .split_once("fn require_planes")
            .expect("the replay helper has a bounded source region")
            .0;
        let block = replay_helper
            .find("self.scheduler.write_block()")
            .expect("the write block is consulted");
        for later in ["precedence_conflict(", "self.identity_of("] {
            let at = replay_helper.find(later).unwrap_or_else(|| {
                panic!("the replay helper must still contain {later}");
            });
            assert!(block < at, "the write block must be checked before {later}");
        }
        let replay = mutation
            .find("self.replay_report(")
            .expect("the mutation path enters the replay helper");
        for later in [
            "self.require_planes(",
            "admit_mutation(",
            "open_file_transaction(",
            "self.authorize(",
        ] {
            let at = mutation.find(later).unwrap_or_else(|| {
                panic!("the mutation path must still contain {later}");
            });
            assert!(
                replay < at,
                "the replay helper must be entered before {later}"
            );
        }
    }

    /// Availability is re-derived from the gateway, never taken from the caller.
    #[test]
    fn availability_is_re_derived_from_the_gateway() {
        let router = code_region(ROUTER_SOURCE);
        assert!(
            router.contains(".with_boundaries(self.plane_availability())"),
            "the caller's claimed boundary set must be replaced before execution"
        );
        assert_eq!(
            router.matches("fn boundary_planes").count(),
            1,
            "exactly one place may derive the boundary set"
        );
        assert!(
            router.contains("gateway.boundary_availability()"),
            "the boundary set must come from the gateway's own report"
        );
    }
}
