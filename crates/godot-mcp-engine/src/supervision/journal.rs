//! The launch journal **port**.
//!
//! Todo 6 owns the one durable transaction journal, its canonical schema
//! (`schemas/transactions/v1.json`) and its four canonical fsynced phases. This
//! module deliberately owns *none* of that. It is a port: a narrow trait the
//! server layer implements over Todo 6's `TransactionStore`, plus the exact
//! record/proof shapes Todo 6 froze, so the engine can drive the launch protocol
//! without importing the server crate (which the architecture-boundary gate
//! forbids).
//!
//! Three properties make "reuse, never re-create" structural rather than a
//! promise:
//!
//! 1. [`LaunchPhase`] contains **exactly** Todo 6's four canonical fsynced
//!    phases. There is no `process_launch`-specific phase enum, and no fifth
//!    name is representable.
//! 2. [`Disposition`] contains **exactly** the three terminal dispositions. A
//!    fourth classification is not representable.
//! 3. [`LaunchRecord::to_canonical_value`] assembles a record in Todo 6's
//!    canonical shape and validates it against the repository's canonical
//!    schema, embedded at compile time from the *same* file Todo 6 embeds. A
//!    record the canonical schema rejects can never be handed to the journal, so
//!    the engine cannot smuggle a second schema, a second phase set or a fourth
//!    proof kind past this boundary.
//!
//! The launch-specific payload (project generation, root/project identity,
//! binary hash, boundary identity, planned/actual endpoint claims, PID, start
//! time) travels in the canonical record's `staged_write_set` and
//! `touched_object_identities` fields, exactly as the plan requires, never in
//! invented top-level fields.

use std::collections::BTreeMap;
use std::fmt;

use godot_mcp_domain::jsonschema::{SchemaError, validate};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The canonical journal schema, embedded from the repository. This is the exact
/// same file Todo 6's `journal::record` embeds; there is no engine-local copy.
pub const TRANSACTION_SCHEMA_JSON: &str = include_str!("../../../../schemas/transactions/v1.json");

/// Version marker carried by every canonical record.
pub const SCHEMA_VERSION: &str = "1";

/// The single compensation reason this todo records, frozen by the plan.
pub const LAUNCH_ABANDONED_PRE_RELEASE: &str = "launch_abandoned_pre_release";

/// Lowercase hexadecimal SHA-256 of `bytes`.
#[must_use]
pub fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

/// Exactly Todo 6's four canonical fsynced phases — no launch-specific phase
/// enum, and no fifth variant. The declaration order is ascending, so comparing
/// two phases compares how far a transaction durably progressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchPhase {
    /// The durable intent, carrying the already-created boundary's identity (or
    /// the weak path's inert locator). No process exists before this mark.
    IntentFsynced,
    /// The child/supervisor is confirmed created but is not running user code.
    StagedFsynced,
    /// Containment is durably proven and the receipt is fsynced, strictly before
    /// any gate release.
    EffectStartedFsynced,
    /// The gate release/resume completed and was confirmed.
    CommittedFsynced,
}

impl LaunchPhase {
    /// Every canonical phase in ascending order.
    pub const ALL: [Self; 4] = [
        Self::IntentFsynced,
        Self::StagedFsynced,
        Self::EffectStartedFsynced,
        Self::CommittedFsynced,
    ];

    /// Canonical wire spelling, identical to Todo 6's.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IntentFsynced => "intent_fsynced",
            Self::StagedFsynced => "staged_fsynced",
            Self::EffectStartedFsynced => "effect_started_fsynced",
            Self::CommittedFsynced => "committed_fsynced",
        }
    }

    /// Ascending rank, used by reconciliation to inspect which marks are present.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::IntentFsynced => 1,
            Self::StagedFsynced => 2,
            Self::EffectStartedFsynced => 3,
            Self::CommittedFsynced => 4,
        }
    }

    /// The phase that must already be durable before this one may be written.
    #[must_use]
    pub const fn predecessor(self) -> Option<Self> {
        match self {
            Self::IntentFsynced => None,
            Self::StagedFsynced => Some(Self::IntentFsynced),
            Self::EffectStartedFsynced => Some(Self::StagedFsynced),
            Self::CommittedFsynced => Some(Self::EffectStartedFsynced),
        }
    }
}

impl fmt::Display for LaunchPhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Exactly the three terminal dispositions. A fourth classification is not
/// representable, so reconciliation cannot invent one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// Proven not to have completed; every member is proven gone.
    Compensated,
    /// Proven complete, including an independently proven resume/release.
    Committed,
    /// Not independently provable either way. Blocks further mutation pending
    /// Todo-34 operator repair.
    Quarantined,
}

impl Disposition {
    /// Every disposition. There are three, permanently.
    pub const ALL: [Self; 3] = [Self::Compensated, Self::Committed, Self::Quarantined];

    /// Canonical wire spelling, identical to Todo 6's terminal phase names.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compensated => "compensated",
            Self::Committed => "committed_fsynced",
            Self::Quarantined => "quarantined",
        }
    }

    /// Whether this disposition blocks further mutation.
    #[must_use]
    pub const fn blocks_mutation(self) -> bool {
        matches!(self, Self::Quarantined)
    }
}

impl fmt::Display for Disposition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Whether a launched process was observed inside its boundary. Mirrors Todo 6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryMembership {
    /// Membership was confirmed by live inspection of the boundary.
    Confirmed,
    /// Membership was not confirmed. The gate is never opened in this state.
    Unconfirmed,
}

impl BoundaryMembership {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Unconfirmed => "unconfirmed",
        }
    }
}

/// Release/terminal disposition of a supervised process. Mirrors Todo 6's
/// `ReleaseStatus` wire spellings exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReleaseStatus {
    /// Created and contained, but the gate was never opened.
    #[serde(rename = "suspended")]
    Suspended,
    /// Released and still running inside its boundary.
    #[serde(rename = "resumed-contained")]
    ResumedContained,
    /// Exited with status 0.
    #[serde(rename = "exited-clean")]
    ExitedClean,
    /// Terminated by a signal, including a deadline-driven termination.
    #[serde(rename = "signaled")]
    Signaled,
    /// Exited with a nonzero status.
    #[serde(rename = "nonzero-exit")]
    NonzeroExit,
    /// The release state could not be determined.
    #[serde(rename = "unknown")]
    Unknown,
}

impl ReleaseStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Suspended => "suspended",
            Self::ResumedContained => "resumed-contained",
            Self::ExitedClean => "exited-clean",
            Self::Signaled => "signaled",
            Self::NonzeroExit => "nonzero-exit",
            Self::Unknown => "unknown",
        }
    }
}

/// Todo 6's `process_launch` typed proof, byte-for-byte the same shape. This is
/// the *only* proof form this todo writes; there is no fourth proof kind, and a
/// content hash is never a launch proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessLaunchProof {
    /// The containment boundary's concrete identity.
    pub boundary_identity: String,
    /// The supervised process's PID.
    pub pid: Option<u32>,
    /// The supervised process's start time, paired with the PID so reuse is never
    /// mistaken for the same process.
    pub start_time: Option<String>,
    /// Whether containment was confirmed by live inspection.
    pub boundary_membership: BoundaryMembership,
    /// How the release/run ended.
    pub release_status: ReleaseStatus,
    /// Exit status, when the process exited normally.
    pub exit_code: Option<i64>,
    /// Signal spelling, when the process was signalled.
    pub signal: Option<String>,
    /// Whether the boundary was proven empty after the process ended.
    pub boundary_empty_after_exit: Option<bool>,
}

impl ProcessLaunchProof {
    /// The canonical tagged JSON encoding Todo 6's schema accepts.
    #[must_use]
    pub fn to_canonical_value(&self) -> Value {
        json!({
            "kind": "process_launch",
            "boundary_identity": self.boundary_identity,
            "pid": self.pid,
            "start_time": self.start_time,
            "boundary_membership": self.boundary_membership.as_str(),
            "release_status": self.release_status.as_str(),
            "exit_code": self.exit_code,
            "signal": self.signal,
            "boundary_empty_after_exit": self.boundary_empty_after_exit,
        })
    }

    /// Rebuild a proof from its canonical tagged JSON encoding.
    ///
    /// # Errors
    /// Returns [`JournalError::Payload`] when the value is not a
    /// `process_launch`-tagged proof, so a `file` or `editor_runtime` proof can
    /// never be read back as a launch proof.
    pub fn from_canonical_value(value: &Value) -> Result<Self, JournalError> {
        if value["kind"] != json!("process_launch") {
            return Err(JournalError::Payload(
                "proof is not the process_launch form".to_owned(),
            ));
        }
        let membership = match value["boundary_membership"].as_str() {
            Some("confirmed") => BoundaryMembership::Confirmed,
            Some("unconfirmed") => BoundaryMembership::Unconfirmed,
            _ => {
                return Err(JournalError::Payload(
                    "process_launch proof has no boundary membership".to_owned(),
                ));
            }
        };
        let release = match value["release_status"].as_str() {
            Some("suspended") => ReleaseStatus::Suspended,
            Some("resumed-contained") => ReleaseStatus::ResumedContained,
            Some("exited-clean") => ReleaseStatus::ExitedClean,
            Some("signaled") => ReleaseStatus::Signaled,
            Some("nonzero-exit") => ReleaseStatus::NonzeroExit,
            Some("unknown") => ReleaseStatus::Unknown,
            _ => {
                return Err(JournalError::Payload(
                    "process_launch proof has no release status".to_owned(),
                ));
            }
        };
        Ok(Self {
            boundary_identity: value["boundary_identity"]
                .as_str()
                .ok_or_else(|| {
                    JournalError::Payload(
                        "process_launch proof has no boundary identity".to_owned(),
                    )
                })?
                .to_owned(),
            pid: value["pid"]
                .as_u64()
                .and_then(|pid| u32::try_from(pid).ok()),
            start_time: value["start_time"].as_str().map(str::to_owned),
            boundary_membership: membership,
            release_status: release,
            exit_code: value["exit_code"].as_i64(),
            signal: value["signal"].as_str().map(str::to_owned),
            boundary_empty_after_exit: value["boundary_empty_after_exit"].as_bool(),
        })
    }

    /// Whether this proof satisfies Todo 6's terminal-completion requirement: a
    /// confirmed-contained clean exit with a proven-empty boundary afterwards. A
    /// live survivor is never accepted.
    #[must_use]
    pub fn is_clean_terminal_process(&self) -> bool {
        self.boundary_membership == BoundaryMembership::Confirmed
            && self.release_status == ReleaseStatus::ExitedClean
            && self.exit_code == Some(0)
            && self.signal.is_none()
            && self.boundary_empty_after_exit == Some(true)
    }
}

/// The transport/auth mode that established the requesting identity. Mirrors
/// Todo 6's `TransportMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportMode {
    /// Local stdio transport.
    Stdio,
    /// HTTP with a per-launch user-scoped secret.
    TrustedLocal,
    /// HTTP behind an authenticated proxy hop with a frozen OIDC principal.
    ProxiedOidc,
}

impl TransportMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::TrustedLocal => "trusted_local",
            Self::ProxiedOidc => "proxied_oidc",
        }
    }

    /// Parse Todo 6's canonical wire spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "stdio" => Some(Self::Stdio),
            "trusted_local" => Some(Self::TrustedLocal),
            "proxied_oidc" => Some(Self::ProxiedOidc),
            _ => None,
        }
    }
}

/// Operation family the transaction belongs to. Mirrors Todo 6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationFamily {
    /// A project-file mutation.
    ProjectMutation,
    /// A live-editor mutation.
    EditorMutation,
    /// A supervised runtime execution.
    RuntimeExecution,
    /// A destructive export.
    DestructiveExport,
    /// A read-only diagnostic probe.
    DiagnosticProbe,
}

impl OperationFamily {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ProjectMutation => "project_mutation",
            Self::EditorMutation => "editor_mutation",
            Self::RuntimeExecution => "runtime_execution",
            Self::DestructiveExport => "destructive_export",
            Self::DiagnosticProbe => "diagnostic_probe",
        }
    }

    /// Parse Todo 6's canonical wire spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "project_mutation" => Some(Self::ProjectMutation),
            "editor_mutation" => Some(Self::EditorMutation),
            "runtime_execution" => Some(Self::RuntimeExecution),
            "destructive_export" => Some(Self::DestructiveExport),
            "diagnostic_probe" => Some(Self::DiagnosticProbe),
            _ => None,
        }
    }
}

/// The held project's revalidated identity, as recorded on every canonical
/// record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectIdentityClaim {
    /// Public opaque `project_<64hex>` identity.
    pub object_identity: String,
    /// Encoded root-directory OS object identity.
    pub root_object: String,
    /// Encoded `project.godot` OS object identity.
    pub file_object: String,
    /// Current project generation.
    pub generation: u64,
}

/// The non-launch-specific transaction envelope the canonical record requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionEnvelope {
    /// The canonical transaction ID.
    pub transaction_id: String,
    /// The revalidated project identity.
    pub project: ProjectIdentityClaim,
    /// How the requesting identity was established.
    pub transport_mode: TransportMode,
    /// The mode-specific principal, when there is one.
    pub principal: Option<String>,
    /// The mode-specific session, when there is one.
    pub session: Option<String>,
    /// Which operation family this transaction belongs to.
    pub operation_family: OperationFamily,
    /// The tool that opened the transaction.
    pub tool_name: String,
    /// The caller's idempotency key.
    pub idempotency_id: String,
    /// SHA-256 over the complete canonical request.
    pub canonical_request_hash: String,
    /// Byte length of the canonical request.
    pub canonical_request_bytes: u64,
    /// When the transaction was opened.
    pub opened_at_unix_ms: u64,
}

/// One launch payload document: the launch-specific facts for one phase,
/// serialized to canonical bytes and carried as a `staged_write_set` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPayload {
    /// Logical name of the payload document (its `staged_write_set` path).
    pub name: String,
    /// Canonical payload bytes. Hashed into the staged entry.
    pub bytes: Vec<u8>,
    /// Path → OS object identity pairs recorded as `touched_object_identities`.
    pub touched: Vec<(String, Option<String>)>,
}

impl LaunchPayload {
    /// Build a payload document from a sorted map of launch facts.
    #[must_use]
    pub fn new(
        name: &str,
        facts: &BTreeMap<String, Value>,
        touched: Vec<(String, Option<String>)>,
    ) -> Self {
        let value = Value::Object(facts.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
        let bytes = serde_json::to_vec(&value).unwrap_or_default();
        Self {
            name: name.to_owned(),
            bytes,
            touched,
        }
    }

    /// Deterministic 16-hex staging ID, derived from the payload's own bytes and
    /// name so an identical payload always stages identically.
    #[must_use]
    pub fn staging_id(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.name.as_bytes());
        hasher.update([0_u8]);
        hasher.update(&self.bytes);
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(16);
        for byte in digest.iter().take(8) {
            use fmt::Write as _;
            let _ = write!(&mut hex, "{byte:02x}");
        }
        hex
    }

    /// The payload's parsed facts, for reconciliation.
    ///
    /// # Errors
    /// Returns [`JournalError::Payload`] when the bytes are not a canonical JSON
    /// object.
    pub fn facts(&self) -> Result<BTreeMap<String, Value>, JournalError> {
        let value: Value = serde_json::from_slice(&self.bytes)
            .map_err(|error| JournalError::Payload(error.to_string()))?;
        match value {
            Value::Object(map) => Ok(map.into_iter().collect()),
            _ => Err(JournalError::Payload(
                "launch payload is not a JSON object".to_owned(),
            )),
        }
    }
}

/// One durable phase record: the canonical transaction plus the phase-specific
/// launch payloads it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRecord {
    /// The canonical transaction envelope.
    pub envelope: TransactionEnvelope,
    /// The furthest canonical phase that is durable.
    pub phase: LaunchPhase,
    /// Every payload document staged so far, oldest first.
    pub payloads: Vec<LaunchPayload>,
    /// The typed proof, present from `effect_started_fsynced` onwards.
    pub proof: Option<ProcessLaunchProof>,
}

impl LaunchRecord {
    /// Logical name of the payload document `intent_fsynced` carries.
    pub const INTENT_PAYLOAD: &'static str = "launch/intent.json";

    /// Logical name of the payload document `staged_fsynced` carries.
    pub const STAGED_PAYLOAD: &'static str = "launch/staged.json";

    /// Logical name of the payload document `effect_started_fsynced` carries.
    pub const RECEIPT_PAYLOAD: &'static str = "launch/receipt.json";

    /// Assemble the canonical record value and validate it against the
    /// repository's canonical schema.
    ///
    /// # Errors
    /// Returns [`JournalError::Schema`] when the assembled record violates the
    /// canonical schema, which is what makes an invented field, phase name or
    /// proof kind unrepresentable at this boundary.
    pub fn to_canonical_value(&self) -> Result<Value, JournalError> {
        let mut touched_paths: Vec<String> = Vec::new();
        let mut touched_identities: Vec<Value> = Vec::new();
        let mut staged: Vec<Value> = Vec::new();
        for payload in &self.payloads {
            staged.push(json!({
                "path": payload.name,
                "staging_id": payload.staging_id(),
                "sha256": hex_sha256(&payload.bytes),
                "byte_length": payload.bytes.len() as u64,
            }));
            for (path, identity) in &payload.touched {
                if !touched_paths.iter().any(|existing| existing == path) {
                    touched_paths.push(path.clone());
                    touched_identities.push(json!({
                        "path": path,
                        "object_identity": identity,
                    }));
                }
            }
        }

        let mut history: Vec<Value> = Vec::new();
        for phase in LaunchPhase::ALL {
            if phase.rank() <= self.phase.rank() {
                history.push(json!({
                    "phase": phase.as_str(),
                    "sequence": u64::from(phase.rank()),
                }));
            }
        }

        let record = json!({
            "schema_version": SCHEMA_VERSION,
            "transaction_id": self.envelope.transaction_id,
            "effect_kind": "process_launch",
            "effect_boundary": "process",
            "project_object_identity": self.envelope.project.object_identity,
            "project_root_object": self.envelope.project.root_object,
            "project_file_object": self.envelope.project.file_object,
            "project_generation": self.envelope.project.generation,
            "transport_mode": self.envelope.transport_mode.as_str(),
            "principal": self.envelope.principal,
            "session": self.envelope.session,
            "operation_family": self.envelope.operation_family.as_str(),
            "tool_name": self.envelope.tool_name,
            "idempotency_id": self.envelope.idempotency_id,
            "canonical_request_hash": self.envelope.canonical_request_hash,
            "canonical_request_bytes": self.envelope.canonical_request_bytes,
            "touched_paths": touched_paths,
            "touched_object_identities": touched_identities,
            "pre_hashes": Vec::<Value>::new(),
            "staged_write_set": staged,
            "intended_post_hashes": Vec::<Value>::new(),
            "phase": self.phase.as_str(),
            "phase_history": history,
            "proof": self
                .proof
                .as_ref()
                .map(ProcessLaunchProof::to_canonical_value),
            "required_subproofs": Value::Null,
            "terminal_reason": Value::Null,
            "opened_at_unix_ms": self.envelope.opened_at_unix_ms,
        });
        validate_canonical(&record)?;
        Ok(record)
    }

    /// The payload document with this logical name, if staged.
    #[must_use]
    pub fn payload(&self, name: &str) -> Option<&LaunchPayload> {
        self.payloads.iter().find(|payload| payload.name == name)
    }

    /// The already-durable containment-boundary (or weak-locator) identity this
    /// transaction bound **before** it created anything.
    ///
    /// This is the recovery anchor: it is present from `intent_fsynced` onwards,
    /// so reconciliation enumerates the boundary directly instead of inferring
    /// absence from a missing later mark.
    #[must_use]
    pub fn boundary_identity(&self) -> Option<super::boundary::BoundaryIdentity> {
        let facts = self.payload(Self::INTENT_PAYLOAD)?.facts().ok()?;
        super::boundary::BoundaryIdentity::from_facts(&facts)
    }

    /// The launch facts one payload document carries, or an empty map when that
    /// document is not staged.
    ///
    /// # Errors
    /// Returns [`JournalError::Payload`] when the staged bytes are not canonical.
    pub fn facts(&self, name: &str) -> Result<BTreeMap<String, Value>, JournalError> {
        match self.payload(name) {
            Some(payload) => payload.facts(),
            None => Ok(BTreeMap::new()),
        }
    }

    /// Rebuild a record from its canonical durable value plus the staged payload
    /// bytes, exactly as a restarted process must.
    ///
    /// `fetch` resolves one canonical staging ID to its durable bytes. A payload
    /// whose bytes are missing or whose hash does not match the staged entry is
    /// dropped rather than trusted, so a partially durable payload can never be
    /// read as a complete one.
    ///
    /// The phase is derived from which of the four canonical fsync marks are
    /// present in `phase_history`, in ascending order — never from a
    /// launch-specific phase name.
    ///
    /// # Errors
    /// Returns [`JournalError::Schema`] when the value is not canonical, or
    /// [`JournalError::Payload`] when a required envelope field is unusable.
    pub fn from_canonical_value(
        value: &Value,
        fetch: &dyn Fn(&str) -> Option<Vec<u8>>,
    ) -> Result<Self, JournalError> {
        validate_canonical(value)?;
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

        let touched: Vec<(String, Option<String>)> = value["touched_object_identities"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        Some((
                            entry["path"].as_str()?.to_owned(),
                            entry["object_identity"].as_str().map(str::to_owned),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut payloads = Vec::new();
        if let Some(entries) = value["staged_write_set"].as_array() {
            for (index, entry) in entries.iter().enumerate() {
                let Some(name) = entry["path"].as_str() else {
                    continue;
                };
                let Some(staging_id) = entry["staging_id"].as_str() else {
                    continue;
                };
                let Some(bytes) = fetch(staging_id) else {
                    continue;
                };
                if entry["sha256"] != json!(hex_sha256(&bytes)) {
                    continue;
                }
                payloads.push(LaunchPayload {
                    name: name.to_owned(),
                    bytes,
                    touched: if index == 0 {
                        touched.clone()
                    } else {
                        Vec::new()
                    },
                });
            }
        }

        let proof = match &value["proof"] {
            Value::Null => None,
            proof => Some(ProcessLaunchProof::from_canonical_value(proof)?),
        };

        Ok(Self {
            envelope: TransactionEnvelope {
                transaction_id: text("transaction_id")?,
                project: ProjectIdentityClaim {
                    object_identity: text("project_object_identity")?,
                    root_object: text("project_root_object")?,
                    file_object: text("project_file_object")?,
                    generation: number("project_generation")?,
                },
                transport_mode: TransportMode::parse(&text("transport_mode")?).ok_or_else(
                    || JournalError::Payload("record carries an unknown transport mode".to_owned()),
                )?,
                principal: value["principal"].as_str().map(str::to_owned),
                session: value["session"].as_str().map(str::to_owned),
                operation_family: OperationFamily::parse(&text("operation_family")?).ok_or_else(
                    || {
                        JournalError::Payload(
                            "record carries an unknown operation family".to_owned(),
                        )
                    },
                )?,
                tool_name: text("tool_name")?,
                idempotency_id: text("idempotency_id")?,
                canonical_request_hash: text("canonical_request_hash")?,
                canonical_request_bytes: number("canonical_request_bytes")?,
                opened_at_unix_ms: number("opened_at_unix_ms")?,
            },
            phase,
            payloads,
            proof,
        })
    }
}

/// Assemble and validate a terminal (settled) canonical record.
///
/// # Errors
/// Returns [`JournalError::Schema`] when the assembled record violates the
/// canonical schema.
pub fn canonical_terminal_value(
    record: &LaunchRecord,
    disposition: Disposition,
    reason: &str,
    proof: Option<&ProcessLaunchProof>,
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
        proof.map_or(Value::Null, ProcessLaunchProof::to_canonical_value),
    );
    if let Some(Value::Array(history)) = object.get_mut("phase_history") {
        let sequence = history.len() as u64 + 1;
        history.push(json!({ "phase": disposition.as_str(), "sequence": sequence }));
    }
    validate_canonical(&value)?;
    Ok(value)
}

/// Validate one already-assembled record value against the canonical schema.
///
/// # Errors
/// Returns [`JournalError::SchemaUnusable`] when the embedded canonical schema
/// cannot be parsed, or [`JournalError::Schema`] when the record violates it.
pub fn validate_canonical(value: &Value) -> Result<(), JournalError> {
    let schema: Value =
        serde_json::from_str(TRANSACTION_SCHEMA_JSON).map_err(|_| JournalError::SchemaUnusable)?;
    validate(&schema, value).map_err(JournalError::Schema)
}

/// A journal-port failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JournalError {
    /// The canonical schema itself could not be parsed.
    #[error("the canonical transaction schema is unusable")]
    SchemaUnusable,
    /// The assembled record violates the canonical schema.
    #[error("launch record violates the canonical transaction schema: {0}")]
    Schema(SchemaError),
    /// A launch payload document is malformed.
    #[error("launch payload is malformed: {0}")]
    Payload(String),
    /// The durable store refused or failed the write. The launch protocol treats
    /// this as fatal: it never proceeds past a phase whose mark is not durable.
    #[error("durable journal write failed at {phase}: {reason}")]
    Durability {
        /// Which phase failed to become durable.
        phase: LaunchPhase,
        /// Why the durable write failed.
        reason: String,
    },
    /// The transaction is not journaled at all.
    #[error("transaction {0} is not journaled")]
    NotFound(String),
    /// A transition the canonical phase machine does not allow.
    #[error("illegal phase transition {from:?} -> {to} for transaction {transaction_id}")]
    IllegalTransition {
        /// The transaction that was asked to transition.
        transaction_id: String,
        /// The durable phase it was in.
        from: Option<LaunchPhase>,
        /// The phase that was refused.
        to: LaunchPhase,
    },
}

/// The durable journal, as the engine sees it.
///
/// Todo 6's `TransactionStore` is the only production implementation; the server
/// layer injects it. Every method is expected to be durable (fsynced) before it
/// returns — the launch protocol's ordering guarantees are meaningless otherwise.
pub trait LaunchJournal {
    /// Durably record `record` at its current phase, fsyncing before returning.
    ///
    /// # Errors
    /// Returns [`JournalError`] when the record is not canonical or the durable
    /// write failed. The caller must treat any error as "this phase is not
    /// durable" and must not proceed.
    fn write_phase(&self, record: &LaunchRecord) -> Result<(), JournalError>;

    /// Durably record one of exactly three terminal dispositions.
    ///
    /// # Errors
    /// Returns [`JournalError`] when the record is not canonical or the durable
    /// write failed.
    fn settle(
        &self,
        record: &LaunchRecord,
        disposition: Disposition,
        reason: &str,
        proof: Option<&ProcessLaunchProof>,
    ) -> Result<(), JournalError>;

    /// Every transaction that is journaled but not yet settled, oldest first.
    ///
    /// `committed_fsynced` is terminal in Todo 6's phase machine, so a committed
    /// launch is **not** open. Case 5's "commit mark already present" arm reads
    /// [`LaunchJournal::committed_launches`] instead.
    ///
    /// # Errors
    /// Returns [`JournalError`] when the durable store cannot be read.
    fn open_launches(&self) -> Result<Vec<LaunchRecord>, JournalError>;

    /// Every transaction whose durable phase is exactly `committed_fsynced`,
    /// oldest first. These are the owned survivors a restart must reconcile: the
    /// receipt and commit mark already prove containment and resume, so what
    /// remains is confirming whether the survivor is still live and contained.
    ///
    /// # Errors
    /// Returns [`JournalError`] when the durable store cannot be read.
    fn committed_launches(&self) -> Result<Vec<LaunchRecord>, JournalError>;

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

    fn envelope() -> TransactionEnvelope {
        TransactionEnvelope {
            transaction_id: "00000000-0000-4000-8000-000000000141".to_owned(),
            project: ProjectIdentityClaim {
                object_identity: format!("project_{}", "a".repeat(64)),
                root_object: "unix:dev=1,ino=2".to_owned(),
                file_object: "unix:dev=1,ino=3".to_owned(),
                generation: 7,
            },
            transport_mode: TransportMode::Stdio,
            principal: None,
            session: None,
            operation_family: OperationFamily::RuntimeExecution,
            tool_name: "process.launch".to_owned(),
            idempotency_id: "idem-14".to_owned(),
            canonical_request_hash: "b".repeat(64),
            canonical_request_bytes: 42,
            opened_at_unix_ms: 1_700_000_000_000,
        }
    }

    fn payload() -> LaunchPayload {
        let mut facts = BTreeMap::new();
        facts.insert("boundary_kind".to_owned(), json!("container_adapter"));
        facts.insert("boundary_identity".to_owned(), json!("container:tx-141"));
        facts.insert("boundary_strength".to_owned(), json!("strong"));
        LaunchPayload::new(
            LaunchRecord::INTENT_PAYLOAD,
            &facts,
            vec![(
                "project.godot".to_owned(),
                Some("unix:dev=1,ino=3".to_owned()),
            )],
        )
    }

    #[test]
    fn there_are_exactly_four_canonical_phases_and_three_dispositions() {
        assert_eq!(LaunchPhase::ALL.len(), 4);
        assert_eq!(Disposition::ALL.len(), 3);
        let names: Vec<&str> = LaunchPhase::ALL.iter().map(|p| p.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "intent_fsynced",
                "staged_fsynced",
                "effect_started_fsynced",
                "committed_fsynced"
            ]
        );
        assert!(LaunchPhase::IntentFsynced < LaunchPhase::CommittedFsynced);
        assert_eq!(
            LaunchPhase::CommittedFsynced.predecessor(),
            Some(LaunchPhase::EffectStartedFsynced)
        );
        assert!(Disposition::Quarantined.blocks_mutation());
        assert!(!Disposition::Committed.blocks_mutation());
    }

    #[test]
    fn a_canonical_record_validates_against_the_repository_schema() {
        let record = LaunchRecord {
            envelope: envelope(),
            phase: LaunchPhase::IntentFsynced,
            payloads: vec![payload()],
            proof: None,
        };
        let value = record.to_canonical_value().expect("canonical record");
        assert_eq!(value["effect_kind"], json!("process_launch"));
        assert_eq!(value["effect_boundary"], json!("process"));
        assert_eq!(value["phase"], json!("intent_fsynced"));
        assert_eq!(
            value["staged_write_set"][0]["path"],
            json!(LaunchRecord::INTENT_PAYLOAD)
        );
        assert_eq!(value["pre_hashes"], json!([]));
        assert_eq!(value["intended_post_hashes"], json!([]));
    }

    #[test]
    fn the_intent_record_is_the_recovery_anchor() {
        let record = LaunchRecord {
            envelope: envelope(),
            phase: LaunchPhase::IntentFsynced,
            payloads: vec![payload()],
            proof: None,
        };
        let identity = record
            .boundary_identity()
            .expect("the intent payload binds the already-created boundary identity");
        assert_eq!(identity.identity, "container:tx-141");
    }

    #[test]
    fn a_record_round_trips_through_its_canonical_value_and_staged_bytes() {
        let staged = payload();
        let record = LaunchRecord {
            envelope: envelope(),
            phase: LaunchPhase::StagedFsynced,
            payloads: vec![staged.clone()],
            proof: None,
        };
        let value = record.to_canonical_value().expect("canonical record");
        let restored = LaunchRecord::from_canonical_value(&value, &|staging_id| {
            (staging_id == staged.staging_id()).then(|| staged.bytes.clone())
        })
        .expect("round trip");
        assert_eq!(restored.phase, LaunchPhase::StagedFsynced);
        assert_eq!(restored.envelope, record.envelope);
        assert_eq!(restored.boundary_identity(), record.boundary_identity());
    }

    #[test]
    fn a_payload_whose_bytes_do_not_hash_is_dropped_rather_than_trusted() {
        let staged = payload();
        let record = LaunchRecord {
            envelope: envelope(),
            phase: LaunchPhase::IntentFsynced,
            payloads: vec![staged],
            proof: None,
        };
        let value = record.to_canonical_value().expect("canonical record");
        let restored = LaunchRecord::from_canonical_value(&value, &|_| Some(b"tampered".to_vec()))
            .expect("round trip");
        assert!(
            restored.payloads.is_empty(),
            "a payload whose durable bytes do not match its recorded hash is never trusted"
        );
        assert!(restored.boundary_identity().is_none());
    }

    #[test]
    fn a_launch_proof_uses_todo_sixs_only_process_launch_form() {
        let proof = ProcessLaunchProof {
            boundary_identity: "container:tx-141".to_owned(),
            pid: Some(4242),
            start_time: Some("991122".to_owned()),
            boundary_membership: BoundaryMembership::Confirmed,
            release_status: ReleaseStatus::ExitedClean,
            exit_code: Some(0),
            signal: None,
            boundary_empty_after_exit: Some(true),
        };
        let record = LaunchRecord {
            envelope: envelope(),
            phase: LaunchPhase::CommittedFsynced,
            payloads: vec![payload()],
            proof: Some(proof.clone()),
        };
        let value = record.to_canonical_value().expect("canonical record");
        assert_eq!(value["proof"]["kind"], json!("process_launch"));
        assert!(value["proof"].get("sha256").is_none());
        assert!(proof.is_clean_terminal_process());
        assert_eq!(
            ProcessLaunchProof::from_canonical_value(&value["proof"]).expect("round trip"),
            proof
        );
    }

    #[test]
    fn a_file_proof_is_never_read_back_as_a_launch_proof() {
        let file_proof = json!({ "kind": "file", "entries": [] });
        assert!(matches!(
            ProcessLaunchProof::from_canonical_value(&file_proof),
            Err(JournalError::Payload(_))
        ));
    }

    #[test]
    fn a_live_survivor_is_never_a_clean_terminal_proof() {
        let survivor = ProcessLaunchProof {
            boundary_identity: "container:tx-141".to_owned(),
            pid: Some(4242),
            start_time: Some("991122".to_owned()),
            boundary_membership: BoundaryMembership::Confirmed,
            release_status: ReleaseStatus::ResumedContained,
            exit_code: None,
            signal: None,
            boundary_empty_after_exit: Some(false),
        };
        assert!(!survivor.is_clean_terminal_process());
    }

    #[test]
    fn a_terminal_record_carries_its_disposition_as_the_canonical_phase() {
        let record = LaunchRecord {
            envelope: envelope(),
            phase: LaunchPhase::IntentFsynced,
            payloads: vec![payload()],
            proof: None,
        };
        for disposition in Disposition::ALL {
            let value =
                canonical_terminal_value(&record, disposition, LAUNCH_ABANDONED_PRE_RELEASE, None)
                    .expect("terminal record");
            assert_eq!(value["phase"], json!(disposition.as_str()));
            assert_eq!(
                value["terminal_reason"],
                json!(LAUNCH_ABANDONED_PRE_RELEASE)
            );
        }
    }

    #[test]
    fn a_non_canonical_record_is_refused_rather_than_journaled() {
        let mut broken = envelope();
        broken.transaction_id = "not-a-uuid".to_owned();
        let record = LaunchRecord {
            envelope: broken,
            phase: LaunchPhase::IntentFsynced,
            payloads: vec![payload()],
            proof: None,
        };
        assert!(matches!(
            record.to_canonical_value(),
            Err(JournalError::Schema(_))
        ));
    }
}
