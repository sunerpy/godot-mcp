//! Canonical durable transaction records.
//!
//! Every record written to (or read from) the durable store is validated against
//! the canonical restrictive schema `schemas/transactions/v1.json`, which is
//! embedded at compile time so a shipped binary can never disagree with the
//! repository's canonical schema. Serialization is canonical JSON: UTF-8, object
//! keys in sorted order (`serde_json`'s default map ordering), no trailing
//! whitespace.

use std::fmt;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::jsonschema::{SchemaError, validate};
use godot_mcp_domain::objid::StableObjectId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The canonical journal schema, embedded from the repository.
pub(crate) const TRANSACTION_SCHEMA_JSON: &str =
    include_str!("../../../../schemas/transactions/v1.json");

/// Version marker carried by every record.
pub(crate) const SCHEMA_VERSION: &str = "1";

/// Which typed effect proof a transaction requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EffectKind {
    /// Existence + content-hash proof through Todo 2 `SafeRoot` primitives.
    File,
    /// Containment-boundary / PID / start-time / release proof (Todo 14).
    ProcessLaunch,
    /// Authoritative bridge receipt plus live probe (Todo 19/23/25).
    EditorRuntime,
}

impl EffectKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::ProcessLaunch => "process_launch",
            Self::EditorRuntime => "editor_runtime",
        }
    }
}

/// The five frozen effect boundaries. The boundary names *where* an effect is
/// executed; [`EffectKind`] names *how* it is proven.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EffectBoundaryKind {
    /// Descriptor-relative project file writes.
    File,
    /// Supervised child processes.
    Process,
    /// Language/debug adapter channels.
    LspDap,
    /// Live editor plugin bridge.
    Editor,
    /// Running game instance.
    Runtime,
}

/// Transport/auth mode that established the requesting identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TransportMode {
    /// Local stdio transport.
    Stdio,
    /// HTTP with a per-launch user-scoped secret.
    TrustedLocal,
    /// HTTP behind an authenticated proxy hop with a frozen OIDC principal.
    ProxiedOidc,
}

/// Operation family the transaction belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationFamily {
    ProjectMutation,
    EditorMutation,
    RuntimeExecution,
    DestructiveExport,
    DiagnosticProbe,
}

/// The four fsynced legal phases plus the two explicit terminal records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    IntentFsynced,
    StagedFsynced,
    EffectStartedFsynced,
    CommittedFsynced,
    Compensated,
    Quarantined,
}

impl Phase {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::IntentFsynced => "intent_fsynced",
            Self::StagedFsynced => "staged_fsynced",
            Self::EffectStartedFsynced => "effect_started_fsynced",
            Self::CommittedFsynced => "committed_fsynced",
            Self::Compensated => "compensated",
            Self::Quarantined => "quarantined",
        }
    }

    /// Whether this phase is terminal (no further transition is ever legal).
    pub(crate) const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::CommittedFsynced | Self::Compensated | Self::Quarantined
        )
    }

    /// Exactly the legal successors of this phase. Anything else is rejected
    /// with `conflict` and leaves the durable record untouched.
    pub(crate) fn legal_successors(self) -> &'static [Self] {
        match self {
            Self::IntentFsynced => &[Self::StagedFsynced, Self::Compensated, Self::Quarantined],
            Self::StagedFsynced => &[
                Self::EffectStartedFsynced,
                Self::Compensated,
                Self::Quarantined,
            ],
            Self::EffectStartedFsynced => {
                &[Self::CommittedFsynced, Self::Compensated, Self::Quarantined]
            }
            Self::CommittedFsynced | Self::Compensated | Self::Quarantined => &[],
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Per-path mutation shape. Determines which proof rule applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FileOperation {
    Create,
    Update,
    Delete,
}

/// Existence tombstone observed (or intended) for one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Existence {
    Present,
    Absent,
}

/// One path's existence + content-hash state.
///
/// `sha256` is `None` exactly when `existence` is `Absent`: an absent path has no
/// content to hash, so a `create` pre-state and a `delete` post-state are proven
/// by the existence tombstone alone, never by a hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileStateEntry {
    pub(crate) path: String,
    pub(crate) operation: FileOperation,
    pub(crate) existence: Existence,
    pub(crate) sha256: Option<String>,
}

/// One touched path bound to the OS object identity observed for it (or `None`
/// when the path was absent at observation time).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TouchedObject {
    pub(crate) path: String,
    pub(crate) object_identity: Option<String>,
}

/// One staged output byte-range, durably written before any effect starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StagedWrite {
    pub(crate) path: String,
    pub(crate) staging_id: String,
    pub(crate) sha256: String,
    pub(crate) byte_length: u64,
}

/// Confirmation that a launched process was observed inside its boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BoundaryMembership {
    Confirmed,
    Unconfirmed,
}

/// Release/terminal disposition of a supervised process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ReleaseStatus {
    #[serde(rename = "suspended")]
    Suspended,
    #[serde(rename = "resumed-contained")]
    ResumedContained,
    #[serde(rename = "exited-clean")]
    ExitedClean,
    #[serde(rename = "signaled")]
    Signaled,
    #[serde(rename = "nonzero-exit")]
    NonzeroExit,
    #[serde(rename = "unknown")]
    Unknown,
}

/// Exactly the three typed durable proof forms. There is deliberately no fourth
/// variant: Todo 17's composite requirement is expressed as a `File` proof plus
/// [`RequiredSubproofs::process_launch_transaction_id`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TypedProof {
    /// Existence + content-hash proof for every touched path.
    File { entries: Vec<FileStateEntry> },
    /// Containment/PID/start-time/release proof, never a content hash.
    ProcessLaunch {
        boundary_identity: String,
        pid: Option<u32>,
        start_time: Option<String>,
        boundary_membership: BoundaryMembership,
        release_status: ReleaseStatus,
        exit_code: Option<i64>,
        signal: Option<String>,
        boundary_empty_after_exit: Option<bool>,
    },
    /// Authoritative bridge receipt plus, where reachable, a live state probe.
    EditorRuntime {
        bridge_reachable: bool,
        receipt_transaction_id: Option<String>,
        receipt_action_id: Option<String>,
        live_state_probe: Option<String>,
    },
}

impl TypedProof {
    pub(crate) const fn kind(&self) -> EffectKind {
        match self {
            Self::File { .. } => EffectKind::File,
            Self::ProcessLaunch { .. } => EffectKind::ProcessLaunch,
            Self::EditorRuntime { .. } => EffectKind::EditorRuntime,
        }
    }

    /// Whether a `process_launch` proof satisfies the terminal-completion
    /// requirement: an observed clean exit (code 0, no signal), confirmed
    /// boundary membership, and a proven-empty boundary after exit. A live
    /// survivor is never accepted.
    pub(crate) fn is_clean_terminal_process(&self) -> bool {
        match self {
            Self::ProcessLaunch {
                boundary_membership,
                release_status,
                exit_code,
                signal,
                boundary_empty_after_exit,
                ..
            } => {
                *boundary_membership == BoundaryMembership::Confirmed
                    && *release_status == ReleaseStatus::ExitedClean
                    && *exit_code == Some(0)
                    && signal.is_none()
                    && *boundary_empty_after_exit == Some(true)
            }
            _ => false,
        }
    }
}

/// Durable link to the canonical supervised-launch transaction whose independent
/// process proof is required by a composite file or outer process transaction.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequiredSubproofs {
    pub(crate) process_launch_transaction_id: Option<String>,
}

/// One durable phase transition, ordered by the store's monotonic sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PhaseStep {
    pub(crate) phase: Phase,
    pub(crate) sequence: u64,
}

/// Caller-visible terminal verdict retained independently from the human-readable
/// terminal reason. Replays consume this type directly; they never parse
/// `terminal_reason` to rediscover whether the original operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TerminalOutcome {
    /// The original operation completed successfully.
    Succeeded,
    /// The original operation failed with one stable public error category.
    Failed { code: ErrorKind },
}

impl TerminalOutcome {
    pub(crate) const fn failed(code: ErrorKind) -> Self {
        Self::Failed { code }
    }
}

/// The complete durable transaction record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransactionRecord {
    pub(crate) schema_version: String,
    pub(crate) transaction_id: String,
    pub(crate) effect_kind: EffectKind,
    pub(crate) effect_boundary: EffectBoundaryKind,
    pub(crate) project_object_identity: String,
    pub(crate) project_root_object: String,
    pub(crate) project_file_object: String,
    pub(crate) project_generation: u64,
    pub(crate) transport_mode: TransportMode,
    pub(crate) principal: Option<String>,
    pub(crate) session: Option<String>,
    pub(crate) operation_family: OperationFamily,
    pub(crate) tool_name: String,
    pub(crate) idempotency_id: String,
    pub(crate) canonical_request_hash: String,
    pub(crate) canonical_request_bytes: u64,
    pub(crate) touched_paths: Vec<String>,
    pub(crate) touched_object_identities: Vec<TouchedObject>,
    pub(crate) pre_hashes: Vec<FileStateEntry>,
    pub(crate) staged_write_set: Vec<StagedWrite>,
    pub(crate) intended_post_hashes: Vec<FileStateEntry>,
    pub(crate) phase: Phase,
    pub(crate) phase_history: Vec<PhaseStep>,
    pub(crate) proof: Option<TypedProof>,
    pub(crate) required_subproofs: Option<RequiredSubproofs>,
    pub(crate) terminal_reason: Option<String>,
    /// Optional only for compatibility with v1 records written before exact
    /// replay retained a typed verdict. New terminal writes always set it.
    #[serde(default)]
    pub(crate) terminal_outcome: Option<TerminalOutcome>,
    pub(crate) opened_at_unix_ms: u64,
}

impl TransactionRecord {
    /// Serialize to canonical bytes and validate against the canonical schema.
    pub(crate) fn to_canonical_bytes(&self) -> Result<Vec<u8>, RecordError> {
        let value = serde_json::to_value(self).map_err(|_| RecordError::Serialization)?;
        validate_value(&value)?;
        serde_json::to_vec(&value).map_err(|_| RecordError::Serialization)
    }

    /// Parse canonical bytes, validating against the canonical schema first so a
    /// tampered or foreign record can never be loaded as a valid transaction.
    pub(crate) fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, RecordError> {
        let value: Value = serde_json::from_slice(bytes).map_err(|_| RecordError::Malformed)?;
        validate_value(&value)?;
        serde_json::from_value(value).map_err(|_| RecordError::Malformed)
    }

    /// Composite requirement, if this record carries one.
    pub(crate) fn required_process_transaction(&self) -> Option<&str> {
        self.required_subproofs
            .as_ref()
            .and_then(|subproofs| subproofs.process_launch_transaction_id.as_deref())
    }
}

/// Validate an already-parsed record value against the canonical schema.
pub(crate) fn validate_value(value: &Value) -> Result<(), RecordError> {
    let schema: Value =
        serde_json::from_str(TRANSACTION_SCHEMA_JSON).map_err(|_| RecordError::SchemaUnusable)?;
    validate(&schema, value).map_err(RecordError::Schema)
}

/// A record that cannot be represented, or does not satisfy the canonical schema.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum RecordError {
    /// The canonical schema itself could not be parsed.
    #[error("the canonical transaction schema is unusable")]
    SchemaUnusable,
    /// The record does not satisfy the canonical schema.
    #[error("transaction record violates the canonical schema: {0}")]
    Schema(SchemaError),
    /// The record could not be serialized.
    #[error("transaction record could not be serialized")]
    Serialization,
    /// Stored bytes are not a valid canonical record.
    #[error("stored transaction record is malformed")]
    Malformed,
}

/// Encode a held-handle OS object identity into the canonical journal spelling.
///
/// The encoding is platform-tagged so a Unix identity can never compare equal to
/// a Windows identity, matching Todo 2's `StableObjectId` invariant.
pub(crate) fn encode_object_identity(identity: StableObjectId) -> String {
    match identity {
        StableObjectId::Unix { dev, ino } => format!("unix:dev={dev},ino={ino}"),
        StableObjectId::Windows {
            volume_serial,
            file_id,
        } => {
            let mut hex = String::with_capacity(32);
            for byte in file_id {
                use fmt::Write as _;
                let _ = write!(&mut hex, "{byte:02x}");
            }
            format!("windows:volume={volume_serial},file_id={hex}")
        }
    }
}

/// Lowercase hexadecimal SHA-256 of `bytes`.
pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

/// Canonical request hash: SHA-256 over the canonical JSON encoding of the
/// complete request (tool name plus every argument), never a truncation.
pub(crate) fn canonical_request(tool: &str, arguments: &Value) -> (String, u64) {
    let canonical = serde_json::json!({ "arguments": arguments, "tool": tool });
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    (hex_sha256(&bytes), bytes.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> TransactionRecord {
        TransactionRecord {
            schema_version: SCHEMA_VERSION.to_owned(),
            transaction_id: "00000000-0000-4000-8000-000000000101".to_owned(),
            effect_kind: EffectKind::File,
            effect_boundary: EffectBoundaryKind::File,
            project_object_identity: format!("project_{}", "a".repeat(64)),
            project_root_object: "unix:dev=1,ino=2".to_owned(),
            project_file_object: "unix:dev=1,ino=3".to_owned(),
            project_generation: 1,
            transport_mode: TransportMode::Stdio,
            principal: Some("principal_0123456789abcdef".to_owned()),
            session: None,
            operation_family: OperationFamily::ProjectMutation,
            tool_name: "resource.update".to_owned(),
            idempotency_id: "idem-1".to_owned(),
            canonical_request_hash: "b".repeat(64),
            canonical_request_bytes: 12,
            touched_paths: vec!["scenes/main.tscn".to_owned()],
            touched_object_identities: vec![TouchedObject {
                path: "scenes/main.tscn".to_owned(),
                object_identity: None,
            }],
            pre_hashes: vec![FileStateEntry {
                path: "scenes/main.tscn".to_owned(),
                operation: FileOperation::Create,
                existence: Existence::Absent,
                sha256: None,
            }],
            staged_write_set: Vec::new(),
            intended_post_hashes: vec![FileStateEntry {
                path: "scenes/main.tscn".to_owned(),
                operation: FileOperation::Create,
                existence: Existence::Present,
                sha256: Some("c".repeat(64)),
            }],
            phase: Phase::IntentFsynced,
            phase_history: vec![PhaseStep {
                phase: Phase::IntentFsynced,
                sequence: 1,
            }],
            proof: None,
            required_subproofs: None,
            terminal_reason: None,
            terminal_outcome: None,
            opened_at_unix_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn canonical_round_trip_validates() {
        let record = sample();
        let bytes = record.to_canonical_bytes().expect("canonical bytes");
        assert_eq!(
            TransactionRecord::from_canonical_bytes(&bytes).expect("round trip"),
            record
        );
    }

    #[test]
    fn canonical_bytes_are_sorted_and_have_no_trailing_whitespace() {
        let bytes = sample().to_canonical_bytes().expect("canonical bytes");
        let text = String::from_utf8(bytes).expect("utf-8");
        assert!(text.starts_with("{\"canonical_request_bytes\":"), "{text}");
        assert!(!text.ends_with('\n'));
    }

    #[test]
    fn unknown_field_is_rejected_by_the_canonical_schema() {
        let mut value = serde_json::to_value(sample()).expect("value");
        value
            .as_object_mut()
            .expect("object")
            .insert("extra".to_owned(), Value::Bool(true));
        let bytes = serde_json::to_vec(&value).expect("bytes");
        assert!(matches!(
            TransactionRecord::from_canonical_bytes(&bytes),
            Err(RecordError::Schema(_))
        ));
    }

    #[test]
    fn legacy_record_without_terminal_outcome_remains_readable() {
        let mut value = serde_json::to_value(sample()).expect("value");
        value
            .as_object_mut()
            .expect("object")
            .remove("terminal_outcome");
        let bytes = serde_json::to_vec(&value).expect("bytes");

        let decoded = TransactionRecord::from_canonical_bytes(&bytes)
            .expect("a v1 record written before terminal outcomes remains readable");
        assert_eq!(decoded.terminal_outcome, None);
    }

    #[test]
    fn unreadable_terminal_outcome_code_is_rejected() {
        let mut value = serde_json::to_value(sample()).expect("value");
        value.as_object_mut().expect("object").insert(
            "terminal_outcome".to_owned(),
            serde_json::json!({ "verdict": "failed", "code": "not_a_real_code" }),
        );
        let bytes = serde_json::to_vec(&value).expect("bytes");

        assert!(matches!(
            TransactionRecord::from_canonical_bytes(&bytes),
            Err(RecordError::Schema(_))
        ));
    }

    #[test]
    fn each_typed_proof_form_validates() {
        for proof in [
            TypedProof::File {
                entries: vec![FileStateEntry {
                    path: "a.tscn".to_owned(),
                    operation: FileOperation::Delete,
                    existence: Existence::Absent,
                    sha256: None,
                }],
            },
            TypedProof::ProcessLaunch {
                boundary_identity: "cgroup:/godot-mcp/tx".to_owned(),
                pid: Some(4242),
                start_time: Some("12345".to_owned()),
                boundary_membership: BoundaryMembership::Confirmed,
                release_status: ReleaseStatus::ExitedClean,
                exit_code: Some(0),
                signal: None,
                boundary_empty_after_exit: Some(true),
            },
            TypedProof::EditorRuntime {
                bridge_reachable: true,
                receipt_transaction_id: Some("00000000-0000-4000-8000-000000000101".to_owned()),
                receipt_action_id: Some("undo-42".to_owned()),
                live_state_probe: Some("node:/root/Main".to_owned()),
            },
        ] {
            let mut record = sample();
            record.effect_kind = proof.kind();
            record.proof = Some(proof);
            record.to_canonical_bytes().expect("proof validates");
        }
    }

    #[test]
    fn a_live_survivor_is_never_terminal_proof() {
        let survivor = TypedProof::ProcessLaunch {
            boundary_identity: "cgroup:/godot-mcp/tx".to_owned(),
            pid: Some(4242),
            start_time: Some("12345".to_owned()),
            boundary_membership: BoundaryMembership::Confirmed,
            release_status: ReleaseStatus::ResumedContained,
            exit_code: None,
            signal: None,
            boundary_empty_after_exit: Some(false),
        };
        assert!(!survivor.is_clean_terminal_process());
    }

    #[test]
    fn terminal_phases_have_no_legal_successor() {
        for phase in [
            Phase::CommittedFsynced,
            Phase::Compensated,
            Phase::Quarantined,
        ] {
            assert!(phase.is_terminal());
            assert!(phase.legal_successors().is_empty());
        }
    }
}
