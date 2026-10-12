//! Policy decisions, their non-sensitive decision IDs, and refusal mapping.
//!
//! A decision ID is a one-way digest over the decision's own facts. It carries no
//! path spelling, no principal plaintext, no session plaintext and no secret, so
//! it is safe to write into the durable audit log and safe to re-check at the
//! final effect boundary. The digest binds the principal and session *as inputs*
//! only, exactly the way Todo 3's public project object identity binds the held
//! object identity, so two different callers never share a decision ID while
//! neither identity is recoverable from it.

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{
    Capability, PolicyDecision, PolicyReason, PolicyVerdict, PrincipalId, ProjectIdentity,
    SessionId,
};
use sha2::{Digest, Sha256};

use crate::audit::PolicyAuditDecision;

/// Prefix every decision ID carries, so an audit reader can recognise one.
const DECISION_PREFIX: &str = "decision_";

/// The identity one policy authority is bound to for its whole lifetime.
///
/// The three facts travel together because a decision is only meaningful for all
/// three at once: a project without its generation, or a principal without its
/// project, would authorize work the transport boundary never established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundIdentity {
    /// Project object identity and generation, from the held lease.
    pub(crate) project: ProjectIdentity,
    /// Local or authenticated principal the transport boundary established.
    pub(crate) principal: Option<PrincipalId>,
    /// Session the transport boundary established.
    pub(crate) session: Option<SessionId>,
}

/// Derive the non-sensitive decision ID for one evaluated request.
///
/// Every field is length-prefixed before hashing, so no two distinct decisions
/// can collide by concatenation.
pub(crate) fn decision_id(
    identity: &BoundIdentity,
    tool: &str,
    capability: Capability,
    verdict: PolicyVerdict,
    reason: PolicyReason,
    epoch: u64,
) -> String {
    let project = &identity.project;
    let principal = identity.principal.as_ref();
    let session = identity.session.as_ref();
    let mut hasher = Sha256::new();
    hasher.update(b"godot-mcp-policy-decision-v1");
    let mut field = |label: &[u8], value: &[u8]| {
        hasher.update((label.len() as u64).to_le_bytes());
        hasher.update(label);
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(value);
    };
    field(b"project", project.object_id().as_str().as_bytes());
    field(b"generation", &project.generation().to_le_bytes());
    field(
        b"principal",
        principal.map_or("", PrincipalId::as_str).as_bytes(),
    );
    field(b"session", session.map_or("", SessionId::as_str).as_bytes());
    field(b"tool", tool.as_bytes());
    field(b"capability", capability_name(capability).as_bytes());
    field(b"verdict", verdict_name(verdict).as_bytes());
    field(b"reason", reason_name(reason).as_bytes());
    field(b"epoch", &epoch.to_le_bytes());
    let digest = hasher.finalize();
    let mut value = String::with_capacity(DECISION_PREFIX.len() + 64);
    value.push_str(DECISION_PREFIX);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut value, "{byte:02x}").expect("writing to a String cannot fail");
    }
    value
}

/// Stable snake_case name of a capability, for the digest and for audit text.
pub(crate) const fn capability_name(capability: Capability) -> &'static str {
    match capability {
        Capability::Read => "read",
        Capability::ProjectWrite => "project_write",
        Capability::EditorMutate => "editor_mutate",
        Capability::RuntimeExecute => "runtime_execute",
        Capability::ExportDestructive => "export_destructive",
    }
}

const fn verdict_name(verdict: PolicyVerdict) -> &'static str {
    match verdict {
        PolicyVerdict::Allow => "allow",
        PolicyVerdict::Deny => "deny",
    }
}

/// Stable snake_case name of a policy reason.
pub(crate) const fn reason_name(reason: PolicyReason) -> &'static str {
    match reason {
        PolicyReason::Granted => "granted",
        PolicyReason::MissingCapability => "missing_capability",
        PolicyReason::PrincipalMismatch => "principal_mismatch",
        PolicyReason::SessionMismatch => "session_mismatch",
        PolicyReason::ProjectGenerationMismatch => "project_generation_mismatch",
        PolicyReason::ProjectQuarantined => "project_quarantined",
        PolicyReason::SchedulerUnavailable => "scheduler_unavailable",
    }
}

/// The stable domain error class one denial reason maps to.
///
/// The mapping is exhaustive over the frozen seven-item reason set, so a new
/// reason cannot silently inherit a permissive class.
pub(crate) const fn reason_error_kind(reason: PolicyReason) -> ErrorKind {
    match reason {
        // A granted decision has no error class; the caller never asks for one,
        // and returning `internal` here keeps the function total without
        // inventing a permissive default.
        PolicyReason::Granted => ErrorKind::Internal,
        PolicyReason::MissingCapability
        | PolicyReason::PrincipalMismatch
        | PolicyReason::SessionMismatch => ErrorKind::PermissionDenied,
        PolicyReason::ProjectGenerationMismatch | PolicyReason::ProjectQuarantined => {
            ErrorKind::Conflict
        }
        PolicyReason::SchedulerUnavailable => ErrorKind::Unavailable,
    }
}

/// Build the public policy decision for one evaluated request.
///
/// The decision is the *only* value the policy layer publishes; it contains no
/// permit, no credential and no implementation data, and its ID is the digest
/// above.
pub(crate) fn build_decision(
    identity: &BoundIdentity,
    tool: &str,
    capability: Capability,
    reason: PolicyReason,
    epoch: u64,
) -> Result<PolicyDecision, godot_mcp_domain::contracts::ContractViolation> {
    let verdict = if reason == PolicyReason::Granted {
        PolicyVerdict::Allow
    } else {
        PolicyVerdict::Deny
    };
    let id = decision_id(identity, tool, capability, verdict, reason, epoch);
    PolicyDecision::new(
        id,
        identity.project.clone(),
        identity.principal.clone(),
        identity.session.clone(),
        capability,
        verdict,
        reason,
    )
}

pub(crate) fn admission_audit_decision(
    profile: godot_mcp_domain::CapabilityProfile,
    identity: &BoundIdentity,
    tool: &str,
    capability: Capability,
) -> PolicyAuditDecision {
    let reason = if godot_mcp_domain::manifests::canonical_manifest(profile)
        .capabilities
        .contains(&capability)
    {
        PolicyReason::Granted
    } else {
        PolicyReason::MissingCapability
    };
    let verdict = if reason == PolicyReason::Granted {
        PolicyVerdict::Allow
    } else {
        PolicyVerdict::Deny
    };
    PolicyAuditDecision {
        capability,
        verdict,
        reason,
        decision_id: decision_id(identity, tool, capability, verdict, reason, 1),
    }
}
