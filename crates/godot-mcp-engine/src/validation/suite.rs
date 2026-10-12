//! Running a project's **already-installed** test framework headlessly.
//!
//! The runner is deliberately thin, because every framework-specific fact lives in
//! its [`FrameworkAdapter`](crate::validation::framework::FrameworkAdapter): which
//! committed markers prove it is installed, the argument array that runs it, the
//! exit statuses it documents, and the parser for its own summary text.
//!
//! Three rules decide the verdict:
//!
//! * **No framework → `unsupported`, with the detection detail.** Nothing is
//!   launched, and nothing is installed into the project to make a run possible.
//! * **A pass needs every signal to agree.** The exit status must be one the
//!   framework documents as clean, the framework's own summary must have parsed and
//!   report zero failures, and the engine must not have emitted an Error-severity
//!   diagnostic. A zero exit with no parsable summary, or clean framework counts
//!   contradicted by an engine error, is `inconclusive` rather than a pass or a
//!   claim that the tests themselves failed.
//! * **A deadline is `timed_out`, never `failed`.** "It did not finish" is a
//!   different fact from "it finished and reported failures", and the launch
//!   protocol already compensates a timed-out transaction rather than committing
//!   it.
//!
//! Counts are never synthesized. If the summary did not parse, [`TestRunReport`]
//! carries no summary at all rather than a plausible-looking zero.

use std::time::Duration;

use crate::supervision::journal::OperationFamily;
use crate::validation::diagnostic::{Diagnostic, has_error, parse_engine_diagnostics};
use crate::validation::framework::{
    Detection, FrameworkKind, TestSummary, adapter_for, detect_frameworks,
};
use crate::validation::is_valid_project_relative_path;
use crate::validation::outcome::{
    Classification, Evidence, ExitAssertion, Observed, Verdict, classify,
};
use crate::validation::runner::{EngineRunner, RunError, headless_project_args};

/// The tool name every test run is journaled under.
pub const TOOL_NAME: &str = "validation.run_tests";

/// The flags a framework test run requires the build to advertise.
pub const REQUIRED_CAPABILITIES: [&str; 3] = ["--headless", "--path", "--script"];

/// One test run to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestRunRequest {
    /// Which suites or directories to run, as `res://` paths. Empty means the
    /// framework's own default selection.
    pub selection: Vec<String>,
    /// Wall-clock budget for the whole run.
    pub deadline: Duration,
}

/// One completed (or refused) test run.
#[derive(Debug, Clone)]
pub struct TestRunReport {
    /// Which framework ran, when one was detected.
    pub framework: Option<FrameworkKind>,
    /// The verdict.
    pub verdict: Verdict,
    /// Counts parsed out of the framework's own summary. `None` means nothing
    /// parsable was produced — never "zero".
    pub summary: Option<TestSummary>,
    /// Normalized engine diagnostics observed during the run.
    pub diagnostics: Vec<Diagnostic>,
    /// What framework detection looked for and found.
    pub detection: Detection,
    /// The exact argument array issued.
    pub argv: Vec<String>,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// The transaction this run was journaled under, when it ran.
    pub transaction_id: Option<String>,
}

/// Run the project's committed test framework.
///
/// # Errors
/// Returns [`RunError`] when a selection path is invalid or the engine could not
/// be launched or contained. A failing suite, an absent framework and a deadline
/// are all reports.
pub fn run_tests(
    runner: &EngineRunner<'_>,
    request: &TestRunRequest,
) -> Result<TestRunReport, RunError> {
    for selection in &request.selection {
        if !is_valid_project_relative_path(selection) {
            return Err(RunError::InvalidProjectPath(
                "test selection",
                selection.clone(),
            ));
        }
    }
    let detection = detect_frameworks(runner.project_root());
    let Some(detected) = detection.sole().cloned() else {
        return Ok(unsupported(runner, request, detection));
    };
    if let Some(missing) = runner.missing_capability(&REQUIRED_CAPABILITIES) {
        let detail = format!(
            "the configured Godot build does not advertise {missing}, so {}'s command-line runner \
             cannot be started",
            detected.kind
        );
        return Ok(refuse(Some(detected.kind), detection, detail));
    }

    let adapter = adapter_for(detected.kind);
    let mut arguments = headless_project_args(runner.project_root());
    arguments.extend(adapter.arguments(&request.selection)?);
    let run = runner.invoke(
        TOOL_NAME,
        OperationFamily::RuntimeExecution,
        arguments,
        request.deadline,
    )?;
    let output = run.combined_output();
    let diagnostics = parse_engine_diagnostics(&output);
    let summary = adapter.parse_summary(&output);

    let verdict = suite_verdict(
        detected.kind,
        summary.as_ref(),
        &diagnostics,
        &run.observed,
        &adapter.completion_statuses(),
        &adapter.clean_statuses(),
    );
    // A timed-out or otherwise uninterpretable run never carries counts.
    let reportable = matches!(
        verdict.classification,
        Classification::Passed | Classification::Failed
    );
    Ok(TestRunReport {
        framework: Some(detected.kind),
        verdict,
        summary: summary.filter(|_| reportable),
        diagnostics,
        detection,
        argv: run.argv.clone(),
        stdout: run.stdout.clone(),
        stderr: run.stderr.clone(),
        transaction_id: Some(run.transaction_id),
    })
}

fn suite_verdict(
    framework: FrameworkKind,
    summary: Option<&TestSummary>,
    diagnostics: &[Diagnostic],
    observed: &Observed,
    completion_statuses: &ExitAssertion,
    clean_statuses: &ExitAssertion,
) -> Verdict {
    let evidence = match summary {
        None => Evidence::Absent {
            reason: format!(
                "{} produced no parsable end-of-run summary, so no test counts exist and the run \
                 cannot be interpreted from its exit status alone",
                framework
            ),
        },
        Some(summary) if summary.is_clean() => {
            if has_error(diagnostics) {
                Evidence::Absent {
                    reason: format!(
                        "{framework} reported a clean summary, but Godot emitted at least one \
                         Error-severity diagnostic; the test counts are clean but the run is not \
                         trustworthy"
                    ),
                }
            } else if completion_statuses.satisfied_by(observed.exit_code) {
                Evidence::ParsedClean
            } else {
                Evidence::Absent {
                    reason: format!(
                        "{} reported a clean summary but exited {:?}, which is not one of the \
                         statuses it documents ({}); the run cannot be interpreted",
                        framework, observed.exit_code, completion_statuses
                    ),
                }
            }
        }
        Some(_) => Evidence::ParsedFailure,
    };
    classify(observed, &evidence, clean_statuses)
}

fn unsupported(
    runner: &EngineRunner<'_>,
    request: &TestRunRequest,
    detection: Detection,
) -> TestRunReport {
    let _ = (runner, request);
    let detail = if detection.detected.is_empty() {
        format!(
            "no committed test framework was found in this project, and none will be installed to \
             make a run possible: {}",
            detection.detail()
        )
    } else {
        format!(
            "more than one test framework is installed, so which one to run is ambiguous: {}",
            detection.detail()
        )
    };
    refuse(None, detection, detail)
}

fn refuse(framework: Option<FrameworkKind>, detection: Detection, detail: String) -> TestRunReport {
    let verdict = classify(
        &Observed {
            exit_code: None,
            signal: None,
            timed_out: false,
        },
        &Evidence::Unsupported { detection: detail },
        &ExitAssertion::exactly(0),
    );
    TestRunReport {
        framework,
        verdict,
        summary: None,
        diagnostics: Vec::new(),
        detection,
        argv: Vec::new(),
        stdout: String::new(),
        stderr: String::new(),
        transaction_id: None,
    }
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
    fn summary_and_diagnostics_decision_table_is_honest() {
        let clean = TestSummary {
            total: 1,
            passed: 1,
            ..TestSummary::default()
        };
        let failing = TestSummary {
            total: 1,
            failed: 1,
            ..TestSummary::default()
        };
        let summaries = [
            (
                "clean",
                Some(clean),
                [
                    Classification::Passed,
                    Classification::Passed,
                    Classification::Inconclusive,
                ],
            ),
            (
                "failing",
                Some(failing),
                [
                    Classification::Failed,
                    Classification::Failed,
                    Classification::Failed,
                ],
            ),
            (
                "absent",
                None,
                [
                    Classification::Inconclusive,
                    Classification::Inconclusive,
                    Classification::Inconclusive,
                ],
            ),
        ];
        let warning = parse_engine_diagnostics(concat!(
            "WARNING: a benign warning during the run\n",
            "   at: push_warning (core/variant/variant_utility.cpp:1034)\n"
        ));
        let error = parse_engine_diagnostics(concat!(
            "ERROR: ERR_FILE_CORRUPT: a resource failed to load during the run\n",
            "   at: push_error (core/variant/variant_utility.cpp:1023)\n"
        ));
        let diagnostic_sets = [("no", Vec::new()), ("warning", warning), ("error", error)];
        let completion = ExitAssertion::any_of([0, 1]);
        let clean_statuses = ExitAssertion::exactly(0);

        for (summary_label, summary, expected) in summaries {
            for (index, (diagnostic_label, diagnostics)) in diagnostic_sets.iter().enumerate() {
                let verdict = suite_verdict(
                    FrameworkKind::Gut,
                    summary.as_ref(),
                    diagnostics,
                    &exited(0),
                    &completion,
                    &clean_statuses,
                );
                assert_eq!(
                    verdict.classification, expected[index],
                    "{summary_label} summary with {diagnostic_label} diagnostics must preserve the framework counts without overstating trust"
                );
            }
        }
    }

    #[test]
    fn a_project_without_a_framework_is_unsupported_and_never_gains_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let detection = detect_frameworks(dir.path());
        let report = refuse(
            None,
            detection.clone(),
            format!("nothing installed: {}", detection.detail()),
        );
        assert_eq!(report.verdict.classification, Classification::Unsupported);
        assert!(report.summary.is_none());
        assert!(report.transaction_id.is_none());
        assert!(
            !dir.path().join("addons").exists(),
            "detection never creates anything"
        );
        for kind in FrameworkKind::ALL {
            assert!(report.detection.detail().contains(kind.as_str()));
        }
    }

    #[test]
    fn the_required_capabilities_are_what_a_cli_runner_needs() {
        assert!(REQUIRED_CAPABILITIES.contains(&"--script"));
        assert!(REQUIRED_CAPABILITIES.contains(&"--headless"));
    }
}
