//! The in-crate operator-recovery entry point: the production mint site of a
//! [`RepairProof`](crate::scheduler::RepairProof).
//!
//! A write block — a quarantined transaction or an unresolved process survivor —
//! is lifted by exactly one thing: a proof that an operator repair re-derived a
//! typed proof and journaled a terminal record. This module is where that happens
//! in production. Before Todo 32 the only mint site was the acceptance harness,
//! which meant the whole write-block protocol had no way to be cleared in a
//! shipped binary.
//!
//! ## What this path cannot do
//!
//! It cannot force, clear or override. Every operation is driven by Todo 6's
//! [`OperatorRecoveryAuthority`], which:
//!
//! * refuses to issue a permit for a transaction that is not `quarantined`;
//! * writes its audit record before any state transition; and
//! * is structurally incapable of recording `committed` without an independently
//!   re-derived post-state proof, or `compensated` without a pre-state proof.
//!
//! A proof is minted here **only** when the repair produced a terminal,
//! proof-backed record. A `reconcile` — which deliberately records nothing —
//! mints nothing, and the block stays exactly where it was.
//!
//! The local-only `transaction repair` CLI subcommand that drives this path from
//! an operator's shell is Todo 34's; this module is the crate-internal path it
//! will call.

use std::sync::Arc;

use godot_mcp_domain::contracts::OutcomeError;

use crate::effect::{
    EffectGateway, LocalLeaseEvidence, OperatorRecoveryAuthority, RepairOperation,
};
use crate::journal::{HeldProject, Phase, TransactionStore};
use crate::scheduler::{RepairProof, Scheduler};

/// Which proof-driven recovery operation to apply.
///
/// This is the caller-facing mirror of Todo 6's `RepairOperation`. It exists so an
/// operator entry point can name an operation without the crate-private permit
/// vocabulary leaking into a public signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RepairIntent {
    /// Re-derive and compare the typed proof; record nothing.
    Reconcile,
    /// `quarantined → compensated`, only with a proven pre-state.
    Compensate,
    /// Terminate a still-contained process survivor, then record `compensated`.
    Terminate,
    /// Record `committed`, only with an independently re-derived post-state proof.
    AcknowledgeSafeState,
}

impl RepairIntent {
    /// Every intent, in a stable order.
    pub const ALL: [Self; 4] = [
        Self::Reconcile,
        Self::Compensate,
        Self::Terminate,
        Self::AcknowledgeSafeState,
    ];

    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reconcile => "reconcile",
            Self::Compensate => "compensate",
            Self::Terminate => "terminate",
            Self::AcknowledgeSafeState => "acknowledge-safe-state",
        }
    }

    const fn operation(self) -> RepairOperation {
        match self {
            Self::Reconcile => RepairOperation::Reconcile,
            Self::Compensate => RepairOperation::Compensate,
            Self::Terminate => RepairOperation::Terminate,
            Self::AcknowledgeSafeState => RepairOperation::AcknowledgeSafeState,
        }
    }
}

/// What one repair actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairReport {
    transaction_id: String,
    operation: &'static str,
    conclusion: &'static str,
    reason: String,
    write_block_cleared: bool,
}

impl RepairReport {
    /// The transaction the repair addressed.
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    /// Stable name of the applied operation.
    #[must_use]
    pub const fn operation(&self) -> &'static str {
        self.operation
    }

    /// Stable name of the re-derived conclusion.
    #[must_use]
    pub const fn conclusion(&self) -> &'static str {
        self.conclusion
    }

    /// The recovery authority's own reason, surfaced unchanged.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Whether mutations are permitted again.
    ///
    /// This is re-read from the durable journal *after* the repair, so a repair
    /// that settled one transaction while another is still quarantined reports
    /// `false` rather than claiming the project is writable.
    #[must_use]
    pub const fn write_block_cleared(&self) -> bool {
        self.write_block_cleared
    }
}

/// Apply one proof-driven repair and, only on a proof-backed terminal record,
/// present the resulting proof to the scheduler's write-block gate.
///
/// # Errors
/// Returns the recovery refusal. A transaction that is not quarantined, or a
/// repair that could not re-derive its required proof, is an error rather than a
/// silently degraded success.
pub(crate) fn apply_repair(
    store: &Arc<TransactionStore>,
    gateway: &EffectGateway,
    scheduler: &Scheduler,
    held: &HeldProject,
    transaction_id: &str,
    intent: RepairIntent,
) -> Result<RepairReport, OutcomeError> {
    let authority = OperatorRecoveryAuthority::new(
        Arc::clone(store),
        LocalLeaseEvidence::new(held.object_identity.clone(), held.generation),
    );
    let permit = authority
        .issue(transaction_id, intent.operation())
        .map_err(|error| crate::effect::EffectError::from(error).to_outcome())?;
    let outcome = gateway
        .apply_repair(&permit)
        .map_err(|error| error.to_outcome())?;

    // A repair that produced no terminal, proof-backed record mints nothing. The
    // block is left exactly where it was, because there is no proof to lift it.
    let write_block_cleared = if outcome.phase == Phase::Quarantined {
        false
    } else {
        let proof = RepairProof::sealed(outcome.transaction_id.clone(), intent.as_str());
        scheduler.resolve_write_block(&proof).is_ok()
    };

    Ok(RepairReport {
        transaction_id: outcome.transaction_id,
        operation: intent.as_str(),
        conclusion: outcome.conclusion.as_str(),
        reason: outcome.reason,
        write_block_cleared,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every intent maps to exactly one Todo 6 operation, and the names agree, so
    /// an audit record and a caller-facing report can never name different things.
    #[test]
    fn every_intent_maps_to_exactly_one_operation_with_the_same_name() {
        for intent in RepairIntent::ALL {
            assert_eq!(intent.as_str(), intent.operation().as_str());
        }
        let mut names: Vec<&str> = RepairIntent::ALL.iter().map(|i| i.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), RepairIntent::ALL.len());
    }
}
