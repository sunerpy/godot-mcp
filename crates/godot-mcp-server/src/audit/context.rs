use std::sync::{Arc, Mutex};

use godot_mcp_domain::contracts::{Capability, PolicyReason, PolicyVerdict};

use super::{AuditBoundary, AuditDigest, AuditPhase};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PolicyAuditDecision {
    pub(crate) capability: Capability,
    pub(crate) verdict: PolicyVerdict,
    pub(crate) reason: PolicyReason,
    pub(crate) decision_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequestEffectFacts {
    pub(crate) boundary: AuditBoundary,
    pub(crate) phase: AuditPhase,
    pub(crate) content_digest: Option<AuditDigest>,
}

#[derive(Debug, Default)]
struct RequestAuditState {
    policy: Option<PolicyAuditDecision>,
    effect: Option<RequestEffectFacts>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RequestAudit {
    state: Arc<Mutex<RequestAuditState>>,
}

impl RequestAudit {
    pub(crate) fn record_policy_decision(&self, decision: PolicyAuditDecision) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .policy = Some(decision);
    }

    pub(crate) fn policy_decision(&self) -> Option<PolicyAuditDecision> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .policy
            .clone()
    }

    pub(crate) fn record_effect_facts(&self, effect: RequestEffectFacts) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .effect = Some(effect);
    }

    pub(crate) fn effect_facts(&self) -> Option<RequestEffectFacts> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .effect
            .clone()
    }
}
