//! The `test-hooks`-gated permit / fault-hook factory.
//!
//! This harness is the **only** way a test can reach the crate-private effect
//! gateway, the crate-private permit authorities and the durability fault points.
//! It is compiled solely under the `test-hooks` feature, so a release build
//! contains neither the harness nor the fault-injection branches — the
//! `release_build_has_no_test_hooks` acceptance test proves that from the outside.
//!
//! The harness deliberately exposes *behaviour*, not the permit types: a test can
//! ask it to commit an effect, or to attempt an illegal transition, but it never
//! receives a permit value it could smuggle elsewhere.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use godot_mcp_domain::contracts::{Capability, ProjectIdentity};
use godot_mcp_domain::lease::{Lease, LeaseManager};

use crate::effect::{
    EffectBoundaries, EffectGateway, FileEffectIntent, LocalLeaseEvidence,
    OperatorRecoveryAuthority, PermitAuthority, PermitRequest, PlannedWrite, RepairOperation,
    SafeRootFileBoundary, UnavailableBoundaries,
};
use crate::journal::{
    Conclusion, EffectBoundaryKind, EffectKind, FileOperation, HeldProject, OperationFamily, Phase,
    SafeRootProofSource, TransactionStore, TransportMode, TypedProof, encode_object_identity,
    reconcile_open_transactions,
};
use crate::stdio::INITIAL_PROJECT_GENERATION;

/// Which mock file mutation to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockFileOperation {
    /// Publish a path that must currently be absent.
    Create,
    /// Replace a path that must currently hold the recorded preimage.
    Update,
    /// Remove a path that must currently hold the recorded preimage.
    Delete,
}

impl MockFileOperation {
    const fn to_journal(self) -> FileOperation {
        match self {
            Self::Create => FileOperation::Create,
            Self::Update => FileOperation::Update,
            Self::Delete => FileOperation::Delete,
        }
    }
}

/// One mock file effect a test drives end to end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockFileEffect {
    /// Fixed transaction ID so a test can assert on exact durable records.
    pub transaction_id: String,
    /// Project-relative path.
    pub path: String,
    /// Mutation shape.
    pub operation: MockFileOperation,
    /// Bytes to publish (ignored for a delete).
    pub bytes: Vec<u8>,
    /// Optional linked `process_launch` transaction (Todo 17's composite).
    pub required_process_transaction: Option<String>,
}

/// One durability fault point a test can arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultInjection {
    /// Fail before the named phase's fsync.
    BeforeFsync(JournalPhase),
    /// Fail after the named phase's fsync.
    AfterFsync(JournalPhase),
}

/// A journal phase usable in a fault selector or an assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalPhase {
    Intent,
    Staged,
    EffectStarted,
    Committed,
    Compensated,
    Quarantined,
}

impl JournalPhase {
    const fn to_journal(self) -> Phase {
        match self {
            Self::Intent => Phase::IntentFsynced,
            Self::Staged => Phase::StagedFsynced,
            Self::EffectStarted => Phase::EffectStartedFsynced,
            Self::Committed => Phase::CommittedFsynced,
            Self::Compensated => Phase::Compensated,
            Self::Quarantined => Phase::Quarantined,
        }
    }

    const fn from_journal(phase: Phase) -> Self {
        match phase {
            Phase::IntentFsynced => Self::Intent,
            Phase::StagedFsynced => Self::Staged,
            Phase::EffectStartedFsynced => Self::EffectStarted,
            Phase::CommittedFsynced => Self::Committed,
            Phase::Compensated => Self::Compensated,
            Phase::Quarantined => Self::Quarantined,
        }
    }

    fn to_fault(self) -> crate::journal::PhaseName {
        crate::journal::PhaseName::of(self.to_journal())
    }
}

/// The terminal disposition of a settled transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalDisposition {
    /// The effect provably completed.
    ProvenPostState,
    /// The effect provably did not happen.
    ProvenPreState,
    /// Neither state is proven; writes are blocked.
    Quarantine,
}

impl TerminalDisposition {
    const fn from_conclusion(conclusion: Conclusion) -> Self {
        match conclusion {
            Conclusion::ProvenPostState => Self::ProvenPostState,
            Conclusion::ProvenPreState => Self::ProvenPreState,
            Conclusion::Quarantine => Self::Quarantine,
        }
    }
}

/// A durable record as a test observes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedTransaction {
    pub transaction_id: String,
    pub phase: JournalPhase,
    pub phase_history: Vec<JournalPhase>,
    pub effect_kind: String,
    pub staged_write_count: usize,
    pub has_proof: bool,
    pub terminal_reason: Option<String>,
    pub terminal_outcome: Option<&'static str>,
    pub required_process_transaction: Option<String>,
}

/// The five frozen effect boundaries, as a test names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectBoundary {
    File,
    Process,
    LspDap,
    Editor,
    Runtime,
}

impl EffectBoundary {
    /// Every boundary a test can name.
    #[must_use]
    pub const fn all() -> [Self; 5] {
        [
            Self::File,
            Self::Process,
            Self::LspDap,
            Self::Editor,
            Self::Runtime,
        ]
    }

    const fn to_journal(self) -> EffectBoundaryKind {
        match self {
            Self::File => EffectBoundaryKind::File,
            Self::Process => EffectBoundaryKind::Process,
            Self::LspDap => EffectBoundaryKind::LspDap,
            Self::Editor => EffectBoundaryKind::Editor,
            Self::Runtime => EffectBoundaryKind::Runtime,
        }
    }

    const fn capability(self) -> Capability {
        match self {
            Self::File | Self::LspDap => Capability::ProjectWrite,
            Self::Editor => Capability::EditorMutate,
            Self::Process | Self::Runtime => Capability::RuntimeExecute,
        }
    }

    const fn operation_family(self) -> OperationFamily {
        match self {
            Self::File | Self::LspDap => OperationFamily::ProjectMutation,
            Self::Editor => OperationFamily::EditorMutation,
            Self::Process | Self::Runtime => OperationFamily::RuntimeExecution,
        }
    }
}

/// Which proof-driven recovery operation to request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryOperation {
    Reconcile,
    Compensate,
    Terminate,
    AcknowledgeSafeState,
}

impl RecoveryOperation {
    const fn to_permit(self) -> RepairOperation {
        match self {
            Self::Reconcile => RepairOperation::Reconcile,
            Self::Compensate => RepairOperation::Compensate,
            Self::Terminate => RepairOperation::Terminate,
            Self::AcknowledgeSafeState => RepairOperation::AcknowledgeSafeState,
        }
    }
}

/// A harness failure. Every variant is a genuine production rejection surfaced to
/// the test; the harness never fabricates a success.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("project ownership could not be established: {0}")]
    Lease(String),
    #[error("durable store failed: {0}")]
    Store(String),
    #[error("permit was refused: {0}")]
    Permit(String),
    #[error("effect was refused [{code}]: {message}")]
    Effect {
        /// Stable domain error code from the ten-item taxonomy.
        code: &'static str,
        /// Caller-safe message.
        message: String,
    },
}

impl HarnessError {
    /// Stable domain error code, where the failure carries one.
    #[must_use]
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::Effect { code, .. } => Some(code),
            _ => None,
        }
    }
}

fn effect_error(error: crate::effect::EffectError) -> HarnessError {
    let outcome = error.to_outcome();
    HarnessError::Effect {
        code: outcome.code(),
        message: outcome.message().to_owned(),
    }
}

/// The Todo 6 acceptance harness over one held project.
pub struct JournalFoundationHarness {
    lease: Lease,
    store: Arc<TransactionStore>,
    gateway: EffectGateway,
    authority: PermitAuthority,
    held: HeldProject,
    project: ProjectIdentity,
    state_dir: PathBuf,
}

impl JournalFoundationHarness {
    /// Acquire the project, open the durable journal and assemble the gateway.
    pub fn open(project_root: &Path) -> Result<Self, HarnessError> {
        Self::open_with_epoch(project_root, 1)
    }

    /// Same as [`Self::open`], with an explicit permit-authority epoch so a test
    /// can prove a permit minted by a superseded epoch is rejected as stale.
    pub fn open_with_epoch(project_root: &Path, epoch: u64) -> Result<Self, HarnessError> {
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
        let gateway = EffectGateway::new(
            Arc::clone(&store),
            held.clone(),
            epoch,
            Box::new(SafeRootProofSource::new(lease.safe_root())),
            EffectBoundaries {
                file: Box::new(SafeRootFileBoundary::new(lease.safe_root())),
                process: Box::new(UnavailableBoundaries),
                lsp_dap: Box::new(UnavailableBoundaries),
                editor: Box::new(UnavailableBoundaries),
                runtime: Box::new(UnavailableBoundaries),
            },
        );
        let authority = PermitAuthority::new(Arc::clone(&store), epoch);
        Ok(Self {
            lease,
            store,
            gateway,
            authority,
            held,
            project,
            state_dir,
        })
    }

    /// Directory holding the durable journal files.
    #[must_use]
    pub fn journal_directory(&self) -> PathBuf {
        self.store.directory().to_path_buf()
    }

    /// The private state directory for this project.
    #[must_use]
    pub fn state_directory(&self) -> &Path {
        &self.state_dir
    }

    /// The public project identity the journal binds every record to.
    #[must_use]
    pub fn project(&self) -> ProjectIdentity {
        self.project.clone()
    }

    /// Which effect boundaries have a real implementation compiled in.
    #[must_use]
    pub fn boundary_availability(&self) -> Vec<(String, bool)> {
        self.gateway
            .boundary_availability()
            .into_iter()
            .map(|(boundary, available)| (format!("{boundary:?}").to_lowercase(), available))
            .collect()
    }

    /// Arm (or clear) one durability fault point.
    pub fn arm_fault(&self, fault: Option<FaultInjection>) {
        let point = fault.map(|fault| match fault {
            FaultInjection::BeforeFsync(phase) => {
                crate::journal::FaultPoint::BeforeFsync(phase.to_fault())
            }
            FaultInjection::AfterFsync(phase) => {
                crate::journal::FaultPoint::AfterFsync(phase.to_fault())
            }
        });
        self.store.arm_fault(point);
    }

    /// Journal `intent_fsynced` for one mock effect.
    pub fn open_transaction(&self, effect: &MockFileEffect) -> Result<(), HarnessError> {
        self.gateway
            .open_file_transaction(&intent_of(effect))
            .map(|_record| ())
            .map_err(effect_error)
    }

    /// Durably stage the mock effect's payload (`staged_fsynced`).
    pub fn stage_transaction(&self, effect: &MockFileEffect) -> Result<(), HarnessError> {
        self.gateway
            .stage_file_transaction(&intent_of(effect))
            .map(|_record| ())
            .map_err(effect_error)
    }

    /// Execute the staged effect under a freshly issued ordinary permit.
    pub fn execute_transaction(
        &self,
        effect: &MockFileEffect,
    ) -> Result<TerminalDisposition, HarnessError> {
        let permit = self
            .authority
            .issue(PermitRequest {
                transaction_id: effect.transaction_id.clone(),
                capability: Capability::ProjectWrite,
                operation_family: OperationFamily::ProjectMutation,
                boundary: EffectBoundaryKind::File,
                project_object_identity: self.held.object_identity.clone(),
                project_generation: self.held.generation,
            })
            .map_err(|error| HarnessError::Permit(error.to_string()))?;
        self.gateway
            .execute_file_effect(&permit)
            .map(|outcome| TerminalDisposition::from_conclusion(outcome.conclusion))
            .map_err(effect_error)
    }

    /// Attempt one non-`file` effect boundary under a freshly issued permit.
    ///
    /// Todo 6 ships the four later boundaries as fail-closed stand-ins, so this
    /// returns their refusal. It exists so the acceptance suite proves all five
    /// boundaries exist, are permit-gated and cannot fabricate a proof.
    pub fn attempt_boundary(
        &self,
        effect: &MockFileEffect,
        boundary: EffectBoundary,
    ) -> Result<(), HarnessError> {
        let permit = self
            .authority
            .issue(PermitRequest {
                transaction_id: effect.transaction_id.clone(),
                capability: boundary.capability(),
                operation_family: boundary.operation_family(),
                boundary: boundary.to_journal(),
                project_object_identity: self.held.object_identity.clone(),
                project_generation: self.held.generation,
            })
            .map_err(|error| HarnessError::Permit(error.to_string()))?;
        self.gateway
            .attempt_boundary_effect(&permit, boundary.to_journal())
            .map(|_proof| ())
            .map_err(effect_error)
    }

    /// Drive one mock effect through all four phases.
    pub fn commit_transaction(
        &self,
        effect: &MockFileEffect,
    ) -> Result<TerminalDisposition, HarnessError> {
        self.open_transaction(effect)?;
        self.stage_transaction(effect)?;
        self.execute_transaction(effect)
    }

    /// Attempt to execute an effect whose permit was minted by a *different*
    /// authority epoch, proving a stale permit is rejected.
    pub fn execute_with_stale_permit(
        &self,
        effect: &MockFileEffect,
        stale_epoch: u64,
    ) -> Result<TerminalDisposition, HarnessError> {
        let stale = PermitAuthority::new(Arc::clone(&self.store), stale_epoch);
        let permit = stale
            .issue(PermitRequest {
                transaction_id: effect.transaction_id.clone(),
                capability: Capability::ProjectWrite,
                operation_family: OperationFamily::ProjectMutation,
                boundary: EffectBoundaryKind::File,
                project_object_identity: self.held.object_identity.clone(),
                project_generation: self.held.generation,
            })
            .map_err(|error| HarnessError::Permit(error.to_string()))?;
        self.gateway
            .execute_file_effect(&permit)
            .map(|outcome| TerminalDisposition::from_conclusion(outcome.conclusion))
            .map_err(effect_error)
    }

    /// Attempt to execute an effect whose permit names a different project
    /// generation, proving generation mismatch is rejected.
    pub fn execute_with_wrong_generation(
        &self,
        effect: &MockFileEffect,
    ) -> Result<TerminalDisposition, HarnessError> {
        let permit = self
            .authority
            .issue(PermitRequest {
                transaction_id: effect.transaction_id.clone(),
                capability: Capability::ProjectWrite,
                operation_family: OperationFamily::ProjectMutation,
                boundary: EffectBoundaryKind::File,
                project_object_identity: self.held.object_identity.clone(),
                project_generation: self.held.generation + 1,
            })
            .map_err(|error| HarnessError::Permit(error.to_string()))?;
        self.gateway
            .execute_file_effect(&permit)
            .map(|outcome| TerminalDisposition::from_conclusion(outcome.conclusion))
            .map_err(effect_error)
    }

    /// Attempt to issue an ordinary permit. Used to prove quarantine blocks every
    /// ordinary capability-profile-driven permit unconditionally.
    pub fn try_issue_ordinary_permit(&self, transaction_id: &str) -> Result<(), HarnessError> {
        self.authority
            .issue(PermitRequest {
                transaction_id: transaction_id.to_owned(),
                capability: Capability::ProjectWrite,
                operation_family: OperationFamily::ProjectMutation,
                boundary: EffectBoundaryKind::File,
                project_object_identity: self.held.object_identity.clone(),
                project_generation: self.held.generation,
            })
            .map(|_permit| ())
            .map_err(|error| HarnessError::Permit(error.to_string()))
    }

    /// Attempt an arbitrary phase transition directly on the store, proving the
    /// phase machine rejects an illegal one without mutating the record.
    pub fn try_transition(
        &self,
        transaction_id: &str,
        target: JournalPhase,
    ) -> Result<(), HarnessError> {
        self.store
            .advance(transaction_id, target.to_journal(), None, None, None)
            .map(|_record| ())
            .map_err(|error| HarnessError::Store(error.to_string()))
    }

    /// Overwrite one staged payload with genuinely partial bytes.
    pub fn corrupt_staged_bytes(
        &self,
        transaction_id: &str,
        partial: &[u8],
    ) -> Result<(), HarnessError> {
        let record = self
            .store
            .load(transaction_id)
            .map_err(|error| HarnessError::Store(error.to_string()))?;
        let staging_id = record
            .staged_write_set
            .first()
            .map(|entry| entry.staging_id.clone())
            .ok_or_else(|| HarnessError::Store("transaction has no staged write set".to_owned()))?;
        self.store
            .corrupt_staged_bytes(transaction_id, &staging_id, partial)
            .map_err(|error| HarnessError::Store(error.to_string()))
    }

    /// Read one durable record exactly as it is stored.
    pub fn observe(&self, transaction_id: &str) -> Result<ObservedTransaction, HarnessError> {
        let record = self
            .store
            .load(transaction_id)
            .map_err(|error| HarnessError::Store(error.to_string()))?;
        Ok(ObservedTransaction {
            transaction_id: record.transaction_id.clone(),
            phase: JournalPhase::from_journal(record.phase),
            phase_history: record
                .phase_history
                .iter()
                .map(|step| JournalPhase::from_journal(step.phase))
                .collect(),
            effect_kind: record.effect_kind.as_str().to_owned(),
            staged_write_count: record.staged_write_set.len(),
            has_proof: record.proof.is_some(),
            terminal_reason: record.terminal_reason.clone(),
            terminal_outcome: record.terminal_outcome.map(|outcome| match outcome {
                crate::journal::TerminalOutcome::Succeeded => "succeeded",
                crate::journal::TerminalOutcome::Failed { code } => code.code(),
            }),
            required_process_transaction: record.required_process_transaction().map(str::to_owned),
        })
    }

    pub fn observe_all(&self) -> Result<Vec<ObservedTransaction>, HarnessError> {
        self.store
            .all_transactions()
            .map_err(|error| HarnessError::Store(error.to_string()))?
            .into_iter()
            .map(|record| self.observe(&record.transaction_id))
            .collect()
    }

    /// Every transaction currently quarantined.
    pub fn quarantined(&self) -> Result<Vec<String>, HarnessError> {
        self.store
            .quarantined_transactions()
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

    /// Run startup reconciliation over every open transaction.
    pub fn reconcile(&self) -> Result<Vec<(String, String)>, HarnessError> {
        let settled = reconcile_open_transactions(
            self.store.as_ref(),
            &self.held,
            &SafeRootProofSource::new(self.lease.safe_root()),
        )
        .map_err(|error| HarnessError::Store(error.to_string()))?;
        Ok(settled
            .into_iter()
            .map(|entry| (entry.transaction_id, entry.conclusion.as_str().to_owned()))
            .collect())
    }

    /// Run reconciliation against a *different* held project identity, proving a
    /// retargeted or regenerated project quarantines rather than compares hashes
    /// against the wrong project.
    pub fn reconcile_with_foreign_project(&self) -> Result<Vec<(String, String)>, HarnessError> {
        let mut foreign = self.held.clone();
        foreign.generation += 1;
        let settled = reconcile_open_transactions(
            self.store.as_ref(),
            &foreign,
            &SafeRootProofSource::new(self.lease.safe_root()),
        )
        .map_err(|error| HarnessError::Store(error.to_string()))?;
        Ok(settled
            .into_iter()
            .map(|entry| (entry.transaction_id, entry.conclusion.as_str().to_owned()))
            .collect())
    }

    /// Journal a synthetic `process_launch` transaction with an explicit terminal
    /// proof, so the Todo 17 composite gate can be exercised before Todo 14 lands.
    pub fn journal_process_transaction(
        &self,
        transaction_id: &str,
        clean_terminal: bool,
    ) -> Result<(), HarnessError> {
        use crate::journal::{BoundaryMembership, ReleaseStatus, TransactionRecord};
        let (release_status, exit_code, empty) = if clean_terminal {
            (ReleaseStatus::ExitedClean, Some(0), Some(true))
        } else {
            (ReleaseStatus::ResumedContained, None, Some(false))
        };
        let (hash, bytes) = crate::journal::canonical_request(
            "project.run",
            &serde_json::json!({ "boundary": transaction_id }),
        );
        let record = TransactionRecord {
            schema_version: String::new(),
            transaction_id: transaction_id.to_owned(),
            effect_kind: EffectKind::ProcessLaunch,
            effect_boundary: EffectBoundaryKind::Process,
            project_object_identity: self.held.object_identity.clone(),
            project_root_object: self.held.root_object.clone(),
            project_file_object: self.held.file_object.clone(),
            project_generation: self.held.generation,
            transport_mode: TransportMode::Stdio,
            principal: None,
            session: None,
            operation_family: OperationFamily::RuntimeExecution,
            tool_name: "project.run".to_owned(),
            idempotency_id: format!("helper-{transaction_id}"),
            canonical_request_hash: hash,
            canonical_request_bytes: bytes,
            touched_paths: Vec::new(),
            touched_object_identities: Vec::new(),
            pre_hashes: Vec::new(),
            staged_write_set: Vec::new(),
            intended_post_hashes: Vec::new(),
            phase: Phase::IntentFsynced,
            phase_history: Vec::new(),
            proof: None,
            required_subproofs: None,
            terminal_reason: None,
            terminal_outcome: None,
            opened_at_unix_ms: 1_700_000_000_000,
        };
        self.store
            .open_transaction(record)
            .map_err(|error| HarnessError::Store(error.to_string()))?;
        let proof = TypedProof::ProcessLaunch {
            boundary_identity: format!("boundary:{transaction_id}"),
            pid: Some(4242),
            start_time: Some("12345".to_owned()),
            boundary_membership: BoundaryMembership::Confirmed,
            release_status,
            exit_code,
            signal: None,
            boundary_empty_after_exit: empty,
        };
        for phase in [Phase::StagedFsynced, Phase::EffectStartedFsynced] {
            self.store
                .advance(transaction_id, phase, None, None, None)
                .map_err(|error| HarnessError::Store(error.to_string()))?;
        }
        self.store
            .advance(
                transaction_id,
                Phase::CommittedFsynced,
                Some(proof),
                Some("synthetic helper proof".to_owned()),
                Some(crate::journal::TerminalOutcome::Succeeded),
            )
            .map_err(|error| HarnessError::Store(error.to_string()))?;
        Ok(())
    }

    /// Request one proof-driven operator-recovery operation through the
    /// non-profile-driven recovery authority.
    ///
    /// The authority requires local lease evidence, which is only available here
    /// because the harness holds the real lease; no network path can present it.
    pub fn operator_recovery(
        &self,
        transaction_id: &str,
        operation: RecoveryOperation,
    ) -> Result<TerminalDisposition, HarnessError> {
        let authority = OperatorRecoveryAuthority::new(
            Arc::clone(&self.store),
            LocalLeaseEvidence::new(self.held.object_identity.clone(), self.held.generation),
        );
        let permit = authority
            .issue(transaction_id, operation.to_permit())
            .map_err(|error| HarnessError::Permit(error.to_string()))?;
        self.gateway
            .apply_repair(&permit)
            .map(|outcome| TerminalDisposition::from_conclusion(outcome.conclusion))
            .map_err(effect_error)
    }
}

fn intent_of(effect: &MockFileEffect) -> FileEffectIntent {
    FileEffectIntent {
        transaction_id: effect.transaction_id.clone(),
        tool_name: "resource.update".to_owned(),
        operation_family: OperationFamily::ProjectMutation,
        idempotency_id: format!("idem-{}", effect.transaction_id),
        transport_mode: TransportMode::Stdio,
        principal: Some("principal_0123456789abcdef".to_owned()),
        session: None,
        arguments: serde_json::json!({
            "operation": format!("{:?}", effect.operation).to_lowercase(),
            "path": effect.path,
        }),
        writes: vec![PlannedWrite {
            path: effect.path.clone(),
            operation: effect.operation.to_journal(),
            bytes: match effect.operation {
                MockFileOperation::Delete => None,
                MockFileOperation::Create | MockFileOperation::Update => Some(effect.bytes.clone()),
            },
            precondition: crate::effect::WritePrecondition::Observed,
        }],
        required_process_transaction: effect.required_process_transaction.clone(),
        opened_at_unix_ms: 1_700_000_000_000,
        expected_revision: None,
    }
}
