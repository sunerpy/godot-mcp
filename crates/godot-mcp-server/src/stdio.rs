//! The stdio transport: the default, always-available local transport.
//!
//! stdio invokes the *same* [`ServiceKernel`] and [`RmcpAdapter`] as the optional
//! HTTP transport — there is no second handler and no custom framing. Its
//! lifecycle is:
//!
//! 1. load and validate configuration (Todo 2's typed loader);
//! 2. install protocol-safe diagnostics — stderr or a rolling file, never stdout;
//! 3. acquire the Todo 2 project lease (held root + `project.godot` handles);
//! 4. open the durable journal and reconcile every transaction a previous process
//!    left open, before a single request is served;
//! 5. serve MCP over stdin/stdout until the peer closes, the process is
//!    cancelled, or a shutdown signal arrives; and
//! 6. release the lease and the journal deterministically.
//!
//! Stdout carries protocol frames only.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use godot_mcp_analysis::project::ResolvedProject;
use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::config::{Limits, ServerConfig};
use godot_mcp_domain::contracts::{PrincipalId, SchedulerMode};
use godot_mcp_domain::lease::{Lease, LeaseError};
use rmcp::ServiceExt;

use crate::artifacts::ArtifactStore;
use crate::audit::{AuditFailurePolicy, AuditLog, AuditSinkConfig};
use crate::authoring::{AuthoringRuntime, AuthoringRuntimeConfig, register_authoring_tools};
use crate::core::{CoreLimits, CoreSession, register_core_tools};
use crate::diagnostics::DiagnosticsContext;
use crate::editor::boundary::BridgeEditorBoundary;
use crate::editor::bridge::EditorBridge;
use crate::editor::tools::{EditorToolRuntime, register_editor_tools};
use crate::effect::{
    EffectBoundaries, EffectGateway, PermitAuthority, SafeRootFileBoundary, UnavailableBoundaries,
};
use crate::engine_plane::session::SessionManager;
use crate::engine_plane::session::boundary::SessionRuntimeBoundary;
use crate::engine_plane::session::debug::{DebugManager, SessionDebugBoundary};
use crate::engine_plane::session::export::ExportArtifacts;
use crate::engine_plane::{
    EngineRuntime, EngineRuntimeConfig, ProductionLspDapBoundary, ProductionProcessBoundary,
    reconcile_project_launches, register_engine_tools, register_session_tools,
};
use crate::idempotency::TransportAuthMode;
use crate::journal::{
    HeldProject, ReconciliationReport, SafeRootProofSource, TransactionStore, inspect_legacy_store,
    reconcile_linked_process_transactions, reconcile_open_transactions,
};
use crate::kernel::{AccessContext, PrivatePaths, ServiceKernel};
use crate::limits::{LimitEnforcer, LimitPolicy};
use crate::observability::{TracingConfig, init_tracing, validate_default_stderr};
use crate::policy::{
    BoundIdentity, BoundaryAvailability, NoLaunchPlane, PolicyAuthority, SurvivorProbe,
};
use crate::project_context::{LegacyPlan, ProjectContext, ProjectContextError};
use crate::rmcp_adapter::RmcpAdapter;
use crate::router::{OutcomeRouter, RouterIdentity, RouterParts, SharedSurvivors};
use crate::scheduler::{
    JournalBinding, OperationDeadline, QueueDeadline, Scheduler, SchedulerLimits,
};
use crate::workspace::WorkspaceCoordinator;
use godot_mcp_bridge::protocol::envelope::ProjectClaim;
use godot_mcp_domain::manifests::canonical_manifest;

pub use crate::project_context::INITIAL_PROJECT_GENERATION;

/// Directory name, inside the discovered project root, holding private server
/// state. It is opened relative to the held root handle by
/// [`ProjectContext::acquire`](crate::project_context::ProjectContext::acquire).
pub const STATE_DIR_NAME: &str = godot_mcp_domain::lease::PROJECT_STATE_DIR_NAME;

const AUDIT_MAX_BYTES: u64 = 8 * 1024 * 1024;
const AUDIT_MAX_FILES: u8 = 4;

/// The connection/session component of the stdio identity tuple.
///
/// It is a stable constant rather than a per-process value, and that is
/// deliberate. The session dimension exists so one context cannot replay another
/// context's recorded mutation. For stdio the context is the local OS principal
/// plus the held project object identity and generation — all three of which are
/// already separate dimensions of the tuple — and the advisory project lease
/// guarantees a single writer. A per-process value would add no isolation while
/// making a recorded mutation unreplayable after the very crash the durable
/// journal exists to survive.
pub const STDIO_CONNECTION_SESSION: &str = "stdio";

/// The permit-authority epoch the stdio assembly binds every component to.
const STDIO_EPOCH: u64 = 1;

/// One resolved stdio serve request.
#[derive(Debug, Clone)]
pub struct StdioOptions {
    /// Candidate project directory; the real root is discovered upward from it.
    pub project: PathBuf,
    /// Explicit Godot executable configured by the operator.
    pub godot_binary: Option<PathBuf>,
    /// Capability profile to expose.
    pub profile: CapabilityProfile,
    /// Local principal identifier for the stdio boundary.
    pub principal: PrincipalId,
    /// Trusted observation timestamp used for retention checks.
    pub observed_at_unix_ms: u64,
    /// Operational limits shared by stdio and optional HTTP.
    pub limits: Limits,
    /// Which boundary contains every Godot launch, and whether the project is
    /// trusted.
    pub process: godot_mcp_domain::config::ProcessConfig,
}

/// A stdio startup failure. No message carries project source text or secrets.
#[derive(Debug, thiserror::Error)]
pub enum StdioError {
    /// Project ownership could not be established.
    #[error("project ownership could not be established: {0}")]
    Lease(#[from] LeaseError),
    /// The durable journal is unavailable or reconciliation failed.
    #[error("durable transaction journal is unavailable: {0}")]
    Store(String),
    /// The structured audit sink could not be installed.
    #[error("audit log is unavailable: {0}")]
    Audit(String),
    /// Static project analysis could not be assembled safely.
    #[error("static project analysis is unavailable: {0}")]
    Analysis(String),
    /// Diagnostics could not be installed.
    #[error("diagnostics could not be installed: {0}")]
    Diagnostics(String),
    /// The transport failed.
    #[error("stdio transport failed: {0}")]
    Transport(String),
}

/// The assembled, running-capable server for one held project.
pub struct ProjectRuntime {
    context: ProjectContext,
    /// The live-editor bridge, for a profile with editor tools. Dropping the
    /// runtime stops it and removes its rendezvous file.
    editor_bridge: Option<Arc<EditorBridge>>,
    /// The managed Godot sessions, for a profile that runs the project.
    /// Dropping the runtime stops every one of them.
    sessions: Option<Arc<SessionManager>>,
    legacy_block: Option<String>,
    kernel: Arc<ServiceKernel>,
    access: AccessContext,
    scheduler: Arc<Scheduler>,
    gateway: Arc<EffectGateway>,
    authority: Arc<PermitAuthority>,
    // The production store handles live in the router, the gateway, the policy
    // authority and the scheduler; this one exists solely so the acceptance suite
    // can journal a transaction directly. Gating it is honest: the feature it
    // serves is a test affordance, not an unwired production capability.
    #[cfg(feature = "test-hooks")]
    store: Arc<TransactionStore>,
    router: Arc<OutcomeRouter>,
    reconciliation: ReconciliationReport,
}

impl Drop for ProjectRuntime {
    fn drop(&mut self) {
        // Tell a connected plugin the server is stopping and remove the
        // rendezvous file now, rather than whenever the last handle drops.
        if let Some(bridge) = &self.editor_bridge {
            bridge.stop();
        }
        if let Some(sessions) = &self.sessions {
            sessions.stop_all();
        }
    }
}

impl ProjectRuntime {
    /// Acquire the project, open the journal, reconcile, and assemble the kernel.
    ///
    /// Reconciliation runs here — before any request can be served — so a
    /// transaction interrupted by a previous crash is classified as proven
    /// pre-state, proven post-state or write-blocking quarantine first.
    pub fn open(options: &StdioOptions) -> Result<Self, StdioError> {
        // The stdio lane attaches no process boundary, so there is provably no
        // journaled launch that could have survived. That is a *fact* about this
        // lane, not a stand-in for an unreadable survivor state: an unreadable state
        // is `WriteBlock::Unreadable`, and it refuses rather than reporting "clear".
        Self::open_with(options, Arc::new(NoLaunchPlane))
    }

    /// Open the runtime with an injected launch survivor probe.
    ///
    /// This shares its whole body with [`Self::open`]; only the probe differs, so a
    /// test that drives a survivor block exercises the production assembly rather
    /// than a parallel one.
    #[cfg(feature = "test-hooks")]
    pub(crate) fn open_with_survivors(
        options: &StdioOptions,
        survivors: Arc<dyn SurvivorProbe>,
    ) -> Result<Self, StdioError> {
        Self::open_with(options, survivors)
    }

    fn open_with(
        options: &StdioOptions,
        survivors: Arc<dyn SurvivorProbe>,
    ) -> Result<Self, StdioError> {
        // The candidate spelling is consumed here and never reaches another
        // component: every state owner below is derived from the context.
        let mut context = ProjectContext::acquire(&options.project).map_err(context_error)?;
        let lease = context.lease().clone();
        let state_dir = context.state_root().to_path_buf();
        let project = context.identity().clone();
        // One coordinator owns the live workspace snapshot: inventory, settings,
        // index and revision. Revision 1 is published from a full scan here; a
        // scan that cannot complete yields an explicitly stale snapshot instead
        // of a startup failure, so the server can say why.
        let workspace = WorkspaceCoordinator::open(
            lease.clone(),
            INITIAL_PROJECT_GENERATION,
            state_dir.join("index"),
        );
        if let Err(error) = workspace.start_watching() {
            tracing::warn!(
                event = "workspace_watch_unavailable",
                reason = %error.message(),
                "the workspace watcher is unavailable; committed mutations still refresh the snapshot"
            );
        }
        let resolved = ResolvedProject::from_lease(lease.clone(), INITIAL_PROJECT_GENERATION)
            .map_err(|error| StdioError::Analysis(error.to_string()))?;
        let core_session = Arc::new(
            CoreSession::with_workspace(resolved, Arc::clone(&workspace), CoreLimits::default())
                .with_engine_binary(options.godot_binary.clone()),
        );
        let held = context.held().clone();
        // One registry decides which boundary contains every launch of this
        // runtime; reconciliation below reopens records of every provider it
        // knows.
        let providers = Arc::new(crate::engine_plane::providers::LaunchProviders::for_host(
            &options.process,
            &state_dir,
            crate::engine_plane::process_supervisor_binary(),
        ));

        let store = Arc::new(
            TransactionStore::open(&state_dir.join("journal"))
                .map_err(|error| StdioError::Store(error.to_string()))?,
        );
        // Plan section 14.3: the legacy inventory is decided against the
        // canonical journal's open records *before* anything is settled.
        let canonical_open = store
            .open_transactions()
            .map_err(|error| StdioError::Store(error.to_string()))?
            .len();
        if let LegacyPlan::Reconcile(relative) = context.legacy_plan(canonical_open) {
            let legacy_dir = context.legacy_root_path(&relative);
            let summary = reconcile_state_root(&legacy_dir, &held, &lease, &providers)?;
            context.note_reconciled(&relative, summary);
        }
        context.record_migration().map_err(context_error)?;
        let legacy_block = context.legacy_write_block();
        let survivors: Arc<dyn SurvivorProbe> = match &legacy_block {
            Some(reason) => Arc::new(LegacyStateBlock {
                reason: reason.clone(),
            }),
            None => survivors,
        };
        // A profile that runs the project owns managed Godot sessions. Every
        // session a previous server left running is terminated before the
        // journal is reconciled, so its launch scratch can be reclaimed.
        let sessions = canonical_manifest(options.profile)
            .capabilities
            .contains(&godot_mcp_domain::contracts::Capability::RuntimeExecute)
            .then(|| {
                Arc::new(SessionManager::new(
                    Arc::clone(&store),
                    &state_dir,
                    context.display_root(),
                    ProjectClaim {
                        object_identity: held.object_identity.clone(),
                        root_object: held.root_object.clone(),
                        file_object: held.file_object.clone(),
                        generation: held.generation,
                    },
                    Arc::clone(&providers),
                ))
            });
        // Editor sessions' language servers and debug adapters are reached
        // only through their launch receipts in this journal.
        let debug = sessions.as_ref().map(|sessions| {
            Arc::new(DebugManager::new(
                Arc::clone(sessions),
                Arc::clone(&store),
                held.object_identity.clone(),
                held.generation,
            ))
        });
        if let Some(sessions) = &sessions {
            for (session, state) in sessions.reconcile() {
                tracing::info!(
                    event = "session_reconciled",
                    session = %session,
                    state = state.as_str(),
                    "a session left by a previous server was reconciled"
                );
            }
        }
        let settled = reconcile_journal(&store, &state_dir, &held, &lease, &providers)?;
        let reconciliation = ReconciliationReport::from_settled(&settled);

        let held_for_router = held.clone();
        let held_for_engine = held.clone();
        // A profile with editor tools serves the live editor: the bridge
        // listens on loopback and writes its private rendezvous file now, so a
        // plugin can connect as soon as the server is up.
        let editor_bridge = if canonical_manifest(options.profile)
            .capabilities
            .contains(&godot_mcp_domain::contracts::Capability::EditorMutate)
        {
            Some(
                EditorBridge::start(
                    &state_dir,
                    ProjectClaim {
                        object_identity: held.object_identity.clone(),
                        root_object: held.root_object.clone(),
                        file_object: held.file_object.clone(),
                        generation: held.generation,
                    },
                )
                .map_err(StdioError::Analysis)?,
            )
        } else {
            None
        };
        let editor_boundary: Box<dyn crate::effect::boundary::EditorEffectBoundary> =
            match &editor_bridge {
                Some(bridge) => Box::new(BridgeEditorBoundary::new(Arc::clone(bridge))),
                None => Box::new(UnavailableBoundaries),
            };
        let limit_policy = LimitPolicy::from_config(&options.limits);
        let artifacts = Arc::new(
            ArtifactStore::open(&state_dir.join("artifacts"), options.observed_at_unix_ms)
                .map_err(StdioError::Analysis)?,
        );
        if let Some(sessions) = &sessions {
            sessions.attach_export_artifacts(ExportArtifacts {
                store: Arc::clone(&artifacts),
                project: project.clone(),
                retention_ms: limit_policy.artifact_retention_ms,
                max_range_bytes: limit_policy.max_range_bytes,
            });
        }
        let mut process =
            ProductionProcessBoundary::for_project(Arc::clone(&store), Arc::clone(&providers));
        let runtime: Box<dyn crate::effect::boundary::RuntimeEffectBoundary> = match &sessions {
            Some(sessions) => {
                process = process.with_sessions(Arc::clone(sessions));
                Box::new(SessionRuntimeBoundary::new(
                    Arc::clone(sessions),
                    Arc::clone(&artifacts),
                    project.clone(),
                    limit_policy.artifact_retention_ms,
                    limit_policy.max_range_bytes,
                ))
            }
            None => Box::new(UnavailableBoundaries),
        };
        let lsp_dap: Box<dyn crate::effect::boundary::LspDapEffectBoundary> = match &debug {
            Some(debug) => Box::new(SessionDebugBoundary::new(Arc::clone(debug))),
            None => Box::new(ProductionLspDapBoundary),
        };
        let gateway = Arc::new(EffectGateway::new(
            Arc::clone(&store),
            held,
            STDIO_EPOCH,
            Box::new(SafeRootProofSource::new(lease.safe_root())),
            EffectBoundaries {
                file: Box::new(SafeRootFileBoundary::new(lease.safe_root())),
                process: Box::new(process),
                lsp_dap,
                editor: editor_boundary,
                runtime,
            },
        ));
        let authority = Arc::new(PermitAuthority::new(Arc::clone(&store), STDIO_EPOCH));
        let defaults = SchedulerLimits::default();
        let scheduler_limits = SchedulerLimits {
            queue_limit: options.limits.max_queue,
            queue_deadline: QueueDeadline::new(options.limits.deadline_ms)
                .map_err(|error| StdioError::Analysis(error.to_string()))?,
            operation_deadline: OperationDeadline::new(options.limits.deadline_ms)
                .map_err(|error| StdioError::Analysis(error.to_string()))?,
            shutdown_budget_ms: options.limits.deadline_ms,
            ..defaults
        };
        let scheduler = Arc::new(Scheduler::new(project.clone(), scheduler_limits));

        let bound_identity = BoundIdentity {
            project: project.clone(),
            principal: Some(options.principal.clone()),
            session: None,
        };
        let policy = Arc::new(PolicyAuthority::new(
            options.profile,
            bound_identity,
            Arc::clone(&store),
            BoundaryAvailability::from_gateway(gateway.as_ref()),
            Box::new(SharedSurvivors(Arc::clone(&survivors))),
            STDIO_EPOCH,
        ));
        // Assembling the router is what binds the cross-plane idempotency gate, the
        // durable store and the survivor probe to the scheduler, so the shipped
        // binary enforces them rather than compiling three inert `None`s.
        let router = OutcomeRouter::assemble(RouterParts {
            project: project.clone(),
            held: held_for_router,
            store: Arc::clone(&store),
            gateway: Arc::clone(&gateway),
            scheduler: Arc::clone(&scheduler),
            policy: Arc::clone(&policy),
            survivors,
            identity: RouterIdentity {
                transport_mode: TransportAuthMode::Stdio,
                principal: options.principal.as_str().to_owned(),
                session: STDIO_CONNECTION_SESSION.to_owned(),
            },
            principal: Some(options.principal.clone()),
            session: None,
        });

        router.attach_workspace(Arc::clone(&workspace));
        if let Some(bridge) = &editor_bridge {
            router.attach_editor(Arc::new(BridgeEditorBoundary::new(Arc::clone(bridge))));
        }

        let access = AccessContext::new(
            project.clone(),
            options.principal.clone(),
            None,
            options.observed_at_unix_ms,
        );
        let limits = Arc::new(
            LimitEnforcer::new(limit_policy)
                .map_err(|error| StdioError::Analysis(error.to_string()))?,
        );
        let audit = Arc::new(
            AuditLog::open(AuditSinkConfig {
                path: state_dir.join("audit").join("audit.jsonl"),
                max_bytes: AUDIT_MAX_BYTES,
                max_files: AUDIT_MAX_FILES,
                failure_policy: AuditFailurePolicy::FailClosed,
            })
            .map_err(|error| StdioError::Audit(error.to_string()))?,
        );
        let diagnostics = Arc::new(
            DiagnosticsContext::new(
                options.profile,
                project.clone(),
                Arc::clone(&core_session),
                Arc::clone(&scheduler),
                Arc::clone(&router),
                scheduler_limits,
            )
            .with_process_boundary(Arc::clone(&providers)),
        );
        let mut kernel = ServiceKernel::new(options.profile)
            .with_scheduler(Arc::clone(&scheduler))
            .with_diagnostics(diagnostics)
            .with_operational_controls(limits, audit)
            .with_artifact_store(Arc::clone(&artifacts))
            .with_private_paths(PrivatePaths::for_project(
                context.display_root(),
                context.state_root(),
            ))
            .with_plane_preflight(Arc::clone(&router) as Arc<dyn crate::kernel::PlanePreflight>);
        register_core_tools(&mut kernel, core_session)
            .map_err(|error| StdioError::Analysis(error.to_string()))?;
        let engine_runtime = Arc::new(EngineRuntime::new(EngineRuntimeConfig {
            store: Arc::clone(&store),
            root: lease.safe_root(),
            project_root: context.display_root().to_path_buf(),
            state_dir: state_dir.clone(),
            godot_binary: options.godot_binary.clone(),
            held: held_for_engine,
            router: Arc::clone(&router),
            operation_deadline: scheduler_limits.operation_deadline,
            workspace: Arc::clone(&workspace),
            editor: editor_bridge.clone(),
            providers: Arc::clone(&providers),
        }));
        register_engine_tools(&mut kernel, Arc::clone(&engine_runtime))
            .map_err(|error| StdioError::Analysis(error.to_string()))?;
        if let Some(debug) = &debug {
            register_session_tools(&mut kernel, Arc::clone(&engine_runtime), Arc::clone(debug))
                .map_err(|error| StdioError::Analysis(error.to_string()))?;
        }
        let authoring = Arc::new(AuthoringRuntime::new(AuthoringRuntimeConfig {
            root: lease.safe_root(),
            router: Arc::clone(&router),
            workspace: Arc::clone(&workspace),
            uploads_dir: state_dir.join("uploads"),
            generation: INITIAL_PROJECT_GENERATION,
        }));
        register_authoring_tools(&mut kernel, authoring)
            .map_err(|error| StdioError::Analysis(error.to_string()))?;
        if let Some(bridge) = &editor_bridge {
            register_editor_tools(
                &mut kernel,
                Arc::new(EditorToolRuntime {
                    bridge: Arc::clone(bridge),
                    router: Arc::clone(&router),
                    workspace: Arc::clone(&workspace),
                    root: lease.safe_root(),
                }),
            )
            .map_err(|error| StdioError::Analysis(error.to_string()))?;
        }
        // Discovery is the catalog filtered by profile, so a catalog tool without a
        // production handler would be advertised and unanswerable. That build is
        // refused here rather than served.
        let missing = kernel.missing_handlers();
        if !missing.is_empty() {
            return Err(StdioError::Analysis(format!(
                "catalog tools have no production handler: {}",
                missing.join(", ")
            )));
        }

        drop(lease);
        Ok(Self {
            context,
            editor_bridge,
            sessions,
            legacy_block,
            kernel: Arc::new(kernel),
            access,
            scheduler,
            gateway,
            authority,
            #[cfg(feature = "test-hooks")]
            store,
            router,
            reconciliation,
        })
    }

    /// The cross-plane outcome router every transport routes through.
    #[must_use]
    pub fn router(&self) -> Arc<OutcomeRouter> {
        Arc::clone(&self.router)
    }

    /// Which durable components the assembled scheduler actually has bound.
    #[must_use]
    pub fn journal_binding(&self) -> JournalBinding {
        self.scheduler.journal_binding()
    }

    /// Whether one transaction is journaled at all.
    #[cfg(feature = "test-hooks")]
    #[must_use]
    pub(crate) fn transaction_exists(&self, transaction_id: &str) -> bool {
        self.store.load(transaction_id).is_ok()
    }

    #[cfg(feature = "test-hooks")]
    pub(crate) fn transaction_phase_for_tests(&self, transaction_id: &str) -> Option<String> {
        self.store
            .load(transaction_id)
            .ok()
            .map(|record| record.phase.as_str().to_owned())
    }

    /// Journal one transaction and force it into a terminal phase.
    ///
    /// This exists so the acceptance suite can reach a write-blocking quarantine
    /// without corrupting a project: it drives the real gateway to open the
    /// transaction and then the real store to record the phase.
    ///
    /// # Errors
    /// Returns the store failure that prevented the transition.
    #[cfg(feature = "test-hooks")]
    pub(crate) fn force_phase_for_tests(
        &self,
        transaction_id: &str,
        path: &str,
        phase: crate::journal::Phase,
        reason: &str,
    ) -> Result<(), StdioError> {
        let intent = crate::effect::FileEffectIntent {
            transaction_id: transaction_id.to_owned(),
            tool_name: godot_mcp_domain::catalog::EngineOperation::UpdateResource
                .tool_name()
                .to_owned(),
            operation_family: crate::journal::OperationFamily::ProjectMutation,
            idempotency_id: format!("forced-{transaction_id}"),
            transport_mode: crate::journal::TransportMode::Stdio,
            principal: Some(self.access.principal().as_str().to_owned()),
            session: Some(STDIO_CONNECTION_SESSION.to_owned()),
            arguments: serde_json::json!({ "contents": "forced", "path": path }),
            writes: vec![crate::effect::PlannedWrite {
                path: path.to_owned(),
                operation: crate::journal::FileOperation::Create,
                bytes: Some(b"forced".to_vec()),
                precondition: crate::effect::WritePrecondition::Observed,
            }],
            required_process_transaction: None,
            opened_at_unix_ms: 0,
            expected_revision: None,
        };
        self.gateway
            .open_file_transaction(&intent)
            .map_err(|error| StdioError::Store(error.to_outcome().message().to_owned()))?;
        self.store
            .advance(
                transaction_id,
                phase,
                None,
                Some(reason.to_owned()),
                Some(match phase {
                    crate::journal::Phase::CommittedFsynced => {
                        crate::journal::TerminalOutcome::Succeeded
                    }
                    crate::journal::Phase::Compensated => crate::journal::TerminalOutcome::failed(
                        godot_mcp_domain::ErrorKind::Unavailable,
                    ),
                    crate::journal::Phase::Quarantined => crate::journal::TerminalOutcome::failed(
                        godot_mcp_domain::ErrorKind::Uncertain,
                    ),
                    crate::journal::Phase::IntentFsynced
                    | crate::journal::Phase::StagedFsynced
                    | crate::journal::Phase::EffectStartedFsynced => {
                        crate::journal::TerminalOutcome::failed(
                            godot_mcp_domain::ErrorKind::Internal,
                        )
                    }
                }),
            )
            .map(|_record| ())
            .map_err(|error| StdioError::Store(error.to_string()))
    }

    /// Which effect boundaries the router derived from the gateway.
    #[must_use]
    pub fn router_planes(&self) -> Vec<(String, bool)> {
        let availability = self.router.plane_availability();
        crate::scheduler::EffectPlane::ALL
            .into_iter()
            .map(|plane| (plane.as_str().to_owned(), availability.of(plane)))
            .collect()
    }

    /// Put the scheduler into the mode implied by the reconciliation result.
    ///
    /// A quarantined transaction makes the effect lane refuse admission, so the
    /// write-blocking property is enforced by the scheduler *and* independently by
    /// the permit authority and the gateway.
    pub fn apply_reconciliation_mode(&self) {
        if self.reconciliation.quarantined.is_empty() && self.legacy_block.is_none() {
            self.scheduler.set_mode(SchedulerMode::Accepting);
        } else {
            self.scheduler.set_mode(SchedulerMode::Quarantined);
        }
    }

    /// Per-conclusion reconciliation counts produced at startup.
    #[must_use]
    pub fn reconciliation_counts(&self) -> Vec<(String, usize)> {
        self.reconciliation
            .counts
            .iter()
            .map(|(conclusion, count)| ((*conclusion).to_owned(), *count))
            .collect()
    }

    /// Transactions startup reconciliation classified as write-blocking.
    #[must_use]
    pub fn quarantined_transactions(&self) -> Vec<String> {
        self.reconciliation.quarantined.clone()
    }

    /// The shared kernel every transport invokes.
    #[must_use]
    pub fn kernel(&self) -> Arc<ServiceKernel> {
        Arc::clone(&self.kernel)
    }

    /// The transport-established access context.
    #[must_use]
    pub fn access(&self) -> AccessContext {
        self.access.clone()
    }

    /// The per-project scheduler.
    #[must_use]
    pub fn scheduler(&self) -> Arc<Scheduler> {
        Arc::clone(&self.scheduler)
    }

    /// Whether ordinary writes are currently permitted.
    ///
    /// Both independent gates are consulted: the permit authority (which refuses
    /// to mint any ordinary permit while a transaction is quarantined) and the
    /// gateway's own live quarantine query. Neither constructs a permit.
    ///
    /// # Errors
    /// Returns the reason writes are blocked.
    pub fn write_gate_status(&self) -> Result<(), String> {
        if let Some(reason) = &self.legacy_block {
            return Err(reason.clone());
        }
        self.authority
            .can_issue()
            .map_err(|error| error.to_string())?;
        match self.gateway.quarantined_transactions() {
            Ok(quarantined) if quarantined.is_empty() => Ok(()),
            Ok(quarantined) => Err(format!(
                "project has quarantined transactions ({quarantined:?}); writes are blocked"
            )),
            Err(error) => Err(error.to_string()),
        }
    }

    /// Which effect boundaries have a real implementation compiled in, so health
    /// reporting never claims a fail-closed boundary is ready.
    #[must_use]
    pub fn boundary_availability(&self) -> Vec<(String, bool)> {
        self.gateway
            .boundary_availability()
            .into_iter()
            .map(|(boundary, available)| (format!("{boundary:?}").to_lowercase(), available))
            .collect()
    }

    /// The held project ownership key. Its presence in the runtime is what keeps
    /// the advisory OS lock (and therefore single-writer ownership) alive.
    #[must_use]
    pub fn project_generation(&self) -> u64 {
        let _held = self.context.lease().lease_key();
        INITIAL_PROJECT_GENERATION
    }

    /// Absolute display spelling of the private state root every component of
    /// this runtime was built from.
    #[must_use]
    pub fn state_root(&self) -> &Path {
        self.context.state_root()
    }

    /// The canonical project context this runtime owns.
    #[must_use]
    pub const fn context(&self) -> &ProjectContext {
        &self.context
    }
}

fn context_error(error: ProjectContextError) -> StdioError {
    match error {
        ProjectContextError::Lease(error) => StdioError::Lease(error),
        ProjectContextError::Inventory(reason) => StdioError::Store(reason),
    }
}

/// Run startup reconciliation over one journal: launches first, then linked
/// process transactions, then every remaining open transaction.
fn reconcile_journal(
    store: &Arc<TransactionStore>,
    state_dir: &Path,
    held: &HeldProject,
    lease: &Lease,
    providers: &crate::engine_plane::providers::LaunchProviders,
) -> Result<Vec<crate::journal::reconcile::Reconciled>, StdioError> {
    reconcile_project_launches(Arc::clone(store), state_dir, providers)
        .map_err(|error| StdioError::Store(error.to_string()))?;
    let mut settled = reconcile_linked_process_transactions(store.as_ref(), held)
        .map_err(|error| StdioError::Store(error.to_string()))?;
    settled.extend(
        reconcile_open_transactions(
            store.as_ref(),
            held,
            &SafeRootProofSource::new(lease.safe_root()),
        )
        .map_err(|error| StdioError::Store(error.to_string()))?,
    );
    Ok(settled)
}

/// Reconcile a legacy state root's journal in place and re-read its summary.
///
/// The legacy lock is already held by the project context, so this process is
/// that journal's only writer. Its records carry the same project object
/// identity as the canonical root, so the ordinary proof rules apply unchanged.
fn reconcile_state_root(
    state_dir: &Path,
    held: &HeldProject,
    lease: &Lease,
    providers: &crate::engine_plane::providers::LaunchProviders,
) -> Result<crate::journal::LegacyStoreSummary, StdioError> {
    let journal = state_dir.join("journal");
    let store = Arc::new(
        TransactionStore::open(&journal).map_err(|error| StdioError::Store(error.to_string()))?,
    );
    reconcile_journal(&store, state_dir, held, lease, providers)?;
    drop(store);
    inspect_legacy_store(&journal).map_err(|error| StdioError::Store(error.to_string()))
}

/// A write block carried by the survivor gate while a legacy state root holds
/// evidence of a possibly unresolved effect. Routing it through the survivor
/// gate makes the scheduler and the policy authority refuse mutations with the
/// same reason the write-gate status reports.
struct LegacyStateBlock {
    reason: String,
}

impl SurvivorProbe for LegacyStateBlock {
    fn unresolved_survivor(&self) -> Result<Option<String>, String> {
        Ok(Some(self.reason.clone()))
    }
}

/// The private state directory spelled from `project` without discovery.
///
/// This is the version 0.1 candidate-derived spelling. It is retained only for
/// test harnesses that already pass a discovered root (for which it equals the
/// canonical state root) and for reproducing legacy state roots; production
/// state roots come from [`ProjectContext::acquire`].
#[must_use]
pub fn state_dir_for(project: &Path) -> PathBuf {
    project.join(STATE_DIR_NAME)
}

/// Install protocol-safe diagnostics for a stdio process.
///
/// Process-level fallback diagnostics can still write to stderr when tracing uses
/// a rolling file, so every stdio launch requires distinct stdout and stderr
/// objects before tracing or protocol framing starts.
pub fn install_stdio_diagnostics(config: &ServerConfig) -> Result<(), StdioError> {
    validate_default_stderr().map_err(|error| StdioError::Diagnostics(error.to_string()))?;
    let tracing_config = TracingConfig::from_log_sink(&config.log_sink);
    init_tracing(&tracing_config).map_err(|error| StdioError::Diagnostics(error.to_string()))
}

/// Serve MCP over stdin/stdout until the peer disconnects or shutdown is
/// requested.
///
/// Cancellation and shutdown both resolve through the rmcp service's own
/// quitting path, so an in-flight request observes cancellation rather than being
/// dropped mid-effect.
pub async fn serve_stdio(runtime: &ProjectRuntime) -> Result<(), StdioError> {
    runtime.apply_reconciliation_mode();
    let adapter = RmcpAdapter::new(runtime.kernel(), runtime.access());
    let service = adapter
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|error| StdioError::Transport(error.to_string()))?;
    tracing::info!(
        event = "stdio_serving",
        reconciled = runtime.reconciliation_counts().len(),
        quarantined = runtime.quarantined_transactions().len(),
        "stdio transport is serving on the shared kernel"
    );
    service
        .waiting()
        .await
        .map(|_reason| ())
        .map_err(|error| StdioError::Transport(error.to_string()))
}
