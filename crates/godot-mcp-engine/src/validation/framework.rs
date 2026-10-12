//! Detecting **already-committed** test frameworks, and nothing else.
//!
//! This module never installs, vendors, downloads or scaffolds a test framework.
//! It answers one question — "does this project already contain a working GUT or
//! gdUnit4 setup?" — by looking for the exact files a real installation commits,
//! and when the answer is no it says so with the full list of what it inspected.
//! Adding a framework to a user's project to make a test run possible would be a
//! mutation nobody asked for.
//!
//! Detection is a registry, not a hard-coded pair. Each [`FrameworkAdapter`]
//! contributes:
//!
//! * the committed markers that prove it is installed,
//! * the argument array that runs it headlessly,
//! * the exit statuses that framework asserts mean "the suite completed",
//! * the parser for that framework's own end-of-run summary.
//!
//! The exit statuses matter and are deliberately per-framework, because the two
//! frameworks disagree: GUT documents "`0` will be returned if all tests pass and
//! `1` will be returned if any fail", while gdUnit4 uses `RETURN_ERROR = 100` and
//! `RETURN_WARNING = 101` (tests passed, nodes leaked) and never uses `1` at all.
//! A single global "nonzero means failure" rule would misclassify one of them.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::validation::outcome::ExitAssertion;

/// Which test framework. There are exactly two adapters today; the registry is
/// the extension point, not this enum's call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameworkKind {
    /// GUT — Godot Unit Test (`bitwes/Gut`).
    Gut,
    /// gdUnit4 (`MikeSchulze/gdUnit4`).
    GdUnit4,
}

impl FrameworkKind {
    /// Every supported framework.
    pub const ALL: [Self; 2] = [Self::Gut, Self::GdUnit4];

    /// Stable wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gut => "gut",
            Self::GdUnit4 => "gdunit4",
        }
    }
}

impl fmt::Display for FrameworkKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Counts parsed out of a framework's own end-of-run summary.
///
/// Every field is a number the framework printed, never a number this layer
/// inferred from an exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TestSummary {
    /// Total test cases the framework reported.
    pub total: u32,
    /// Tests that passed.
    pub passed: u32,
    /// Tests that failed.
    pub failed: u32,
    /// Tests that were skipped or left pending.
    pub skipped: u32,
    /// Framework-level errors, distinct from test failures.
    pub errors: u32,
}

impl TestSummary {
    /// Whether the framework reported a clean suite.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.failed == 0 && self.errors == 0
    }
}

/// One committed file detection looked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkerObservation {
    /// The project-relative path.
    pub path: String,
    /// Whether it is present.
    pub present: bool,
    /// Whether this marker is required for the framework to count as installed.
    pub required: bool,
}

/// A framework that was found installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedFramework {
    /// Which framework.
    pub kind: FrameworkKind,
    /// The `res://` runner script the CLI invocation uses.
    pub runner_script: String,
    /// The markers that proved it, in the order they were inspected.
    pub markers: Vec<MarkerObservation>,
}

/// What detection looked for and what it found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detection {
    /// Frameworks that are installed, in registry order.
    pub detected: Vec<DetectedFramework>,
    /// Every marker inspected, across every framework, in registry order.
    pub inspected: Vec<MarkerObservation>,
}

impl Detection {
    /// A one-line, quotable account of exactly what was inspected.
    ///
    /// This is what an `unsupported` verdict carries, so the answer to "why did
    /// you say no framework?" is never "it just did not find one".
    #[must_use]
    pub fn detail(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for adapter in adapters() {
            let kind = adapter.kind();
            let markers: Vec<String> = self
                .inspected
                .iter()
                .filter(|marker| {
                    adapter
                        .markers()
                        .iter()
                        .any(|candidate| candidate.path == marker.path)
                })
                .map(|marker| {
                    format!(
                        "{}={}",
                        marker.path,
                        if marker.present { "present" } else { "absent" }
                    )
                })
                .collect();
            let found = self.detected.iter().any(|detected| detected.kind == kind);
            parts.push(format!(
                "{kind}: {} [{}]",
                if found { "installed" } else { "not installed" },
                markers.join(", ")
            ));
        }
        parts.join("; ")
    }

    /// The single framework to run, when exactly one is installed.
    #[must_use]
    pub fn sole(&self) -> Option<&DetectedFramework> {
        match self.detected.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }
}

/// One committed marker an adapter looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Marker {
    /// The project-relative path.
    pub path: &'static str,
    /// Whether the framework counts as installed only if this exists.
    pub required: bool,
}

/// How one adapter passes test-selection elements to its command-line parser.
///
/// The adapter owns this declaration and the shared argument builder consumes it,
/// so a delimiter cannot be added to argv construction without also becoming part
/// of the validation contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionPassing {
    /// One flag followed by one value containing every selection element.
    Delimited {
        /// The framework flag that introduces the list.
        flag: &'static str,
        /// The delimiter used when this adapter joins elements.
        separator: char,
        /// Other characters the framework also interprets as list delimiters.
        additional_separators: &'static [char],
    },
    /// One flag/value pair per element; the value is not split again.
    Repeated {
        /// The framework flag repeated for every selection element.
        flag: &'static str,
    },
}

impl SelectionPassing {
    /// Every character the downstream parser can use to split one element.
    #[must_use]
    pub fn separators(self) -> Vec<char> {
        match self {
            Self::Delimited {
                separator,
                additional_separators,
                ..
            } => std::iter::once(separator)
                .chain(additional_separators.iter().copied())
                .collect(),
            Self::Repeated { .. } => Vec::new(),
        }
    }

    fn separator_in(self, element: &str) -> Option<char> {
        self.separators()
            .into_iter()
            .find(|separator| element.contains(*separator))
    }
}

/// A selection element that an adapter would split into multiple paths.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "test selection element `{element}` contains `{separator}`, which {framework} treats as a path separator"
)]
pub struct SelectionSeparatorError {
    element: String,
    separator: char,
    framework: FrameworkKind,
}

/// A pluggable test-framework integration.
pub trait FrameworkAdapter: Sync {
    /// Which framework this adapter integrates.
    fn kind(&self) -> FrameworkKind;

    /// The committed files that prove this framework is installed.
    fn markers(&self) -> &'static [Marker];

    /// The `res://` runner script the CLI invocation runs.
    fn runner_script(&self) -> &'static str;

    /// Arguments that do not depend on the caller's selection.
    fn fixed_arguments(&self) -> Vec<String>;

    /// How the framework consumes selection elements after process launch.
    fn selection_passing(&self) -> SelectionPassing;

    /// The argument array that runs `selection` headlessly, appended after the
    /// shared `--headless --path <root>` prefix. Always an array, and refuses an
    /// element before argv assembly if this adapter would split it again.
    fn arguments(&self, selection: &[String]) -> Result<Vec<String>, SelectionSeparatorError> {
        let passing = self.selection_passing();
        for element in selection {
            if let Some(separator) = passing.separator_in(element) {
                return Err(SelectionSeparatorError {
                    element: element.clone(),
                    separator,
                    framework: self.kind(),
                });
            }
        }

        let mut arguments = self.fixed_arguments();
        match passing {
            SelectionPassing::Delimited {
                flag, separator, ..
            } if !selection.is_empty() => {
                arguments.push(flag.to_owned());
                arguments.push(selection.join(&separator.to_string()));
            }
            SelectionPassing::Repeated { flag } => {
                for element in selection {
                    arguments.push(flag.to_owned());
                    arguments.push(element.clone());
                }
            }
            SelectionPassing::Delimited { .. } => {}
        }
        Ok(arguments)
    }

    /// The exit statuses this framework asserts mean "the suite ran to
    /// completion", clean or not. A status outside this set means the run did not
    /// complete the way the framework documents.
    fn completion_statuses(&self) -> ExitAssertion;

    /// The exit statuses this framework asserts mean "the suite ran and every
    /// test passed".
    fn clean_statuses(&self) -> ExitAssertion;

    /// Parse this framework's own end-of-run summary out of captured output.
    /// `None` means nothing parsable was produced — never "assume zero".
    fn parse_summary(&self, output: &str) -> Option<TestSummary>;
}

/// GUT — `bitwes/Gut`.
struct GutAdapter;

const GUT_MARKERS: [Marker; 2] = [
    Marker {
        path: "addons/gut/plugin.cfg",
        required: true,
    },
    Marker {
        path: "addons/gut/gut_cmdln.gd",
        required: true,
    },
];

impl FrameworkAdapter for GutAdapter {
    fn kind(&self) -> FrameworkKind {
        FrameworkKind::Gut
    }

    fn markers(&self) -> &'static [Marker] {
        &GUT_MARKERS
    }

    fn runner_script(&self) -> &'static str {
        "res://addons/gut/gut_cmdln.gd"
    }

    fn fixed_arguments(&self) -> Vec<String> {
        // `-gexit` is required: without it GUT leaves the window open and never
        // exits (`GutRunner._handle_quit`).
        vec![
            "-s".to_owned(),
            self.runner_script().to_owned(),
            "-gexit".to_owned(),
        ]
    }

    fn selection_passing(&self) -> SelectionPassing {
        // GUT's option parser documents `-gdir` as a comma-delimited list and
        // explicitly states that commas cannot be escaped.
        SelectionPassing::Delimited {
            flag: "-gdir",
            separator: ',',
            additional_separators: &[],
        }
    }

    fn completion_statuses(&self) -> ExitAssertion {
        // GUT documents exactly two: 0 (all passed) and 1 (some failed).
        ExitAssertion::any_of([0, 1])
    }

    fn clean_statuses(&self) -> ExitAssertion {
        ExitAssertion::exactly(0)
    }

    fn parse_summary(&self, output: &str) -> Option<TestSummary> {
        parse_gut_summary(output)
    }
}

/// gdUnit4 — `MikeSchulze/gdUnit4`.
struct GdUnit4Adapter;

const GDUNIT4_MARKERS: [Marker; 2] = [
    Marker {
        path: "addons/gdUnit4/plugin.cfg",
        required: true,
    },
    Marker {
        path: "addons/gdUnit4/bin/GdUnitCmdTool.gd",
        required: true,
    },
];

impl FrameworkAdapter for GdUnit4Adapter {
    fn kind(&self) -> FrameworkKind {
        FrameworkKind::GdUnit4
    }

    fn markers(&self) -> &'static [Marker] {
        &GDUNIT4_MARKERS
    }

    fn runner_script(&self) -> &'static str {
        "res://addons/gdUnit4/bin/GdUnitCmdTool.gd"
    }

    fn fixed_arguments(&self) -> Vec<String> {
        vec!["-s".to_owned(), self.runner_script().to_owned()]
    }

    fn selection_passing(&self) -> SelectionPassing {
        // gdUnit4's CmdArgumentParser consumes the next argv element whole and
        // accumulates multiple paths only from repeated `-a` options.
        SelectionPassing::Repeated { flag: "-a" }
    }

    fn completion_statuses(&self) -> ExitAssertion {
        // gdUnit4 never uses 1: RETURN_SUCCESS = 0, RETURN_WARNING = 101
        // (passed, orphans leaked), RETURN_ERROR = 100.
        ExitAssertion::any_of([0, 100, 101])
    }

    fn clean_statuses(&self) -> ExitAssertion {
        // 101 means every test passed but nodes leaked. That is deliberately
        // *not* clean here: an orphan leak is a real defect the framework chose
        // to report distinctly, so it is surfaced rather than rounded to a pass.
        ExitAssertion::exactly(0)
    }

    fn parse_summary(&self, output: &str) -> Option<TestSummary> {
        parse_gdunit4_summary(output)
    }
}

static ADAPTERS: [&dyn FrameworkAdapter; 2] = [&GutAdapter, &GdUnit4Adapter];

/// Every registered framework adapter, in a stable order.
#[must_use]
pub fn adapters() -> &'static [&'static dyn FrameworkAdapter] {
    &ADAPTERS
}

/// The union of downstream path separators declared by registered adapters.
///
/// The public request contract cannot know which framework a project contains
/// when its JSON Schema is advertised, so it conservatively excludes this union.
#[must_use]
pub fn selection_separators() -> Vec<char> {
    let mut separators = BTreeSet::new();
    for adapter in adapters() {
        separators.extend(adapter.selection_passing().separators());
    }
    separators.into_iter().collect()
}

/// Refuse an element that any registered adapter could reinterpret as two paths.
///
/// # Errors
/// Returns [`SelectionSeparatorError`] naming the first adapter and delimiter
/// that would split `element`.
pub fn validate_selection_element(element: &str) -> Result<(), SelectionSeparatorError> {
    for adapter in adapters() {
        if let Some(separator) = adapter.selection_passing().separator_in(element) {
            return Err(SelectionSeparatorError {
                element: element.to_owned(),
                separator,
                framework: adapter.kind(),
            });
        }
    }
    Ok(())
}

/// The adapter for `kind`.
#[must_use]
pub fn adapter_for(kind: FrameworkKind) -> &'static dyn FrameworkAdapter {
    adapters()
        .iter()
        .find(|adapter| adapter.kind() == kind)
        .copied()
        .unwrap_or(&GutAdapter)
}

/// Inspect `project_root` for committed framework installations.
///
/// Only reads: every marker is a stat, and nothing is created.
#[must_use]
pub fn detect_frameworks(project_root: &Path) -> Detection {
    let mut detected = Vec::new();
    let mut inspected = Vec::new();
    for adapter in adapters() {
        let mut observations = Vec::new();
        for marker in adapter.markers() {
            let observation = MarkerObservation {
                path: marker.path.to_owned(),
                present: project_root.join(marker.path).exists(),
                required: marker.required,
            };
            inspected.push(observation.clone());
            observations.push(observation);
        }
        let installed = observations
            .iter()
            .filter(|observation| observation.required)
            .all(|observation| observation.present);
        if installed {
            detected.push(DetectedFramework {
                kind: adapter.kind(),
                runner_script: adapter.runner_script().to_owned(),
                markers: observations,
            });
        }
    }
    Detection {
        detected,
        inspected,
    }
}

/// Parse GUT's `Totals` block.
///
/// GUT renders each line as `label.rpad(18) + str(value).lpad(5)` and — this is
/// the trap — renders the value `0` as the literal string `none`. `Failing Tests`
/// is omitted entirely on a clean run, so its absence means zero rather than
/// "unparsable".
fn parse_gut_summary(output: &str) -> Option<TestSummary> {
    let mut in_totals = false;
    let mut total = None;
    let mut passed = None;
    let mut failed = 0;
    let mut skipped = 0;
    let mut errors = 0;
    for line in output.lines() {
        let trimmed = line.trim_end();
        // GUT 9.5 and later head the block `Totals`; 9.4.0, the release for
        // Godot 4.3 and 4.4, heads it `---- Totals ----`.
        if trimmed.trim().trim_matches('-').trim() == "Totals" {
            in_totals = true;
            continue;
        }
        if !in_totals {
            continue;
        }
        // 9.4.0 labels the per-test counts `Passing` and `Failing`, indented
        // under `Tests`; later releases spell them `Passing Tests` and
        // `Failing Tests`. `Risky/Pending` and `Errors` are the same in both.
        if let Some(value) = gut_total(trimmed, "Tests") {
            total = Some(value);
        } else if let Some(value) =
            gut_total(trimmed, "Passing Tests").or_else(|| gut_total(trimmed, "Passing"))
        {
            passed = Some(value);
        } else if let Some(value) =
            gut_total(trimmed, "Failing Tests").or_else(|| gut_total(trimmed, "Failing"))
        {
            failed = value;
        } else if let Some(value) = gut_total(trimmed, "Risky/Pending") {
            skipped = value;
        } else if let Some(value) = gut_total(trimmed, "Errors") {
            errors = value;
        }
    }
    // Both load-bearing counts must have been printed. Without them there is no
    // summary, and inventing one from the exit status is exactly what this module
    // must never do.
    Some(TestSummary {
        total: total?,
        passed: passed?,
        failed,
        skipped,
        errors,
    })
}

/// One `label` + right-aligned value line from GUT's totals block.
fn gut_total(line: &str, label: &str) -> Option<u32> {
    let rest = line.trim_start().strip_prefix(label)?;
    // `Tests` must not also match `Passing Tests`; the label is a prefix of the
    // trimmed line, so a longer label simply never reaches here for the shorter
    // one's line. The remainder must be whitespace then the value.
    if !rest.starts_with(' ') {
        return None;
    }
    let value = rest.trim();
    if value == "none" {
        return Some(0);
    }
    // `Asserts` can be `17/19`; the counts parsed here are never that field.
    value.parse::<u32>().ok()
}

/// Parse gdUnit4's `Overall Summary:` line.
///
/// The shape is ` N test cases | N errors | N failures | N flaky | N skipped | N
/// orphans |`, printed by `GdUnitConsoleTestReporter._print_summary`. Output goes
/// through a CSI writer, so ANSI escapes are stripped before matching.
fn parse_gdunit4_summary(output: &str) -> Option<TestSummary> {
    let stripped = strip_ansi(output);
    let mut lines = stripped.lines().peekable();
    while let Some(line) = lines.next() {
        if !line.trim_end().ends_with("Overall Summary:") {
            continue;
        }
        let counts = lines.next()?;
        return parse_gdunit4_counts(counts);
    }
    None
}

fn parse_gdunit4_counts(line: &str) -> Option<TestSummary> {
    let mut total = None;
    let mut failed = None;
    let mut skipped = None;
    let mut errors = None;
    for field in line.split('|') {
        let field = field.trim();
        if field.is_empty() {
            continue;
        }
        let (number, label) = field.split_once(' ')?;
        let value: u32 = number.parse().ok()?;
        match label.trim() {
            "test cases" => total = Some(value),
            "failures" => failed = Some(value),
            "skipped" => skipped = Some(value),
            "errors" => errors = Some(value),
            _ => {}
        }
    }
    let total = total?;
    let failed = failed?;
    let skipped = skipped?;
    Some(TestSummary {
        total,
        // gdUnit4 does not print a passing count, so it is derived from the
        // counts it *did* print rather than assumed.
        passed: total.saturating_sub(failed).saturating_sub(skipped),
        failed,
        skipped,
        errors: errors.unwrap_or(0),
    })
}

/// Remove ANSI/CSI escape sequences.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            out.push(character);
            continue;
        }
        // Consume the escape sequence up to and including its final byte.
        for escaped in chars.by_ref() {
            if escaped.is_ascii_alphabetic() {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn there_is_exactly_one_adapter_per_framework_kind() {
        assert_eq!(adapters().len(), FrameworkKind::ALL.len());
        for kind in FrameworkKind::ALL {
            assert_eq!(adapter_for(kind).kind(), kind);
        }
    }

    #[test]
    fn detection_reports_every_marker_it_inspected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let detection = detect_frameworks(dir.path());
        assert!(detection.detected.is_empty());
        assert_eq!(
            detection.inspected.len(),
            GUT_MARKERS.len() + GDUNIT4_MARKERS.len()
        );
        assert!(detection.inspected.iter().all(|marker| !marker.present));
        let detail = detection.detail();
        assert!(detail.contains("gut: not installed"));
        assert!(detail.contains("gdunit4: not installed"));
        assert!(detail.contains("addons/gut/plugin.cfg=absent"));
    }

    #[test]
    fn a_partial_installation_is_not_an_installation() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("addons/gut")).expect("mkdir");
        std::fs::write(dir.path().join("addons/gut/plugin.cfg"), "[plugin]").expect("write");
        let detection = detect_frameworks(dir.path());
        assert!(
            detection.detected.is_empty(),
            "a plugin.cfg without the CLI entry point cannot be run"
        );
    }

    #[test]
    fn gut_totals_parse_with_none_and_an_absent_failing_line() {
        let clean = concat!(
            "Totals\n",
            "------\n",
            "Scripts               2\n",
            "Tests                 7\n",
            "Passing Tests         7\n",
            "Asserts              19\n",
            "Time              0.417s\n"
        );
        let summary = parse_gut_summary(clean).expect("a clean run still has a summary");
        assert_eq!(summary.total, 7);
        assert_eq!(summary.passed, 7);
        assert_eq!(summary.failed, 0, "an absent Failing Tests line means zero");
        assert!(summary.is_clean());

        let noisy = concat!(
            "Totals\n",
            "------\n",
            "Errors             none\n",
            "Scripts               2\n",
            "Tests                 7\n",
            "Passing Tests         5\n",
            "Failing Tests         2\n",
            "Asserts           17/19\n"
        );
        let summary = parse_gut_summary(noisy).expect("summary");
        assert_eq!(summary.failed, 2);
        assert_eq!(
            summary.errors, 0,
            "the literal `none` is zero, not unparsable"
        );
        assert!(!summary.is_clean());
    }

    /// GUT before 9.5 — 9.4.0 is the release for Godot 4.3 and 4.4 — heads
    /// its totals `---- Totals ----` and labels them `Passing`, `Failing` and
    /// `Risky/Pending`, indented; 9.5 and later print `Totals` and
    /// `Passing Tests`. Both are real runs of the exact builds, colour codes
    /// and all.
    #[test]
    fn gut_totals_parse_in_the_format_of_every_supported_gut() {
        let older = parse_gut_summary(include_str!(
            "../../tests/fixtures/validation/gut-output/gut-9.4.0-godot-4.4.1-pass.out"
        ))
        .expect("GUT 9.4.0 prints a summary");
        assert_eq!((older.total, older.passed, older.failed), (1, 1, 0));
        assert!(older.is_clean());

        let mixed = parse_gut_summary(include_str!(
            "../../tests/fixtures/validation/gut-output/gut-9.4.0-godot-4.4.1-mixed.out"
        ))
        .expect("GUT 9.4.0 prints a summary for a failing run");
        assert_eq!(
            (mixed.total, mixed.passed, mixed.failed, mixed.skipped),
            (3, 1, 1, 1)
        );
        assert!(!mixed.is_clean());

        let newer = parse_gut_summary(include_str!(
            "../../tests/fixtures/validation/gut-output/gut-9.7.1-godot-4.7.1-pass.out"
        ))
        .expect("GUT 9.7.1 prints a summary");
        assert_eq!((newer.total, newer.passed, newer.failed), (1, 1, 0));
    }

    #[test]
    fn output_without_a_totals_block_yields_no_summary() {
        assert!(parse_gut_summary("godot-mcp-fake: started\n").is_none());
        assert!(parse_gdunit4_summary("godot-mcp-fake: started\n").is_none());
    }

    #[test]
    fn gdunit4_counts_parse_and_derive_passed() {
        let output = concat!(
            "Overall Summary:\n",
            " 4 test cases | 0 errors | 1 failures | 0 flaky | 1 skipped | 0 orphans |\n"
        );
        let summary = parse_gdunit4_summary(output).expect("summary");
        assert_eq!(summary.total, 4);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.passed, 2);
        assert_eq!(summary.errors, 0);
    }

    #[test]
    fn ansi_escapes_do_not_defeat_the_gdunit4_parser() {
        let output = concat!(
            "\u{1b}[1;34mOverall Summary:\u{1b}[0m\n",
            "\u{1b}[0m 2 test cases | 0 errors | 0 failures | 0 flaky | 0 skipped | 0 orphans |\n"
        );
        let summary = parse_gdunit4_summary(output).expect("summary");
        assert_eq!(summary.total, 2);
        assert_eq!(summary.passed, 2);
    }

    #[test]
    fn the_two_frameworks_assert_different_exit_statuses() {
        assert_eq!(
            adapter_for(FrameworkKind::Gut)
                .completion_statuses()
                .asserted(),
            vec![0, 1]
        );
        assert_eq!(
            adapter_for(FrameworkKind::GdUnit4)
                .completion_statuses()
                .asserted(),
            vec![0, 100, 101],
            "gdUnit4 never uses exit 1"
        );
    }

    #[test]
    fn framework_arguments_are_arrays_with_the_runner_script_after_a_flag() {
        let arguments = adapter_for(FrameworkKind::Gut)
            .arguments(&["res://test".to_owned()])
            .expect("an atomic selection");
        assert_eq!(arguments[0], "-s");
        assert_eq!(arguments[1], "res://addons/gut/gut_cmdln.gd");
        assert!(arguments.contains(&"-gexit".to_owned()));
        assert!(
            arguments
                .iter()
                .all(|argument| !argument.contains(';') && !argument.contains("&&"))
        );

        let gdunit_arguments = adapter_for(FrameworkKind::GdUnit4)
            .arguments(&["res://tests/a".to_owned(), "res://tests/b".to_owned()])
            .expect("gdUnit4 receives each selection as one argv element");
        assert_eq!(
            gdunit_arguments,
            vec![
                "-s",
                "res://addons/gdUnit4/bin/GdUnitCmdTool.gd",
                "-a",
                "res://tests/a",
                "-a",
                "res://tests/b",
            ]
        );
        assert!(
            adapter_for(FrameworkKind::GdUnit4)
                .selection_passing()
                .separators()
                .is_empty(),
            "repeated -a values are not split inside an argv element"
        );
    }

    #[test]
    fn every_adapter_refuses_its_declared_selection_separators_before_argv() {
        let control = vec!["res://tests/unit".to_owned()];

        for adapter in adapters() {
            adapter
                .arguments(&control)
                .unwrap_or_else(|error| panic!("{} refused the control: {error}", adapter.kind()));

            for separator in adapter.selection_passing().separators() {
                let escaped = format!("res://tests{separator}../esc");
                let error = match adapter.arguments(std::slice::from_ref(&escaped)) {
                    Ok(arguments) => panic!(
                        "{} let {escaped:?} reach argv as {arguments:?} even though {separator:?} splits it",
                        adapter.kind()
                    ),
                    Err(error) => error,
                };
                assert!(
                    error.to_string().contains(&escaped),
                    "{} returned an unrelated separator error: {error}",
                    adapter.kind()
                );
            }
        }
    }
}
