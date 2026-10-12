//! `--import`: import the project's resources and quit.
//!
//! The engine's own handler is exactly:
//!
//! ```cpp
//! } else if (arg == "--import") {
//!     editor = true;
//!     cmdline_tool = true;
//!     wait_for_import = true;
//!     quit_after = 1;
//! }
//! ```
//!
//! so `--import` implies `--editor` and `--quit`, is editor-build-only, and was
//! introduced in Godot 4.2. It does **not** imply `--headless`, which is why this
//! module always passes `--headless` itself.
//!
//! Two honesty problems shape this module:
//!
//! * There is no `--import`-specific summary line anywhere in the engine, and no
//!   documented exit-code contract for it. The progress text is ordinary
//!   `EditorFileSystem` logging whose format is not stable, so it is **never**
//!   parsed as proof of success.
//! * An export-template build ignores `--import` entirely and exits 0. Todo 13
//!   observed exactly this class of silent no-op directly.
//!
//! So success is proven by the conjunction of **parsed-clean capped output, an
//! asserted zero exit and every nominated artifact being present**. The caller
//! nominates the artifacts (`.godot/uid_cache.bin`, a specific `.import` file,
//! whatever the operation actually needs). A parsed error proves failure even when
//! Godot exits zero and the artifact already existed; truncated output or a missing
//! artifact leaves the result inconclusive because neither can prove a clean run.
//!
//! Writing `.godot/` into the project is the explicit, requested semantics of an
//! import — it is the only thing this module lets the engine change, and nothing
//! outside that is touched.

use std::path::Path;
use std::time::Duration;

use godot_mcp_domain::safe_root::SafeRoot;

use crate::supervision::journal::OperationFamily;
use crate::validation::diagnostic::{Diagnostic, has_error, parse_engine_diagnostics};
use crate::validation::outcome::{Evidence, ExitAssertion, Observed, Verdict, classify};
use crate::validation::runner::{EngineRunner, RunError, headless_project_args};
use crate::validation::{is_valid_project_relative_path, project_relative_path};

/// The tool name every import is journaled under.
pub const TOOL_NAME: &str = "validation.import_project";

/// The flags an import requires the build to advertise. `--import` is
/// editor-only, so a build without it is an export template and can never import.
pub const REQUIRED_CAPABILITIES: [&str; 3] = ["--headless", "--path", "--import"];

/// One import to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRequest {
    /// Project-relative paths the import is expected to produce. Empty means the
    /// caller nominated nothing, which can never be proven and is refused as
    /// inconclusive rather than assumed successful.
    pub expected_artifacts: Vec<String>,
    /// Wall-clock budget for the engine.
    pub deadline: Duration,
}

/// One completed import.
#[derive(Debug, Clone)]
pub struct ImportReport {
    /// The verdict, requiring both an asserted status and observed artifacts.
    pub verdict: Verdict,
    /// Normalized diagnostics.
    pub diagnostics: Vec<Diagnostic>,
    /// Nominated artifacts that exist now and did not exist before, plus those
    /// that exist now and existed before — in short, everything present.
    pub produced: Vec<String>,
    /// Nominated artifacts that are still absent.
    pub missing: Vec<String>,
    /// The exact argument array issued.
    pub argv: Vec<String>,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// The transaction this import was journaled under, when it ran.
    pub transaction_id: Option<String>,
}

/// Import the held project's resources.
///
/// # Errors
/// Returns [`RunError`] when an expected artifact path is invalid or the engine
/// could not be launched or contained. A failed import, a silently ignored flag
/// and a deadline are all reports.
pub fn import_project(
    runner: &EngineRunner<'_>,
    root: &SafeRoot,
    request: &ImportRequest,
) -> Result<ImportReport, RunError> {
    if !root.is_bound_to(runner.project_root()) {
        return Err(RunError::EvidenceRootMismatch);
    }
    for artifact in &request.expected_artifacts {
        if !is_valid_project_relative_path(artifact) {
            return Err(RunError::InvalidProjectPath(
                "expected import artifact",
                artifact.clone(),
            ));
        }
    }
    if let Some(missing) = runner.missing_capability(&REQUIRED_CAPABILITIES) {
        return Ok(unsupported(request, &missing));
    }

    let arguments = import_arguments(runner);
    let run = runner.invoke(
        TOOL_NAME,
        // An import writes `.godot/` inside the project, so it is a project
        // mutation, not a read-only probe.
        OperationFamily::ProjectMutation,
        arguments,
        request.deadline,
    )?;
    let diagnostics = parse_engine_diagnostics(&run.combined_output());

    let mut produced = Vec::new();
    let mut missing = Vec::new();
    for artifact in &request.expected_artifacts {
        if root
            .read(Path::new(project_relative_path(artifact)))
            .is_ok()
        {
            produced.push(artifact.clone());
        } else {
            missing.push(artifact.clone());
        }
    }

    let verdict = import_verdict(
        &run.observed,
        &diagnostics,
        &request.expected_artifacts,
        &missing,
        run.truncated,
    );
    Ok(ImportReport {
        verdict,
        diagnostics,
        produced,
        missing,
        argv: run.argv.clone(),
        stdout: run.stdout.clone(),
        stderr: run.stderr.clone(),
        transaction_id: Some(run.transaction_id),
    })
}

/// Decide one import outcome from the complete evidence shared by every caller.
///
/// This is the single owner of import success semantics. Both the direct validation
/// runner and the production MCP process route must call it after observing
/// artifacts and parsing the capped engine output.
#[must_use]
pub fn import_verdict(
    observed: &Observed,
    diagnostics: &[Diagnostic],
    expected_artifacts: &[String],
    missing: &[String],
    output_truncated: bool,
) -> Verdict {
    let evidence = if expected_artifacts.is_empty() {
        Evidence::Absent {
            reason: "no expected import artifact was nominated, and the engine emits no stable \
                     import summary, so a successful import cannot be observed"
                .to_owned(),
        }
    } else if has_error(diagnostics) {
        Evidence::ParsedFailure
    } else if output_truncated {
        Evidence::Absent {
            reason: "the capped engine output was truncated, so an unobserved import error may \
                     remain and a clean import cannot be proven"
                .to_owned(),
        }
    } else if missing.is_empty() {
        Evidence::ParsedClean
    } else {
        Evidence::Absent {
            reason: format!(
                "the engine reported no error but produced none of {}: the import cannot be \
                 confirmed from its exit status alone",
                missing.join(", ")
            ),
        }
    };
    classify(observed, &evidence, &ExitAssertion::exactly(0))
}

/// The argument array for one import.
///
/// `--headless` is explicit because `--import` does not imply it; `--quit` is not
/// passed because `--import` already sets `quit_after = 1` in the engine itself.
fn import_arguments(runner: &EngineRunner<'_>) -> Vec<String> {
    let mut arguments = headless_project_args(runner.project_root());
    arguments.push("--import".to_owned());
    arguments
}

fn unsupported(request: &ImportRequest, missing: &str) -> ImportReport {
    let detection = format!(
        "the configured Godot build does not advertise {missing}; --import is editor-only and an \
         export-template build ignores it silently while exiting 0"
    );
    let verdict = classify(
        &Observed {
            exit_code: None,
            signal: None,
            timed_out: false,
        },
        &Evidence::Unsupported { detection },
        &ExitAssertion::exactly(0),
    );
    ImportReport {
        verdict,
        diagnostics: Vec::new(),
        produced: Vec::new(),
        missing: request.expected_artifacts.clone(),
        argv: Vec::new(),
        stdout: String::new(),
        stderr: String::new(),
        transaction_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validation::outcome::Classification;

    fn observed(exit_code: i32) -> Observed {
        Observed {
            exit_code: Some(exit_code),
            signal: None,
            timed_out: false,
        }
    }

    fn expected_artifact() -> Vec<String> {
        vec![".godot/uid_cache.bin".to_owned()]
    }

    #[test]
    fn corrupt_import_with_artifacts_and_zero_exit_fails() {
        let expected = expected_artifact();
        let diagnostics = parse_engine_diagnostics(concat!(
            "ERROR: Condition \"rl != len\" is true. Returning: ERR_FILE_CORRUPT\n",
            "   at: check_error (drivers/png/png_driver_common.cpp:57)\n"
        ));

        let verdict = import_verdict(&observed(0), &diagnostics, &expected, &[], false);

        assert_eq!(
            verdict.classification,
            Classification::Failed,
            "parsed import errors must defeat a zero exit and pre-existing artifacts"
        );
    }

    #[test]
    fn clean_import_with_all_artifacts_passes() {
        let expected = expected_artifact();

        let verdict = import_verdict(&observed(0), &[], &expected, &[], false);

        assert_eq!(
            verdict.classification,
            Classification::Passed,
            "a clean zero exit with every nominated artifact must still pass"
        );
    }

    #[test]
    fn nonzero_import_with_all_artifacts_fails() {
        let expected = expected_artifact();

        let verdict = import_verdict(&observed(17), &[], &expected, &[], false);

        assert_eq!(
            verdict.classification,
            Classification::Failed,
            "artifacts cannot turn a nonzero engine exit into a pass"
        );
    }

    #[test]
    fn missing_import_artifact_remains_inconclusive() {
        let expected = expected_artifact();

        let verdict = import_verdict(&observed(0), &[], &expected, &expected, false);

        assert_eq!(
            verdict.classification,
            Classification::Inconclusive,
            "missing nominated artifacts remain unproven rather than failed or passed"
        );
    }

    #[test]
    fn truncated_clean_import_evidence_is_inconclusive() {
        let expected = expected_artifact();

        let verdict = import_verdict(&observed(0), &[], &expected, &[], true);

        assert_eq!(
            verdict.classification,
            Classification::Inconclusive,
            "capped output that may have hidden an error cannot prove a clean import"
        );
    }

    #[test]
    fn timed_out_import_remains_timed_out() {
        let expected = expected_artifact();
        let timed_out = Observed {
            exit_code: None,
            signal: None,
            timed_out: true,
        };

        let verdict = import_verdict(&timed_out, &[], &expected, &expected, false);

        assert_eq!(
            verdict.classification,
            Classification::TimedOut,
            "a supervisor deadline must remain timed_out, never generic failure or success"
        );
    }

    #[test]
    fn an_import_never_passes_headless_implicitly() {
        assert!(
            REQUIRED_CAPABILITIES.contains(&"--headless"),
            "--import implies --editor and --quit but never --headless"
        );
        assert!(REQUIRED_CAPABILITIES.contains(&"--import"));
    }

    #[test]
    fn an_unsupported_build_reports_every_nominated_artifact_as_missing() {
        let report = unsupported(
            &ImportRequest {
                expected_artifacts: vec![".godot/uid_cache.bin".to_owned()],
                deadline: Duration::from_secs(1),
            },
            "--import",
        );
        assert_eq!(report.verdict.classification, Classification::Unsupported);
        assert_eq!(report.missing, vec![".godot/uid_cache.bin".to_owned()]);
        assert!(report.produced.is_empty());
        assert!(report.transaction_id.is_none());
    }
}
