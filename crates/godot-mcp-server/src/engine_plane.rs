//! Production registration for the public engine-plane tools.
//!
//! The server authority owns effect permits. These handlers therefore never
//! mint an engine `ToolPermit` or treat one as a server policy permit. Until a
//! request can carry an already-journaled transaction through the sealed
//! authority/gateway path, it is rejected before any side effect is attempted.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use crate::effect::boundary::{
    BoundaryUnavailable, EffectTicket, LspDapEffectBoundary, LspDapExchange, ProcessEffectBoundary,
    ProcessEffectOutcome, ProcessLaunchRequest, ProcessOutputExcerpt, ProcessOutputMetadata,
};
use crate::journal::{BoundaryMembership, ReleaseStatus, TransactionStore, TypedProof};
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_engine::supervision::gate;
use godot_mcp_engine::supervision::journal::{
    BoundaryMembership as EngineBoundaryMembership, JournalError, LaunchJournal,
    ProcessLaunchProof as EngineProcessLaunchProof, ReleaseStatus as EngineReleaseStatus,
};
#[cfg(not(feature = "test-hooks"))]
use godot_mcp_engine::supervision::protocol::SilentObserver;
#[cfg(feature = "test-hooks")]
use godot_mcp_engine::supervision::protocol::{CrashStage, LaunchObserver};
use godot_mcp_engine::supervision::protocol::{
    FaultInjection, LaunchDiagnostics, LaunchProtocol, LaunchRequest, bundled_supervisor,
};
use godot_mcp_engine::supervision::reconcile::{
    ReconciledLaunch, reconcile_launches, terminal_scratch_reclaimable,
};
use godot_mcp_engine::validation::diagnostic::parse_engine_diagnostics;
use godot_mcp_engine::validation::outcome::Observed;

mod authored;
mod canonical;
mod handler;
mod journal;
mod process;
pub(crate) mod providers;
mod public;
mod request;
mod runtime;
pub(crate) mod session;

pub(crate) use handler::register_engine_tools;
pub(crate) use request::input_schema as engine_input_schema;
pub(crate) use runtime::{EngineRuntime, EngineRuntimeConfig};
pub(crate) use session::tools::{input_schema as session_input_schema, register_session_tools};

fn outcome_error(kind: ErrorKind, message: impl Into<String>) -> OutcomeError {
    OutcomeError::new(kind, message, false, BTreeMap::new())
}

trait ProcessLauncher: Send + Sync {
    fn available(&self) -> bool;
    /// Why a launch cannot start, when it cannot.
    fn unavailable_reason(&self) -> Option<String> {
        None
    }
    fn launch(&self, request: &LaunchRequest) -> Result<ProcessEffectOutcome, ()>;
}

struct ProtocolProcessLauncher {
    journal: Arc<journal::EngineJournal>,
    providers: Arc<providers::LaunchProviders>,
}

impl ProcessLauncher for ProtocolProcessLauncher {
    fn available(&self) -> bool {
        self.providers.usable()
    }

    fn unavailable_reason(&self) -> Option<String> {
        self.providers.status().err()
    }

    fn launch(&self, request: &LaunchRequest) -> Result<ProcessEffectOutcome, ()> {
        #[cfg(feature = "test-hooks")]
        let observer = ProcessCrashObserver::from_environment();
        #[cfg(not(feature = "test-hooks"))]
        let observer = SilentObserver;
        let provider = self.providers.selected().map_err(|_| ())?;
        LaunchProtocol::new(self.journal.as_ref())
            .launch(
                request,
                provider.as_ref(),
                &FaultInjection::none(),
                &observer,
            )
            .inspect_err(|error| {
                tracing::warn!(
                    event = "process_launch_failed",
                    boundary = provider.kind().as_str(),
                    reason = %error,
                    "a supervised launch did not start"
                );
            })
            .map(|outcome| {
                let observed = Observed::from_launch(&outcome.diagnostics);
                let diagnostics = parse_process_diagnostics(&outcome.diagnostics);
                let output = ProcessOutputMetadata::from_diagnostics(&outcome.diagnostics);
                let excerpt = ProcessOutputExcerpt::from_diagnostics(&outcome.diagnostics);
                ProcessEffectOutcome {
                    proof: process_proof(outcome.proof),
                    termination: outcome.diagnostics.termination,
                    observed,
                    diagnostics,
                    output,
                    excerpt,
                    boundary_kind: outcome.boundary.kind.as_str(),
                    boundary_strength: outcome.strength.as_str(),
                }
            })
            .map_err(|_| ())
    }
}

fn parse_process_diagnostics(
    diagnostics: &LaunchDiagnostics,
) -> Vec<godot_mcp_engine::validation::diagnostic::Diagnostic> {
    let stderr = String::from_utf8_lossy(&diagnostics.stderr.bytes);
    let stdout = String::from_utf8_lossy(&diagnostics.stdout.bytes);
    let mut combined = String::with_capacity(stderr.len() + stdout.len() + 1);
    combined.push_str(&stderr);
    if !combined.is_empty() && !combined.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str(&stdout);
    parse_engine_diagnostics(&combined)
}

#[cfg(feature = "test-hooks")]
struct ProcessCrashObserver {
    stage: Option<CrashStage>,
}

#[cfg(feature = "test-hooks")]
impl ProcessCrashObserver {
    fn from_environment() -> Self {
        let configured = std::env::var("GODOT_MCP_TEST_LAUNCH_CRASH_STAGE").ok();
        Self {
            stage: CrashStage::ALL
                .into_iter()
                .find(|stage| configured.as_deref() == Some(stage.as_str())),
        }
    }
}

#[cfg(feature = "test-hooks")]
impl LaunchObserver for ProcessCrashObserver {
    fn stage(
        &self,
        stage: CrashStage,
        _view: &godot_mcp_engine::supervision::protocol::LaunchView<'_>,
    ) {
        if self.stage == Some(stage) {
            std::process::exit(86);
        }
    }
}

pub(crate) struct ProductionProcessBoundary {
    launcher: Box<dyn ProcessLauncher>,
    sessions: Option<Arc<session::SessionManager>>,
}

impl ProductionProcessBoundary {
    fn new(
        journal: Arc<journal::EngineJournal>,
        providers: Arc<providers::LaunchProviders>,
    ) -> Self {
        Self {
            launcher: Box::new(ProtocolProcessLauncher { journal, providers }),
            sessions: None,
        }
    }

    /// Start managed sessions through `sessions`.
    pub(crate) fn with_sessions(mut self, sessions: Arc<session::SessionManager>) -> Self {
        self.sessions = Some(sessions);
        self
    }

    pub(crate) fn for_project(
        store: Arc<TransactionStore>,
        providers: Arc<providers::LaunchProviders>,
    ) -> Self {
        let journal = Arc::new(journal::EngineJournal::new(store));
        Self::new(journal, providers)
    }

    #[cfg(test)]
    fn with_launcher(launcher: Box<dyn ProcessLauncher>) -> Self {
        Self {
            launcher,
            sessions: None,
        }
    }
}

pub(crate) fn reconcile_project_launches(
    store: Arc<TransactionStore>,
    state_dir: &Path,
    providers: &providers::LaunchProviders,
) -> Result<Vec<ReconciledLaunch>, JournalError> {
    let journal = journal::EngineJournal::new(store);
    // Every provider a record may name, so a configuration change since the
    // record was made never orphans its process.
    let providers = providers.known();
    let reconciled = reconcile_launches(&journal, &providers)?;
    let mut retained = journal
        .open_launches()?
        .into_iter()
        .map(|record| record.envelope.transaction_id)
        .collect::<BTreeSet<_>>();
    retained.extend(journal.quarantined()?);
    for record in journal.committed_launches()? {
        if !terminal_scratch_reclaimable(&record, &providers) {
            retained.insert(record.envelope.transaction_id);
        }
    }
    gate::sweep_scratch(
        &state_dir.join("engine-launches"),
        retained.iter().map(String::as_str),
    )
    .map_err(|error| JournalError::Payload(error.to_string()))?;
    Ok(reconciled)
}

pub(crate) fn process_supervisor_binary() -> std::path::PathBuf {
    #[cfg(feature = "test-hooks")]
    if let Ok(executable) = std::env::current_exe()
        && executable
            .file_stem()
            .is_some_and(|name| name == "qa-journal-foundation")
    {
        return executable;
    }
    bundled_supervisor()
}

impl ProcessEffectBoundary for ProductionProcessBoundary {
    fn available(&self) -> bool {
        self.launcher.available()
    }

    fn unavailable_reason(&self) -> Option<String> {
        self.launcher.unavailable_reason()
    }

    fn launch(
        &self,
        ticket: &EffectTicket,
        request: &ProcessLaunchRequest,
    ) -> Result<ProcessEffectOutcome, BoundaryUnavailable> {
        if ticket.boundary() != crate::journal::EffectBoundaryKind::Process
            || ticket.transaction_id().is_empty()
        {
            return Err(BoundaryUnavailable {
                boundary: "process",
            });
        }
        self.launcher
            .launch(&request.launch)
            .map_err(|()| BoundaryUnavailable {
                boundary: "process",
            })
    }

    fn start_session(
        &self,
        ticket: &EffectTicket,
        request: &crate::effect::boundary::SessionStartRequest,
    ) -> Result<crate::effect::boundary::SessionStartOutcome, OutcomeError> {
        if ticket.boundary() != crate::journal::EffectBoundaryKind::Process
            || ticket.transaction_id().is_empty()
        {
            return Err(outcome_error(
                ErrorKind::PermissionDenied,
                "the process ticket does not authorize this session start",
            ));
        }
        match &self.sessions {
            Some(sessions) => sessions.start(request, ticket.transaction_id()),
            None => Err(outcome_error(
                ErrorKind::Unavailable,
                "managed Godot sessions are not available in this assembly",
            )),
        }
    }

    fn stop_session(
        &self,
        ticket: &EffectTicket,
        command: &crate::effect::boundary::EditorMutation,
    ) -> Result<crate::effect::boundary::EditorEffectOutcome, BoundaryUnavailable> {
        use godot_mcp_bridge::protocol::mutation::MutationStatus;

        if ticket.boundary() != crate::journal::EffectBoundaryKind::Process
            || ticket.transaction_id().is_empty()
        {
            return Err(BoundaryUnavailable {
                boundary: "process",
            });
        }
        let Some(sessions) = &self.sessions else {
            return Err(BoundaryUnavailable {
                boundary: "process",
            });
        };
        let unbound = || TypedProof::EditorRuntime {
            bridge_reachable: false,
            receipt_transaction_id: None,
            receipt_action_id: None,
            live_state_probe: None,
        };
        let session = serde_json::from_str::<serde_json::Value>(&command.payload)
            .ok()
            .and_then(|payload| {
                payload
                    .get("session")
                    .and_then(|id| id.as_str().map(str::to_owned))
            });
        let Some(session) = session else {
            return Ok(crate::effect::boundary::EditorEffectOutcome {
                status: MutationStatus::NotStarted,
                payload: serde_json::json!({}),
                error: Some((
                    ErrorKind::InvalidInput,
                    "a stop names its session".to_owned(),
                )),
                proof: unbound(),
            });
        };
        Ok(match sessions.stop(&session, &command.idempotency_id) {
            Ok(value) => crate::effect::boundary::EditorEffectOutcome {
                status: MutationStatus::Committed,
                proof: crate::editor::boundary::receipt(
                    ticket,
                    &command.action,
                    &serde_json::json!({
                        "digest": value.get("state").and_then(serde_json::Value::as_str).unwrap_or("stopped")
                    }),
                ),
                payload: value,
                error: None,
            },
            Err(error) => crate::effect::boundary::EditorEffectOutcome {
                status: if error.kind() == ErrorKind::Uncertain {
                    MutationStatus::Unresolved
                } else {
                    MutationStatus::NotStarted
                },
                payload: serde_json::json!({}),
                error: Some((error.kind(), error.message().to_owned())),
                proof: unbound(),
            },
        })
    }
}

fn process_proof(proof: EngineProcessLaunchProof) -> TypedProof {
    TypedProof::ProcessLaunch {
        boundary_identity: proof.boundary_identity,
        pid: proof.pid,
        start_time: proof.start_time,
        boundary_membership: match proof.boundary_membership {
            EngineBoundaryMembership::Confirmed => BoundaryMembership::Confirmed,
            EngineBoundaryMembership::Unconfirmed => BoundaryMembership::Unconfirmed,
        },
        release_status: match proof.release_status {
            EngineReleaseStatus::Suspended => ReleaseStatus::Suspended,
            EngineReleaseStatus::ResumedContained => ReleaseStatus::ResumedContained,
            EngineReleaseStatus::ExitedClean => ReleaseStatus::ExitedClean,
            EngineReleaseStatus::Signaled => ReleaseStatus::Signaled,
            EngineReleaseStatus::NonzeroExit => ReleaseStatus::NonzeroExit,
            EngineReleaseStatus::Unknown => ReleaseStatus::Unknown,
        },
        exit_code: proof.exit_code,
        signal: proof.signal,
        boundary_empty_after_exit: proof.boundary_empty_after_exit,
    }
}

pub(crate) struct ProductionLspDapBoundary;

impl LspDapEffectBoundary for ProductionLspDapBoundary {
    fn available(&self) -> bool {
        false
    }

    fn exchange(
        &self,
        _ticket: &EffectTicket,
        _exchange: &LspDapExchange,
    ) -> Result<String, BoundaryUnavailable> {
        Err(BoundaryUnavailable {
            boundary: "lsp_dap",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AvailableLauncher;

    impl ProcessLauncher for AvailableLauncher {
        fn available(&self) -> bool {
            true
        }

        fn launch(&self, _request: &LaunchRequest) -> Result<ProcessEffectOutcome, ()> {
            unreachable!("this test only observes boundary availability")
        }
    }

    #[test]
    fn production_process_boundary_reports_the_real_launcher_availability() {
        let boundary = ProductionProcessBoundary::with_launcher(Box::new(AvailableLauncher));

        let available = boundary.available();

        assert!(available);
    }

    #[test]
    fn clean_engine_process_proof_remains_clean_at_the_gateway_boundary() {
        let proof = EngineProcessLaunchProof {
            boundary_identity: "boundary-1".to_owned(),
            pid: Some(42),
            start_time: Some("100".to_owned()),
            boundary_membership: EngineBoundaryMembership::Confirmed,
            release_status: EngineReleaseStatus::ExitedClean,
            exit_code: Some(0),
            signal: None,
            boundary_empty_after_exit: Some(true),
        };

        let mapped = process_proof(proof);

        assert!(mapped.is_clean_terminal_process());
    }

    #[test]
    fn lsp_dap_boundary_reports_unavailable_until_a_real_endpoint_is_connected() {
        let boundary = ProductionLspDapBoundary;

        let available = boundary.available();

        assert!(!available);
    }
}
