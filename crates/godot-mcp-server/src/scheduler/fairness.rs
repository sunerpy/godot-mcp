//! Queue priority, fairness bounds, deadlines and the two cancellation paths.
//!
//! Backpressure here is *classed*, not global: a burst of long project mutations
//! must not starve interactive reads, and a flood of reads must not indefinitely
//! postpone a mutation that is already at the head of the effect lane. So every
//! admission carries a [`QueuePriority`], and the scheduler enforces an explicit
//! [`StarvationBound`]: after that many higher-priority admissions have overtaken
//! a waiter, the waiter is promoted. The bound is a number the acceptance suite
//! can assert, not an emergent property of the runtime's wake order.
//!
//! ## Two deadlines, because two different things can go wrong
//!
//! * A [`QueueDeadline`] bounds how long work may *wait*. Exceeding it means the
//!   system is overloaded: the work never started, so `unavailable` is honest and
//!   retryable.
//! * An [`OperationDeadline`] bounds how long work may *run*. Exceeding it means
//!   the operation itself did not finish, and — critically — its effect may or may
//!   not have landed. That is `timeout`, and it must never be reported as "did not
//!   happen".
//!
//! ## Two cancellation paths, for the same reason
//!
//! [`CancellationOutcome::CancelledBeforeStart`] is a *proof* that no effect ran:
//! the work was still queued, so nothing was dispatched and the caller may safely
//! retry. [`CancellationOutcome::CancelledDuringExecution`] is not that proof. The
//! boundary was already touched, so the honest answer is that the outcome is
//! unresolved until the journal's own reconciliation proves a pre- or post-state.
//! Collapsing these two into one "cancelled" would be the same lie as inferring
//! success from dispatch.

use std::collections::BTreeMap;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;

use crate::idempotency::OperationToolFamily;

/// Admission priority class. Higher classes are served first, subject to the
/// starvation bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QueuePriority {
    /// Bulk or project-wide background work.
    Bulk,
    /// Ordinary tool calls.
    Normal,
    /// Latency-sensitive reads a caller is waiting on.
    Interactive,
    /// Operator recovery and shutdown-critical work.
    Operator,
}

impl QueuePriority {
    /// Every class, lowest first.
    pub const ALL: [Self; 4] = [Self::Bulk, Self::Normal, Self::Interactive, Self::Operator];

    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bulk => "bulk",
            Self::Normal => "normal",
            Self::Interactive => "interactive",
            Self::Operator => "operator",
        }
    }
}

/// Assign the server-owned queue priority for one canonical operation family.
///
/// This policy deliberately accepts operation metadata rather than a tool name or
/// client argument. [`crate::idempotency::family_of`] remains the only tool-name
/// classification table, while this exhaustive match makes a newly added family a
/// compile error until its scheduling semantics are chosen.
#[must_use]
pub const fn priority_for(family: OperationToolFamily) -> QueuePriority {
    match family {
        OperationToolFamily::StaticDiscovery
        | OperationToolFamily::StaticAnalysis
        | OperationToolFamily::StaticSceneGraph
        | OperationToolFamily::EditorInspection
        | OperationToolFamily::RuntimeDebug
        | OperationToolFamily::RuntimeInspection
        | OperationToolFamily::LanguageService
        | OperationToolFamily::DiagnosticProbe => QueuePriority::Interactive,
        OperationToolFamily::ProjectMutation
        | OperationToolFamily::EditorSceneMutation
        | OperationToolFamily::RuntimeExecution
        | OperationToolFamily::RuntimeCommand => QueuePriority::Normal,
        OperationToolFamily::ProjectImport | OperationToolFamily::DestructiveExport => {
            QueuePriority::Bulk
        }
    }
}

/// The explicit fairness bound: how many times a waiter may be overtaken by
/// higher-priority admissions before it is promoted ahead of them.
///
/// A finite bound is what makes "no starvation" a testable claim instead of a
/// hope about scheduler wake order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StarvationBound(u32);

impl StarvationBound {
    /// Bind one positive bound.
    ///
    /// # Errors
    /// Returns [`FairnessError::ZeroStarvationBound`] for a zero bound, which
    /// would mean priorities are ignored entirely.
    pub const fn new(overtakes: u32) -> Result<Self, FairnessError> {
        if overtakes == 0 {
            return Err(FairnessError::ZeroStarvationBound);
        }
        Ok(Self(overtakes))
    }

    /// Maximum overtakes before promotion.
    #[must_use]
    pub const fn overtakes(self) -> u32 {
        self.0
    }
}

impl Default for StarvationBound {
    fn default() -> Self {
        Self(16)
    }
}

/// How long queued work may wait before admission is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueDeadline {
    wait_budget_ms: u64,
}

impl QueueDeadline {
    /// Bind one positive wait budget.
    ///
    /// # Errors
    /// Returns [`FairnessError::ZeroDeadline`] for a zero budget.
    pub const fn new(wait_budget_ms: u64) -> Result<Self, FairnessError> {
        if wait_budget_ms == 0 {
            return Err(FairnessError::ZeroDeadline);
        }
        Ok(Self { wait_budget_ms })
    }

    /// Wait budget in milliseconds.
    #[must_use]
    pub const fn wait_budget_ms(self) -> u64 {
        self.wait_budget_ms
    }

    /// The refusal for work that never started: `unavailable`, and retryable,
    /// because nothing was attempted.
    #[must_use]
    pub fn expired_error() -> OutcomeError {
        OutcomeError::new(
            ErrorKind::Unavailable,
            "the operation waited longer than its queue deadline and was never started",
            true,
            BTreeMap::new(),
        )
    }
}

/// How long admitted work may run before it is abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperationDeadline {
    run_budget_ms: u64,
}

impl OperationDeadline {
    /// Bind one positive run budget.
    ///
    /// # Errors
    /// Returns [`FairnessError::ZeroDeadline`] for a zero budget.
    pub const fn new(run_budget_ms: u64) -> Result<Self, FairnessError> {
        if run_budget_ms == 0 {
            return Err(FairnessError::ZeroDeadline);
        }
        Ok(Self { run_budget_ms })
    }

    /// Run budget in milliseconds.
    #[must_use]
    pub const fn run_budget_ms(self) -> u64 {
        self.run_budget_ms
    }

    /// The refusal for work that started and did not finish: `timeout`, and not
    /// retryable, because whether the effect landed is not yet proven.
    #[must_use]
    pub fn expired_error() -> OutcomeError {
        OutcomeError::new(
            ErrorKind::Timeout,
            "the operation exceeded its execution deadline; its effect is not yet proven",
            false,
            BTreeMap::new(),
        )
    }
}

/// The two structurally different cancellation results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancellationOutcome {
    /// Cancelled while still queued. Proven that no boundary was touched.
    CancelledBeforeStart,
    /// Cancelled after execution began. The boundary was already reached, so the
    /// outcome is unresolved until the journal proves a pre- or post-state.
    CancelledDuringExecution,
}

impl CancellationOutcome {
    /// Whether this outcome proves the effect did not happen.
    #[must_use]
    pub const fn proves_no_effect(self) -> bool {
        matches!(self, Self::CancelledBeforeStart)
    }

    /// The honest public error for this cancellation.
    ///
    /// Before start: `cancelled`, retryable — nothing happened. During execution:
    /// `uncertain`, not retryable — an automatic retry could duplicate an effect
    /// that may already have landed.
    #[must_use]
    pub fn to_outcome(self) -> OutcomeError {
        match self {
            Self::CancelledBeforeStart => OutcomeError::new(
                ErrorKind::Cancelled,
                "the operation was cancelled before it started; no effect was attempted",
                true,
                BTreeMap::new(),
            ),
            Self::CancelledDuringExecution => OutcomeError::new(
                ErrorKind::Uncertain,
                "the operation was cancelled after it started; its effect is not yet proven",
                false,
                BTreeMap::new(),
            ),
        }
    }
}

/// How a clean shutdown finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownOutcome {
    /// Every admitted operation settled and the lanes are empty.
    Drained,
    /// The shutdown budget expired with work still running. The scheduler stops
    /// admitting, and the surviving work is reported rather than assumed settled.
    DeadlineExpired {
        /// Operations still running when the budget expired.
        still_running: u32,
    },
}

impl ShutdownOutcome {
    /// Whether shutdown completed with nothing left in flight.
    #[must_use]
    pub const fn is_clean(self) -> bool {
        matches!(self, Self::Drained)
    }
}

/// A bounded snapshot of per-class queue occupancy, for fairness assertions.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FairnessSnapshot {
    per_class_queued: BTreeMap<&'static str, u32>,
    per_class_admitted: BTreeMap<&'static str, u64>,
    max_observed_overtakes: u32,
    promotions: u64,
}

impl FairnessSnapshot {
    /// Assemble one snapshot.
    #[must_use]
    pub fn new(
        per_class_queued: BTreeMap<&'static str, u32>,
        per_class_admitted: BTreeMap<&'static str, u64>,
        max_observed_overtakes: u32,
        promotions: u64,
    ) -> Self {
        Self {
            per_class_queued,
            per_class_admitted,
            max_observed_overtakes,
            promotions,
        }
    }

    /// Currently queued operations in one class.
    #[must_use]
    pub fn queued(&self, priority: QueuePriority) -> u32 {
        self.per_class_queued
            .get(priority.as_str())
            .copied()
            .unwrap_or(0)
    }

    /// Total admissions served in one class.
    #[must_use]
    pub fn admitted(&self, priority: QueuePriority) -> u64 {
        self.per_class_admitted
            .get(priority.as_str())
            .copied()
            .unwrap_or(0)
    }

    /// The largest number of overtakes any single waiter observed.
    ///
    /// The acceptance suite asserts this never exceeds the configured
    /// [`StarvationBound`].
    #[must_use]
    pub const fn max_observed_overtakes(&self) -> u32 {
        self.max_observed_overtakes
    }

    /// How many waiters were promoted because they hit the bound.
    #[must_use]
    pub const fn promotions(&self) -> u64 {
        self.promotions
    }
}

/// A fairness or deadline configuration that cannot be honoured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FairnessError {
    /// A starvation bound of zero would disable priorities entirely.
    #[error("the starvation bound must be positive")]
    ZeroStarvationBound,
    /// A zero deadline can never be met.
    #[error("a deadline budget must be positive")]
    ZeroDeadline,
}

impl FairnessError {
    /// Map to the stable domain error taxonomy.
    #[must_use]
    pub fn to_outcome(self) -> OutcomeError {
        OutcomeError::new(
            ErrorKind::InvalidInput,
            self.to_string(),
            false,
            BTreeMap::new(),
        )
    }
}

#[cfg(test)]
mod tests {
    use godot_mcp_domain::CapabilityProfile;
    use godot_mcp_domain::manifests::canonical_manifest;

    use super::{QueuePriority, priority_for};
    use crate::idempotency::{OperationToolFamily, family_of};

    #[test]
    fn every_advertised_tool_resolves_to_exactly_one_priority_class() {
        for profile in [
            CapabilityProfile::Core,
            CapabilityProfile::Editor,
            CapabilityProfile::Runtime,
            CapabilityProfile::Full,
        ] {
            for tool in &canonical_manifest(profile).tools {
                let family = family_of(tool).unwrap_or_else(|error| {
                    panic!(
                        "{tool} in the {} manifest has no family: {error}",
                        profile.manifest_name()
                    )
                });
                assert!(
                    QueuePriority::ALL.contains(&priority_for(family)),
                    "{tool} in the {} manifest has no queue priority",
                    profile.manifest_name()
                );
            }
        }

        for family in OperationToolFamily::ALL {
            assert!(
                QueuePriority::ALL.contains(&priority_for(family)),
                "{} has no queue priority",
                family.as_str()
            );
        }
    }
}
