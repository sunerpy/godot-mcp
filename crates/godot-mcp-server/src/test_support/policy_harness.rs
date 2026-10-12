//! The `test-hooks`-gated capability-policy harness.
//!
//! This harness is the **only** way a test can reach the crate-private policy
//! authority, its crate-private permit wrapper and the recording effect mocks. It
//! deliberately exposes *behaviour and observations*, never a permit value: no
//! method returns [`PolicyPermit`](crate::policy::PolicyPermit), so a test cannot
//! smuggle one anywhere.
//!
//! The five effect mocks count every invocation, so a test can assert that a
//! refused, quarantined or survivor-blocked dispatch reached **zero** boundaries
//! rather than merely failing afterwards.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::contracts::{PrincipalId, ProjectIdentity, SessionId};
use godot_mcp_domain::lease::{Lease, LeaseManager};
use godot_mcp_domain::safe_root::SafeRootError;
use serde_json::Value;

use super::HarnessError;
use crate::effect::boundary::{
    BoundaryUnavailable, EditorEffectBoundary, EditorMutation, EffectTicket, FileEffectBoundary,
    FileWrite, LspDapEffectBoundary, LspDapExchange, ProcessEffectBoundary, ProcessEffectOutcome,
    ProcessLaunchRequest, RuntimeEffectBoundary, RuntimeOperation,
};
use crate::effect::{
    EffectBoundaries, EffectGateway, FileEffectIntent, PlannedWrite, SafeRootFileBoundary,
};
use crate::journal::{
    EffectBoundaryKind, FileOperation, HeldProject, OperationFamily, Phase, SafeRootProofSource,
    TransactionStore, TransportMode, TypedProof, encode_object_identity,
};
use crate::policy::{
    BoundIdentity, BoundaryAvailability, DispatchRequest, PolicyAuthority, PolicyDispatchError,
    PolicyRefusal, SurvivorProbe, all_rows,
};
use crate::stdio::INITIAL_PROJECT_GENERATION;

/// One canonical capability-matrix row, as a test reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyMatrixRow {
    /// Canonical tool name.
    pub tool: String,
    /// Required capability, in its stable snake_case name.
    pub capability: String,
    /// Public outcome plane, in its stable snake_case name.
    pub plane: String,
    /// Effect boundary the tool reaches, if any.
    pub boundary: Option<String>,
    /// Whether the tool produces a side effect and therefore needs a permit.
    pub requires_permit: bool,
}

/// The complete canonical matrix, independent of any held project.
#[must_use]
pub fn policy_matrix() -> Vec<PolicyMatrixRow> {
    all_rows()
        .into_iter()
        .map(|row| PolicyMatrixRow {
            tool: row.tool.to_owned(),
            capability: crate::policy::decision::capability_name(row.capability).to_owned(),
            plane: plane_name(row.plane).to_owned(),
            boundary: row
                .boundary
                .map(|boundary| crate::policy::authority::boundary_name(boundary).to_owned()),
            requires_permit: row.requires_permit(),
        })
        .collect()
}

const fn plane_name(plane: godot_mcp_domain::contracts::OperationPlane) -> &'static str {
    use godot_mcp_domain::contracts::OperationPlane;
    match plane {
        OperationPlane::Static => "static",
        OperationPlane::Project => "project",
        OperationPlane::Editor => "editor",
        OperationPlane::Runtime => "runtime",
    }
}

/// The outcome of one policy evaluation, as a test observes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyOutcome {
    /// Whether the dispatch was authorized.
    pub allowed: bool,
    /// Stable domain error code for a refusal.
    pub code: Option<&'static str>,
    /// Caller-safe message.
    pub message: String,
    /// Non-sensitive decision ID, present whenever a permit was minted.
    pub decision_id: Option<String>,
    /// Whether a production permit was minted.
    pub permit_issued: bool,
    /// The published public decision, serialized.
    pub decision: Value,
}

/// A completed dispatch through the policy layer to a real boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDispatchReport {
    /// Journaled transaction the effect settled.
    pub transaction_id: String,
    /// Terminal phase, in its stable snake_case name.
    pub phase: String,
    /// Non-sensitive decision ID recorded in the audit log.
    pub decision_id: String,
}

/// How many times each effect mock was invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BoundaryTouches {
    pub file: usize,
    pub process: usize,
    pub lsp_dap: usize,
    pub editor: usize,
    pub runtime: usize,
}

impl BoundaryTouches {
    /// Total invocations across every boundary.
    #[must_use]
    pub const fn total(self) -> usize {
        self.file + self.process + self.lsp_dap + self.editor + self.runtime
    }
}

#[derive(Debug, Default)]
struct TouchCounters {
    file: AtomicUsize,
    process: AtomicUsize,
    lsp_dap: AtomicUsize,
    editor: AtomicUsize,
    runtime: AtomicUsize,
}

impl TouchCounters {
    fn snapshot(&self) -> BoundaryTouches {
        BoundaryTouches {
            file: self.file.load(Ordering::Relaxed),
            process: self.process.load(Ordering::Relaxed),
            lsp_dap: self.lsp_dap.load(Ordering::Relaxed),
            editor: self.editor.load(Ordering::Relaxed),
            runtime: self.runtime.load(Ordering::Relaxed),
        }
    }
}

/// The production `file` mock: it counts, then delegates to the real
/// `SafeRoot`-backed boundary, so a permitted write really lands on disk.
struct RecordingFileBoundary {
    inner: SafeRootFileBoundary,
    counters: Arc<TouchCounters>,
}

impl FileEffectBoundary for RecordingFileBoundary {
    fn available(&self) -> bool {
        self.inner.available()
    }

    fn publish(&self, ticket: &EffectTicket, write: &FileWrite) -> Result<(), SafeRootError> {
        self.counters.file.fetch_add(1, Ordering::Relaxed);
        self.inner.publish(ticket, write)
    }

    fn delete(
        &self,
        ticket: &EffectTicket,
        path: &str,
        expected: &[u8],
    ) -> Result<(), SafeRootError> {
        self.counters.file.fetch_add(1, Ordering::Relaxed);
        self.inner.delete(ticket, path, expected)
    }
}

/// The fail-closed stand-in this harness installs for every boundary other than
/// `file`, so a policy test has exactly one executable downstream. It covers four
/// boundaries here because the harness isolates `file` behaviour; the shipped
/// `serve` assembly fail-closes three of the five — `lsp_dap`, `editor` and
/// `runtime` — and delivers `file` and `process`. Each stand-in still counts its
/// invocations, so "never reached" is an assertion rather than an assumption.
struct RecordingUnavailable {
    counters: Arc<TouchCounters>,
}

impl ProcessEffectBoundary for RecordingUnavailable {
    fn available(&self) -> bool {
        false
    }

    fn launch(
        &self,
        _ticket: &EffectTicket,
        _request: &ProcessLaunchRequest,
    ) -> Result<ProcessEffectOutcome, BoundaryUnavailable> {
        self.counters.process.fetch_add(1, Ordering::Relaxed);
        Err(BoundaryUnavailable {
            boundary: "process",
        })
    }
}

impl LspDapEffectBoundary for RecordingUnavailable {
    fn available(&self) -> bool {
        false
    }

    fn exchange(
        &self,
        _ticket: &EffectTicket,
        _exchange: &LspDapExchange,
    ) -> Result<String, BoundaryUnavailable> {
        self.counters.lsp_dap.fetch_add(1, Ordering::Relaxed);
        Err(BoundaryUnavailable {
            boundary: "lsp_dap",
        })
    }
}

impl EditorEffectBoundary for RecordingUnavailable {
    fn available(&self) -> bool {
        false
    }

    fn mutate(
        &self,
        _ticket: &EffectTicket,
        _mutation: &EditorMutation,
    ) -> Result<TypedProof, BoundaryUnavailable> {
        self.counters.editor.fetch_add(1, Ordering::Relaxed);
        Err(BoundaryUnavailable { boundary: "editor" })
    }
}

impl RuntimeEffectBoundary for RecordingUnavailable {
    fn available(&self) -> bool {
        false
    }

    fn invoke(
        &self,
        _ticket: &EffectTicket,
        _operation: &RuntimeOperation,
    ) -> Result<TypedProof, BoundaryUnavailable> {
        self.counters.runtime.fetch_add(1, Ordering::Relaxed);
        Err(BoundaryUnavailable {
            boundary: "runtime",
        })
    }
}

/// A survivor probe a test can drive, standing in for Todo 17's launch journal.
#[derive(Debug, Default)]
struct MutableSurvivors {
    blocked: Mutex<Option<String>>,
    unreadable: Mutex<bool>,
}

struct SharedSurvivors(Arc<MutableSurvivors>);

impl SurvivorProbe for SharedSurvivors {
    fn unresolved_survivor(&self) -> Result<Option<String>, String> {
        if *self
            .0
            .unreadable
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            return Err("launch survivor state cannot be read".to_owned());
        }
        Ok(self
            .0
            .blocked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone())
    }
}

/// The Todo 26 acceptance harness over one held project and one bound identity.
pub struct PolicyHarness {
    lease: Lease,
    store: Arc<TransactionStore>,
    gateway: EffectGateway,
    authority: PolicyAuthority,
    project: ProjectIdentity,
    principal: PrincipalId,
    session: Option<SessionId>,
    availability: BoundaryAvailability,
    counters: Arc<TouchCounters>,
    survivors: Arc<MutableSurvivors>,
    epoch: u64,
}

/// The epoch every harness authority is bound to.
const EPOCH: u64 = 1;

/// Principal the harness binds. It is an opaque Todo 3 identifier, never a
/// profile name and never a host location.
const BOUND_PRINCIPAL: &str = "principal_0123456789abcdef";
/// A different, equally valid opaque principal, used to prove identity is checked.
const OTHER_PRINCIPAL: &str = "principal_fedcba9876543210";

impl PolicyHarness {
    /// Acquire the project, open the journal, assemble the gateway and bind one
    /// policy authority to one identity.
    pub fn open(project_root: &Path, profile: CapabilityProfile) -> Result<Self, HarnessError> {
        let state_dir = crate::stdio::state_dir_for(project_root);
        let manager = LeaseManager::new(&state_dir);
        let lease = manager
            .acquire(project_root)
            .map_err(|error| HarnessError::Lease(error.to_string()))?;
        let key = lease.lease_key();
        let project = ProjectIdentity::from_lease_key(key, INITIAL_PROJECT_GENERATION);
        let held = HeldProject {
            object_identity: project.object_id().to_string(),
            root_object: encode_object_identity(key.root),
            file_object: encode_object_identity(key.project_file),
            generation: INITIAL_PROJECT_GENERATION,
        };
        let store = Arc::new(
            TransactionStore::open(&state_dir.join("journal"))
                .map_err(|error| HarnessError::Store(error.to_string()))?,
        );
        let counters = Arc::new(TouchCounters::default());
        let gateway = EffectGateway::new(
            Arc::clone(&store),
            held.clone(),
            EPOCH,
            Box::new(SafeRootProofSource::new(lease.safe_root())),
            EffectBoundaries {
                file: Box::new(RecordingFileBoundary {
                    inner: SafeRootFileBoundary::new(lease.safe_root()),
                    counters: Arc::clone(&counters),
                }),
                process: Box::new(RecordingUnavailable {
                    counters: Arc::clone(&counters),
                }),
                lsp_dap: Box::new(RecordingUnavailable {
                    counters: Arc::clone(&counters),
                }),
                editor: Box::new(RecordingUnavailable {
                    counters: Arc::clone(&counters),
                }),
                runtime: Box::new(RecordingUnavailable {
                    counters: Arc::clone(&counters),
                }),
            },
        );
        let availability = BoundaryAvailability::from_gateway(&gateway);
        let principal = PrincipalId::parse(BOUND_PRINCIPAL)
            .map_err(|error| HarnessError::Permit(error.to_string()))?;
        let survivors = Arc::new(MutableSurvivors::default());
        let authority = PolicyAuthority::new(
            profile,
            BoundIdentity {
                project: project.clone(),
                principal: Some(principal.clone()),
                session: None,
            },
            Arc::clone(&store),
            availability,
            Box::new(SharedSurvivors(Arc::clone(&survivors))),
            EPOCH,
        );
        Ok(Self {
            lease,
            store,
            gateway,
            authority,
            project,
            principal,
            session: None,
            availability,
            counters,
            survivors,
            epoch: EPOCH,
        })
    }

    /// How many times each effect mock has been invoked.
    #[must_use]
    pub fn touches(&self) -> BoundaryTouches {
        self.counters.snapshot()
    }

    /// Which boundaries the gateway reports as real, as policy sees them.
    #[must_use]
    pub fn boundary_availability(&self) -> Vec<(String, bool)> {
        [
            EffectBoundaryKind::File,
            EffectBoundaryKind::Process,
            EffectBoundaryKind::LspDap,
            EffectBoundaryKind::Editor,
            EffectBoundaryKind::Runtime,
        ]
        .into_iter()
        .map(|boundary| {
            (
                crate::policy::authority::boundary_name(boundary).to_owned(),
                self.availability.of(boundary),
            )
        })
        .collect()
    }

    /// Declare (or clear) an unresolved launch survivor.
    pub fn set_unresolved_survivor(&self, reason: Option<&str>) {
        *self
            .survivors
            .blocked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = reason.map(str::to_owned);
    }

    /// Make the survivor state unreadable, proving policy refuses rather than
    /// assuming "clear".
    pub fn set_survivor_state_unreadable(&self, unreadable: bool) {
        *self
            .survivors
            .unreadable
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = unreadable;
    }

    /// Evaluate one dispatch with the bound identity and the held project, naming
    /// no transaction. A mutating row therefore has no permit to be issued.
    pub fn evaluate(&self, tool: &str) -> PolicyOutcome {
        self.evaluate_request(&self.request(tool, None))
    }

    /// Evaluate one frozen version 0.1 planned operation shape with the bound
    /// identity. The shape is never a catalog tool; evaluating it keeps the
    /// boundary-availability gate exercised for planes later milestones ship.
    ///
    /// # Panics
    /// Panics when `tool` was not a version 0.1 planned operation.
    pub fn evaluate_historical(&self, tool: &str) -> PolicyOutcome {
        let descriptor = godot_mcp_domain::catalog::historical_v0_1_descriptor(tool)
            .unwrap_or_else(|| panic!("{tool} is not a version 0.1 planned operation"));
        let request = self.request(tool, None);
        match self.authority.authorize_descriptor(&request, descriptor) {
            Ok(grant) => PolicyOutcome {
                allowed: true,
                code: None,
                message: "granted".to_owned(),
                decision_id: grant
                    .permit
                    .as_ref()
                    .map(|permit| permit.decision_id().to_owned()),
                permit_issued: grant.permit.is_some(),
                decision: serde_json::to_value(&grant.decision).unwrap_or(Value::Null),
            },
            Err(refusal) => self.refusal_outcome(tool, &refusal),
        }
    }

    /// Evaluate one dispatch presenting a *different* principal.
    pub fn evaluate_as_other_principal(&self, tool: &str) -> PolicyOutcome {
        let mut request = self.request(tool, None);
        request.principal = PrincipalId::parse(OTHER_PRINCIPAL).ok();
        self.evaluate_request(&request)
    }

    /// Evaluate one dispatch presenting a *later* project generation.
    pub fn evaluate_with_next_generation(&self, tool: &str) -> PolicyOutcome {
        let mut request = self.request(tool, None);
        request.project =
            ProjectIdentity::from_lease_key(self.lease.lease_key(), self.project.generation() + 1);
        self.evaluate_request(&request)
    }

    /// Journal, authorize and dispatch one `file` write end to end.
    ///
    /// # Errors
    /// Returns the policy outcome when the dispatch was refused at any gate.
    pub fn commit_file_write(
        &self,
        tool: &str,
        transaction_id: &str,
        path: &str,
        bytes: &[u8],
    ) -> Result<PolicyDispatchReport, PolicyOutcome> {
        self.journal_file_transaction(tool, transaction_id, path, bytes)
            .map_err(|error| self.harness_outcome(tool, &error))?;
        let grant = self
            .authority
            .authorize(&self.request(tool, Some(transaction_id)))
            .map_err(|refusal| self.refusal_outcome(tool, &refusal))?;
        let permit = grant.permit.ok_or_else(|| PolicyOutcome {
            allowed: false,
            code: Some("permission_denied"),
            message: "a mutating dispatch produced no permit".to_owned(),
            decision_id: None,
            permit_issued: false,
            decision: Value::Null,
        })?;
        let decision_id = permit.decision_id().to_owned();
        let outcome = self
            .authority
            .dispatch_file_effect(&self.gateway, &permit)
            .map_err(|error| self.dispatch_outcome(tool, &error))?;
        Ok(PolicyDispatchReport {
            transaction_id: outcome.transaction_id,
            phase: outcome.phase.as_str().to_owned(),
            decision_id,
        })
    }

    /// Dispatch one already-journaled `file` write under a permit minted by a
    /// *superseded* authority epoch.
    pub fn dispatch_with_stale_permit(
        &self,
        tool: &str,
        transaction_id: &str,
        path: &str,
        bytes: &[u8],
    ) -> PolicyOutcome {
        self.dispatch_with_alternate_authority(
            tool,
            transaction_id,
            path,
            bytes,
            self.epoch - 1,
            self.principal.clone(),
        )
    }

    /// Dispatch one already-journaled `file` write under a permit whose decision
    /// was made for a *different* identity, i.e. a permit this authority never
    /// authorized.
    pub fn dispatch_with_foreign_decision(
        &self,
        tool: &str,
        transaction_id: &str,
        path: &str,
        bytes: &[u8],
    ) -> PolicyOutcome {
        let Some(other) = PrincipalId::parse(OTHER_PRINCIPAL).ok() else {
            return PolicyOutcome {
                allowed: false,
                code: Some("internal"),
                message: "the alternate principal is not a valid identifier".to_owned(),
                decision_id: None,
                permit_issued: false,
                decision: Value::Null,
            };
        };
        self.dispatch_with_alternate_authority(tool, transaction_id, path, bytes, self.epoch, other)
    }

    /// Journal one transaction and drive it to `quarantined`, so every later
    /// dispatch must be refused.
    pub fn quarantine(&self, transaction_id: &str, path: &str) -> Result<(), HarnessError> {
        let intent = self.intent("resource.update", transaction_id, path, b"quarantine seed");
        self.gateway
            .open_file_transaction(&intent)
            .map_err(|error| HarnessError::Store(error.to_string()))?;
        self.store
            .advance(
                transaction_id,
                Phase::Quarantined,
                None,
                Some("harness-forced quarantine".to_owned()),
                Some(crate::journal::TerminalOutcome::failed(
                    godot_mcp_domain::ErrorKind::Uncertain,
                )),
            )
            .map(|_record| ())
            .map_err(|error| HarnessError::Store(error.to_string()))
    }

    /// Audit entries recorded for one transaction, oldest first.
    pub fn audit_entries(
        &self,
        transaction_id: &str,
    ) -> Result<Vec<(String, String)>, HarnessError> {
        self.store
            .audit_entries(transaction_id)
            .map_err(|error| HarnessError::Store(error.to_string()))
    }

    /// Durable phase of one transaction, in its stable snake_case name.
    pub fn phase_of(&self, transaction_id: &str) -> Result<String, HarnessError> {
        self.store
            .load(transaction_id)
            .map(|record| record.phase.as_str().to_owned())
            .map_err(|error| HarnessError::Store(error.to_string()))
    }

    fn dispatch_with_alternate_authority(
        &self,
        tool: &str,
        transaction_id: &str,
        path: &str,
        bytes: &[u8],
        epoch: u64,
        principal: PrincipalId,
    ) -> PolicyOutcome {
        if let Err(error) = self.journal_file_transaction(tool, transaction_id, path, bytes) {
            return self.harness_outcome(tool, &error);
        }
        let alternate = PolicyAuthority::new(
            self.authority.profile(),
            BoundIdentity {
                project: self.project.clone(),
                principal: Some(principal.clone()),
                session: self.session.clone(),
            },
            Arc::clone(&self.store),
            self.availability,
            Box::new(SharedSurvivors(Arc::clone(&self.survivors))),
            epoch,
        );
        let request = DispatchRequest {
            tool: tool.to_owned(),
            project: self.project.clone(),
            principal: Some(principal),
            session: self.session.clone(),
            transaction_id: Some(transaction_id.to_owned()),
        };
        match alternate.authorize(&request) {
            Ok(grant) => match grant.permit {
                Some(permit) => match self.authority.dispatch_file_effect(&self.gateway, &permit) {
                    Ok(outcome) => PolicyOutcome {
                        allowed: true,
                        code: None,
                        message: outcome.reason,
                        decision_id: Some(permit.decision_id().to_owned()),
                        permit_issued: true,
                        decision: Value::Null,
                    },
                    Err(error) => self.dispatch_outcome(tool, &error),
                },
                None => PolicyOutcome {
                    allowed: false,
                    code: Some("permission_denied"),
                    message: "the alternate authority minted no permit".to_owned(),
                    decision_id: None,
                    permit_issued: false,
                    decision: Value::Null,
                },
            },
            Err(refusal) => self.refusal_outcome(tool, &refusal),
        }
    }

    fn journal_file_transaction(
        &self,
        tool: &str,
        transaction_id: &str,
        path: &str,
        bytes: &[u8],
    ) -> Result<(), HarnessError> {
        let intent = self.intent(tool, transaction_id, path, bytes);
        if self.store.load(transaction_id).is_err() {
            self.gateway
                .open_file_transaction(&intent)
                .map_err(|error| HarnessError::Store(error.to_string()))?;
            self.gateway
                .stage_file_transaction(&intent)
                .map_err(|error| HarnessError::Store(error.to_string()))?;
        }
        Ok(())
    }

    fn intent(
        &self,
        tool: &str,
        transaction_id: &str,
        path: &str,
        bytes: &[u8],
    ) -> FileEffectIntent {
        FileEffectIntent {
            transaction_id: transaction_id.to_owned(),
            tool_name: tool.to_owned(),
            operation_family: OperationFamily::ProjectMutation,
            idempotency_id: format!("idem-{transaction_id}"),
            transport_mode: TransportMode::Stdio,
            principal: Some(self.principal.as_str().to_owned()),
            session: None,
            arguments: serde_json::json!({ "path": path }),
            writes: vec![PlannedWrite {
                path: path.to_owned(),
                operation: FileOperation::Create,
                bytes: Some(bytes.to_vec()),
                precondition: crate::effect::WritePrecondition::Observed,
            }],
            required_process_transaction: None,
            opened_at_unix_ms: 1_700_000_000_000,
            expected_revision: None,
        }
    }

    fn request(&self, tool: &str, transaction_id: Option<&str>) -> DispatchRequest {
        DispatchRequest {
            tool: tool.to_owned(),
            project: self.project.clone(),
            principal: Some(self.principal.clone()),
            session: self.session.clone(),
            transaction_id: transaction_id.map(str::to_owned),
        }
    }

    fn evaluate_request(&self, request: &DispatchRequest) -> PolicyOutcome {
        match self.authority.authorize(request) {
            Ok(grant) => PolicyOutcome {
                allowed: true,
                code: None,
                message: "granted".to_owned(),
                decision_id: grant
                    .permit
                    .as_ref()
                    .map(|permit| permit.decision_id().to_owned()),
                permit_issued: grant.permit.is_some(),
                decision: serde_json::to_value(&grant.decision).unwrap_or(Value::Null),
            },
            Err(refusal) => self.refusal_outcome(&request.tool, &refusal),
        }
    }

    fn refusal_outcome(&self, tool: &str, refusal: &PolicyRefusal) -> PolicyOutcome {
        let capability = crate::policy::row_for(tool)
            .map_or(godot_mcp_domain::contracts::Capability::Read, |row| {
                row.capability
            });
        let decision = self
            .authority
            .refusal_decision(tool, capability, refusal)
            .ok()
            .and_then(|decision| serde_json::to_value(&decision).ok())
            .unwrap_or(Value::Null);
        PolicyOutcome {
            allowed: false,
            code: Some(refusal.error_kind().code()),
            message: refusal.to_string(),
            decision_id: None,
            permit_issued: false,
            decision,
        }
    }

    fn dispatch_outcome(&self, tool: &str, error: &PolicyDispatchError) -> PolicyOutcome {
        match error {
            PolicyDispatchError::Refused(refusal) => self.refusal_outcome(tool, refusal),
            PolicyDispatchError::Effect(effect) => PolicyOutcome {
                allowed: false,
                code: Some(error.error_kind().code()),
                message: effect.to_string(),
                decision_id: None,
                permit_issued: false,
                decision: Value::Null,
            },
        }
    }

    fn harness_outcome(&self, _tool: &str, error: &HarnessError) -> PolicyOutcome {
        PolicyOutcome {
            allowed: false,
            code: Some("internal"),
            message: error.to_string(),
            decision_id: None,
            permit_issued: false,
            decision: Value::Null,
        }
    }
}
