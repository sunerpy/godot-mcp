//! `--check-only --script <path>`: parse one script and quit.
//!
//! The engine's contract is exactly:
//!
//! ```cpp
//! if (check_only) {
//!     return script_res->is_valid() ? EXIT_SUCCESS : EXIT_FAILURE;
//! }
//! ```
//!
//! so a clean script exits 0 **printing nothing at all**, and a broken one exits 1
//! after writing the two-line `SCRIPT ERROR:` / `   at:` diagnostic to stderr.
//! Godot's macOS builds write the same diagnostic and exit 0, so a broken
//! script is recognised by its diagnostic, never by its status alone.
//!
//! That asymmetry is the whole problem this module solves. "Exit 0 and silence" is
//! also exactly what a build that does not support `--check-only` produces, since
//! the engine ignores unsupported flags without a word. A pass therefore cannot be
//! read off the target run alone, and this module does not try to:
//!
//! 1. the build must **advertise** `--check-only` and `--script` (Todo 13 parsed
//!    the flags out of `--help`), otherwise the operation is `unsupported` and no
//!    process is created at all;
//! 2. before trusting a silent success, the module runs a **calibration canary** —
//!    a script it writes itself, containing a deliberate syntax error, outside the
//!    user's project. A build that honours `--check-only` must fail that canary
//!    with a located parse diagnostic. If the canary comes back clean, the build is
//!    provably not doing the check, so the operation is `unsupported` rather than
//!    trusting any target verdict.
//!
//! The canary is what turns "we asked nicely" into evidence. It costs one extra
//! headless invocation per check and is the only reason a silent zero exit can be
//! believed.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::supervision::journal::OperationFamily;
use crate::validation::diagnostic::{
    Diagnostic, DiagnosticCategory, DiagnosticSeverity, parse_engine_diagnostics,
};
use crate::validation::is_valid_project_relative_path;
use crate::validation::outcome::{Evidence, ExitAssertion, Verdict, classify};
use crate::validation::runner::{EngineRunner, RunError, headless_project_args};

/// The tool name every script check is journaled under.
pub const TOOL_NAME: &str = "validation.check_script";

/// The flags a script check requires the build to advertise.
pub const REQUIRED_CAPABILITIES: [&str; 4] = ["--headless", "--path", "--script", "--check-only"];

/// The token the canary's deliberate syntax error is built from. Only the
/// drift-guard test needs it by name; production code matches on the line.
#[cfg(test)]
const CANARY_ERROR_TOKEN: &str = "godot_mcp_calibration_syntax_error";

/// The 1-based line the canary's deliberate syntax error sits on. Asserted
/// against [`CANARY_SOURCE`] by a unit test, so the two can never drift apart.
const CANARY_ERROR_LINE: u32 = 9;

/// The canary's deliberate syntax error, and the token the engine reports on.
const CANARY_SOURCE: &str = concat!(
    "extends Node\n",
    "\n",
    "# godot-mcp calibration canary. This file is written to the engine plane's own\n",
    "# scratch directory, never into the user's project, and exists for exactly one\n",
    "# reason: a Godot build that honours `--check-only` must report a parse error\n",
    "# here. A build that reports success is silently ignoring the flag, so its\n",
    "# verdict on the real script cannot be believed.\n",
    "func _ready() -> void:\n",
    "\tgodot_mcp_calibration_syntax_error not_a_statement\n"
);

/// One script to check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptCheckRequest {
    /// The script to parse, as a `res://` path inside the held project.
    pub script: String,
    /// Wall-clock budget for the engine.
    pub deadline: Duration,
}

/// What the calibration canary proved about this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Calibration {
    /// Whether the build provably honours `--check-only`.
    pub honored: bool,
    /// Exactly what was observed, quotable in a report.
    pub detail: String,
    /// The diagnostics the canary produced.
    pub diagnostics: Vec<Diagnostic>,
    /// The canary's exit status.
    pub exit_code: Option<i32>,
}

/// One completed script check.
#[derive(Debug, Clone)]
pub struct ScriptCheckReport {
    /// The script that was checked.
    pub script: String,
    /// The verdict, which requires both an asserted status and parsed evidence.
    pub verdict: Verdict,
    /// Normalized diagnostics from the target run.
    pub diagnostics: Vec<Diagnostic>,
    /// What the canary proved about this build.
    pub calibration: Calibration,
    /// The exact argument array issued for the target run.
    pub argv: Vec<String>,
    /// Captured stdout from the target run.
    pub stdout: String,
    /// Captured stderr from the target run.
    pub stderr: String,
    /// The transaction the target run was journaled under, when it ran.
    pub transaction_id: Option<String>,
}

/// Check one script's syntax with `--check-only --script`.
///
/// # Errors
/// Returns [`RunError`] when the script path is invalid or the engine could not be
/// launched or contained. A script with a syntax error, a build that ignores the
/// flag, and a deadline are all *reports*, not errors.
pub fn check_script(
    runner: &EngineRunner<'_>,
    request: &ScriptCheckRequest,
) -> Result<ScriptCheckReport, RunError> {
    if !is_valid_project_relative_path(&request.script) {
        return Err(RunError::InvalidProjectPath(
            "script",
            request.script.clone(),
        ));
    }
    // A build that does not advertise the flags would ignore them silently, so the
    // refusal happens before anything is created.
    if let Some(missing) = runner.missing_capability(&REQUIRED_CAPABILITIES) {
        return Ok(unsupported_report(request, &missing));
    }

    let calibration = calibrate(runner, request.deadline)?;
    if !calibration.honored {
        return Ok(unsupported_after_calibration(request, calibration));
    }
    let arguments = check_arguments(runner, &request.script);
    let run = runner.invoke(
        TOOL_NAME,
        OperationFamily::DiagnosticProbe,
        arguments,
        request.deadline,
    )?;
    let diagnostics = parse_engine_diagnostics(&run.combined_output());
    let located: Vec<Diagnostic> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.severity == DiagnosticSeverity::Error
                && matches!(
                    diagnostic.category,
                    DiagnosticCategory::ScriptParse | DiagnosticCategory::ScriptLoad
                )
        })
        .cloned()
        .collect();

    // A clean parse is proven by two independent facts: this build demonstrably
    // performs the check (the canary failed as it must), and this script produced
    // no parse diagnostic. Neither alone is sufficient.
    let evidence = if located.is_empty() {
        if calibration.honored {
            Evidence::ParsedClean
        } else {
            Evidence::Absent {
                reason: format!(
                    "this build did not report the calibration canary's deliberate syntax error, \
                     so its silence about {} proves nothing ({})",
                    request.script, calibration.detail
                ),
            }
        }
    } else {
        Evidence::ParsedFailure
    };
    let verdict = classify(&run.observed, &evidence, &ExitAssertion::exactly(0));
    Ok(ScriptCheckReport {
        script: request.script.clone(),
        verdict,
        diagnostics: located,
        calibration,
        argv: run.argv.clone(),
        stdout: run.stdout.clone(),
        stderr: run.stderr.clone(),
        transaction_id: Some(run.transaction_id),
    })
}

/// The argument array for one `--check-only --script` invocation.
///
/// `--check-only` is documented as "only parse for errors and quit (use with
/// `--script`)", and the engine only consults it when a script was given, so the
/// two always travel together.
fn check_arguments(runner: &EngineRunner<'_>, script: &str) -> Vec<String> {
    let mut arguments = headless_project_args(runner.project_root());
    arguments.push("--check-only".to_owned());
    arguments.push("--script".to_owned());
    arguments.push(script.to_owned());
    arguments
}

/// Run the calibration canary.
///
/// The canary lives in the engine plane's own work directory, never in the user's
/// project, so checking a project never adds a file to it. It is passed by
/// absolute filesystem path, which the engine accepts for `--script`.
fn calibrate(runner: &EngineRunner<'_>, deadline: Duration) -> Result<Calibration, RunError> {
    let path = canary_path(runner);
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        return Ok(Calibration {
            honored: false,
            detail: format!(
                "the calibration canary directory {} could not be created: {error}",
                parent.display()
            ),
            diagnostics: Vec::new(),
            exit_code: None,
        });
    }
    if let Err(error) = std::fs::write(&path, CANARY_SOURCE) {
        return Ok(Calibration {
            honored: false,
            detail: format!(
                "the calibration canary {} could not be written: {error}",
                path.display()
            ),
            diagnostics: Vec::new(),
            exit_code: None,
        });
    }
    let mut arguments = headless_project_args(runner.project_root());
    arguments.push("--check-only".to_owned());
    arguments.push("--script".to_owned());
    arguments.push(path.display().to_string());
    let run = runner.invoke(
        TOOL_NAME,
        OperationFamily::DiagnosticProbe,
        arguments,
        deadline,
    )?;
    let diagnostics: Vec<Diagnostic> = parse_engine_diagnostics(&run.combined_output())
        .into_iter()
        .filter(|diagnostic| diagnostic.category == DiagnosticCategory::ScriptParse)
        .collect();
    let located = diagnostics
        .iter()
        .filter(|diagnostic| points_at_canary(diagnostic, &path))
        .count();
    // The canary must be reported *and* located, in the canary itself: a status
    // with no diagnostic located in the canary is not proof the parser ran.
    // Godot's macOS builds report the error and still exit 0, so the status may
    // be either the documented 1 or that 0; a target's verdict then rests on its
    // own diagnostics, which the canary has just shown this build prints.
    let honored =
        !run.observed.timed_out && matches!(run.observed.exit_code, Some(0 | 1)) && located > 0;
    let detail = match run.observed.exit_code {
        Some(1) if honored => format!(
            "the calibration canary failed as required: exit 1 with {located} parse diagnostic(s) \
             located in the canary itself"
        ),
        Some(0) if honored => format!(
            "the calibration canary reported its deliberate syntax error with {located} parse \
             diagnostic(s) located in the canary but exited 0: this build's exit status does not \
             carry a parse failure, so a script's verdict rests on its parse diagnostics"
        ),
        status => format!(
            "the calibration canary returned exit {status:?} with {located} parse diagnostic(s) \
             located in the canary; a build that honours --check-only must answer a deliberate \
             syntax error with a diagnostic naming the canary and its line"
        ),
    };
    Ok(Calibration {
        honored,
        detail,
        diagnostics,
        exit_code: run.observed.exit_code,
    })
}

/// Whether `diagnostic` names the canary file.
///
/// The canary is handed to the engine as an absolute filesystem path, not a
/// `res://` resource, so the engine reports it in the `at:` line's non-resource
/// position. That is deliberately *not* treated as a project location by
/// [`crate::validation::diagnostic`], so the canary is matched against its own
/// path in either field rather than by relaxing that rule.
fn points_at_canary(diagnostic: &Diagnostic, canary: &Path) -> bool {
    let name = canary.file_name().and_then(|name| name.to_str());
    let matches_name = |candidate: &str| {
        name.is_some_and(|name| candidate.contains(name))
            && candidate.rsplit(':').next().is_some_and(|line| {
                line.parse::<u32>()
                    .is_ok_and(|line| line == CANARY_ERROR_LINE)
            })
    };
    diagnostic
        .engine_location
        .as_deref()
        .is_some_and(matches_name)
        || diagnostic.location.as_ref().is_some_and(|location| {
            name.is_some_and(|name| location.file.contains(name))
                && location.line == Some(CANARY_ERROR_LINE)
        })
}

fn canary_path(runner: &EngineRunner<'_>) -> PathBuf {
    runner
        .binding()
        .work_dir
        .join(".godot-mcp")
        .join("calibration")
        .join("check_only_canary.gd")
}

/// Build the structured refusal returned when capability negotiation proves this
/// Godot build cannot perform a trustworthy script check.
#[must_use]
pub fn unsupported_report(request: &ScriptCheckRequest, missing: &str) -> ScriptCheckReport {
    let detection = format!(
        "the configured Godot build does not advertise {missing}; a build that lacks it ignores \
         the flag silently and exits 0, so no result could be trusted"
    );
    let verdict = classify(
        &crate::validation::outcome::Observed {
            exit_code: None,
            signal: None,
            timed_out: false,
        },
        &Evidence::Unsupported {
            detection: detection.clone(),
        },
        &ExitAssertion::exactly(0),
    );
    ScriptCheckReport {
        script: request.script.clone(),
        verdict,
        diagnostics: Vec::new(),
        calibration: Calibration {
            honored: false,
            detail: detection,
            diagnostics: Vec::new(),
            exit_code: None,
        },
        argv: Vec::new(),
        stdout: String::new(),
        stderr: String::new(),
        transaction_id: None,
    }
}

fn unsupported_after_calibration(
    request: &ScriptCheckRequest,
    calibration: Calibration,
) -> ScriptCheckReport {
    let verdict = classify(
        &crate::validation::outcome::Observed {
            exit_code: None,
            signal: None,
            timed_out: false,
        },
        &Evidence::Unsupported {
            detection: format!(
                "the configured Godot build did not prove that it honours --check-only: {}",
                calibration.detail
            ),
        },
        &ExitAssertion::exactly(0),
    );
    ScriptCheckReport {
        script: request.script.clone(),
        verdict,
        diagnostics: Vec::new(),
        calibration,
        argv: Vec::new(),
        stdout: String::new(),
        stderr: String::new(),
        transaction_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_canary_source_carries_exactly_one_deliberate_error_on_the_recorded_line() {
        assert_eq!(CANARY_SOURCE.matches(CANARY_ERROR_TOKEN).count(), 1);
        assert!(
            CANARY_SOURCE.starts_with("extends Node"),
            "the canary is a real GDScript file"
        );
        let line = CANARY_SOURCE
            .lines()
            .position(|line| line.contains(CANARY_ERROR_TOKEN))
            .map(|index| u32::try_from(index).expect("a short file") + 1)
            .expect("the canary contains its own error");
        assert_eq!(
            line, CANARY_ERROR_LINE,
            "the recorded canary error line must match the source it describes"
        );
    }

    #[test]
    fn the_required_capabilities_pair_check_only_with_script() {
        assert!(REQUIRED_CAPABILITIES.contains(&"--check-only"));
        assert!(
            REQUIRED_CAPABILITIES.contains(&"--script"),
            "--check-only is only consulted when a script was given"
        );
    }

    #[test]
    fn an_unsupported_build_produces_a_report_naming_the_flag() {
        let report = unsupported_report(
            &ScriptCheckRequest {
                script: "res://a.gd".to_owned(),
                deadline: Duration::from_secs(1),
            },
            "--check-only",
        );
        assert_eq!(
            report.verdict.classification,
            crate::validation::outcome::Classification::Unsupported
        );
        assert!(report.verdict.reason.contains("--check-only"));
        assert!(
            report.argv.is_empty() && report.transaction_id.is_none(),
            "an unsupported operation never launches the engine"
        );
    }
}
