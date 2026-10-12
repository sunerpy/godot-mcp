//! The verdict of one supervised project run.
//!
//! A run is judged by the same both-halves rule as every other engine
//! operation (see [`super::outcome`]): an exit status alone proves nothing. The
//! evidence half for a run is what the engine printed:
//!
//! * any error-severity diagnostic is a parsed failure, whatever the exit code;
//! * otherwise the engine's own startup banner on stdout is parsed-clean
//!   evidence that this was a real engine run rather than a silently ignored
//!   flag set or a foreign executable; and
//! * a run that printed neither is inconclusive, never a pass.

use super::diagnostic::{Diagnostic, DiagnosticSeverity};
use super::outcome::{Evidence, ExitAssertion, Observed, Verdict, classify};

/// The prefix every official Godot 4 build prints as its first stdout line.
pub const ENGINE_BANNER_PREFIX: &str = "Godot Engine v";

/// Classify one completed run from its observation, its parsed diagnostics and
/// its captured stdout.
#[must_use]
pub fn run_verdict(observed: &Observed, diagnostics: &[Diagnostic], stdout: &str) -> Verdict {
    let evidence = if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
    {
        Evidence::ParsedFailure
    } else if stdout
        .lines()
        .any(|line| line.trim_start().starts_with(ENGINE_BANNER_PREFIX))
    {
        Evidence::ParsedClean
    } else {
        Evidence::Absent {
            reason: "the process printed neither the Godot engine banner nor an engine diagnostic"
                .to_owned(),
        }
    };
    classify(observed, &evidence, &ExitAssertion::exactly(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validation::diagnostic::parse_engine_diagnostics;
    use crate::validation::outcome::Classification;

    const BANNER: &str =
        "Godot Engine v4.7.1.stable.official.a13da4feb - https://godotengine.org\n";

    fn exited(code: i32) -> Observed {
        Observed {
            exit_code: Some(code),
            signal: None,
            timed_out: false,
        }
    }

    #[test]
    fn a_clean_engine_run_with_the_banner_passes() {
        let verdict = run_verdict(&exited(0), &[], BANNER);
        assert_eq!(verdict.classification, Classification::Passed);
    }

    #[test]
    fn a_parsed_engine_error_fails_even_with_exit_zero() {
        let diagnostics = parse_engine_diagnostics(
            "SCRIPT ERROR: Parse Error: Expected statement.\n   at: GDScript::reload (res://main.gd:3)\n",
        );
        let verdict = run_verdict(&exited(0), &diagnostics, BANNER);
        assert_eq!(verdict.classification, Classification::Failed);
    }

    #[test]
    fn a_nonzero_exit_with_only_the_banner_fails() {
        let verdict = run_verdict(&exited(1), &[], BANNER);
        assert_eq!(verdict.classification, Classification::Failed);
    }

    #[test]
    fn a_warning_alone_does_not_fail_a_clean_run() {
        let diagnostics = parse_engine_diagnostics(
            "WARNING: Could not set V-Sync mode.\n   at: set_use_vsync (platform/linuxbsd/x11/gl_manager_x11.cpp:372)\n",
        );
        let verdict = run_verdict(&exited(0), &diagnostics, BANNER);
        assert_eq!(verdict.classification, Classification::Passed);
    }

    #[test]
    fn exit_zero_without_any_engine_evidence_is_inconclusive() {
        let verdict = run_verdict(&exited(0), &[], "");
        assert_eq!(verdict.classification, Classification::Inconclusive);
    }

    #[test]
    fn a_deadline_or_signal_is_never_a_pass() {
        let timed_out = Observed {
            exit_code: None,
            signal: None,
            timed_out: true,
        };
        assert_eq!(
            run_verdict(&timed_out, &[], BANNER).classification,
            Classification::TimedOut
        );
        let signalled = Observed {
            exit_code: None,
            signal: Some("SIGKILL".to_owned()),
            timed_out: false,
        };
        assert_eq!(
            run_verdict(&signalled, &[], BANNER).classification,
            Classification::Failed
        );
    }
}
