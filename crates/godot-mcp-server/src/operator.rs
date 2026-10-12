//! The local-only operator-recovery port.
//!
//! Todo 6 deliberately keeps its durable store, its phase machine, its permit
//! types and its effect gateway crate-private: a permit has no public
//! constructor, so no adapter, plugin or external crate can reach an effect at
//! all. Todo 34's `transaction repair` subcommand is nonetheless the **sole
//! authorized caller** of the operator-recovery authority, and it lives in the
//! `godot-mcp-cli` crate.
//!
//! This module is that one narrow port, and nothing more. It:
//!
//! * acquires the Todo 2 project-lease advisory OS file lock, failing closed with
//!   [`OperatorError::Conflict`] when another owner holds it;
//! * opens the one durable journal Todo 6 owns — never a second store;
//! * hands the caller a **read-only, plain-data view** of one transaction record;
//!   and
//! * applies exactly one proof-backed transition through the same
//!   [`OperatorRecoveryAuthority`](crate::effect::OperatorRecoveryAuthority) and
//!   the same [`EffectGateway`](crate::effect::EffectGateway) every other
//!   mutation flows through, using a `RepairPermit` rather than any ordinary
//!   production permit.
//!
//! No permit, store handle, record, proof or boundary type crosses this
//! boundary; every published type is inert data. A caller can therefore describe
//! what it observed, but can never smuggle journal authority anywhere else — and
//! in particular cannot reach this module from an MCP/HTTP handler, because
//! [`OperatorSession::open`] requires a held advisory lease that a network peer
//! has no way to present.

use std::path::Path;
use std::sync::Arc;

use godot_mcp_domain::lease::Lease;
use godot_mcp_domain::objid::StableObjectId;
use godot_mcp_domain::safe_root::SafeRoot;

use crate::effect::{
    DerivedDisposition, DerivedTerminal, EffectBoundaries, EffectGateway, LocalLeaseEvidence,
    OperatorRecoveryAuthority, RepairOperation, SafeRootFileBoundary, UnavailableBoundaries,
};
use crate::journal::{
    BoundaryMembership, EffectKind, Existence, FileOperation, FileStateEntry, HeldProject, Phase,
    ReleaseStatus, SafeRootProofSource, TransactionStore, TypedProof, encode_object_identity,
};
use crate::project_context::ProjectContext;
use crate::router::RepairIntent;

/// The authority epoch the operator lane runs under. Ordinary permits are never
/// minted here, and a `RepairPermit` carries no epoch, so this only distinguishes
/// the lane in diagnostics.
const OPERATOR_EPOCH: u64 = 34;

/// Why an operator-recovery step was refused.
#[derive(Debug, thiserror::Error)]
pub enum OperatorError {
    /// Another owner holds the project lease, or the project cannot be owned.
    #[error("the project lease could not be acquired: {0}")]
    Conflict(String),
    /// The durable journal could not be opened or read.
    #[error("the durable transaction journal could not be used: {0}")]
    Store(String),
    /// No such transaction is journaled.
    #[error("transaction {0} is not journaled for this project")]
    NotFound(String),
    /// The transaction is not in a repairable status.
    #[error("transaction {transaction_id} is {phase}, which this command may not repair")]
    WrongPhase {
        /// The transaction that was targeted.
        transaction_id: String,
        /// Its current durable phase, in canonical spelling.
        phase: String,
    },
    /// The recovery authority or the gateway refused the requested transition.
    #[error("the operator-recovery authority refused: {0}")]
    Refused(String),
}

/// Which typed proof form a record requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofKind {
    /// Existence plus content hash of every touched path.
    File,
    /// Containment boundary, PID/start time, membership and release status.
    ProcessLaunch,
    /// Authoritative bridge receipt plus a live state probe.
    EditorRuntime,
}

impl ProofKind {
    /// Canonical spelling, matching the record schema's `effect_kind` enum.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::ProcessLaunch => "process_launch",
            Self::EditorRuntime => "editor_runtime",
        }
    }

    const fn from_journal(kind: EffectKind) -> Self {
        match kind {
            EffectKind::File => Self::File,
            EffectKind::ProcessLaunch => Self::ProcessLaunch,
            EffectKind::EditorRuntime => Self::EditorRuntime,
        }
    }
}

/// Whether one path is recorded as present or absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordedExistence {
    /// The path exists.
    Present,
    /// The path does not exist; this is the existence tombstone.
    Absent,
}

impl RecordedExistence {
    /// Canonical spelling, matching the repair-proof schema's enum.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Absent => "absent",
        }
    }

    const fn from_journal(existence: Existence) -> Self {
        match existence {
            Existence::Present => Self::Present,
            Existence::Absent => Self::Absent,
        }
    }

    const fn to_journal(self) -> Existence {
        match self {
            Self::Present => Existence::Present,
            Self::Absent => Existence::Absent,
        }
    }
}

/// The declared shape of one path's mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordedFileOperation {
    /// The path did not exist and was to be created.
    Create,
    /// The path existed and its bytes were to be replaced.
    Update,
    /// The path existed and was to be removed.
    Delete,
}

impl RecordedFileOperation {
    const fn from_journal(operation: FileOperation) -> Self {
        match operation {
            FileOperation::Create => Self::Create,
            FileOperation::Update => Self::Update,
            FileOperation::Delete => Self::Delete,
        }
    }

    const fn to_journal(self) -> FileOperation {
        match self {
            Self::Create => FileOperation::Create,
            Self::Update => FileOperation::Update,
            Self::Delete => FileOperation::Delete,
        }
    }
}

/// One path's recorded or observed existence and content hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileState {
    /// Project-relative path.
    pub path: String,
    /// Declared mutation shape.
    pub operation: RecordedFileOperation,
    /// Whether the path is present.
    pub existence: RecordedExistence,
    /// Lowercase hex SHA-256 of the bytes, or `None` for an absent path.
    pub sha256: Option<String>,
}

impl FileState {
    fn from_journal(entry: &FileStateEntry) -> Self {
        Self {
            path: entry.path.clone(),
            operation: RecordedFileOperation::from_journal(entry.operation),
            existence: RecordedExistence::from_journal(entry.existence),
            sha256: entry.sha256.clone(),
        }
    }

    fn to_journal(&self) -> FileStateEntry {
        FileStateEntry {
            path: self.path.clone(),
            operation: self.operation.to_journal(),
            existence: self.existence.to_journal(),
            sha256: self.sha256.clone(),
        }
    }
}

/// A supervised process's observed release or terminal disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedRelease {
    /// Still blocked at the launch barrier.
    Suspended,
    /// Resumed and still inside its boundary.
    ResumedContained,
    /// Exited with code 0 and no signal.
    ExitedClean,
    /// Terminated by a signal.
    Signaled,
    /// Exited with a nonzero code.
    NonzeroExit,
    /// The disposition could not be determined.
    Unknown,
}

impl ObservedRelease {
    /// Canonical spelling, matching the repair-proof schema's enum.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Suspended => "suspended",
            Self::ResumedContained => "resumed-contained",
            Self::ExitedClean => "exited-clean",
            Self::Signaled => "signaled",
            Self::NonzeroExit => "nonzero-exit",
            Self::Unknown => "unknown",
        }
    }

    /// Parse the canonical spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "suspended" => Some(Self::Suspended),
            "resumed-contained" => Some(Self::ResumedContained),
            "exited-clean" => Some(Self::ExitedClean),
            "signaled" => Some(Self::Signaled),
            "nonzero-exit" => Some(Self::NonzeroExit),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }

    const fn from_journal(status: ReleaseStatus) -> Self {
        match status {
            ReleaseStatus::Suspended => Self::Suspended,
            ReleaseStatus::ResumedContained => Self::ResumedContained,
            ReleaseStatus::ExitedClean => Self::ExitedClean,
            ReleaseStatus::Signaled => Self::Signaled,
            ReleaseStatus::NonzeroExit => Self::NonzeroExit,
            ReleaseStatus::Unknown => Self::Unknown,
        }
    }

    const fn to_journal(self) -> ReleaseStatus {
        match self {
            Self::Suspended => ReleaseStatus::Suspended,
            Self::ResumedContained => ReleaseStatus::ResumedContained,
            Self::ExitedClean => ReleaseStatus::ExitedClean,
            Self::Signaled => ReleaseStatus::Signaled,
            Self::NonzeroExit => ReleaseStatus::NonzeroExit,
            Self::Unknown => ReleaseStatus::Unknown,
        }
    }
}

/// One typed proof, as plain data, in exactly the three forms Todo 6 defines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservedProof {
    /// Existence plus content hash of every touched path.
    File {
        /// One entry per touched path.
        entries: Vec<FileState>,
    },
    /// Containment/PID/start-time/release proof; never a content hash.
    ProcessLaunch {
        /// The boundary's own concrete identity, `<kind>:<identity>`.
        boundary_identity: String,
        /// The observed PID, or `None` when no live process was found.
        pid: Option<u32>,
        /// The observed platform-native start time.
        start_time: Option<String>,
        /// Whether boundary membership was independently confirmed.
        boundary_membership_confirmed: bool,
        /// The observed release or terminal disposition.
        release_status: ObservedRelease,
        /// The observed exit code, when the process exited normally.
        exit_code: Option<i64>,
        /// The observed terminating signal, when the process was signalled.
        signal: Option<String>,
        /// Whether the boundary was proven empty after exit.
        boundary_empty_after_exit: Option<bool>,
    },
    /// Authoritative bridge receipt plus, where reachable, a live state probe.
    EditorRuntime {
        /// Whether the bridge answered at all.
        bridge_reachable: bool,
        /// The transaction the receipt is bound to.
        receipt_transaction_id: Option<String>,
        /// The plugin-acknowledged action or operation identifier.
        receipt_action_id: Option<String>,
        /// The live state-probe answer.
        live_state_probe: Option<String>,
    },
}

impl ObservedProof {
    /// Which typed form this proof is.
    #[must_use]
    pub const fn kind(&self) -> ProofKind {
        match self {
            Self::File { .. } => ProofKind::File,
            Self::ProcessLaunch { .. } => ProofKind::ProcessLaunch,
            Self::EditorRuntime { .. } => ProofKind::EditorRuntime,
        }
    }

    /// Whether this is a `process_launch` proof of a contained, clean, terminal
    /// run: confirmed membership, an observed clean exit with code 0 and no
    /// signal, and a proven-empty boundary after exit.
    ///
    /// A live survivor never satisfies it, and neither does a signalled or
    /// nonzero exit. This is Todo 6's own terminal-completion rule, evaluated
    /// through the journal's type rather than re-stated here.
    #[must_use]
    pub fn is_clean_terminal_process(&self) -> bool {
        self.to_journal().is_clean_terminal_process()
    }

    fn from_journal(proof: &TypedProof) -> Self {
        match proof {
            TypedProof::File { entries } => Self::File {
                entries: entries.iter().map(FileState::from_journal).collect(),
            },
            TypedProof::ProcessLaunch {
                boundary_identity,
                pid,
                start_time,
                boundary_membership,
                release_status,
                exit_code,
                signal,
                boundary_empty_after_exit,
            } => Self::ProcessLaunch {
                boundary_identity: boundary_identity.clone(),
                pid: *pid,
                start_time: start_time.clone(),
                boundary_membership_confirmed: *boundary_membership
                    == BoundaryMembership::Confirmed,
                release_status: ObservedRelease::from_journal(*release_status),
                exit_code: *exit_code,
                signal: signal.clone(),
                boundary_empty_after_exit: *boundary_empty_after_exit,
            },
            TypedProof::EditorRuntime {
                bridge_reachable,
                receipt_transaction_id,
                receipt_action_id,
                live_state_probe,
            } => Self::EditorRuntime {
                bridge_reachable: *bridge_reachable,
                receipt_transaction_id: receipt_transaction_id.clone(),
                receipt_action_id: receipt_action_id.clone(),
                live_state_probe: live_state_probe.clone(),
            },
        }
    }

    fn to_journal(&self) -> TypedProof {
        match self {
            Self::File { entries } => TypedProof::File {
                entries: entries.iter().map(FileState::to_journal).collect(),
            },
            Self::ProcessLaunch {
                boundary_identity,
                pid,
                start_time,
                boundary_membership_confirmed,
                release_status,
                exit_code,
                signal,
                boundary_empty_after_exit,
            } => TypedProof::ProcessLaunch {
                boundary_identity: boundary_identity.clone(),
                pid: *pid,
                start_time: start_time.clone(),
                boundary_membership: if *boundary_membership_confirmed {
                    BoundaryMembership::Confirmed
                } else {
                    BoundaryMembership::Unconfirmed
                },
                release_status: release_status.to_journal(),
                exit_code: *exit_code,
                signal: signal.clone(),
                boundary_empty_after_exit: *boundary_empty_after_exit,
            },
            Self::EditorRuntime {
                bridge_reachable,
                receipt_transaction_id,
                receipt_action_id,
                live_state_probe,
            } => TypedProof::EditorRuntime {
                bridge_reachable: *bridge_reachable,
                receipt_transaction_id: receipt_transaction_id.clone(),
                receipt_action_id: receipt_action_id.clone(),
                live_state_probe: live_state_probe.clone(),
            },
        }
    }
}

/// Which recorded state a live re-derivation proved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvenDisposition {
    /// The recorded pre-state still holds.
    PreState,
    /// The recorded intended post-state holds.
    PostState,
}

impl ProvenDisposition {
    /// Canonical spelling, matching the repair-proof schema's enum.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreState => "pre-state",
            Self::PostState => "post-state",
        }
    }

    const fn to_gateway(self) -> DerivedDisposition {
        match self {
            Self::PreState => DerivedDisposition::PreState,
            Self::PostState => DerivedDisposition::PostState,
        }
    }
}

/// One live re-derivation: which recorded state it proved, and what it observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedProof {
    /// Which recorded state the observation proved.
    pub disposition: ProvenDisposition,
    /// What was actually observed, in the record's own typed form.
    pub observed: ObservedProof,
}

/// A read-only view of one journaled transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairTarget {
    /// The transaction's identifier.
    pub transaction_id: String,
    /// Which typed proof form it requires.
    pub effect_kind: ProofKind,
    /// Its current durable phase, in canonical spelling.
    pub phase: String,
    /// Whether it is currently `quarantined`.
    pub quarantined: bool,
    /// Whether it sits in Todo 14's case-4/5 ambiguous live-survivor state.
    pub ambiguous_survivor: bool,
    /// The public project object identity it binds to.
    pub project_object_identity: String,
    /// The project generation it binds to.
    pub project_generation: u64,
    /// Project-relative touched paths, in declaration order.
    pub touched_paths: Vec<String>,
    /// The recorded platform-tagged OS object identity of every touched path.
    pub touched_object_identities: Vec<(String, Option<String>)>,
    /// The recorded pre-state of every touched path.
    pub pre_hashes: Vec<FileState>,
    /// The recorded intended post-state of every touched path.
    pub intended_post_hashes: Vec<FileState>,
    /// The typed proof already recorded, when one was observed.
    pub recorded_proof: Option<ObservedProof>,
    /// Todo 17's composite requirement: the linked `process_launch` transaction.
    pub required_process_transaction: Option<String>,
}

/// What one applied repair actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairOutcome {
    /// The transaction that was addressed.
    pub transaction_id: String,
    /// The applied operation, in canonical spelling.
    pub operation: &'static str,
    /// The transaction's durable phase after the repair, canonically spelled.
    pub phase: String,
    /// The recovery authority's own reason, surfaced unchanged.
    pub reason: String,
}

/// A held project plus its durable journal, for the local operator lane.
///
/// Holding this value holds the Todo 2 advisory OS lease, so a second owner
/// cannot interleave with a repair. Dropping it releases the lock.
pub struct OperatorSession {
    _context: ProjectContext,
    lease: Lease,
    store: Arc<TransactionStore>,
    gateway: EffectGateway,
    held: HeldProject,
}

impl OperatorSession {
    /// Acquire the project lease and open the one durable journal.
    ///
    /// # Errors
    /// Returns [`OperatorError::Conflict`] when another owner holds the lease or
    /// the project cannot be owned, and [`OperatorError::Store`] when the durable
    /// journal cannot be opened.
    pub fn open(project_root: &Path) -> Result<Self, OperatorError> {
        // The operator lane converges on the same canonical project context the
        // server uses, so a repair through any spelling of the project reaches the
        // one journal, and a running server through any spelling is a conflict.
        let context = ProjectContext::acquire(project_root)
            .map_err(|error| OperatorError::Conflict(error.to_string()))?;
        let lease = context.lease().clone();
        let state_dir = context.state_root().to_path_buf();
        let held = context.held().clone();
        let store = Arc::new(
            TransactionStore::open(&state_dir.join("journal"))
                .map_err(|error| OperatorError::Store(error.to_string()))?,
        );
        let gateway = EffectGateway::new(
            Arc::clone(&store),
            held.clone(),
            OPERATOR_EPOCH,
            Box::new(SafeRootProofSource::new(lease.safe_root())),
            EffectBoundaries {
                file: Box::new(SafeRootFileBoundary::new(lease.safe_root())),
                process: Box::new(UnavailableBoundaries),
                lsp_dap: Box::new(UnavailableBoundaries),
                editor: Box::new(UnavailableBoundaries),
                runtime: Box::new(UnavailableBoundaries),
            },
        );
        Ok(Self {
            _context: context,
            lease,
            store,
            gateway,
            held,
        })
    }

    /// The public project object identity the held lease designates.
    #[must_use]
    pub fn project_object_identity(&self) -> &str {
        &self.held.object_identity
    }

    /// The project generation the held lease designates.
    #[must_use]
    pub const fn project_generation(&self) -> u64 {
        self.held.generation
    }

    /// A descriptor-relative reader rooted at the held project.
    ///
    /// Every path read through it is re-walked and re-identity-checked before
    /// each access, and a symlink at any component is refused, so a proof
    /// re-derived through it is relative to the object the lease designates and
    /// never to an aliased target.
    #[must_use]
    pub fn safe_root(&self) -> SafeRoot {
        self.lease.safe_root()
    }

    /// The platform-tagged OS object identity encoding the journal records.
    #[must_use]
    pub fn encode_object_identity(identity: StableObjectId) -> String {
        encode_object_identity(identity)
    }

    /// Load one journaled transaction as inert data.
    ///
    /// # Errors
    /// Returns [`OperatorError::NotFound`] when the transaction is not journaled
    /// and [`OperatorError::Store`] when the durable read fails.
    pub fn load(&self, transaction_id: &str) -> Result<RepairTarget, OperatorError> {
        let record = self.store.load(transaction_id).map_err(|error| {
            if error.error_kind() == godot_mcp_domain::ErrorKind::NotFound {
                OperatorError::NotFound(transaction_id.to_owned())
            } else {
                OperatorError::Store(error.to_string())
            }
        })?;
        Ok(RepairTarget {
            transaction_id: record.transaction_id.clone(),
            effect_kind: ProofKind::from_journal(record.effect_kind),
            phase: record.phase.as_str().to_owned(),
            quarantined: record.phase == Phase::Quarantined,
            ambiguous_survivor: record.effect_kind == EffectKind::ProcessLaunch
                && matches!(
                    record.phase,
                    Phase::EffectStartedFsynced | Phase::CommittedFsynced
                ),
            project_object_identity: record.project_object_identity.clone(),
            project_generation: record.project_generation,
            touched_paths: record.touched_paths.clone(),
            touched_object_identities: record
                .touched_object_identities
                .iter()
                .map(|touched| (touched.path.clone(), touched.object_identity.clone()))
                .collect(),
            pre_hashes: record
                .pre_hashes
                .iter()
                .map(FileState::from_journal)
                .collect(),
            intended_post_hashes: record
                .intended_post_hashes
                .iter()
                .map(FileState::from_journal)
                .collect(),
            recorded_proof: record.proof.as_ref().map(ObservedProof::from_journal),
            required_process_transaction: record.required_process_transaction().map(str::to_owned),
        })
    }

    /// The current durable phase of one transaction, canonically spelled.
    ///
    /// # Errors
    /// Returns [`OperatorError::NotFound`] or [`OperatorError::Store`].
    pub fn phase_of(&self, transaction_id: &str) -> Result<String, OperatorError> {
        self.load(transaction_id).map(|target| target.phase)
    }

    /// Request a `RepairPermit` and apply exactly one proof-backed transition.
    ///
    /// `prelude` runs **after** the permit is issued (and therefore after its
    /// audit record is written) and **before** any transition is journaled. It is
    /// how `terminate` kills its proven still-contained survivor: a failure there
    /// aborts the repair with nothing journaled.
    ///
    /// # Errors
    /// Returns [`OperatorError::Refused`] when the authority or the gateway
    /// refuses, [`OperatorError::WrongPhase`] when the transaction may not be
    /// repaired, and [`OperatorError::Store`] on a durable failure. In every error
    /// case no repair record is written and the transaction's phase is unchanged.
    pub fn apply(
        &self,
        transaction_id: &str,
        intent: RepairIntent,
        derived: Option<DerivedProof>,
        prelude: Option<&dyn Fn() -> Result<(), String>>,
    ) -> Result<RepairOutcome, OperatorError> {
        let target = self.load(transaction_id)?;
        let authority = OperatorRecoveryAuthority::new(
            Arc::clone(&self.store),
            LocalLeaseEvidence::new(self.held.object_identity.clone(), self.held.generation),
        );
        let permit = if target.quarantined {
            authority.issue(transaction_id, operation_of(intent))
        } else if target.ambiguous_survivor && intent == RepairIntent::AcknowledgeSafeState {
            authority.issue_ambiguous_survivor(transaction_id)
        } else {
            return Err(OperatorError::WrongPhase {
                transaction_id: transaction_id.to_owned(),
                phase: target.phase,
            });
        }
        .map_err(|error| OperatorError::Refused(error.to_string()))?;

        if let Some(prelude) = prelude {
            prelude().map_err(OperatorError::Refused)?;
        }

        let derived = derived.map(|derived| DerivedTerminal {
            disposition: derived.disposition.to_gateway(),
            proof: derived.observed.to_journal(),
        });
        let outcome = self
            .gateway
            .apply_operator_repair(&permit, derived)
            .map_err(|error| OperatorError::Refused(error.to_string()))?;
        Ok(RepairOutcome {
            transaction_id: outcome.transaction_id,
            operation: intent.as_str(),
            phase: outcome.phase.as_str().to_owned(),
            reason: outcome.reason,
        })
    }
}

const fn operation_of(intent: RepairIntent) -> RepairOperation {
    match intent {
        RepairIntent::Reconcile => RepairOperation::Reconcile,
        RepairIntent::Compensate => RepairOperation::Compensate,
        RepairIntent::Terminate => RepairOperation::Terminate,
        RepairIntent::AcknowledgeSafeState => RepairOperation::AcknowledgeSafeState,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every published spelling must equal the canonical schema spelling, or a
    /// proof file and a journal record would disagree about the same fact.
    #[test]
    fn every_published_spelling_is_canonical() {
        assert_eq!(ProofKind::File.as_str(), "file");
        assert_eq!(ProofKind::ProcessLaunch.as_str(), "process_launch");
        assert_eq!(ProofKind::EditorRuntime.as_str(), "editor_runtime");
        assert_eq!(RecordedExistence::Present.as_str(), "present");
        assert_eq!(RecordedExistence::Absent.as_str(), "absent");
        assert_eq!(ProvenDisposition::PreState.as_str(), "pre-state");
        assert_eq!(ProvenDisposition::PostState.as_str(), "post-state");
        for release in [
            ObservedRelease::Suspended,
            ObservedRelease::ResumedContained,
            ObservedRelease::ExitedClean,
            ObservedRelease::Signaled,
            ObservedRelease::NonzeroExit,
            ObservedRelease::Unknown,
        ] {
            assert_eq!(ObservedRelease::parse(release.as_str()), Some(release));
        }
        assert_eq!(ObservedRelease::parse("resumed"), None);
    }

    /// The terminal-completion rule is Todo 6's, so the published wrapper must
    /// agree with it on every field that can refuse.
    #[test]
    fn only_a_contained_clean_exited_empty_process_is_terminal() {
        let base = ObservedProof::ProcessLaunch {
            boundary_identity: "unix_locator:/tmp/boundary".to_owned(),
            pid: Some(4242),
            start_time: Some("12345".to_owned()),
            boundary_membership_confirmed: true,
            release_status: ObservedRelease::ExitedClean,
            exit_code: Some(0),
            signal: None,
            boundary_empty_after_exit: Some(true),
        };
        assert!(base.is_clean_terminal_process());

        let mutate = |change: &dyn Fn(&mut ObservedProof)| {
            let mut proof = base.clone();
            change(&mut proof);
            proof
        };
        let refusals: Vec<ObservedProof> = vec![
            mutate(&|proof| {
                if let ObservedProof::ProcessLaunch {
                    boundary_membership_confirmed,
                    ..
                } = proof
                {
                    *boundary_membership_confirmed = false;
                }
            }),
            mutate(&|proof| {
                if let ObservedProof::ProcessLaunch { release_status, .. } = proof {
                    *release_status = ObservedRelease::ResumedContained;
                }
            }),
            mutate(&|proof| {
                if let ObservedProof::ProcessLaunch { exit_code, .. } = proof {
                    *exit_code = Some(7);
                }
            }),
            mutate(&|proof| {
                if let ObservedProof::ProcessLaunch { signal, .. } = proof {
                    *signal = Some("SIGKILL".to_owned());
                }
            }),
            mutate(&|proof| {
                if let ObservedProof::ProcessLaunch {
                    boundary_empty_after_exit,
                    ..
                } = proof
                {
                    *boundary_empty_after_exit = Some(false);
                }
            }),
            mutate(&|proof| {
                if let ObservedProof::ProcessLaunch {
                    boundary_empty_after_exit,
                    ..
                } = proof
                {
                    *boundary_empty_after_exit = None;
                }
            }),
        ];
        for refusal in refusals {
            assert!(
                !refusal.is_clean_terminal_process(),
                "{refusal:?} must not be accepted as a terminal helper run"
            );
        }
        assert!(
            !ObservedProof::File {
                entries: Vec::new()
            }
            .is_clean_terminal_process(),
            "a file proof is never a terminal process proof"
        );
    }

    /// Every intent maps to exactly one Todo 6 operation with the same spelling.
    #[test]
    fn every_intent_maps_to_one_operation() {
        for intent in RepairIntent::ALL {
            assert_eq!(intent.as_str(), operation_of(intent).as_str());
        }
    }
}
