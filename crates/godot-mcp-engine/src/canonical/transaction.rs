//! The `file`-effect transaction, its composed proof rule, and startup
//! classification.
//!
//! # This transaction's effect kind is `file`, never `editor_runtime`
//!
//! A canonical write runs a **disposable, exiting subprocess**. There is no live
//! editor session and no `EditorUndoRedoManager` action, so `editor_runtime` would
//! be a false claim about what produced the change. [`FileRecord`] hard-codes
//! `effect_kind: "file"` and `effect_boundary: "file"`; the spelling is not a
//! parameter, so no caller can misclassify one of these transactions.
//!
//! # The composed proof rule
//!
//! The mutation's own pre/post-hash rules are **never sufficient**. Every
//! [`FileRecord`] carries
//! `required_subproofs.process_launch_transaction_id`, naming the linked
//! helper-launch transaction, and a commit requires the **conjunction** of:
//!
//! 1. the file's own evidence — the observed bytes equal the intended
//!    post-state recorded before the publish; **and**
//! 2. the linked transaction's `process_launch` proof independently showing a
//!    clean terminal process, via Todo 14's own
//!    [`ProcessLaunchProof::is_clean_terminal_process`]: confirmed containment,
//!    `exited-clean`, exit code 0, no signal, and a proven-empty boundary
//!    afterwards.
//!
//! Todo 15 established why half of this is not enough by direct observation: a
//! real Godot build **exits 0 with empty output when handed a flag it does not
//! support**, and the official CLI reference states unknown arguments "have no
//! effect whatsoever". Matching bytes are likewise not enough: bytes can match
//! because a previous run wrote them, because an external actor wrote them, or
//! because the helper is *still running* and may yet write more.
//! [`classify_startup`] therefore refuses to commit on either half alone, and
//! there is no force-overwrite path and no fourth proof kind anywhere here.
//!
//! # Exactly three terminal classifications
//!
//! [`StartupClassification`] has three variants and no more, mirroring Todo 6's
//! three dispositions. A restarted process inspecting an interrupted canonical
//! write can establish the **exact pre-state**, the **exact post-state**, or
//! **quarantine** — never a fourth answer, and never an unproven success.

use std::fmt;

use serde_json::{Value, json};

use crate::canonical::error::CanonicalError;
use crate::supervision::journal::{
    Disposition, JournalError, LaunchPhase, ProcessLaunchProof, SCHEMA_VERSION,
    TransactionEnvelope, hex_sha256, validate_canonical,
};

/// The canonical `file` effect kind. Frozen: a canonical write is a file effect.
pub const EFFECT_KIND: &str = "file";

/// The canonical `file` effect boundary.
pub const EFFECT_BOUNDARY: &str = "file";

/// Terminal reason recorded when a canonical write is compensated because the
/// exact pre-state is still on disk.
pub const CANONICAL_PRE_STATE_RESTORED: &str = "canonical_write_exact_pre_state";

/// Terminal reason recorded when a canonical write commits on the full composite
/// proof.
pub const CANONICAL_POST_STATE_PROVEN: &str = "canonical_write_exact_post_state";

/// Terminal reason recorded when neither exact state is establishable.
pub const CANONICAL_UNPROVEN: &str = "canonical_write_unproven_composite";

/// What a write set entry does to a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileOperation {
    /// The path did not exist and will.
    Create,
    /// The path existed and its bytes change.
    Update,
    /// The path existed and will not.
    Delete,
}

impl FileOperation {
    /// Canonical wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }

    /// Parse the canonical wire spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "create" => Some(Self::Create),
            "update" => Some(Self::Update),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }
}

/// Whether a path is present at a recorded moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Existence {
    /// The path exists.
    Present,
    /// The path does not exist.
    Absent,
}

impl Existence {
    /// Canonical wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Absent => "absent",
        }
    }

    /// Parse the canonical wire spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "present" => Some(Self::Present),
            "absent" => Some(Self::Absent),
            _ => None,
        }
    }
}

/// One path's state at one recorded moment: pre-state, intended post-state, or
/// the observed post-state carried by the typed `file` proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// The project-relative path.
    pub path: String,
    /// What the transaction does to it.
    pub operation: FileOperation,
    /// Whether it exists at this moment.
    pub existence: Existence,
    /// Its SHA-256 when present, `None` when absent.
    pub sha256: Option<String>,
}

impl FileEntry {
    /// An entry for a path that is absent.
    #[must_use]
    pub fn absent(path: &str, operation: FileOperation) -> Self {
        Self {
            path: path.to_owned(),
            operation,
            existence: Existence::Absent,
            sha256: None,
        }
    }

    /// An entry for a path that is present, hashing `bytes`.
    #[must_use]
    pub fn present(path: &str, operation: FileOperation, bytes: &[u8]) -> Self {
        Self {
            path: path.to_owned(),
            operation,
            existence: Existence::Present,
            sha256: Some(hex_sha256(bytes)),
        }
    }

    /// The canonical JSON encoding the schema accepts.
    #[must_use]
    pub fn to_canonical_value(&self) -> Value {
        json!({
            "path": self.path,
            "operation": self.operation.as_str(),
            "existence": self.existence.as_str(),
            "sha256": self.sha256,
        })
    }

    /// Rebuild an entry from its canonical JSON encoding.
    ///
    /// # Errors
    /// Returns [`JournalError::Payload`] when a required field is unusable.
    pub fn from_canonical_value(value: &Value) -> Result<Self, JournalError> {
        let text = |key: &str| -> Result<String, JournalError> {
            value[key]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| JournalError::Payload(format!("file entry field {key} is unusable")))
        };
        let path = text("path")?;
        let operation = FileOperation::parse(&text("operation")?).ok_or_else(|| {
            JournalError::Payload("file entry carries an unknown operation".to_owned())
        })?;
        let existence = Existence::parse(&text("existence")?).ok_or_else(|| {
            JournalError::Payload("file entry carries an unknown existence".to_owned())
        })?;
        Ok(Self {
            path,
            operation,
            existence,
            sha256: value["sha256"].as_str().map(str::to_owned),
        })
    }

    /// Whether `observed` describes the same path in the same state.
    #[must_use]
    pub fn matches(&self, observed: &Self) -> bool {
        self.path == observed.path
            && self.existence == observed.existence
            && self.sha256 == observed.sha256
    }
}

/// The typed `file` proof: what the paths actually looked like after the publish.
///
/// This is Todo 6's `file` proof form, byte for byte. It is one half of a commit
/// and never the whole of one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileProof {
    /// One entry per touched path.
    pub entries: Vec<FileEntry>,
}

impl FileProof {
    /// The canonical tagged JSON encoding the schema accepts.
    #[must_use]
    pub fn to_canonical_value(&self) -> Value {
        json!({
            "kind": EFFECT_KIND,
            "entries": self
                .entries
                .iter()
                .map(FileEntry::to_canonical_value)
                .collect::<Vec<Value>>(),
        })
    }

    /// Rebuild a proof from its canonical tagged JSON encoding.
    ///
    /// # Errors
    /// Returns [`JournalError::Payload`] when the value is not a `file`-tagged
    /// proof, so a `process_launch` proof can never be read back as a file proof.
    pub fn from_canonical_value(value: &Value) -> Result<Self, JournalError> {
        if value["kind"] != json!(EFFECT_KIND) {
            return Err(JournalError::Payload(
                "proof is not the file form".to_owned(),
            ));
        }
        let entries = value["entries"]
            .as_array()
            .ok_or_else(|| JournalError::Payload("file proof has no entries".to_owned()))?
            .iter()
            .map(FileEntry::from_canonical_value)
            .collect::<Result<Vec<FileEntry>, JournalError>>()?;
        Ok(Self { entries })
    }
}

/// One staged output: the engine-produced artifact this transaction will publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedOutput {
    /// The project-relative path of the staged artifact, inside the helper's own
    /// versioned staging area.
    pub path: String,
    /// The deterministic 16-hex staging ID.
    pub staging_id: String,
    /// The artifact's SHA-256.
    pub sha256: String,
    /// The artifact's byte length.
    pub byte_length: u64,
}

impl StagedOutput {
    /// Build a staged entry from the artifact's own bytes.
    #[must_use]
    pub fn of(path: &str, staging_id: &str, bytes: &[u8]) -> Self {
        Self {
            path: path.to_owned(),
            staging_id: staging_id.to_owned(),
            sha256: hex_sha256(bytes),
            byte_length: bytes.len() as u64,
        }
    }

    /// The canonical JSON encoding the schema accepts.
    #[must_use]
    pub fn to_canonical_value(&self) -> Value {
        json!({
            "path": self.path,
            "staging_id": self.staging_id,
            "sha256": self.sha256,
            "byte_length": self.byte_length,
        })
    }

    /// Rebuild a staged entry from its canonical JSON encoding.
    ///
    /// # Errors
    /// Returns [`JournalError::Payload`] when a required field is unusable.
    pub fn from_canonical_value(value: &Value) -> Result<Self, JournalError> {
        Ok(Self {
            path: value["path"]
                .as_str()
                .ok_or_else(|| JournalError::Payload("staged entry has no path".to_owned()))?
                .to_owned(),
            staging_id: value["staging_id"]
                .as_str()
                .ok_or_else(|| JournalError::Payload("staged entry has no staging id".to_owned()))?
                .to_owned(),
            sha256: value["sha256"]
                .as_str()
                .ok_or_else(|| JournalError::Payload("staged entry has no digest".to_owned()))?
                .to_owned(),
            byte_length: value["byte_length"].as_u64().unwrap_or_default(),
        })
    }
}

/// One durable canonical-write transaction record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    /// The canonical transaction envelope, minted by the server layer.
    pub envelope: TransactionEnvelope,
    /// The furthest of Todo 6's four canonical fsynced phases that is durable.
    pub phase: LaunchPhase,
    /// Path → OS object identity pairs for every touched path.
    pub touched: Vec<(String, Option<String>)>,
    /// The pre-state, recorded before anything was created.
    pub pre_hashes: Vec<FileEntry>,
    /// The engine-produced artifacts this transaction publishes.
    pub staged: Vec<StagedOutput>,
    /// The post-state this transaction intends, recorded before the publish.
    pub intended_post_hashes: Vec<FileEntry>,
    /// The typed `file` proof, present once the publish was observed.
    pub proof: Option<FileProof>,
    /// The linked helper-launch transaction whose `process_launch` proof this
    /// transaction's commit additionally requires. **Never `None` for a mutating
    /// canonical write**: the composite rule has nothing to check without it.
    pub required_launch_transaction_id: Option<String>,
}

impl FileRecord {
    /// Assemble the canonical record value and validate it against the
    /// repository's canonical schema.
    ///
    /// # Errors
    /// Returns [`JournalError::Schema`] when the assembled record violates the
    /// canonical schema, which is what makes an invented field, an invented phase
    /// name and a fourth proof kind unrepresentable at this boundary.
    pub fn to_canonical_value(&self) -> Result<Value, JournalError> {
        let mut history: Vec<Value> = Vec::new();
        for phase in LaunchPhase::ALL {
            if phase.rank() <= self.phase.rank() {
                history.push(json!({
                    "phase": phase.as_str(),
                    "sequence": u64::from(phase.rank()),
                }));
            }
        }
        let mut touched_paths: Vec<String> = Vec::new();
        let mut touched_identities: Vec<Value> = Vec::new();
        for (path, identity) in &self.touched {
            if !touched_paths.iter().any(|existing| existing == path) {
                touched_paths.push(path.clone());
                touched_identities.push(json!({
                    "path": path,
                    "object_identity": identity,
                }));
            }
        }
        let record = json!({
            "schema_version": SCHEMA_VERSION,
            "transaction_id": self.envelope.transaction_id,
            // Frozen: a canonical write is a file effect. There is no live editor
            // session here, so `editor_runtime` would be a false claim.
            "effect_kind": EFFECT_KIND,
            "effect_boundary": EFFECT_BOUNDARY,
            "project_object_identity": self.envelope.project.object_identity,
            "project_root_object": self.envelope.project.root_object,
            "project_file_object": self.envelope.project.file_object,
            "project_generation": self.envelope.project.generation,
            "transport_mode": transport_mode_str(self),
            "principal": self.envelope.principal,
            "session": self.envelope.session,
            "operation_family": operation_family_str(self),
            "tool_name": self.envelope.tool_name,
            "idempotency_id": self.envelope.idempotency_id,
            "canonical_request_hash": self.envelope.canonical_request_hash,
            "canonical_request_bytes": self.envelope.canonical_request_bytes,
            "touched_paths": touched_paths,
            "touched_object_identities": touched_identities,
            "pre_hashes": self
                .pre_hashes
                .iter()
                .map(FileEntry::to_canonical_value)
                .collect::<Vec<Value>>(),
            "staged_write_set": self
                .staged
                .iter()
                .map(StagedOutput::to_canonical_value)
                .collect::<Vec<Value>>(),
            "intended_post_hashes": self
                .intended_post_hashes
                .iter()
                .map(FileEntry::to_canonical_value)
                .collect::<Vec<Value>>(),
            "phase": self.phase.as_str(),
            "phase_history": history,
            "proof": self.proof.as_ref().map(FileProof::to_canonical_value),
            "required_subproofs": self
                .required_launch_transaction_id
                .as_ref()
                .map_or(Value::Null, |id| json!({ "process_launch_transaction_id": id })),
            "terminal_reason": Value::Null,
            "opened_at_unix_ms": self.envelope.opened_at_unix_ms,
        });
        validate_canonical(&record)?;
        Ok(record)
    }

    /// Rebuild a record from its canonical durable value, exactly as a restarted
    /// process must.
    ///
    /// The phase is derived from which of the four canonical fsync marks are
    /// present, in ascending order — never from a record-specific phase name.
    ///
    /// # Errors
    /// Returns [`JournalError::Schema`] when the value is not canonical, or
    /// [`JournalError::Payload`] when a required field is unusable.
    pub fn from_canonical_value(value: &Value) -> Result<Self, JournalError> {
        validate_canonical(value)?;
        if value["effect_kind"] != json!(EFFECT_KIND) {
            return Err(JournalError::Payload(
                "record is not a file-effect transaction".to_owned(),
            ));
        }
        let text = |key: &str| -> Result<String, JournalError> {
            value[key]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| JournalError::Payload(format!("record field {key} is not a string")))
        };
        let number = |key: &str| -> Result<u64, JournalError> {
            value[key]
                .as_u64()
                .ok_or_else(|| JournalError::Payload(format!("record field {key} is not a number")))
        };

        let mut phase = None;
        for candidate in LaunchPhase::ALL {
            let present = value["phase_history"].as_array().is_some_and(|history| {
                history
                    .iter()
                    .any(|step| step["phase"] == json!(candidate.as_str()))
            });
            if present {
                phase = Some(candidate);
            }
        }
        let phase = phase.ok_or_else(|| {
            JournalError::Payload(
                "record carries none of the four canonical fsync marks".to_owned(),
            )
        })?;

        let entries = |key: &str| -> Result<Vec<FileEntry>, JournalError> {
            value[key]
                .as_array()
                .map(|items| items.iter().map(FileEntry::from_canonical_value).collect())
                .unwrap_or_else(|| Ok(Vec::new()))
        };

        Ok(Self {
            envelope: TransactionEnvelope {
                transaction_id: text("transaction_id")?,
                project: crate::supervision::journal::ProjectIdentityClaim {
                    object_identity: text("project_object_identity")?,
                    root_object: text("project_root_object")?,
                    file_object: text("project_file_object")?,
                    generation: number("project_generation")?,
                },
                transport_mode: crate::supervision::journal::TransportMode::parse(&text(
                    "transport_mode",
                )?)
                .ok_or_else(|| {
                    JournalError::Payload("record carries an unknown transport mode".to_owned())
                })?,
                principal: value["principal"].as_str().map(str::to_owned),
                session: value["session"].as_str().map(str::to_owned),
                operation_family: crate::supervision::journal::OperationFamily::parse(&text(
                    "operation_family",
                )?)
                .ok_or_else(|| {
                    JournalError::Payload("record carries an unknown operation family".to_owned())
                })?,
                tool_name: text("tool_name")?,
                idempotency_id: text("idempotency_id")?,
                canonical_request_hash: text("canonical_request_hash")?,
                canonical_request_bytes: number("canonical_request_bytes")?,
                opened_at_unix_ms: number("opened_at_unix_ms")?,
            },
            phase,
            touched: value["touched_object_identities"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|entry| {
                            Some((
                                entry["path"].as_str()?.to_owned(),
                                entry["object_identity"].as_str().map(str::to_owned),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            pre_hashes: entries("pre_hashes")?,
            staged: value["staged_write_set"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(StagedOutput::from_canonical_value)
                        .collect()
                })
                .unwrap_or_else(|| Ok(Vec::new()))?,
            intended_post_hashes: entries("intended_post_hashes")?,
            proof: match &value["proof"] {
                Value::Null => None,
                proof => Some(FileProof::from_canonical_value(proof)?),
            },
            required_launch_transaction_id:
                value["required_subproofs"]["process_launch_transaction_id"]
                    .as_str()
                    .map(str::to_owned),
        })
    }

    /// The intended post-state entry for `path`, when there is one.
    #[must_use]
    pub fn intended_for(&self, path: &str) -> Option<&FileEntry> {
        self.intended_post_hashes
            .iter()
            .find(|entry| entry.path == path)
    }

    /// The pre-state entry for `path`, when there is one.
    #[must_use]
    pub fn pre_for(&self, path: &str) -> Option<&FileEntry> {
        self.pre_hashes.iter().find(|entry| entry.path == path)
    }
}

fn transport_mode_str(record: &FileRecord) -> &'static str {
    match record.envelope.transport_mode {
        crate::supervision::journal::TransportMode::Stdio => "stdio",
        crate::supervision::journal::TransportMode::TrustedLocal => "trusted_local",
        crate::supervision::journal::TransportMode::ProxiedOidc => "proxied_oidc",
    }
}

fn operation_family_str(record: &FileRecord) -> &'static str {
    match record.envelope.operation_family {
        crate::supervision::journal::OperationFamily::ProjectMutation => "project_mutation",
        crate::supervision::journal::OperationFamily::EditorMutation => "editor_mutation",
        crate::supervision::journal::OperationFamily::RuntimeExecution => "runtime_execution",
        crate::supervision::journal::OperationFamily::DestructiveExport => "destructive_export",
        crate::supervision::journal::OperationFamily::DiagnosticProbe => "diagnostic_probe",
    }
}

/// Assemble and validate a terminal (settled) canonical record.
///
/// # Errors
/// Returns [`JournalError::Schema`] when the assembled record violates the
/// canonical schema.
pub fn canonical_terminal_value(
    record: &FileRecord,
    disposition: Disposition,
    reason: &str,
    proof: Option<&FileProof>,
) -> Result<Value, JournalError> {
    let mut value = record.to_canonical_value()?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| JournalError::Payload("canonical record is not an object".to_owned()))?;
    object.insert(
        "phase".to_owned(),
        Value::String(disposition.as_str().to_owned()),
    );
    object.insert(
        "terminal_reason".to_owned(),
        Value::String(reason.to_owned()),
    );
    object.insert(
        "proof".to_owned(),
        proof.map_or(Value::Null, FileProof::to_canonical_value),
    );
    if let Some(Value::Array(history)) = object.get_mut("phase_history") {
        let sequence = history.len() as u64 + 1;
        history.push(json!({ "phase": disposition.as_str(), "sequence": sequence }));
    }
    validate_canonical(&value)?;
    Ok(value)
}

/// The linked `process_launch` proof, as this transaction sees it.
///
/// The transaction ID travels with the proof so a proof belonging to a *different*
/// launch can never be accepted: [`LinkedProof::satisfies`] compares it against
/// the record's `required_subproofs.process_launch_transaction_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedProof {
    /// The transaction the proof was read from.
    pub transaction_id: String,
    /// The typed launch proof.
    pub proof: ProcessLaunchProof,
}

impl LinkedProof {
    /// Why this proof does not satisfy `record`'s composite requirement, or
    /// `None` when it does.
    #[must_use]
    pub fn refusal(&self, record: &FileRecord) -> Option<String> {
        let required = record.required_launch_transaction_id.as_deref()?;
        if required != self.transaction_id {
            return Some(format!(
                "the linked proof belongs to transaction {} but this transaction requires {required}",
                self.transaction_id
            ));
        }
        if self.proof.is_clean_terminal_process() {
            return None;
        }
        Some(format!(
            "the linked process_launch proof is not a clean terminal process (membership \
             confirmed: {}, release: {}, exit: {:?}, signal: {:?}, boundary empty after exit: \
             {:?})",
            self.proof.boundary_membership
                == crate::supervision::journal::BoundaryMembership::Confirmed,
            release_spelling(self.proof.release_status),
            self.proof.exit_code,
            self.proof.signal,
            self.proof.boundary_empty_after_exit,
        ))
    }

    /// Whether this proof satisfies `record`'s composite requirement.
    #[must_use]
    pub fn satisfies(&self, record: &FileRecord) -> bool {
        record.required_launch_transaction_id.is_some() && self.refusal(record).is_none()
    }

    /// Whether the linked process is provably no longer running. A live survivor
    /// is never accepted as evidence about the on-disk state, because it may still
    /// write more.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        use crate::supervision::journal::ReleaseStatus;
        matches!(
            self.proof.release_status,
            ReleaseStatus::ExitedClean | ReleaseStatus::Signaled | ReleaseStatus::NonzeroExit
        ) && self.proof.boundary_empty_after_exit == Some(true)
    }
}

fn release_spelling(status: crate::supervision::journal::ReleaseStatus) -> &'static str {
    use crate::supervision::journal::ReleaseStatus;
    match status {
        ReleaseStatus::Suspended => "suspended",
        ReleaseStatus::ResumedContained => "resumed-contained",
        ReleaseStatus::ExitedClean => "exited-clean",
        ReleaseStatus::Signaled => "signaled",
        ReleaseStatus::NonzeroExit => "nonzero-exit",
        ReleaseStatus::Unknown => "unknown",
    }
}

/// The three — and only three — terminal classifications a restart can reach for
/// an interrupted canonical write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupClassification {
    /// The exact pre-state is on disk and the helper is provably gone. Nothing
    /// happened; the transaction compensates.
    ExactPreState,
    /// The exact post-state is on disk **and** the linked `process_launch` proof
    /// independently shows a clean terminal helper. Both halves; the transaction
    /// commits.
    ExactPostState,
    /// Neither exact state is establishable. Blocks further mutation pending
    /// operator repair.
    Quarantine {
        /// Exactly why neither state could be established.
        reason: String,
    },
}

impl StartupClassification {
    /// The disposition this classification settles as.
    #[must_use]
    pub const fn disposition(&self) -> Disposition {
        match self {
            Self::ExactPreState => Disposition::Compensated,
            Self::ExactPostState => Disposition::Committed,
            Self::Quarantine { .. } => Disposition::Quarantined,
        }
    }

    /// The canonical terminal reason for this classification.
    #[must_use]
    pub fn terminal_reason(&self) -> String {
        match self {
            Self::ExactPreState => CANONICAL_PRE_STATE_RESTORED.to_owned(),
            Self::ExactPostState => CANONICAL_POST_STATE_PROVEN.to_owned(),
            Self::Quarantine { reason } => format!("{CANONICAL_UNPROVEN}: {reason}"),
        }
    }

    /// Whether this classification blocks the next mutation.
    #[must_use]
    pub const fn blocks_mutation(&self) -> bool {
        self.disposition().blocks_mutation()
    }
}

impl fmt::Display for StartupClassification {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExactPreState => formatter.write_str("exact_pre_state"),
            Self::ExactPostState => formatter.write_str("exact_post_state"),
            Self::Quarantine { .. } => formatter.write_str("quarantine"),
        }
    }
}

/// Classify one interrupted or completed canonical write.
///
/// `observed` is what the touched paths look like **now**, read from the leased
/// root. `linked` is the `process_launch` proof read back **independently** from
/// the durable journal of the transaction named by
/// `required_subproofs.process_launch_transaction_id`.
///
/// The ordering below is normative:
///
/// 1. a mutating record with no `required_subproofs` link is unresolvable — the
///    composite rule has nothing to check, so it can never commit;
/// 2. a missing or mismatched linked proof is quarantine, whatever the bytes say;
/// 3. a linked process that is not provably gone is quarantine, whatever the bytes
///    say — a live survivor may still write more;
/// 4. bytes equal to the intended post-state commit **only** when the linked proof
///    is a clean terminal process; matching bytes with an unclean proof are
///    quarantine, never a commit and never a repairable compensation;
/// 5. bytes equal to the pre-state compensate;
/// 6. anything else — a partial, foreign or unrecognised state — is quarantine.
#[must_use]
pub fn classify_startup(
    record: &FileRecord,
    observed: &[FileEntry],
    linked: Option<&LinkedProof>,
) -> StartupClassification {
    let Some(required) = record.required_launch_transaction_id.as_deref() else {
        return StartupClassification::Quarantine {
            reason: "the record carries no required_subproofs.process_launch_transaction_id, so \
                     the composed process_launch + file proof rule has nothing to verify"
                .to_owned(),
        };
    };
    let Some(linked) = linked else {
        return StartupClassification::Quarantine {
            reason: format!(
                "the linked process_launch proof for transaction {required} is missing, so no \
                 amount of matching file bytes can establish the post-state"
            ),
        };
    };
    if linked.transaction_id != required {
        return StartupClassification::Quarantine {
            reason: format!(
                "the available process_launch proof belongs to transaction {} but this \
                 transaction requires {required}",
                linked.transaction_id
            ),
        };
    }
    if !linked.is_terminal() {
        return StartupClassification::Quarantine {
            reason: format!(
                "the linked helper process is not provably gone ({}), so the on-disk state is not \
                 settled and may still change",
                release_spelling(linked.proof.release_status)
            ),
        };
    }

    let matches_all = |expected: &[FileEntry]| -> bool {
        !expected.is_empty()
            && expected.iter().all(|entry| {
                observed
                    .iter()
                    .any(|seen| seen.path == entry.path && entry.matches(seen))
            })
    };

    if matches_all(&record.intended_post_hashes) {
        if linked.satisfies(record) {
            return StartupClassification::ExactPostState;
        }
        return StartupClassification::Quarantine {
            reason: linked.refusal(record).unwrap_or_else(|| {
                "the linked process_launch proof does not establish a clean terminal helper"
                    .to_owned()
            }),
        };
    }
    if matches_all(&record.pre_hashes) {
        return StartupClassification::ExactPreState;
    }
    StartupClassification::Quarantine {
        reason: "the observed paths match neither the recorded pre-state nor the intended \
                 post-state"
            .to_owned(),
    }
}

/// Refuse an `acknowledge-safe-state` repair that is not backed by proof.
///
/// A quarantined canonical write can only be acknowledged as safe when the exact
/// pre-state is establishable. In particular, a record whose file bytes match the
/// intended post-state but whose linked proof is missing, mismatched, live,
/// signalled, nonzero-exit or non-empty-boundary can **never** be acknowledged:
/// doing so would assert a post-state that is not proven.
///
/// # Errors
/// Returns [`CanonicalError::RepairRefused`] when the exact pre-state is not
/// establishable.
pub fn acknowledge_safe_state(
    record: &FileRecord,
    observed: &[FileEntry],
    linked: Option<&LinkedProof>,
) -> Result<Disposition, CanonicalError> {
    match classify_startup(record, observed, linked) {
        StartupClassification::ExactPreState => Ok(Disposition::Compensated),
        StartupClassification::ExactPostState => Err(CanonicalError::RepairRefused {
            transaction_id: record.envelope.transaction_id.clone(),
            reason: "the exact post-state is proven, so acknowledging a safe pre-state would \
                     assert something false"
                .to_owned(),
        }),
        StartupClassification::Quarantine { reason } => Err(CanonicalError::RepairRefused {
            transaction_id: record.envelope.transaction_id.clone(),
            reason: format!("the exact pre-state is not establishable: {reason}"),
        }),
    }
}

/// Refuse a `compensate` repair that is not backed by proof.
///
/// Compensation asserts that nothing happened. That is only assertable when the
/// exact pre-state is establishable; matching post-state bytes with an unproven
/// linked launch are neither committable nor compensatable, which is exactly what
/// quarantine means.
///
/// # Errors
/// Returns [`CanonicalError::RepairRefused`] when compensation is not provable.
pub fn compensate(
    record: &FileRecord,
    observed: &[FileEntry],
    linked: Option<&LinkedProof>,
) -> Result<Disposition, CanonicalError> {
    match classify_startup(record, observed, linked) {
        StartupClassification::ExactPreState => Ok(Disposition::Compensated),
        StartupClassification::ExactPostState => Err(CanonicalError::RepairRefused {
            transaction_id: record.envelope.transaction_id.clone(),
            reason: "the effect is proven to have happened, so it cannot be compensated as if it \
                     had not"
                .to_owned(),
        }),
        StartupClassification::Quarantine { reason } => Err(CanonicalError::RepairRefused {
            transaction_id: record.envelope.transaction_id.clone(),
            reason: format!(
                "compensation would assert that nothing happened, which is not provable: {reason}"
            ),
        }),
    }
}

/// The durable journal for canonical-write transactions, as the engine sees it.
///
/// This is a **port over the same Todo 6 store** the launch journal uses, not a
/// second persistence layer: the record shape is Todo 6's canonical schema, the
/// phases are Todo 6's four fsynced phases and the dispositions are Todo 6's
/// three. Every method is expected to be durable (fsynced) before it returns.
pub trait FileJournal {
    /// Durably record `record` at its current phase, fsyncing before returning.
    ///
    /// # Errors
    /// Returns [`JournalError`] when the record is not canonical or the durable
    /// write failed. The caller must treat any error as "this phase is not
    /// durable" and must not proceed to the next one.
    fn write_phase(&self, record: &FileRecord) -> Result<(), JournalError>;

    /// Durably record one of exactly three terminal dispositions.
    ///
    /// # Errors
    /// Returns [`JournalError`] when the record is not canonical or the durable
    /// write failed.
    fn settle(
        &self,
        record: &FileRecord,
        disposition: Disposition,
        reason: &str,
        proof: Option<&FileProof>,
    ) -> Result<(), JournalError>;

    /// Every canonical-write transaction that is journaled but not yet settled.
    ///
    /// # Errors
    /// Returns [`JournalError`] when the durable store cannot be read.
    fn open_transactions(&self) -> Result<Vec<FileRecord>, JournalError>;

    /// Transactions currently quarantined. While this is non-empty, mutation is
    /// blocked.
    ///
    /// # Errors
    /// Returns [`JournalError`] when the durable store cannot be read.
    fn quarantined(&self) -> Result<Vec<String>, JournalError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervision::journal::{
        BoundaryMembership, OperationFamily, ProjectIdentityClaim, ReleaseStatus, TransportMode,
    };

    const LAUNCH_TX: &str = "00000000-0000-4000-8000-000000000171";
    const FILE_TX: &str = "00000000-0000-4000-8000-000000000170";

    fn envelope() -> TransactionEnvelope {
        TransactionEnvelope {
            transaction_id: FILE_TX.to_owned(),
            project: ProjectIdentityClaim {
                object_identity: format!("project_{}", "a".repeat(64)),
                root_object: "unix:dev=1,ino=2".to_owned(),
                file_object: "unix:dev=1,ino=3".to_owned(),
                generation: 17,
            },
            transport_mode: TransportMode::Stdio,
            principal: None,
            session: None,
            operation_family: OperationFamily::ProjectMutation,
            tool_name: "canonical.create_resource".to_owned(),
            idempotency_id: "idem-17".to_owned(),
            canonical_request_hash: "b".repeat(64),
            canonical_request_bytes: 128,
            opened_at_unix_ms: 1_700_000_000_000,
        }
    }

    fn record() -> FileRecord {
        FileRecord {
            envelope: envelope(),
            phase: LaunchPhase::EffectStartedFsynced,
            touched: vec![(
                "scenes/main.tscn".to_owned(),
                Some("unix:dev=1,ino=9".to_owned()),
            )],
            pre_hashes: vec![FileEntry::absent("scenes/main.tscn", FileOperation::Create)],
            staged: vec![StagedOutput::of(
                "godot_mcp_helpers/2/staging/abc.tscn",
                "0123456789abcdef",
                b"[gd_scene]\n",
            )],
            intended_post_hashes: vec![FileEntry::present(
                "scenes/main.tscn",
                FileOperation::Create,
                b"[gd_scene]\n",
            )],
            proof: None,
            required_launch_transaction_id: Some(LAUNCH_TX.to_owned()),
        }
    }

    fn clean_proof() -> LinkedProof {
        LinkedProof {
            transaction_id: LAUNCH_TX.to_owned(),
            proof: ProcessLaunchProof {
                boundary_identity: "container:tx-171".to_owned(),
                pid: Some(4242),
                start_time: Some("991122".to_owned()),
                boundary_membership: BoundaryMembership::Confirmed,
                release_status: ReleaseStatus::ExitedClean,
                exit_code: Some(0),
                signal: None,
                boundary_empty_after_exit: Some(true),
            },
        }
    }

    fn observed_post() -> Vec<FileEntry> {
        vec![FileEntry::present(
            "scenes/main.tscn",
            FileOperation::Create,
            b"[gd_scene]\n",
        )]
    }

    fn observed_pre() -> Vec<FileEntry> {
        vec![FileEntry::absent("scenes/main.tscn", FileOperation::Create)]
    }

    #[test]
    fn a_canonical_write_is_always_a_file_effect_and_never_editor_runtime() {
        let value = record().to_canonical_value().expect("canonical record");
        assert_eq!(value["effect_kind"], json!("file"));
        assert_eq!(value["effect_boundary"], json!("file"));
        assert_ne!(value["effect_kind"], json!("editor_runtime"));
        assert_eq!(value["operation_family"], json!("project_mutation"));
        assert_eq!(
            value["required_subproofs"]["process_launch_transaction_id"],
            json!(LAUNCH_TX)
        );
        assert_eq!(value["pre_hashes"][0]["existence"], json!("absent"));
        assert_eq!(
            value["intended_post_hashes"][0]["path"],
            json!("scenes/main.tscn")
        );
        assert_eq!(
            value["staged_write_set"][0]["staging_id"],
            json!("0123456789abcdef")
        );
    }

    #[test]
    fn a_record_round_trips_through_its_canonical_value() {
        let original = record();
        let value = original.to_canonical_value().expect("canonical record");
        let restored = FileRecord::from_canonical_value(&value).expect("round trip");
        assert_eq!(restored, original);
        assert_eq!(
            restored.required_launch_transaction_id.as_deref(),
            Some(LAUNCH_TX)
        );
    }

    #[test]
    fn a_launch_proof_is_never_read_back_as_a_file_proof() {
        let launch = clean_proof().proof.to_canonical_value();
        assert!(matches!(
            FileProof::from_canonical_value(&launch),
            Err(JournalError::Payload(_))
        ));
    }

    #[test]
    fn matching_post_bytes_commit_only_with_a_clean_terminal_linked_proof() {
        let record = record();
        assert_eq!(
            classify_startup(&record, &observed_post(), Some(&clean_proof())),
            StartupClassification::ExactPostState
        );
        assert_eq!(
            classify_startup(&record, &observed_post(), Some(&clean_proof())).disposition(),
            Disposition::Committed
        );
    }

    #[test]
    fn matching_post_bytes_with_every_unclean_linked_proof_stay_quarantined() {
        let record = record();
        let mut unclean: Vec<(&str, Option<LinkedProof>)> = vec![("missing", None)];

        let mut mismatched = clean_proof();
        mismatched.transaction_id = "00000000-0000-4000-8000-0000000001ff".to_owned();
        unclean.push(("mismatched", Some(mismatched)));

        let mut live = clean_proof();
        live.proof.release_status = ReleaseStatus::ResumedContained;
        live.proof.exit_code = None;
        live.proof.boundary_empty_after_exit = Some(false);
        unclean.push(("live-survivor", Some(live)));

        let mut signaled = clean_proof();
        signaled.proof.release_status = ReleaseStatus::Signaled;
        signaled.proof.signal = Some("SIGKILL".to_owned());
        signaled.proof.exit_code = None;
        unclean.push(("signaled", Some(signaled)));

        let mut nonzero = clean_proof();
        nonzero.proof.release_status = ReleaseStatus::NonzeroExit;
        nonzero.proof.exit_code = Some(70);
        unclean.push(("nonzero-exit", Some(nonzero)));

        let mut unconfirmed = clean_proof();
        unconfirmed.proof.boundary_membership = BoundaryMembership::Unconfirmed;
        unclean.push(("unconfirmed-membership", Some(unconfirmed)));

        let mut occupied = clean_proof();
        occupied.proof.boundary_empty_after_exit = Some(false);
        unclean.push(("non-empty-boundary", Some(occupied)));

        for (label, linked) in unclean {
            let classification = classify_startup(&record, &observed_post(), linked.as_ref());
            assert!(
                matches!(classification, StartupClassification::Quarantine { .. }),
                "{label}: matching file bytes must never commit without a clean terminal linked \
                 proof, got {classification}"
            );
            assert!(classification.blocks_mutation(), "{label} blocks mutation");
            assert!(
                acknowledge_safe_state(&record, &observed_post(), linked.as_ref()).is_err(),
                "{label} cannot be acknowledged as a safe pre-state"
            );
            assert!(
                compensate(&record, &observed_post(), linked.as_ref()).is_err(),
                "{label} cannot be compensated as if nothing had happened"
            );
        }
    }

    #[test]
    fn the_exact_pre_state_compensates_and_can_be_acknowledged() {
        let record = record();
        let mut exited = clean_proof();
        exited.proof.release_status = ReleaseStatus::NonzeroExit;
        exited.proof.exit_code = Some(65);
        let classification = classify_startup(&record, &observed_pre(), Some(&exited));
        assert_eq!(classification, StartupClassification::ExactPreState);
        assert_eq!(classification.disposition(), Disposition::Compensated);
        assert!(!classification.blocks_mutation());
        assert_eq!(
            acknowledge_safe_state(&record, &observed_pre(), Some(&exited))
                .expect("the exact pre-state is establishable"),
            Disposition::Compensated
        );
        assert_eq!(
            compensate(&record, &observed_pre(), Some(&exited)).expect("nothing happened"),
            Disposition::Compensated
        );
    }

    #[test]
    fn a_proven_post_state_can_neither_be_acknowledged_away_nor_compensated() {
        let record = record();
        assert!(
            acknowledge_safe_state(&record, &observed_post(), Some(&clean_proof())).is_err(),
            "a proven post-state is not a safe pre-state"
        );
        assert!(
            compensate(&record, &observed_post(), Some(&clean_proof())).is_err(),
            "a proven effect is never compensated as if it had not happened"
        );
    }

    #[test]
    fn a_record_without_a_required_subproof_link_can_never_commit() {
        let mut orphan = record();
        orphan.required_launch_transaction_id = None;
        let classification = classify_startup(&orphan, &observed_post(), Some(&clean_proof()));
        assert!(matches!(
            classification,
            StartupClassification::Quarantine { .. }
        ));
    }

    #[test]
    fn a_state_matching_neither_recorded_state_is_quarantine() {
        let record = record();
        let foreign = vec![FileEntry::present(
            "scenes/main.tscn",
            FileOperation::Create,
            b"something else entirely",
        )];
        assert!(matches!(
            classify_startup(&record, &foreign, Some(&clean_proof())),
            StartupClassification::Quarantine { .. }
        ));
    }

    #[test]
    fn there_are_exactly_three_terminal_classifications() {
        let all = [
            StartupClassification::ExactPreState,
            StartupClassification::ExactPostState,
            StartupClassification::Quarantine {
                reason: String::new(),
            },
        ];
        let dispositions: std::collections::BTreeSet<&str> = all
            .iter()
            .map(|classification| classification.disposition().as_str())
            .collect();
        assert_eq!(
            dispositions.len(),
            3,
            "the three classifications map onto Todo 6's three dispositions, one each"
        );
        assert_eq!(Disposition::ALL.len(), 3);
    }

    #[test]
    fn a_terminal_record_carries_its_disposition_as_the_canonical_phase() {
        let record = record();
        for disposition in Disposition::ALL {
            let value = canonical_terminal_value(&record, disposition, "because", None)
                .expect("terminal record");
            assert_eq!(value["phase"], json!(disposition.as_str()));
            assert_eq!(value["terminal_reason"], json!("because"));
        }
    }
}
