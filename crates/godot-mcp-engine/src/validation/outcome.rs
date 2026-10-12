//! The one place where "did it succeed?" is decided.
//!
//! Every validation, import and test operation in this module funnels through
//! [`classify`], and [`Classification::Passed`] is *only* reachable when **both**
//! of two independent facts hold:
//!
//! 1. the process's exit status is one the caller asserted in advance
//!    ([`ExitAssertion`]), and
//! 2. structured evidence was actually parsed out of the run
//!    ([`Evidence::ParsedClean`]).
//!
//! The second requirement is not decoration. Todo 13 established by direct
//! observation that a real Godot build **exits 0 with empty output when handed a
//! flag it does not support** — an export-template build answers `--editor` that
//! way. "The process started" and "the process exited 0" therefore prove nothing
//! at all about whether the requested work happened. A run that produced no
//! parsable evidence is [`Classification::Inconclusive`], which is deliberately
//! *not* a pass and *not* a failure: it says the run cannot be interpreted.
//!
//! [`Classification::TimedOut`] is its own category, never folded into
//! `Failed`: a deadline says the operation did not finish, which is a different
//! fact from "it finished and reported a problem".

use std::collections::BTreeSet;
use std::fmt;

use godot_mcp_domain::ErrorKind;

use crate::supervision::protocol::{LaunchDiagnostics, TerminationKind};

/// The exit statuses a caller asserts, in advance, mean "the engine completed the
/// requested work". Anything else is a failure even when evidence parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitAssertion {
    asserted: BTreeSet<i32>,
}

impl ExitAssertion {
    /// Assert exactly one successful status.
    #[must_use]
    pub fn exactly(code: i32) -> Self {
        Self {
            asserted: BTreeSet::from([code]),
        }
    }

    /// Assert a set of successful statuses. An empty set can never be satisfied,
    /// which is the fail-closed direction.
    #[must_use]
    pub fn any_of(codes: impl IntoIterator<Item = i32>) -> Self {
        Self {
            asserted: codes.into_iter().collect(),
        }
    }

    /// Whether `code` is one of the asserted successful statuses.
    #[must_use]
    pub fn satisfied_by(&self, code: Option<i32>) -> bool {
        code.is_some_and(|code| self.asserted.contains(&code))
    }

    /// The asserted statuses, ascending, for diagnostics.
    #[must_use]
    pub fn asserted(&self) -> Vec<i32> {
        self.asserted.iter().copied().collect()
    }
}

impl fmt::Display for ExitAssertion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rendered: Vec<String> = self.asserted.iter().map(i32::to_string).collect();
        formatter.write_str(&rendered.join("|"))
    }
}

/// What the process actually did, as observed by the launch protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    /// The exit status, when it exited normally.
    pub exit_code: Option<i32>,
    /// The signal spelling, when it was signalled.
    pub signal: Option<String>,
    /// Whether the supervisor's deadline, not the process, ended the run.
    pub timed_out: bool,
}

impl Observed {
    /// Adopt the launch protocol's own structured diagnostics. The engine layer
    /// never re-derives exit/signal/timeout facts: Todo 14 already owns them.
    #[must_use]
    pub fn from_launch(diagnostics: &LaunchDiagnostics) -> Self {
        Self {
            exit_code: diagnostics.exit_code,
            signal: diagnostics.signal.clone(),
            timed_out: diagnostics.termination == TerminationKind::TimedOut,
        }
    }
}

/// What was parsed out of a run's output. This is the half of a success verdict
/// that an exit status can never substitute for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    /// Structured, operation-specific output was parsed and reports no problem.
    ParsedClean,
    /// Structured output was parsed and reports at least one problem.
    ParsedFailure,
    /// Nothing interpretable was produced. Never a pass, whatever the exit code.
    Absent {
        /// Why the run could not be interpreted.
        reason: String,
    },
    /// The capability this operation needs is not offered by this build at all,
    /// so the operation was never attempted.
    Unsupported {
        /// Exactly what was inspected and what it showed.
        detection: String,
    },
}

/// The five disjoint verdicts a validation operation can reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Classification {
    /// Asserted exit status **and** parsed clean evidence. Both, always.
    Passed,
    /// The run completed and something is provably wrong.
    Failed,
    /// The deadline ended the run. Distinct from `Failed`.
    TimedOut,
    /// This build cannot do the requested work; nothing was attempted.
    Unsupported,
    /// The run cannot be interpreted either way. Never reported as a pass.
    Inconclusive,
}

impl Classification {
    /// Every classification. There are five.
    pub const ALL: [Self; 5] = [
        Self::Passed,
        Self::Failed,
        Self::TimedOut,
        Self::Unsupported,
        Self::Inconclusive,
    ];

    /// Stable wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Unsupported => "unsupported",
            Self::Inconclusive => "inconclusive",
        }
    }

    /// Whether this verdict says the requested work provably completed.
    #[must_use]
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Passed)
    }

    /// The stable domain error kind a non-passing verdict maps to.
    #[must_use]
    pub const fn error_kind(self) -> Option<ErrorKind> {
        match self {
            Self::Passed => None,
            Self::Failed => Some(ErrorKind::InvalidInput),
            Self::TimedOut => Some(ErrorKind::Timeout),
            Self::Unsupported => Some(ErrorKind::Unsupported),
            Self::Inconclusive => Some(ErrorKind::Uncertain),
        }
    }
}

impl fmt::Display for Classification {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A classification plus the exact reason it was reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// The verdict itself.
    pub classification: Classification,
    /// Why, in terms a report can quote verbatim.
    pub reason: String,
}

impl Verdict {
    /// Whether the requested work provably completed.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.classification.is_success()
    }

    /// The stable domain error kind, when this verdict is not a pass.
    #[must_use]
    pub const fn error_kind(&self) -> Option<ErrorKind> {
        self.classification.error_kind()
    }
}

/// Decide one operation's verdict from an observation and its parsed evidence.
///
/// The ordering below is normative:
///
/// 1. an unsupported capability means nothing was attempted, so no exit status
///    can turn it into a pass;
/// 2. a deadline is its own category and outranks any parse of partial output;
/// 3. a signalled process never completed its work;
/// 4. absent evidence is inconclusive — this is the branch that rejects
///    "exited 0 with nothing to show for it", including a fabricated `started`
///    banner;
/// 5. parsed failures are failures;
/// 6. only a parsed-clean run whose status matches the assertion passes.
#[must_use]
pub fn classify(observed: &Observed, evidence: &Evidence, assertion: &ExitAssertion) -> Verdict {
    if let Evidence::Unsupported { detection } = evidence {
        return Verdict {
            classification: Classification::Unsupported,
            reason: format!("the requested capability is not available: {detection}"),
        };
    }
    if observed.timed_out {
        return Verdict {
            classification: Classification::TimedOut,
            reason:
                "the operation exceeded its deadline and was terminated inside its containment \
                     boundary"
                    .to_owned(),
        };
    }
    if let Some(signal) = &observed.signal {
        return Verdict {
            classification: Classification::Failed,
            reason: format!("the engine was terminated by {signal} before it completed"),
        };
    }
    if let Evidence::Absent { reason } = evidence {
        return Verdict {
            classification: Classification::Inconclusive,
            reason: format!(
                "exit status {} produced no interpretable evidence, so the outcome is not \
                 determinable: {reason}",
                render_exit(observed.exit_code)
            ),
        };
    }
    let status_ok = assertion.satisfied_by(observed.exit_code);
    match (evidence, status_ok) {
        (Evidence::ParsedClean, true) => Verdict {
            classification: Classification::Passed,
            reason: format!(
                "exit status {} matched the asserted {assertion} and the parsed engine evidence \
                 reports no problem",
                render_exit(observed.exit_code)
            ),
        },
        (Evidence::ParsedClean, false) => Verdict {
            classification: Classification::Failed,
            reason: format!(
                "the parsed engine evidence reports no problem but exit status {} does not match \
                 the asserted {assertion}",
                render_exit(observed.exit_code)
            ),
        },
        (Evidence::ParsedFailure, _) => Verdict {
            classification: Classification::Failed,
            reason: format!(
                "the parsed engine evidence reports at least one problem (exit status {})",
                render_exit(observed.exit_code)
            ),
        },
        // Both remaining evidence forms returned above; this arm is unreachable in
        // practice and is spelled out rather than swallowed by a wildcard.
        (Evidence::Absent { reason } | Evidence::Unsupported { detection: reason }, _) => Verdict {
            classification: Classification::Inconclusive,
            reason: reason.clone(),
        },
    }
}

fn render_exit(code: Option<i32>) -> String {
    code.map_or_else(|| "none".to_owned(), |code| code.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exited(code: i32) -> Observed {
        Observed {
            exit_code: Some(code),
            signal: None,
            timed_out: false,
        }
    }

    #[test]
    fn there_are_exactly_five_disjoint_classifications() {
        assert_eq!(Classification::ALL.len(), 5);
        let spellings: BTreeSet<&str> = Classification::ALL
            .iter()
            .map(|classification| classification.as_str())
            .collect();
        assert_eq!(
            spellings.len(),
            5,
            "every classification is distinctly named"
        );
        assert!(
            Classification::ALL
                .iter()
                .filter(|classification| classification.is_success())
                .count()
                == 1,
            "exactly one classification means success"
        );
    }

    #[test]
    fn a_clean_exit_without_parsed_evidence_is_never_a_pass() {
        let verdict = classify(
            &exited(0),
            &Evidence::Absent {
                reason: "the engine printed only a start banner".to_owned(),
            },
            &ExitAssertion::exactly(0),
        );
        assert_eq!(verdict.classification, Classification::Inconclusive);
        assert_eq!(verdict.error_kind(), Some(ErrorKind::Uncertain));
        assert!(!verdict.is_success());
    }

    #[test]
    fn parsed_evidence_alone_is_not_enough_either() {
        let verdict = classify(
            &exited(1),
            &Evidence::ParsedClean,
            &ExitAssertion::exactly(0),
        );
        assert_eq!(
            verdict.classification,
            Classification::Failed,
            "both halves are required, not either half"
        );
    }

    #[test]
    fn a_timeout_is_its_own_classification() {
        let observed = Observed {
            exit_code: Some(0),
            signal: None,
            timed_out: true,
        };
        let verdict = classify(
            &observed,
            &Evidence::ParsedClean,
            &ExitAssertion::exactly(0),
        );
        assert_eq!(verdict.classification, Classification::TimedOut);
        assert_eq!(verdict.error_kind(), Some(ErrorKind::Timeout));
    }

    #[test]
    fn an_unsupported_capability_outranks_a_zero_exit() {
        let verdict = classify(
            &exited(0),
            &Evidence::Unsupported {
                detection: "--check-only is absent from this build's --help".to_owned(),
            },
            &ExitAssertion::exactly(0),
        );
        assert_eq!(verdict.classification, Classification::Unsupported);
        assert_eq!(verdict.error_kind(), Some(ErrorKind::Unsupported));
    }

    #[test]
    fn only_both_halves_together_pass() {
        let verdict = classify(
            &exited(0),
            &Evidence::ParsedClean,
            &ExitAssertion::exactly(0),
        );
        assert_eq!(verdict.classification, Classification::Passed);
        assert_eq!(verdict.error_kind(), None);
    }

    #[test]
    fn an_empty_assertion_can_never_be_satisfied() {
        let assertion = ExitAssertion::any_of(std::iter::empty());
        assert!(!assertion.satisfied_by(Some(0)));
        assert!(!assertion.satisfied_by(None));
        assert_eq!(
            classify(&exited(0), &Evidence::ParsedClean, &assertion).classification,
            Classification::Failed
        );
    }
}
