//! The Task-34 quarantined-fixture journal port.
//!
//! Todo 34's fixture-control binary (`qa-fixture-34`, in `godot-mcp-cli`) is the
//! sole owner of `tests/fixtures/quarantined`. It has to write quarantined
//! transaction records for all three effect kinds — including a `process_launch`
//! record left in a signaled/nonzero/live disposition and an `editor_runtime`
//! record — and Todo 6's store, phase machine and canonical schema are
//! crate-private on purpose.
//!
//! This module is that one narrow, `test-hooks`-gated port. It deliberately does
//! **not** introduce a second journal, a second schema, or a second phase
//! machine: every record it writes goes through
//! [`TransactionStore::open_transaction`] and [`TransactionStore::advance`], is
//! validated against `schemas/transactions/v1.json` on the way in, and reaches
//! `quarantined` only over a transition the phase machine already allows
//! (`intent_fsynced → quarantined`).
//!
//! The types below are plain data. No permit, no store handle and no record type
//! escapes this crate, so a caller can describe a fixture record but can never
//! smuggle journal authority anywhere else.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use godot_mcp_domain::contracts::ProjectIdentity;
use godot_mcp_domain::lease::{Lease, LeaseManager};
use godot_mcp_domain::sysfs::{identity_of_file, open_child_regular_file, walk_dir_no_symlinks};

use super::journal_harness::HarnessError;
use crate::journal::{
    BoundaryMembership, EffectBoundaryKind, EffectKind, FileOperation, FileStateEntry,
    OperationFamily, Phase, ReleaseStatus, RequiredSubproofs, TouchedObject, TransactionRecord,
    TransactionStore, TransportMode, TypedProof, canonical_request, encode_object_identity,
};
use crate::stdio::{INITIAL_PROJECT_GENERATION, state_dir_for};

/// The fixed wall-clock stamp every Task-34 fixture record carries.
///
/// The fixture must be byte-deterministic across runs, so the record's
/// `opened_at_unix_ms` cannot be the real clock.
pub const FIXTURE_OPENED_AT_UNIX_MS: u64 = 1_700_000_000_000;

/// The held project identity every fixture record binds to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task34Held {
    /// Public project object identity (`project_<64 hex>`).
    pub object_identity: String,
    /// Platform-tagged OS object identity of the project root directory.
    pub root_object: String,
    /// Platform-tagged OS object identity of `project.godot`.
    pub file_object: String,
    /// Project generation the lease currently designates.
    pub generation: u64,
}

/// Which typed proof form a fixture record carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Task34EffectKind {
    /// Existence + content-hash proof.
    File,
    /// Containment/PID/start-time/release proof.
    ProcessLaunch,
    /// Bridge receipt plus live probe.
    EditorRuntime,
}

impl Task34EffectKind {
    const fn to_journal(self) -> EffectKind {
        match self {
            Self::File => EffectKind::File,
            Self::ProcessLaunch => EffectKind::ProcessLaunch,
            Self::EditorRuntime => EffectKind::EditorRuntime,
        }
    }

    const fn boundary(self) -> EffectBoundaryKind {
        match self {
            Self::File => EffectBoundaryKind::File,
            Self::ProcessLaunch => EffectBoundaryKind::Process,
            Self::EditorRuntime => EffectBoundaryKind::Editor,
        }
    }

    const fn operation_family(self) -> OperationFamily {
        match self {
            Self::File => OperationFamily::ProjectMutation,
            Self::ProcessLaunch => OperationFamily::RuntimeExecution,
            Self::EditorRuntime => OperationFamily::EditorMutation,
        }
    }
}

/// Per-path mutation shape, as the fixture names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Task34FileOperation {
    Create,
    Update,
    Delete,
}

impl Task34FileOperation {
    const fn to_journal(self) -> FileOperation {
        match self {
            Self::Create => FileOperation::Create,
            Self::Update => FileOperation::Update,
            Self::Delete => FileOperation::Delete,
        }
    }
}

/// One path's existence + content-hash state.
///
/// `sha256` must be `Some` exactly when the path is present; an absent path is
/// proven by the existence tombstone alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task34FileState {
    /// Project-relative path.
    pub path: String,
    /// Declared mutation shape.
    pub operation: Task34FileOperation,
    /// Lowercase hex SHA-256 of the observed bytes, or `None` when absent.
    pub sha256: Option<String>,
}

impl Task34FileState {
    fn to_journal(&self) -> FileStateEntry {
        FileStateEntry {
            path: self.path.clone(),
            operation: self.operation.to_journal(),
            existence: if self.sha256.is_some() {
                crate::journal::record::Existence::Present
            } else {
                crate::journal::record::Existence::Absent
            },
            sha256: self.sha256.clone(),
        }
    }
}

/// Release/terminal disposition of a supervised process, as the fixture names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Task34Release {
    Suspended,
    ResumedContained,
    ExitedClean,
    Signaled,
    NonzeroExit,
    Unknown,
}

impl Task34Release {
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

/// The typed proof a fixture record carries, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Task34Proof {
    /// No observed proof was recorded. This is what "the linked process proof is
    /// deliberately absent/unrecorded" means durably.
    Absent,
    /// Existence + content-hash proof for every touched path.
    File {
        /// One entry per touched path.
        entries: Vec<Task34FileState>,
    },
    /// Containment/PID/start-time/release proof.
    ProcessLaunch {
        /// The boundary's own concrete identity.
        boundary_identity: String,
        /// Live-observed PID, or `None` when the process is gone.
        pid: Option<u32>,
        /// Live-observed platform-native start time.
        start_time: Option<String>,
        /// Whether membership in the boundary was independently confirmed.
        membership_confirmed: bool,
        /// Observed release/terminal disposition.
        release_status: Task34Release,
        /// Observed exit code, when the process exited normally.
        exit_code: Option<i64>,
        /// Observed terminating signal, when the process was signaled.
        signal: Option<String>,
        /// Whether the boundary was proven empty after exit.
        boundary_empty_after_exit: Option<bool>,
    },
    /// Bridge receipt plus, where reachable, a live state probe.
    EditorRuntime {
        /// Whether the bridge answered at all.
        bridge_reachable: bool,
        /// The transaction the receipt is bound to.
        receipt_transaction_id: Option<String>,
        /// The plugin-acknowledged action/operation identifier.
        receipt_action_id: Option<String>,
        /// A live state-probe result through the bridge.
        live_state_probe: Option<String>,
    },
}

impl Task34Proof {
    fn to_journal(&self) -> Option<TypedProof> {
        match self {
            Self::Absent => None,
            Self::File { entries } => Some(TypedProof::File {
                entries: entries.iter().map(Task34FileState::to_journal).collect(),
            }),
            Self::ProcessLaunch {
                boundary_identity,
                pid,
                start_time,
                membership_confirmed,
                release_status,
                exit_code,
                signal,
                boundary_empty_after_exit,
            } => Some(TypedProof::ProcessLaunch {
                boundary_identity: boundary_identity.clone(),
                pid: *pid,
                start_time: start_time.clone(),
                boundary_membership: if *membership_confirmed {
                    BoundaryMembership::Confirmed
                } else {
                    BoundaryMembership::Unconfirmed
                },
                release_status: release_status.to_journal(),
                exit_code: *exit_code,
                signal: signal.clone(),
                boundary_empty_after_exit: *boundary_empty_after_exit,
            }),
            Self::EditorRuntime {
                bridge_reachable,
                receipt_transaction_id,
                receipt_action_id,
                live_state_probe,
            } => Some(TypedProof::EditorRuntime {
                bridge_reachable: *bridge_reachable,
                receipt_transaction_id: receipt_transaction_id.clone(),
                receipt_action_id: receipt_action_id.clone(),
                live_state_probe: live_state_probe.clone(),
            }),
        }
    }
}

/// One quarantined fixture record, described as plain data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task34RecordSpec {
    /// The record's fixed transaction identifier.
    pub transaction_id: String,
    /// Which typed proof form this record requires.
    pub effect_kind: Task34EffectKind,
    /// Tool name recorded for provenance.
    pub tool_name: String,
    /// Project-relative touched paths, in the order the record declares them.
    pub touched_paths: Vec<String>,
    /// The recorded pre-state for every touched path.
    pub pre_hashes: Vec<Task34FileState>,
    /// The recorded intended post-state for every touched path.
    pub intended_post_hashes: Vec<Task34FileState>,
    /// The observed typed proof, when one was recorded.
    pub proof: Task34Proof,
    /// Todo 17's composite requirement: the linked `process_launch` transaction.
    pub required_process_transaction: Option<String>,
    /// Why this record is quarantined.
    pub terminal_reason: String,
}

/// A held Task-34 fixture project plus its durable journal.
///
/// Holding this value holds the Todo 2 advisory OS lease on the project, so two
/// concurrent fixture writers cannot interleave.
pub struct Task34FixtureJournal {
    /// The held lease. Its presence is what keeps single-writer ownership alive;
    /// dropping this value releases the advisory OS lock.
    _lease: Lease,
    store: Arc<TransactionStore>,
    held: Task34Held,
    state_dir: PathBuf,
    root: PathBuf,
}

impl Task34FixtureJournal {
    /// Acquire the project lease and open (creating if absent) its journal.
    ///
    /// # Errors
    /// Returns [`HarnessError::Lease`] when the project cannot be owned and
    /// [`HarnessError::Store`] when the durable journal cannot be opened.
    pub fn open(project_root: &Path) -> Result<Self, HarnessError> {
        let state_dir = state_dir_for(project_root);
        let lease = LeaseManager::new(&state_dir)
            .acquire(project_root)
            .map_err(|error| HarnessError::Lease(error.to_string()))?;
        let key = lease.lease_key();
        let project = ProjectIdentity::from_lease_key(key, INITIAL_PROJECT_GENERATION);
        let held = Task34Held {
            object_identity: project.object_id().to_string(),
            root_object: encode_object_identity(key.root),
            file_object: encode_object_identity(key.project_file),
            generation: INITIAL_PROJECT_GENERATION,
        };
        let store = Arc::new(
            TransactionStore::open(&state_dir.join("journal"))
                .map_err(|error| HarnessError::Store(error.to_string()))?,
        );
        Ok(Self {
            _lease: lease,
            store,
            held,
            state_dir,
            root: project_root.to_path_buf(),
        })
    }

    /// The held project identity every fixture record binds to.
    #[must_use]
    pub const fn held(&self) -> &Task34Held {
        &self.held
    }

    /// The private state directory holding the lease lock and the journal.
    #[must_use]
    pub fn state_directory(&self) -> &Path {
        &self.state_dir
    }

    /// The directory holding the durable journal files.
    #[must_use]
    pub fn journal_directory(&self) -> PathBuf {
        self.store.directory().to_path_buf()
    }

    /// The project root this fixture holds.
    #[must_use]
    pub fn project_root(&self) -> &Path {
        &self.root
    }

    /// The platform-tagged OS object identity of one project-relative path, or
    /// `None` when the path is absent or is not a regular file.
    ///
    /// The walk refuses to follow a symlink at any component, so the identity is
    /// the one the held root designates and never an aliased target's.
    #[must_use]
    pub fn touched_object_identity(&self, relative: &str) -> Option<String> {
        let full = self.project_root().join(relative);
        let parent = full.parent()?;
        let name = full.file_name()?;
        let walked = walk_dir_no_symlinks(parent).ok()?;
        let file = open_child_regular_file(walked.dir(), OsStr::new(name)).ok()?;
        identity_of_file(&file).ok().map(encode_object_identity)
    }

    /// Write one quarantined fixture record.
    ///
    /// The record is journaled as `intent_fsynced` and then advanced to
    /// `quarantined` over the phase machine's own legal transition, attaching the
    /// typed proof (when one was recorded) and the terminal reason.
    ///
    /// # Errors
    /// Returns [`HarnessError::Store`] when the record violates the canonical
    /// schema, duplicates an existing transaction, or the durable write fails.
    pub fn write_quarantined(&self, spec: &Task34RecordSpec) -> Result<(), HarnessError> {
        let (hash, bytes) = canonical_request(
            &spec.tool_name,
            &serde_json::json!({
                "paths": spec.touched_paths,
                "transaction": spec.transaction_id,
            }),
        );
        let record = TransactionRecord {
            schema_version: String::new(),
            transaction_id: spec.transaction_id.clone(),
            effect_kind: spec.effect_kind.to_journal(),
            effect_boundary: spec.effect_kind.boundary(),
            project_object_identity: self.held.object_identity.clone(),
            project_root_object: self.held.root_object.clone(),
            project_file_object: self.held.file_object.clone(),
            project_generation: self.held.generation,
            transport_mode: TransportMode::Stdio,
            principal: None,
            session: None,
            operation_family: spec.effect_kind.operation_family(),
            tool_name: spec.tool_name.clone(),
            idempotency_id: format!("task34-{}", spec.transaction_id),
            canonical_request_hash: hash,
            canonical_request_bytes: bytes,
            touched_paths: spec.touched_paths.clone(),
            touched_object_identities: spec
                .touched_paths
                .iter()
                .map(|path| TouchedObject {
                    path: path.clone(),
                    object_identity: self.touched_object_identity(path),
                })
                .collect(),
            pre_hashes: spec
                .pre_hashes
                .iter()
                .map(Task34FileState::to_journal)
                .collect(),
            staged_write_set: Vec::new(),
            intended_post_hashes: spec
                .intended_post_hashes
                .iter()
                .map(Task34FileState::to_journal)
                .collect(),
            phase: Phase::IntentFsynced,
            phase_history: Vec::new(),
            proof: None,
            required_subproofs: spec.required_process_transaction.as_ref().map(|linked| {
                RequiredSubproofs {
                    process_launch_transaction_id: Some(linked.clone()),
                }
            }),
            terminal_reason: None,
            terminal_outcome: None,
            opened_at_unix_ms: FIXTURE_OPENED_AT_UNIX_MS,
        };
        self.store
            .open_transaction(record)
            .map_err(|error| HarnessError::Store(error.to_string()))?;
        self.store
            .advance(
                &spec.transaction_id,
                Phase::Quarantined,
                spec.proof.to_journal(),
                Some(spec.terminal_reason.clone()),
                Some(crate::journal::TerminalOutcome::failed(
                    godot_mcp_domain::ErrorKind::Uncertain,
                )),
            )
            .map(|_record| ())
            .map_err(|error| HarnessError::Store(error.to_string()))
    }

    /// The stored phase of one transaction, in canonical spelling.
    ///
    /// # Errors
    /// Returns [`HarnessError::Store`] when the transaction is not journaled.
    pub fn phase_of(&self, transaction_id: &str) -> Result<String, HarnessError> {
        self.store
            .load(transaction_id)
            .map(|record| record.phase.as_str().to_owned())
            .map_err(|error| HarnessError::Store(error.to_string()))
    }

    /// Every quarantined transaction identifier, ascending.
    ///
    /// # Errors
    /// Returns [`HarnessError::Store`] when the durable read fails.
    pub fn quarantined(&self) -> Result<Vec<String>, HarnessError> {
        self.store
            .quarantined_transactions()
            .map_err(|error| HarnessError::Store(error.to_string()))
    }
}
