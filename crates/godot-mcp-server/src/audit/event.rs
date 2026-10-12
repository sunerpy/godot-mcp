//! The bounded, content-free audit event and its typed dimensions.
//!
//! ## Why this type cannot leak content
//!
//! An audit record is not "a struct we remember not to put secrets in". Every
//! field here is one of exactly three shapes:
//!
//! * a stable `&'static str` name produced by an exhaustive `match` over a frozen
//!   domain enum, so it can only ever be one of a fixed set of words;
//! * a [`SafeField`] — bounded to 256 bytes and restricted to
//!   `[A-Za-z0-9._:-]`, which structurally rejects source text, JSON payloads,
//!   filesystem paths with separators, and anything containing whitespace or
//!   quotes; or
//! * an [`AuditDigest`] — a one-way SHA-256 digest whose only constructors hash
//!   their input, so the input is not recoverable from the stored value.
//!
//! There is deliberately no `String`, no `Vec<u8>`, no `serde_json::Value` and no
//! `PathBuf` field. Raw arguments, raw results and artifact storage locations are
//! therefore not "omitted by convention"; they have nowhere to go.

use std::fmt;

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::contracts::{Capability, ErrorKind, PolicyReason, PolicyVerdict};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::observability::{
    CorrelationId, RequestId, SafeField, ScreenshotBytes, SecretValue, SourceText, TokenValue,
};

/// Hard bound on one serialized audit record.
///
/// The bound is a consequence of the field types rather than a hope: a record is
/// one `tool` field (≤256 bytes), one decision ID (73 bytes), two bounded
/// correlation identifiers (≤256 bytes each), four 64-character digests and a
/// fixed set of enum names and integers. 2048 bytes leaves headroom for the JSON
/// framing while still refusing anything that could only be oversized because a
/// payload smuggled itself in.
pub const MAX_AUDIT_EVENT_BYTES: usize = 2048;

/// Schema version of the audit record shape.
const AUDIT_SCHEMA_VERSION: u8 = 1;

/// Failure from building, bounding or publishing an audit record.
///
/// No variant carries payload material: the caller learns *that* a field was
/// unsafe, never what the field contained.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// A field was empty, unbounded, or contained a forbidden character.
    #[error("unsafe audit field")]
    UnsafeField(#[from] crate::observability::SafeFieldError),
    /// The serialized record exceeded its fixed bound.
    #[error("audit record exceeds its {MAX_AUDIT_EVENT_BYTES}-byte bound")]
    TooLarge,
    /// Canonical serialization failed without returning source material.
    #[error("audit record serialization failed")]
    Serialization(#[source] serde_json::Error),
    /// The append-only sink could not be created, rotated or appended to.
    #[error("audit sink is unavailable")]
    Io(#[source] std::io::Error),
    /// The configured sink is structurally invalid.
    #[error("invalid audit sink configuration: {0}")]
    InvalidSink(&'static str),
}

/// A one-way 64-hex digest.
///
/// Every constructor hashes its input under a domain separator, so two different
/// kinds of material never collide and neither is recoverable. This is the *only*
/// way sensitive material can be referenced from an audit record.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct AuditDigest(String);

impl AuditDigest {
    /// Digest arbitrary bytes under an explicit domain separator.
    #[must_use]
    pub fn of_bytes(domain: &str, bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"godot-mcp-audit-digest-v1");
        hasher.update((domain.len() as u64).to_le_bytes());
        hasher.update(domain.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
        let digest = hasher.finalize();
        let mut value = String::with_capacity(64);
        for byte in digest {
            use fmt::Write as _;
            write!(&mut value, "{byte:02x}").expect("writing to a String cannot fail");
        }
        Self(value)
    }

    /// Digest a string under an explicit domain separator.
    #[must_use]
    pub fn of_str(domain: &str, value: &str) -> Self {
        Self::of_bytes(domain, value.as_bytes())
    }

    /// Digest secret material. The plaintext never reaches the record.
    #[must_use]
    pub fn of_secret(value: &SecretValue) -> Self {
        Self::of_bytes("secret", value.expose().as_bytes())
    }

    /// Digest bearer/session token material.
    #[must_use]
    pub fn of_token(value: &TokenValue) -> Self {
        Self::of_bytes("token", value.expose().as_bytes())
    }

    /// Digest project source text.
    #[must_use]
    pub fn of_source(value: &SourceText) -> Self {
        Self::of_bytes("source", value.expose().as_bytes())
    }

    /// Digest screenshot/recording bytes.
    #[must_use]
    pub fn of_screenshot(value: &ScreenshotBytes) -> Self {
        Self::of_bytes("screenshot", value.expose())
    }

    /// The already-computed digest.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AuditDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("AuditDigest").field(&self.0).finish()
    }
}

impl fmt::Display for AuditDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Which transport established the request being audited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditTransport {
    /// Newline-delimited JSON-RPC over stdio.
    Stdio,
    /// Streamable HTTP under the per-launch local secret.
    HttpTrustedLocal,
    /// Streamable HTTP behind an authenticated proxy hop plus OIDC.
    HttpProxiedOidc,
}

impl AuditTransport {
    /// Stable snake_case name written to the record.
    #[must_use]
    pub const fn audit_name(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::HttpTrustedLocal => "http_trusted_local",
            Self::HttpProxiedOidc => "http_proxied_oidc",
        }
    }
}

/// Which scheduler lane the work was queued on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditLane {
    /// The concurrent read lane.
    Read,
    /// The serialized mutation lane.
    Mutation,
}

/// Which fairness class the work was queued under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditPriority {
    /// Caller-visible work.
    Interactive,
    /// Deferred maintenance work.
    Background,
}

/// Which effect boundary the work reached, when it reached one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditBoundary {
    /// Publish-to-`SafeRoot` file boundary.
    File,
    /// Supervised subprocess boundary.
    Process,
    /// Receipt-bound LSP/DAP boundary.
    LspDap,
    /// Editor `EditorUndoRedoManager` boundary.
    Editor,
    /// Running-instance runtime boundary.
    Runtime,
}

impl AuditBoundary {
    /// Stable snake_case name written to the record.
    #[must_use]
    pub const fn audit_name(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Process => "process",
            Self::LspDap => "lsp_dap",
            Self::Editor => "editor",
            Self::Runtime => "runtime",
        }
    }
}

/// Which durable transaction phase the audited work settled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditPhase {
    /// The transaction record exists.
    Opened,
    /// Staged content is fsynced.
    Staged,
    /// The boundary was entered.
    EffectStarted,
    /// The effect committed and was proven.
    Committed,
    /// The effect was rolled back and proven absent.
    Compensated,
    /// The transaction is quarantined pending operator repair.
    Quarantined,
}

impl AuditPhase {
    /// Stable snake_case name written to the record.
    #[must_use]
    pub const fn audit_name(self) -> &'static str {
        match self {
            Self::Opened => "opened",
            Self::Staged => "staged",
            Self::EffectStarted => "effect_started",
            Self::Committed => "committed",
            Self::Compensated => "compensated",
            Self::Quarantined => "quarantined",
        }
    }
}

/// How the audited request finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditStatus {
    /// The request completed successfully.
    Ok,
    /// A gate refused the request before it ran.
    Refused,
    /// The request ran and failed.
    Failed,
}

impl AuditStatus {
    /// Stable snake_case name written to the record.
    #[must_use]
    pub const fn audit_name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Refused => "refused",
            Self::Failed => "failed",
        }
    }
}

/// Stable snake_case name of one capability, reused from the Todo 26 decision
/// module so the audit log and the policy digest can never disagree.
#[must_use]
pub const fn capability_audit_name(capability: Capability) -> &'static str {
    crate::policy::decision::capability_name(capability)
}

/// Stable snake_case name of one policy verdict.
#[must_use]
pub const fn verdict_audit_name(verdict: PolicyVerdict) -> &'static str {
    match verdict {
        PolicyVerdict::Allow => "allow",
        PolicyVerdict::Deny => "deny",
    }
}

/// Stable snake_case name of one policy reason, reused from Todo 26.
#[must_use]
pub const fn reason_audit_name(reason: PolicyReason) -> &'static str {
    crate::policy::decision::reason_name(reason)
}

/// Queue facts the caller supplies for one audited request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AuditQueueFacts {
    /// Which lane the work was queued on.
    pub lane: AuditLane,
    /// Which fairness class the work was queued under.
    pub priority: AuditPriority,
    /// How deep the lane was when the work was queued.
    pub queued_depth: u32,
    /// Whether the scheduler admitted the work.
    pub admitted: bool,
}

/// The actor dimension: who acted, referenced only by digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditActorFacts {
    /// Digest of the established principal.
    pub principal_digest: AuditDigest,
    /// Digest of the established session, when one exists.
    pub session_digest: Option<AuditDigest>,
    /// Digest of the credential that authorized the request, when one exists.
    ///
    /// Absent rather than `null` when there is nothing to reference, so a record
    /// carries no field it has no fact for and stays inside its byte bound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_digest: Option<AuditDigest>,
}

/// The project dimension: which held project object and generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditProjectFacts {
    /// Digest of the stable project object identity.
    pub object_digest: AuditDigest,
    /// Generation of the held project lease.
    pub generation: u64,
}

/// The policy dimension: the capability, the verdict and the decision digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditPolicyFacts {
    /// Capability the tool required.
    pub capability: &'static str,
    /// Whether the policy layer allowed the dispatch.
    pub verdict: &'static str,
    /// Why the policy layer reached that verdict.
    pub reason: &'static str,
    /// Non-sensitive Todo 26 decision identifier.
    pub decision_id: SafeField,
}

/// The effect dimension: which boundary, which terminal phase, which content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditEffectFacts {
    /// Boundary the work reached, when it reached one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary: Option<&'static str>,
    /// Durable phase the work settled in, when it was journaled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<&'static str>,
    /// Digest of the content the effect carried, when it carried content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_digest: Option<AuditDigest>,
}

/// The error class of a non-successful outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AuditErrorFacts {
    /// Stable domain error code.
    pub kind: &'static str,
    /// Whether the caller may retry.
    pub retryable: bool,
}

/// The outcome dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AuditOutcomeFacts {
    /// How the request finished.
    pub status: &'static str,
    /// The error class, present only for a refusal or a failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AuditErrorFacts>,
}

/// The correlation dimension: how this event joins the rest of the trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditCorrelationFacts {
    /// Identifier joining work across subsystems.
    pub correlation_id: CorrelationId,
    /// Identifier of the single inbound request.
    pub request_id: RequestId,
}

/// Everything one caller must supply to seal an audit record.
///
/// The draft accepts `&str` for exactly two fields — the tool name and the
/// decision identifier — and both are validated into a [`SafeField`] by
/// [`AuditEvent::seal`]. Every other field is already a typed, content-free
/// value, so there is no unvalidated path into a record.
#[derive(Debug, Clone)]
pub struct AuditEventDraft<'draft> {
    /// Monotonic sequence reserved from the sink.
    pub sequence: u64,
    /// Trusted timestamp supplied by the caller, never read from a global clock.
    pub recorded_at_unix_ms: u64,
    /// Transport that established the request.
    pub transport: AuditTransport,
    /// Active canonical capability profile.
    pub profile: CapabilityProfile,
    /// Canonical tool name.
    pub tool: &'draft str,
    /// Digest of the established principal.
    pub principal_digest: AuditDigest,
    /// Digest of the established session, when one exists.
    pub session_digest: Option<AuditDigest>,
    /// Digest of the authorizing credential, when one exists.
    pub credential_digest: Option<AuditDigest>,
    /// Digest of the held project object identity.
    pub project_digest: AuditDigest,
    /// Generation of the held project lease.
    pub project_generation: u64,
    /// Capability the tool required.
    pub capability: Capability,
    /// Policy verdict.
    pub verdict: PolicyVerdict,
    /// Policy reason.
    pub reason: PolicyReason,
    /// Non-sensitive Todo 26 decision identifier.
    pub decision_id: &'draft str,
    /// Queue facts.
    pub queue: AuditQueueFacts,
    /// Boundary the work reached, when it reached one.
    pub boundary: Option<AuditBoundary>,
    /// Durable phase the work settled in.
    pub phase: Option<AuditPhase>,
    /// Digest of the content the effect carried.
    pub content_digest: Option<AuditDigest>,
    /// How the request finished.
    pub status: AuditStatus,
    /// Error class for a refusal or a failure.
    pub error_kind: Option<ErrorKind>,
    /// Whether the caller may retry the error.
    pub error_retryable: bool,
    /// Correlation identifier.
    pub correlation: CorrelationId,
    /// Request identifier.
    pub request: RequestId,
}

/// One sealed, bounded, content-free audit record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditEvent {
    schema_version: u8,
    sequence: u64,
    recorded_at_unix_ms: u64,
    transport: &'static str,
    profile: &'static str,
    tool: SafeField,
    actor: AuditActorFacts,
    project: AuditProjectFacts,
    policy: AuditPolicyFacts,
    queue: AuditQueueFacts,
    effect: AuditEffectFacts,
    outcome: AuditOutcomeFacts,
    correlation: AuditCorrelationFacts,
}

impl AuditEvent {
    /// Validate a draft into a bounded record.
    ///
    /// # Errors
    /// Returns [`AuditError::UnsafeField`] when the tool name or decision
    /// identifier is empty, unbounded or contains a forbidden character, and
    /// [`AuditError::TooLarge`] when the canonical record would exceed
    /// [`MAX_AUDIT_EVENT_BYTES`].
    pub fn seal(draft: AuditEventDraft<'_>) -> Result<Self, AuditError> {
        let event = Self {
            schema_version: AUDIT_SCHEMA_VERSION,
            sequence: draft.sequence,
            recorded_at_unix_ms: draft.recorded_at_unix_ms,
            transport: draft.transport.audit_name(),
            profile: draft.profile.manifest_name(),
            tool: SafeField::new(draft.tool)?,
            actor: AuditActorFacts {
                principal_digest: draft.principal_digest,
                session_digest: draft.session_digest,
                credential_digest: draft.credential_digest,
            },
            project: AuditProjectFacts {
                object_digest: draft.project_digest,
                generation: draft.project_generation,
            },
            policy: AuditPolicyFacts {
                capability: capability_audit_name(draft.capability),
                verdict: verdict_audit_name(draft.verdict),
                reason: reason_audit_name(draft.reason),
                decision_id: SafeField::new(draft.decision_id)?,
            },
            queue: draft.queue,
            effect: AuditEffectFacts {
                boundary: draft.boundary.map(AuditBoundary::audit_name),
                phase: draft.phase.map(AuditPhase::audit_name),
                content_digest: draft.content_digest,
            },
            outcome: AuditOutcomeFacts {
                status: draft.status.audit_name(),
                error: draft.error_kind.map(|kind| AuditErrorFacts {
                    kind: kind.code(),
                    retryable: draft.error_retryable,
                }),
            },
            correlation: AuditCorrelationFacts {
                correlation_id: draft.correlation,
                request_id: draft.request,
            },
        };
        if event.canonical_line()?.len() > MAX_AUDIT_EVENT_BYTES + 1 {
            return Err(AuditError::TooLarge);
        }
        Ok(event)
    }

    /// Sequence this record was sealed under.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Canonical tool name.
    #[must_use]
    pub fn tool(&self) -> &str {
        self.tool.as_str()
    }

    /// The exact newline-terminated bytes the append-only sink writes.
    ///
    /// # Errors
    /// Returns [`AuditError::Serialization`] if canonical JSON cannot be built.
    pub fn canonical_line(&self) -> Result<Vec<u8>, AuditError> {
        let mut bytes = serde_json::to_vec(self).map_err(AuditError::Serialization)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}
