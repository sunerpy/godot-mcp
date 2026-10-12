//! The crate-private effect gateway.
//!
//! Every effect is a four-step durable sequence:
//! `intent_fsynced → staged_fsynced → effect_started_fsynced → terminal`. The
//! gateway is the only path through those steps, and it validates the permit,
//! re-checks quarantine, requires the transaction to be journaled, and derives
//! the terminal record from an independently re-derived typed proof.

use std::collections::BTreeMap;
use std::sync::Arc;

use godot_mcp_bridge::protocol::mutation::MutationStatus;
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{Capability, OutcomeError};
use godot_mcp_engine::supervision::protocol::TerminationKind;

use super::boundary::{
    BoundaryUnavailable, EditorEffectBoundary, EditorEffectOutcome, EditorMutation, EffectTicket,
    FileEffectBoundary, FileWrite, LspDapEffectBoundary, LspDapExchange, ProcessEffectBoundary,
    ProcessLaunchRequest, RuntimeEffectBoundary, RuntimeOperation,
};
use super::permit::{EffectPermit, PermitError, RepairOperation, RepairPermit};
use crate::journal::{
    Conclusion, EffectBoundaryKind, EffectKind, FileOperation, FileStateEntry, HeldProject,
    OperationFamily, Phase, ProofOutcome, ProofSource, RequiredSubproofs, StagedWrite, StoreError,
    TerminalOutcome, TouchedObject, TransactionRecord, TransactionStore, TransportMode, TypedProof,
    canonical_request, hex_sha256, intended_post_entry, pre_state_entry, verify_file_proof,
};
use crate::scheduler::OperationDeadline;

/// A complete, validated request to open one `file`-kind transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileEffectIntent {
    /// Fixed transaction ID (canonical lowercase dashed hexadecimal).
    pub(crate) transaction_id: String,
    /// Canonical tool name.
    pub(crate) tool_name: String,
    /// Operation family for policy and audit.
    pub(crate) operation_family: OperationFamily,
    /// Caller-supplied idempotency identifier.
    pub(crate) idempotency_id: String,
    /// Transport/auth mode that established the identity.
    pub(crate) transport_mode: TransportMode,
    /// Mode-specific principal.
    pub(crate) principal: Option<String>,
    /// Mode-specific session.
    pub(crate) session: Option<String>,
    /// Complete request arguments; hashed canonically, never truncated.
    pub(crate) arguments: serde_json::Value,
    /// Planned writes, in declaration order.
    pub(crate) writes: Vec<PlannedWrite>,
    /// Optional composite requirement naming a linked `process_launch`
    /// transaction (Todo 17's helper-subprocess composite).
    pub(crate) required_process_transaction: Option<String>,
    /// Trusted observation timestamp.
    pub(crate) opened_at_unix_ms: u64,
    /// The workspace revision the request was built against, when it named
    /// one. The router refuses the effect, inside the serialized effect lane,
    /// unless the workspace is still at exactly this revision.
    pub(crate) expected_revision: Option<u64>,
}

/// One planned file mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedWrite {
    pub(crate) path: String,
    pub(crate) operation: FileOperation,
    /// Bytes to publish; `None` for a delete.
    pub(crate) bytes: Option<Vec<u8>>,
    /// What the caller proved the path holds before the transaction opens.
    pub(crate) precondition: WritePrecondition,
}

/// The state a planned write requires its path to be in when the transaction
/// opens. A mismatch refuses the request before anything is journaled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WritePrecondition {
    /// Whatever the path holds when the transaction opens becomes the recorded
    /// pre-state.
    Observed,
    /// The path must be absent.
    Absent,
    /// The path must hold exactly the bytes with this lowercase hex SHA-256.
    Sha256(String),
}

impl WritePrecondition {
    /// Whether the observed pre-state `entry` satisfies this precondition.
    fn holds(&self, entry: &FileStateEntry) -> bool {
        match self {
            Self::Observed => true,
            Self::Absent => entry.sha256.is_none(),
            Self::Sha256(expected) => entry.sha256.as_deref() == Some(expected.as_str()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessEffectIntent {
    pub(crate) transaction_id: String,
    pub(crate) tool_name: String,
    pub(crate) operation_family: OperationFamily,
    pub(crate) idempotency_id: String,
    pub(crate) transport_mode: TransportMode,
    pub(crate) principal: Option<String>,
    pub(crate) session: Option<String>,
    pub(crate) arguments: serde_json::Value,
    pub(crate) required_process_transaction: String,
    pub(crate) opened_at_unix_ms: u64,
}

/// One journaled live-editor mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditorEffectIntent {
    pub(crate) transaction_id: String,
    pub(crate) tool_name: String,
    pub(crate) operation_family: OperationFamily,
    pub(crate) idempotency_id: String,
    pub(crate) transport_mode: TransportMode,
    pub(crate) principal: Option<String>,
    pub(crate) session: Option<String>,
    pub(crate) arguments: serde_json::Value,
    /// Project files the editor may write for this mutation (a saved scene),
    /// each with the content precondition it must hold when the transaction
    /// opens. Empty for a change to editor state only.
    pub(crate) files: Vec<(String, WritePrecondition)>,
    pub(crate) opened_at_unix_ms: u64,
}

/// The terminal disposition of one executed effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EffectOutcome {
    pub(crate) transaction_id: String,
    pub(crate) phase: Phase,
    pub(crate) conclusion: Conclusion,
    pub(crate) reason: String,
}

/// Why an effect was refused or could not be proven.
#[derive(Debug, thiserror::Error)]
pub(crate) enum EffectError {
    /// The permit was not issued for this project/generation/transaction, or its
    /// authority epoch is stale.
    #[error("effect permit is missing or stale: {0}")]
    StalePermit(String),
    /// The permit does not authorize this boundary/capability/family.
    #[error("effect permit does not authorize this operation: {0}")]
    Unauthorized(String),
    /// A quarantined transaction blocks the write at execution time.
    #[error("project has quarantined transactions ({0:?}); the write is refused")]
    Quarantined(Vec<String>),
    /// The transaction is not in the required phase.
    #[error("transaction {transaction_id} is in phase {phase}, which cannot start an effect")]
    WrongPhase {
        transaction_id: String,
        phase: Phase,
    },
    /// A permit could not be issued.
    #[error(transparent)]
    Permit(#[from] PermitError),
    /// The durable store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A file effect could not be published.
    #[error("file effect failed: {0}")]
    Boundary(String),
    /// A planned path does not hold the state the request was built against.
    #[error("{path} no longer matches the request's precondition ({observed})")]
    Precondition { path: String, observed: String },
    #[error(transparent)]
    OperationDeadline(OutcomeError),
    /// A managed session did not start; whatever was launched was terminated.
    #[error(transparent)]
    Session(OutcomeError),
}

impl EffectError {
    /// Map to the stable domain error taxonomy.
    pub(crate) fn to_outcome(&self) -> OutcomeError {
        let kind = match self {
            Self::StalePermit(_) | Self::Unauthorized(_) => ErrorKind::PermissionDenied,
            Self::Quarantined(_) | Self::WrongPhase { .. } | Self::Precondition { .. } => {
                ErrorKind::Conflict
            }
            Self::Permit(PermitError::Store(error)) | Self::Store(error) => error.error_kind(),
            Self::Permit(_) => ErrorKind::PermissionDenied,
            Self::Boundary(_) => ErrorKind::Unavailable,
            Self::OperationDeadline(error) | Self::Session(error) => return error.clone(),
        };
        OutcomeError::new(kind, self.to_string(), false, BTreeMap::new())
    }
}

/// The five effect boundaries, supplied together so the gateway constructor
/// cannot be called with a boundary silently omitted.
pub(crate) struct EffectBoundaries {
    pub(crate) file: Box<dyn FileEffectBoundary>,
    pub(crate) process: Box<dyn ProcessEffectBoundary>,
    pub(crate) lsp_dap: Box<dyn LspDapEffectBoundary>,
    pub(crate) editor: Box<dyn EditorEffectBoundary>,
    pub(crate) runtime: Box<dyn RuntimeEffectBoundary>,
}

/// The crate-private effect gateway.
pub(crate) struct EffectGateway {
    store: Arc<TransactionStore>,
    held: HeldProject,
    epoch: u64,
    proof_source: Box<dyn ProofSource>,
    file: Box<dyn FileEffectBoundary>,
    process: Box<dyn ProcessEffectBoundary>,
    lsp_dap: Box<dyn LspDapEffectBoundary>,
    editor: Box<dyn EditorEffectBoundary>,
    runtime: Box<dyn RuntimeEffectBoundary>,
}

impl EffectGateway {
    /// Assemble the gateway over one held project and its five boundaries.
    pub(crate) fn new(
        store: Arc<TransactionStore>,
        held: HeldProject,
        epoch: u64,
        proof_source: Box<dyn ProofSource>,
        boundaries: EffectBoundaries,
    ) -> Self {
        Self {
            store,
            held,
            epoch,
            proof_source,
            file: boundaries.file,
            process: boundaries.process,
            lsp_dap: boundaries.lsp_dap,
            editor: boundaries.editor,
            runtime: boundaries.runtime,
        }
    }

    /// Transactions currently quarantined. While this is non-empty every
    /// ordinary permit is refused and the effect lane blocks writes.
    pub(crate) fn quarantined_transactions(&self) -> Result<Vec<String>, EffectError> {
        Ok(self.store.quarantined_transactions()?)
    }

    /// Which boundaries have a real implementation compiled in. Exposed so
    /// health reporting never claims a fail-closed boundary is ready.
    pub(crate) fn boundary_availability(&self) -> [(EffectBoundaryKind, bool); 5] {
        [
            (EffectBoundaryKind::File, self.file.available()),
            (EffectBoundaryKind::Process, self.process.available()),
            (EffectBoundaryKind::LspDap, self.lsp_dap.available()),
            (EffectBoundaryKind::Editor, self.editor.available()),
            (EffectBoundaryKind::Runtime, self.runtime.available()),
        ]
    }

    /// Why the process boundary cannot launch, when it cannot.
    pub(crate) fn process_unavailable_reason(&self) -> Option<String> {
        if self.process.available() {
            None
        } else {
            self.process.unavailable_reason()
        }
    }

    /// Which boundaries can serve right now: compiled in and, for the live
    /// editor, connected. Policy and admission are keyed to what is compiled
    /// in; a call is refused up front only when its boundary cannot serve now.
    /// The runtime boundary can serve while this server knows a game session,
    /// and the LSP/DAP boundary while it knows an editor session, running or
    /// ended; whether that session can take a command is its own answer.
    pub(crate) fn live_boundary_availability(&self) -> [(EffectBoundaryKind, bool); 5] {
        let mut availability = self.boundary_availability();
        for (kind, available) in &mut availability {
            if *kind == EffectBoundaryKind::Editor {
                *available = *available && self.editor.connected();
            }
            if *kind == EffectBoundaryKind::Runtime {
                *available = *available && self.runtime.connected();
            }
            if *kind == EffectBoundaryKind::LspDap {
                *available = *available && self.lsp_dap.connected();
            }
        }
        availability
    }

    /// What each compiled-in boundary has to serve right now, for health:
    /// a connected editor plugin, a running game session, a running editor
    /// session. The file and process boundaries need nothing beyond being
    /// compiled in.
    pub(crate) fn present_boundaries(&self) -> [(EffectBoundaryKind, bool); 5] {
        [
            (EffectBoundaryKind::File, self.file.available()),
            (EffectBoundaryKind::Process, self.process.available()),
            (
                EffectBoundaryKind::LspDap,
                self.lsp_dap.available() && self.lsp_dap.running(),
            ),
            (
                EffectBoundaryKind::Editor,
                self.editor.available() && self.editor.connected(),
            ),
            (
                EffectBoundaryKind::Runtime,
                self.runtime.available() && self.runtime.running(),
            ),
        ]
    }

    /// Attempt one non-`file` effect boundary under a validated permit.
    ///
    /// Todos 13–18 supplied the process and LSP/DAP implementations that write to
    /// *this* journal. The editor and runtime stand-ins remain fail-closed because
    /// Todos 23 and 25 were cancelled. For an unavailable boundary this method
    /// proves refusal rather than silent success or a fabricated proof, while
    /// keeping the permit/ticket path exercised for all five boundaries.
    pub(crate) fn attempt_boundary_effect(
        &self,
        permit: &EffectPermit,
        boundary: EffectBoundaryKind,
    ) -> Result<TypedProof, EffectError> {
        self.validate_permit(permit, boundary)?;
        let record = self.store.load(permit.transaction_id())?;
        if record.phase != Phase::StagedFsynced {
            return Err(EffectError::WrongPhase {
                transaction_id: record.transaction_id,
                phase: record.phase,
            });
        }
        let ticket = EffectTicket::new(record.transaction_id.clone(), boundary, permit.seal());
        match boundary {
            EffectBoundaryKind::File => Err(EffectError::Unauthorized(
                "the file boundary is driven by execute_file_effect".to_owned(),
            )),
            EffectBoundaryKind::Process if !self.process.available() => Err(EffectError::Boundary(
                BoundaryUnavailable {
                    boundary: "process",
                }
                .to_string(),
            )),
            EffectBoundaryKind::Process if self.process.available() => Err(EffectError::Boundary(
                "the process boundary requires a resolved supervised launch request".to_owned(),
            )),
            EffectBoundaryKind::Process => Err(EffectError::Boundary(
                BoundaryUnavailable {
                    boundary: "process",
                }
                .to_string(),
            )),
            EffectBoundaryKind::LspDap => self
                .lsp_dap
                .exchange(
                    &ticket,
                    &LspDapExchange {
                        method: record.tool_name.clone(),
                        payload: String::new(),
                    },
                )
                .map(|_response| TypedProof::File {
                    entries: Vec::new(),
                })
                .map_err(|error| EffectError::Boundary(error.to_string())),
            EffectBoundaryKind::Editor => self
                .editor
                .mutate(
                    &ticket,
                    &EditorMutation {
                        action: record.tool_name.clone(),
                        payload: String::new(),
                        idempotency_id: record.idempotency_id.clone(),
                        timeout: std::time::Duration::from_secs(30),
                        cancellation: crate::effect::boundary::CancelSignal::default(),
                    },
                )
                .map_err(|error| EffectError::Boundary(error.to_string())),
            EffectBoundaryKind::Runtime => self
                .runtime
                .invoke(
                    &ticket,
                    &RuntimeOperation {
                        operation: record.tool_name.clone(),
                        payload: String::new(),
                    },
                )
                .map_err(|error| EffectError::Boundary(error.to_string())),
        }
    }

    /// Journal the complete transaction identity and intended write set
    /// (`intent_fsynced`).
    ///
    /// The identity is written to the *earliest* fsync anchor, before any staging
    /// or effect, so a crash can never leave an effect whose owning project,
    /// principal, request or touched paths are unknown.
    pub(crate) fn open_file_transaction(
        &self,
        intent: &FileEffectIntent,
    ) -> Result<TransactionRecord, EffectError> {
        let mut pre_hashes = Vec::with_capacity(intent.writes.len());
        let mut intended_post_hashes = Vec::with_capacity(intent.writes.len());
        let mut staged_write_set = Vec::new();
        let mut touched_paths = Vec::with_capacity(intent.writes.len());
        let mut touched_object_identities = Vec::with_capacity(intent.writes.len());
        for (index, planned) in intent.writes.iter().enumerate() {
            let pre = pre_state_entry(self.proof_source.as_ref(), &planned.path, planned.operation)
                .map_err(|error| EffectError::Boundary(error.to_string()))?;
            if !planned.precondition.holds(&pre) {
                return Err(EffectError::Precondition {
                    path: planned.path.clone(),
                    observed: match &pre.sha256 {
                        Some(sha256) => format!("it holds sha256 {sha256}"),
                        None => "it is absent".to_owned(),
                    },
                });
            }
            let object_identity = pre.sha256.as_ref().map(|_| ());
            touched_paths.push(planned.path.clone());
            touched_object_identities.push(TouchedObject {
                path: planned.path.clone(),
                // The path's OS object identity is only meaningful while it
                // exists; an absent create target has none.
                object_identity: object_identity.map(|()| self.held.root_object.clone()),
            });
            intended_post_hashes.push(intended_post_entry(
                &planned.path,
                planned.operation,
                planned.bytes.as_deref(),
            ));
            if let Some(bytes) = &planned.bytes {
                staged_write_set.push(StagedWrite {
                    path: planned.path.clone(),
                    staging_id: format!("{:016x}", staging_seed(&intent.transaction_id, index)),
                    sha256: hex_sha256(bytes),
                    byte_length: bytes.len() as u64,
                });
            }
            pre_hashes.push(pre);
        }

        let (canonical_request_hash, canonical_request_bytes) =
            canonical_request(&intent.tool_name, &intent.arguments);
        let record = TransactionRecord {
            schema_version: String::new(),
            transaction_id: intent.transaction_id.clone(),
            effect_kind: EffectKind::File,
            effect_boundary: EffectBoundaryKind::File,
            project_object_identity: self.held.object_identity.clone(),
            project_root_object: self.held.root_object.clone(),
            project_file_object: self.held.file_object.clone(),
            project_generation: self.held.generation,
            transport_mode: intent.transport_mode,
            principal: intent.principal.clone(),
            session: intent.session.clone(),
            operation_family: intent.operation_family,
            tool_name: intent.tool_name.clone(),
            idempotency_id: intent.idempotency_id.clone(),
            canonical_request_hash,
            canonical_request_bytes,
            touched_paths,
            touched_object_identities,
            pre_hashes,
            staged_write_set,
            intended_post_hashes,
            phase: Phase::IntentFsynced,
            phase_history: Vec::new(),
            proof: None,
            required_subproofs: intent.required_process_transaction.as_ref().map(|id| {
                RequiredSubproofs {
                    process_launch_transaction_id: Some(id.clone()),
                }
            }),
            terminal_reason: None,
            terminal_outcome: None,
            opened_at_unix_ms: intent.opened_at_unix_ms,
        };
        Ok(self.store.open_transaction(record)?)
    }

    pub(crate) fn open_process_transaction(
        &self,
        intent: &ProcessEffectIntent,
    ) -> Result<TransactionRecord, EffectError> {
        let (canonical_request_hash, canonical_request_bytes) =
            canonical_request(&intent.tool_name, &intent.arguments);
        let record = TransactionRecord {
            schema_version: String::new(),
            transaction_id: intent.transaction_id.clone(),
            effect_kind: EffectKind::ProcessLaunch,
            effect_boundary: EffectBoundaryKind::Process,
            project_object_identity: self.held.object_identity.clone(),
            project_root_object: self.held.root_object.clone(),
            project_file_object: self.held.file_object.clone(),
            project_generation: self.held.generation,
            transport_mode: intent.transport_mode,
            principal: intent.principal.clone(),
            session: intent.session.clone(),
            operation_family: intent.operation_family,
            tool_name: intent.tool_name.clone(),
            idempotency_id: intent.idempotency_id.clone(),
            canonical_request_hash,
            canonical_request_bytes,
            touched_paths: Vec::new(),
            touched_object_identities: Vec::new(),
            pre_hashes: Vec::new(),
            staged_write_set: Vec::new(),
            intended_post_hashes: Vec::new(),
            phase: Phase::IntentFsynced,
            phase_history: Vec::new(),
            proof: None,
            required_subproofs: Some(RequiredSubproofs {
                process_launch_transaction_id: Some(intent.required_process_transaction.clone()),
            }),
            terminal_reason: None,
            terminal_outcome: None,
            opened_at_unix_ms: intent.opened_at_unix_ms,
        };
        self.store.open_transaction(record)?;
        Ok(self.store.advance(
            &intent.transaction_id,
            Phase::StagedFsynced,
            None,
            None,
            None,
        )?)
    }

    pub(crate) fn execute_process_effect(
        &self,
        permit: &EffectPermit,
        request: &ProcessLaunchRequest,
    ) -> Result<super::boundary::ProcessEffectOutcome, EffectError> {
        self.validate_permit(permit, EffectBoundaryKind::Process)?;
        let record = self.store.load(permit.transaction_id())?;
        if record.phase != Phase::StagedFsynced {
            return Err(EffectError::WrongPhase {
                transaction_id: record.transaction_id,
                phase: record.phase,
            });
        }
        let ticket = EffectTicket::new(
            record.transaction_id.clone(),
            EffectBoundaryKind::Process,
            permit.seal(),
        );
        self.store.advance(
            &record.transaction_id,
            Phase::EffectStartedFsynced,
            None,
            None,
            None,
        )?;
        match self.process.launch(&ticket, request) {
            Ok(outcome) => {
                let proof = outcome.proof.clone();
                if outcome.termination == TerminationKind::TimedOut {
                    let error = OperationDeadline::expired_error();
                    self.store.advance(
                        &record.transaction_id,
                        Phase::Compensated,
                        Some(proof),
                        Some(error.to_string()),
                        Some(TerminalOutcome::failed(ErrorKind::Timeout)),
                    )?;
                    return Err(EffectError::OperationDeadline(error));
                }
                self.store.advance(
                    &record.transaction_id,
                    Phase::CommittedFsynced,
                    Some(proof.clone()),
                    Some("supervised process launch completed with a durable proof".to_owned()),
                    Some(TerminalOutcome::Succeeded),
                )?;
                Ok(outcome)
            }
            Err(error) => {
                self.store.advance(
                    &record.transaction_id,
                    Phase::Compensated,
                    None,
                    Some(error.to_string()),
                    Some(TerminalOutcome::failed(ErrorKind::Unavailable)),
                )?;
                Err(EffectError::Boundary(error.to_string()))
            }
        }
    }

    /// Start one managed session under a validated permit. The transaction was
    /// opened by [`Self::open_process_transaction`] and is linked to the
    /// session's own supervised launch; it commits with the running launch's
    /// proof once the session is ready, and is compensated when the session
    /// did not start, whose launch has then been terminated.
    pub(crate) fn execute_session_start(
        &self,
        permit: &EffectPermit,
        request: &super::boundary::SessionStartRequest,
    ) -> Result<super::boundary::SessionStartOutcome, EffectError> {
        self.validate_permit(permit, EffectBoundaryKind::Process)?;
        let record = self.store.load(permit.transaction_id())?;
        if record.phase != Phase::StagedFsynced {
            return Err(EffectError::WrongPhase {
                transaction_id: record.transaction_id,
                phase: record.phase,
            });
        }
        let ticket = EffectTicket::new(
            record.transaction_id.clone(),
            EffectBoundaryKind::Process,
            permit.seal(),
        );
        self.store.advance(
            &record.transaction_id,
            Phase::EffectStartedFsynced,
            None,
            None,
            None,
        )?;
        match self.process.start_session(&ticket, request) {
            Ok(outcome) => {
                self.store.advance(
                    &record.transaction_id,
                    Phase::CommittedFsynced,
                    Some(outcome.proof.clone()),
                    Some("the managed session started and is ready".to_owned()),
                    Some(TerminalOutcome::Succeeded),
                )?;
                Ok(outcome)
            }
            Err(error) => {
                self.store.advance(
                    &record.transaction_id,
                    Phase::Compensated,
                    None,
                    Some(error.message().to_owned()),
                    Some(TerminalOutcome::failed(error.kind())),
                )?;
                Err(EffectError::Session(error))
            }
        }
    }

    /// Journal one live-editor mutation (`intent_fsynced`, then
    /// `staged_fsynced`: nothing is staged, the editor produces the bytes).
    ///
    /// Every file the editor may write is recorded with its pre-state, and its
    /// precondition is checked here, so a stale request refuses before the
    /// editor is asked to do anything.
    pub(crate) fn open_editor_transaction(
        &self,
        intent: &EditorEffectIntent,
    ) -> Result<TransactionRecord, EffectError> {
        self.open_bridge_transaction(intent, EffectBoundaryKind::Editor)
    }

    /// Journal one runtime command to a managed game session, exactly as a
    /// live-editor mutation is journaled; it touches no project file.
    pub(crate) fn open_runtime_transaction(
        &self,
        intent: &EditorEffectIntent,
    ) -> Result<TransactionRecord, EffectError> {
        self.open_bridge_transaction(intent, EffectBoundaryKind::Runtime)
    }

    /// Journal the stop of a managed session whose stop is a process effect,
    /// exactly as a live-editor mutation is journaled; it touches no project
    /// file.
    pub(crate) fn open_session_stop_transaction(
        &self,
        intent: &EditorEffectIntent,
    ) -> Result<TransactionRecord, EffectError> {
        self.open_bridge_transaction(intent, EffectBoundaryKind::Process)
    }

    /// Carry out one journaled session stop under a validated permit and
    /// settle it from the stop's three-state answer.
    pub(crate) fn execute_session_stop(
        &self,
        permit: &EffectPermit,
        command: &EditorMutation,
    ) -> Result<EditorEffectOutcome, EffectError> {
        self.execute_bridge_effect(permit, command, EffectBoundaryKind::Process)
    }

    /// Journal one LSP/DAP command to a managed editor session, exactly as a
    /// live-editor mutation is journaled; it touches no project file.
    pub(crate) fn open_debug_transaction(
        &self,
        intent: &EditorEffectIntent,
    ) -> Result<TransactionRecord, EffectError> {
        self.open_bridge_transaction(intent, EffectBoundaryKind::LspDap)
    }

    fn open_bridge_transaction(
        &self,
        intent: &EditorEffectIntent,
        boundary: EffectBoundaryKind,
    ) -> Result<TransactionRecord, EffectError> {
        let mut pre_hashes = Vec::with_capacity(intent.files.len());
        let mut touched_paths = Vec::with_capacity(intent.files.len());
        let mut touched_object_identities = Vec::with_capacity(intent.files.len());
        for (path, precondition) in &intent.files {
            let pre = pre_state_entry(self.proof_source.as_ref(), path, FileOperation::Update)
                .map_err(|error| EffectError::Boundary(error.to_string()))?;
            if !precondition.holds(&pre) {
                return Err(EffectError::Precondition {
                    path: path.clone(),
                    observed: match &pre.sha256 {
                        Some(sha256) => format!("it holds sha256 {sha256}"),
                        None => "it is absent".to_owned(),
                    },
                });
            }
            touched_paths.push(path.clone());
            touched_object_identities.push(TouchedObject {
                path: path.clone(),
                object_identity: pre.sha256.as_ref().map(|_| self.held.root_object.clone()),
            });
            pre_hashes.push(pre);
        }
        let (canonical_request_hash, canonical_request_bytes) =
            canonical_request(&intent.tool_name, &intent.arguments);
        let record = TransactionRecord {
            schema_version: String::new(),
            transaction_id: intent.transaction_id.clone(),
            effect_kind: EffectKind::EditorRuntime,
            effect_boundary: boundary,
            project_object_identity: self.held.object_identity.clone(),
            project_root_object: self.held.root_object.clone(),
            project_file_object: self.held.file_object.clone(),
            project_generation: self.held.generation,
            transport_mode: intent.transport_mode,
            principal: intent.principal.clone(),
            session: intent.session.clone(),
            operation_family: intent.operation_family,
            tool_name: intent.tool_name.clone(),
            idempotency_id: intent.idempotency_id.clone(),
            canonical_request_hash,
            canonical_request_bytes,
            touched_paths,
            touched_object_identities,
            pre_hashes,
            staged_write_set: Vec::new(),
            intended_post_hashes: Vec::new(),
            phase: Phase::IntentFsynced,
            phase_history: Vec::new(),
            proof: None,
            required_subproofs: None,
            terminal_reason: None,
            terminal_outcome: None,
            opened_at_unix_ms: intent.opened_at_unix_ms,
        };
        self.store.open_transaction(record)?;
        Ok(self.store.advance(
            &intent.transaction_id,
            Phase::StagedFsynced,
            None,
            None,
            None,
        )?)
    }

    /// Send one journaled live-editor mutation under a validated permit and
    /// settle the transaction from the editor's three-state answer:
    ///
    /// * `committed` — `committed_fsynced`, with the bridge receipt as proof;
    /// * `not_started`, or nothing reached the editor — `compensated`: the
    ///   pre-state is proven;
    /// * `unresolved`, a lost connection or no answer — when the mutation may
    ///   have written a project file, `quarantined`: neither state is proven
    ///   and writes stay blocked until an operator resolves it. A mutation of
    ///   editor state alone touches no project file, so its project pre-state
    ///   is proven and it settles `compensated`; the caller still reports it
    ///   as unresolved and never sends it again.
    pub(crate) fn execute_editor_effect(
        &self,
        permit: &EffectPermit,
        mutation: &EditorMutation,
    ) -> Result<EditorEffectOutcome, EffectError> {
        self.execute_bridge_effect(permit, mutation, EffectBoundaryKind::Editor)
    }

    /// Carry out one journaled runtime command under a validated permit and
    /// settle it from the session's three-state answer, as
    /// [`Self::execute_editor_effect`] settles an editor mutation.
    pub(crate) fn execute_runtime_effect(
        &self,
        permit: &EffectPermit,
        command: &EditorMutation,
    ) -> Result<EditorEffectOutcome, EffectError> {
        self.execute_bridge_effect(permit, command, EffectBoundaryKind::Runtime)
    }

    /// Carry out one journaled LSP/DAP command under a validated permit and
    /// settle it from the editor session's three-state answer, as
    /// [`Self::execute_editor_effect`] settles an editor mutation.
    pub(crate) fn execute_debug_effect(
        &self,
        permit: &EffectPermit,
        command: &EditorMutation,
    ) -> Result<EditorEffectOutcome, EffectError> {
        self.execute_bridge_effect(permit, command, EffectBoundaryKind::LspDap)
    }

    fn execute_bridge_effect(
        &self,
        permit: &EffectPermit,
        mutation: &EditorMutation,
        boundary: EffectBoundaryKind,
    ) -> Result<EditorEffectOutcome, EffectError> {
        let (peer, change, command) = match boundary {
            EffectBoundaryKind::Runtime => (
                "the game session",
                "the game-session change",
                "the runtime command",
            ),
            EffectBoundaryKind::LspDap => (
                "the editor session",
                "the debug-session change",
                "the debug command",
            ),
            EffectBoundaryKind::Process => (
                "the managed session",
                "the session stop",
                "the session stop",
            ),
            _ => (
                "the editor",
                "the editor-state change",
                "the editor mutation",
            ),
        };
        self.validate_permit(permit, boundary)?;
        let record = self.store.load(permit.transaction_id())?;
        if record.phase != Phase::StagedFsynced {
            return Err(EffectError::WrongPhase {
                transaction_id: record.transaction_id,
                phase: record.phase,
            });
        }
        let ticket = EffectTicket::new(record.transaction_id.clone(), boundary, permit.seal());
        self.store.advance(
            &record.transaction_id,
            Phase::EffectStartedFsynced,
            None,
            None,
            None,
        )?;
        let applied = match boundary {
            EffectBoundaryKind::Runtime => self.runtime.apply(&ticket, mutation),
            EffectBoundaryKind::LspDap => self.lsp_dap.apply(&ticket, mutation),
            EffectBoundaryKind::Process => self.process.stop_session(&ticket, mutation),
            _ => self.editor.apply(&ticket, mutation),
        };
        let outcome = match applied {
            Ok(outcome) => outcome,
            Err(error) => {
                self.store.advance(
                    &record.transaction_id,
                    Phase::Compensated,
                    None,
                    Some(error.to_string()),
                    Some(TerminalOutcome::failed(ErrorKind::Unavailable)),
                )?;
                return Err(EffectError::Boundary(error.to_string()));
            }
        };
        let failure = outcome
            .error
            .as_ref()
            .map_or(ErrorKind::Internal, |(kind, _)| *kind);
        let reason = outcome.error.as_ref().map(|(_, message)| message.clone());
        match outcome.status {
            MutationStatus::Committed => {
                self.store.advance(
                    &record.transaction_id,
                    Phase::CommittedFsynced,
                    Some(outcome.proof.clone()),
                    Some(format!(
                        "{peer} committed the mutation and returned a bound receipt"
                    )),
                    Some(if outcome.error.is_some() {
                        TerminalOutcome::failed(failure)
                    } else {
                        TerminalOutcome::Succeeded
                    }),
                )?;
            }
            MutationStatus::NotStarted => {
                self.store.advance(
                    &record.transaction_id,
                    Phase::Compensated,
                    Some(outcome.proof.clone()),
                    Some(reason.unwrap_or_else(|| format!("{peer} did not start the mutation"))),
                    Some(TerminalOutcome::failed(failure)),
                )?;
            }
            MutationStatus::Unresolved if record.touched_paths.is_empty() => {
                self.store.advance(
                    &record.transaction_id,
                    Phase::Compensated,
                    Some(outcome.proof.clone()),
                    Some(format!(
                        "{change} is unresolved ({}); no project file was in scope",
                        reason.unwrap_or_default()
                    )),
                    Some(TerminalOutcome::failed(ErrorKind::Uncertain)),
                )?;
            }
            MutationStatus::Unresolved => {
                self.store.advance(
                    &record.transaction_id,
                    Phase::Quarantined,
                    Some(outcome.proof.clone()),
                    Some(format!(
                        "{command} is unresolved ({}); whether it wrote {} is not decidable",
                        reason.unwrap_or_default(),
                        record.touched_paths.join(", ")
                    )),
                    Some(TerminalOutcome::failed(ErrorKind::Uncertain)),
                )?;
            }
        }
        Ok(outcome)
    }

    /// Durably stage every planned payload (`staged_fsynced`).
    pub(crate) fn stage_file_transaction(
        &self,
        intent: &FileEffectIntent,
    ) -> Result<TransactionRecord, EffectError> {
        let record = self.store.load(&intent.transaction_id)?;
        let mut staged = Vec::with_capacity(record.staged_write_set.len());
        for entry in &record.staged_write_set {
            let bytes = intent
                .writes
                .iter()
                .find(|planned| planned.path == entry.path)
                .and_then(|planned| planned.bytes.clone())
                .ok_or_else(|| {
                    EffectError::Boundary(format!(
                        "planned bytes for staged path {} are missing",
                        entry.path
                    ))
                })?;
            staged.push((entry.staging_id.clone(), bytes));
        }
        Ok(self.store.stage(&intent.transaction_id, staged)?)
    }

    /// Execute one staged `file` effect under a validated permit.
    ///
    /// A journaled delete is not executed here; see
    /// [`Self::execute_file_effect_removing`].
    pub(crate) fn execute_file_effect(
        &self,
        permit: &EffectPermit,
    ) -> Result<EffectOutcome, EffectError> {
        self.execute_file_effect_removing(permit, &[])
    }

    /// Execute one staged `file` effect under a validated permit, removing the
    /// delete paths named in `removals`.
    ///
    /// A path is named there only when the request proved, with a content
    /// precondition, exactly which bytes it removes; the removal then goes
    /// through the recorded preimage. A delete journaled without that proof is
    /// never executed: its path is left as it is and the settle step proves
    /// whichever state holds.
    pub(crate) fn execute_file_effect_removing(
        &self,
        permit: &EffectPermit,
        removals: &[String],
    ) -> Result<EffectOutcome, EffectError> {
        self.validate_permit(permit, EffectBoundaryKind::File)?;
        let record = self.store.load(permit.transaction_id())?;
        if record.phase != Phase::StagedFsynced {
            return Err(EffectError::WrongPhase {
                transaction_id: record.transaction_id,
                phase: record.phase,
            });
        }
        // Every staged payload must still be intact before the effect starts.
        for staged in &record.staged_write_set {
            self.store
                .staged_bytes(&record.transaction_id, &staged.staging_id)?;
        }

        let ticket = EffectTicket::new(
            record.transaction_id.clone(),
            EffectBoundaryKind::File,
            permit.seal(),
        );
        self.store.advance(
            &record.transaction_id,
            Phase::EffectStartedFsynced,
            None,
            None,
            None,
        )?;

        let mut boundary_error = None;
        for intended in &record.intended_post_hashes {
            let Some(staged) = record
                .staged_write_set
                .iter()
                .find(|entry| entry.path == intended.path)
            else {
                // A `delete` has no staged payload. A proven removal goes
                // through the recorded preimage; any other delete leaves the
                // path untouched, and its tombstone is proven (or refuted) by
                // the settle step below.
                if !removals.contains(&intended.path) {
                    continue;
                }
                match self.recorded_preimage(&record, &intended.path) {
                    PreimageOutcome::Absent => {}
                    PreimageOutcome::Matched(expected) => {
                        if let Err(error) = self.file.delete(&ticket, &intended.path, &expected) {
                            boundary_error = Some(error.to_string());
                            break;
                        }
                    }
                    PreimageOutcome::Diverged => {
                        boundary_error = Some(format!(
                            "recorded pre-state for {} no longer holds; the delete is refused",
                            intended.path
                        ));
                        break;
                    }
                }
                continue;
            };
            let bytes = self
                .store
                .staged_bytes(&record.transaction_id, &staged.staging_id)?;
            let expected = match self.recorded_preimage(&record, &intended.path) {
                PreimageOutcome::Absent => None,
                PreimageOutcome::Matched(bytes) => Some(bytes),
                PreimageOutcome::Diverged => {
                    boundary_error = Some(format!(
                        "recorded pre-state for {} no longer holds; the write is refused",
                        intended.path
                    ));
                    break;
                }
            };
            let write = FileWrite {
                path: intended.path.clone(),
                expected,
                bytes,
            };
            if let Err(error) = self.file.publish(&ticket, &write) {
                boundary_error = Some(error.to_string());
                break;
            }
        }

        self.settle(&record, boundary_error)
    }

    /// Re-derive the typed proof and journal exactly one terminal record.
    fn settle(
        &self,
        record: &TransactionRecord,
        boundary_error: Option<String>,
    ) -> Result<EffectOutcome, EffectError> {
        let post = verify_file_proof(self.proof_source.as_ref(), &record.intended_post_hashes);
        let pre = verify_file_proof(self.proof_source.as_ref(), &record.pre_hashes);
        let composite_satisfied = self.composite_satisfied(record)?;
        let (conclusion, reason) = match (post, pre, composite_satisfied) {
            (ProofOutcome::Matches, _, true) => (
                Conclusion::ProvenPostState,
                "every touched path matches its recorded intended post-state".to_owned(),
            ),
            (ProofOutcome::Matches, _, false) => (
                Conclusion::Quarantine,
                "file bytes match but the linked process_launch sub-proof does not confirm a \
                 contained, clean, terminal helper run"
                    .to_owned(),
            ),
            (_, ProofOutcome::Matches, _) => (
                Conclusion::ProvenPreState,
                boundary_error.clone().map_or_else(
                    || "every touched path still matches its recorded pre-state".to_owned(),
                    |error| format!("effect refused and pre-state still holds: {error}"),
                ),
            ),
            (post, pre, _) => (
                Conclusion::Quarantine,
                format!("neither state is proven (post: {post}, pre: {pre})"),
            ),
        };
        let proof = match conclusion {
            Conclusion::Quarantine => None,
            Conclusion::ProvenPostState => Some(self.observed_proof(&record.intended_post_hashes)?),
            Conclusion::ProvenPreState => Some(self.observed_proof(&record.pre_hashes)?),
        };
        let phase = match conclusion {
            Conclusion::ProvenPostState => Phase::CommittedFsynced,
            Conclusion::ProvenPreState => Phase::Compensated,
            Conclusion::Quarantine => Phase::Quarantined,
        };
        let terminal_outcome = match phase {
            Phase::CommittedFsynced | Phase::Compensated => TerminalOutcome::Succeeded,
            Phase::Quarantined => TerminalOutcome::failed(ErrorKind::Uncertain),
            Phase::IntentFsynced | Phase::StagedFsynced | Phase::EffectStartedFsynced => {
                return Err(EffectError::Boundary(
                    "effect settlement did not produce a terminal phase".to_owned(),
                ));
            }
        };
        self.store.advance(
            &record.transaction_id,
            phase,
            proof,
            Some(reason.clone()),
            Some(terminal_outcome),
        )?;
        Ok(EffectOutcome {
            transaction_id: record.transaction_id.clone(),
            phase,
            conclusion,
            reason,
        })
    }

    fn observed_proof(&self, entries: &[FileStateEntry]) -> Result<TypedProof, EffectError> {
        let entries = self
            .proof_source
            .observe_file_states(entries)
            .map_err(|error| EffectError::Boundary(error.to_string()))?;
        Ok(TypedProof::File { entries })
    }

    fn composite_satisfied(&self, record: &TransactionRecord) -> Result<bool, EffectError> {
        let Some(linked) = record.required_process_transaction() else {
            return Ok(true);
        };
        match self.store.load(linked) {
            Ok(linked_record) => Ok(linked_record.phase == Phase::CommittedFsynced
                && linked_record
                    .proof
                    .as_ref()
                    .is_some_and(TypedProof::is_clean_terminal_process)),
            Err(StoreError::NotFound(_)) => Ok(false),
            Err(error) => Err(EffectError::Store(error)),
        }
    }

    /// Re-read the recorded preimage for a compare-and-swap replacement.
    ///
    /// The journal deliberately records only the preimage *hash* (never project
    /// source text), so the bytes are re-read through `SafeRoot` and their hash
    /// re-checked against the record. A divergence refuses the write rather than
    /// overwriting a state nobody recorded.
    fn recorded_preimage(&self, record: &TransactionRecord, path: &str) -> PreimageOutcome {
        let recorded = record
            .pre_hashes
            .iter()
            .find(|entry| entry.path == path)
            .and_then(|entry| entry.sha256.clone());
        let Some(expected) = recorded else {
            return PreimageOutcome::Absent;
        };
        match self.proof_source.read_bytes(path) {
            Ok(Some(bytes)) if hex_sha256(&bytes) == expected => PreimageOutcome::Matched(bytes),
            _ => PreimageOutcome::Diverged,
        }
    }

    fn validate_permit(
        &self,
        permit: &EffectPermit,
        boundary: EffectBoundaryKind,
    ) -> Result<(), EffectError> {
        if permit.epoch() != self.epoch {
            return Err(EffectError::StalePermit(
                "permit was issued by a superseded authority epoch".to_owned(),
            ));
        }
        if permit.project_object_identity() != self.held.object_identity
            || permit.project_generation() != self.held.generation
        {
            return Err(EffectError::StalePermit(
                "permit does not match the held project object identity or generation".to_owned(),
            ));
        }
        if permit.boundary() != boundary {
            return Err(EffectError::Unauthorized(format!(
                "permit authorizes a different effect boundary ({:?})",
                permit.boundary()
            )));
        }
        if !matches!(
            (permit.capability(), permit.operation_family()),
            (Capability::ProjectWrite, OperationFamily::ProjectMutation)
                | (Capability::EditorMutate, OperationFamily::EditorMutation)
                | (
                    Capability::RuntimeExecute,
                    OperationFamily::RuntimeExecution
                )
                | (
                    Capability::ExportDestructive,
                    OperationFamily::DestructiveExport
                )
        ) {
            return Err(EffectError::Unauthorized(
                "permit capability does not match its operation family".to_owned(),
            ));
        }
        let quarantined = self.store.quarantined_transactions()?;
        if !quarantined.is_empty() {
            return Err(EffectError::Quarantined(quarantined));
        }
        Ok(())
    }

    /// Journal one terminal record for a repair whose typed proof the Todo 34
    /// operator command already re-derived live, for the record's own effect kind.
    ///
    /// [`Self::apply_repair`] can only re-derive the `file` kind, because the
    /// gateway's [`ProofSource`] is a `SafeRoot`. A `process_launch` or
    /// `editor_runtime` repair is proven by live OS/boundary inspection or a
    /// bridge round trip, which only the operator command can perform — so the
    /// command hands its own observation in here rather than the gateway
    /// guessing one. The gateway still refuses to take that observation on trust:
    ///
    /// * the permit must match the held project object identity and generation;
    /// * the observed proof's kind must equal the record's recorded `effect_kind`;
    /// * the claimed disposition must match the requested operation
    ///   (`compensate`/`terminate` require a pre-state, `acknowledge-safe-state`
    ///   a post-state);
    /// * for the `file` kind the gateway *additionally* re-derives the recorded
    ///   state through its own `SafeRoot` proof source and requires the composite
    ///   linked-subproof gate, so a file repair is a conjunction of two
    ///   independent derivations; and
    /// * the audit record is written before any transition is journaled.
    pub(crate) fn apply_operator_repair(
        &self,
        permit: &RepairPermit,
        derived: Option<DerivedTerminal>,
    ) -> Result<EffectOutcome, EffectError> {
        if permit.project_object_identity() != self.held.object_identity
            || permit.project_generation() != self.held.generation
        {
            return Err(EffectError::StalePermit(
                "repair permit does not match the held project object identity or generation"
                    .to_owned(),
            ));
        }
        let record = self.store.load(permit.transaction_id())?;
        let ambiguous_survivor = is_ambiguous_survivor_phase(&record);
        if record.phase != Phase::Quarantined
            && !(ambiguous_survivor && permit.operation() == RepairOperation::AcknowledgeSafeState)
        {
            return Err(EffectError::WrongPhase {
                transaction_id: record.transaction_id,
                phase: record.phase,
            });
        }

        let (phase, conclusion, reason, proof) = match (permit.operation(), derived) {
            // Report-only: the audit record is written, nothing is journaled.
            (RepairOperation::Reconcile, derived) => (
                record.phase,
                Conclusion::Quarantine,
                format!(
                    "reconcile only: re-derived {}",
                    derived
                        .as_ref()
                        .map_or("nothing", |derived| derived.disposition.as_str())
                ),
                None,
            ),
            (operation, Some(derived)) => {
                self.check_derived(&record, operation, &derived)?;
                let (phase, conclusion) = match derived.disposition {
                    DerivedDisposition::PreState => {
                        (Phase::Compensated, Conclusion::ProvenPreState)
                    }
                    DerivedDisposition::PostState => {
                        (Phase::CommittedFsynced, Conclusion::ProvenPostState)
                    }
                };
                (
                    phase,
                    conclusion,
                    format!(
                        "{} with an independently re-derived {} proof for effect kind {}",
                        operation.as_str(),
                        derived.disposition.as_str(),
                        record.effect_kind.as_str()
                    ),
                    Some(derived.proof),
                )
            }
            (operation, None) => {
                return Err(EffectError::Unauthorized(format!(
                    "{} carries no re-derived proof, so no transition is authorized",
                    operation.as_str()
                )));
            }
        };

        self.store.audit(
            &record.transaction_id,
            "repair_proof_outcome",
            &format!(
                "operation={} conclusion={} reason={reason}",
                permit.operation().as_str(),
                conclusion.as_str()
            ),
        )?;

        if let Some(proof) = proof {
            if record.phase == Phase::Quarantined {
                self.store.repair_transition(
                    &record.transaction_id,
                    phase,
                    Some(proof),
                    reason.clone(),
                    terminal_outcome_for_repair(phase),
                )?;
            } else if record.phase != phase {
                // The Todo 14 case-4 ambiguous-survivor exception: the record is
                // not quarantined, so `repair_transition` does not apply and the
                // ordinary phase machine's own legal successor is used instead.
                self.store.advance(
                    &record.transaction_id,
                    phase,
                    Some(proof),
                    Some(reason.clone()),
                    Some(terminal_outcome_for_repair(phase)),
                )?;
            }
        }

        Ok(EffectOutcome {
            transaction_id: record.transaction_id,
            phase,
            conclusion,
            reason,
        })
    }

    /// Refuse a re-derived observation that does not agree with the record, the
    /// requested operation, or the gateway's own independent derivation.
    fn check_derived(
        &self,
        record: &TransactionRecord,
        operation: RepairOperation,
        derived: &DerivedTerminal,
    ) -> Result<(), EffectError> {
        if derived.proof.kind() != record.effect_kind {
            return Err(EffectError::Unauthorized(format!(
                "a {} proof cannot settle a {} transaction",
                derived.proof.kind().as_str(),
                record.effect_kind.as_str()
            )));
        }
        let expected = match operation {
            RepairOperation::Compensate | RepairOperation::Terminate => {
                DerivedDisposition::PreState
            }
            RepairOperation::AcknowledgeSafeState => DerivedDisposition::PostState,
            RepairOperation::Reconcile => return Ok(()),
        };
        if derived.disposition != expected {
            return Err(EffectError::Unauthorized(format!(
                "{} requires a {} proof, not a {} proof",
                operation.as_str(),
                expected.as_str(),
                derived.disposition.as_str()
            )));
        }
        if operation == RepairOperation::Terminate
            && record.effect_kind != EffectKind::ProcessLaunch
        {
            return Err(EffectError::Unauthorized(format!(
                "terminate applies only to a process_launch transaction, not {}",
                record.effect_kind.as_str()
            )));
        }
        if record.effect_kind != EffectKind::File {
            return Ok(());
        }
        // The gateway derives the `file` kind for itself, so a file repair is the
        // conjunction of the command's derivation and this one.
        let recorded = match derived.disposition {
            DerivedDisposition::PreState => &record.pre_hashes,
            DerivedDisposition::PostState => &record.intended_post_hashes,
        };
        let outcome = verify_file_proof(self.proof_source.as_ref(), recorded);
        if outcome != ProofOutcome::Matches {
            return Err(EffectError::Unauthorized(format!(
                "the gateway's own re-derivation of the recorded {} state reports {outcome}",
                derived.disposition.as_str()
            )));
        }
        if !self.composite_satisfied(record)? {
            return Err(EffectError::Unauthorized(
                "the linked process_launch sub-proof does not confirm a contained, clean, \
                 terminal helper run"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// Apply one proof-driven operator-recovery operation.
    ///
    /// This is the only quarantine-exempt path, and it is structurally incapable
    /// of committing without independent proof: `acknowledge-safe-state` requires
    /// a re-derived post-state proof, `compensate` requires a re-derived
    /// pre-state proof, and neither can force or guess.
    pub(crate) fn apply_repair(&self, permit: &RepairPermit) -> Result<EffectOutcome, EffectError> {
        if permit.project_object_identity() != self.held.object_identity
            || permit.project_generation() != self.held.generation
        {
            return Err(EffectError::StalePermit(
                "repair permit does not match the held project object identity or generation"
                    .to_owned(),
            ));
        }
        let record = self.store.load(permit.transaction_id())?;
        if record.phase != Phase::Quarantined {
            return Err(EffectError::WrongPhase {
                transaction_id: record.transaction_id,
                phase: record.phase,
            });
        }
        let post = verify_file_proof(self.proof_source.as_ref(), &record.intended_post_hashes);
        let pre = verify_file_proof(self.proof_source.as_ref(), &record.pre_hashes);
        let composite_satisfied = self.composite_satisfied(&record)?;

        let (phase, conclusion, reason) = match permit.operation() {
            RepairOperation::Reconcile => (
                Phase::Quarantined,
                Conclusion::Quarantine,
                format!("reconcile only: post={post} pre={pre}"),
            ),
            RepairOperation::Compensate | RepairOperation::Terminate
                if pre == ProofOutcome::Matches =>
            {
                (
                    Phase::Compensated,
                    Conclusion::ProvenPreState,
                    format!(
                        "{} with a re-derived pre-state proof",
                        permit.operation().as_str()
                    ),
                )
            }
            RepairOperation::AcknowledgeSafeState
                if post == ProofOutcome::Matches && composite_satisfied =>
            {
                (
                    Phase::CommittedFsynced,
                    Conclusion::ProvenPostState,
                    "acknowledge-safe-state with a re-derived post-state proof".to_owned(),
                )
            }
            operation => (
                Phase::Quarantined,
                Conclusion::Quarantine,
                format!(
                    "{} refused: no independent proof (post={post} pre={pre} composite={composite_satisfied})",
                    operation.as_str()
                ),
            ),
        };

        self.store.audit(
            &record.transaction_id,
            "repair_proof_outcome",
            &format!(
                "operation={} conclusion={} reason={reason}",
                permit.operation().as_str(),
                conclusion.as_str()
            ),
        )?;

        if phase != Phase::Quarantined {
            let proof = match conclusion {
                Conclusion::ProvenPostState => {
                    Some(self.observed_proof(&record.intended_post_hashes)?)
                }
                Conclusion::ProvenPreState => Some(self.observed_proof(&record.pre_hashes)?),
                Conclusion::Quarantine => None,
            };
            // A quarantined record has no legal successor in the ordinary phase
            // machine; the recovery authority therefore writes its proof-backed
            // terminal record through the dedicated repair transition.
            self.store.repair_transition(
                &record.transaction_id,
                phase,
                proof,
                reason.clone(),
                terminal_outcome_for_repair(phase),
            )?;
        }
        Ok(EffectOutcome {
            transaction_id: record.transaction_id,
            phase,
            conclusion,
            reason,
        })
    }
}

/// Which recorded state an operator repair proved live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DerivedDisposition {
    /// The recorded pre-state still holds.
    PreState,
    /// The recorded intended post-state holds.
    PostState,
}

impl DerivedDisposition {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::PreState => "pre-state",
            Self::PostState => "post-state",
        }
    }
}

const fn terminal_outcome_for_repair(phase: Phase) -> TerminalOutcome {
    match phase {
        Phase::CommittedFsynced => TerminalOutcome::Succeeded,
        Phase::Compensated => TerminalOutcome::failed(ErrorKind::Unavailable),
        Phase::Quarantined => TerminalOutcome::failed(ErrorKind::Uncertain),
        Phase::IntentFsynced | Phase::StagedFsynced | Phase::EffectStartedFsynced => {
            TerminalOutcome::failed(ErrorKind::Internal)
        }
    }
}

/// One live observation the Todo 34 operator command re-derived for itself.
pub(crate) struct DerivedTerminal {
    pub(crate) disposition: DerivedDisposition,
    pub(crate) proof: TypedProof,
}

/// Whether a record sits in the Todo 14 case-4/5 ambiguous live-survivor state.
///
/// Cases 4 and 5 are exactly the `effect_started_fsynced` and `committed_fsynced`
/// marks of a `process_launch` transaction, so this is the only non-quarantined
/// state `acknowledge-safe-state` may target.
fn is_ambiguous_survivor_phase(record: &TransactionRecord) -> bool {
    record.effect_kind == EffectKind::ProcessLaunch
        && matches!(
            record.phase,
            Phase::EffectStartedFsynced | Phase::CommittedFsynced
        )
}

/// Whether a compare-and-swap preimage is available and still matches.
enum PreimageOutcome {
    /// The recorded pre-state is an existence tombstone: this is a create.
    Absent,
    /// The preimage was re-read and its hash matches the record.
    Matched(Vec<u8>),
    /// The preimage no longer matches the record.
    Diverged,
}

fn staging_seed(transaction_id: &str, index: usize) -> u64 {
    let digest = hex_sha256(format!("{transaction_id}:{index}").as_bytes());
    u64::from_str_radix(&digest[..16], 16).unwrap_or(index as u64)
}

#[cfg(test)]
#[path = "gateway_tests.rs"]
mod tests;
