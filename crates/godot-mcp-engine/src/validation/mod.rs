//! Script/project validation, resource import and automated test execution.
//!
//! Every operation here drives a real headless Godot process through Todo 14's
//! gapless launch protocol, inside a containment boundary, under a supervisor-
//! enforced deadline, with byte-capped output — there is no second, bare
//! `Command::spawn` path in this module.
//!
//! The module is organised around one refusal:
//!
//! > **A process that started, or a process that exited 0, is not evidence that
//! > the requested work happened.**
//!
//! Todo 13 established this by direct observation rather than by argument: a real
//! Godot export-template build handed `--editor` exits 0 with empty output, and
//! the official command-line reference states that "unknown command line arguments
//! have no effect whatsoever. The engine will **not** warn you when using a command
//! line argument that doesn't exist with a given build type." So the engine's own
//! contract is that an unsupported flag is silently ignored — and an ignored flag
//! looks exactly like a clean success.
//!
//! Everything in [`outcome`] exists to make that failure mode unrepresentable:
//! [`outcome::Classification::Passed`] requires an asserted exit status **and**
//! parsed structured evidence, and a run with no interpretable evidence is
//! [`outcome::Classification::Inconclusive`] — not a pass, not a failure.
//!
//! | module | responsibility |
//! | --- | --- |
//! | [`outcome`] | the five disjoint verdicts and the both-halves-required rule |
//! | [`diagnostic`] | normalizing the engine's `SCRIPT ERROR:` / `   at:` output into located diagnostics |
//! | [`runner`] | the single launch-protocol path every invocation takes |
//! | [`script`] | `--check-only --script`, with a calibration canary that proves the flag is honoured |
//! | [`import`] | `--import`, proven by observing the artifacts it was asked to produce |
//! | [`framework`] | detecting **already-committed** GUT / gdUnit4 setups, and their per-framework exit and summary grammars |
//! | [`suite`] | running the detected framework and classifying the result |
//! | [`run`] | the verdict of one supervised project run |
//!
//! Two things this module never does: it never adds a test framework to a user's
//! project (a project without one gets `unsupported` plus the exact detection
//! detail), and it never changes project files outside the explicit import/test
//! semantics the caller asked for.

use std::path::{Component, Path};

pub mod diagnostic;
pub mod framework;
pub mod import;
pub mod outcome;
pub mod run;
pub mod runner;
pub mod script;
pub mod suite;

/// JSON Schema pattern for the project-relative path spelling accepted at
/// engine request boundaries.
///
/// A path may have one `res://` prefix. Its non-empty remainder uses `/` only,
/// contains no residual URI scheme, is not absolute, option-shaped or
/// Windows-prefixed, and contains neither parent traversal nor control characters.
pub const PROJECT_RELATIVE_PATH_PATTERN: &str = r"^(?:res://)?(?!(?:/|-|[A-Za-z]:))(?!(?:\.\.(?:/|$)|.*/\.\.(?:/|$)))(?!.*(?:\\|://|[\x00-\x1F\x7F-\x9F])).+$";

/// Return the project-relative spelling of a path that may use Godot's `res://`
/// prefix. Validation stays with the caller because framework-facing paths keep
/// their original spelling while filesystem consumers use this spelling.
#[must_use]
pub fn project_relative_path(path: &str) -> &str {
    path.strip_prefix("res://").unwrap_or(path)
}

/// Whether `path` has the project-relative spelling advertised by
/// [`PROJECT_RELATIVE_PATH_PATTERN`].
#[must_use]
pub fn is_valid_project_relative_path(path: &str) -> bool {
    let project_relative = project_relative_path(path);
    let candidate = Path::new(project_relative);
    let bytes = project_relative.as_bytes();
    let has_windows_prefix = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';

    !candidate.as_os_str().is_empty()
        && !project_relative.starts_with('-')
        && !path.contains('\\')
        && !project_relative.contains("://")
        && !path.chars().any(char::is_control)
        && !has_windows_prefix
        && !candidate.is_absolute()
        && !candidate.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}
