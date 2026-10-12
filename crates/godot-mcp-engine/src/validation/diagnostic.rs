//! Normalizing the engine's own diagnostic output.
//!
//! Godot writes errors to **stderr** in one exact two-line shape, produced by
//! `Logger::log_error` in `core/io/logger.cpp`:
//!
//! ```text
//! SCRIPT ERROR: Parse Error: Expected statement, found "Identifier" instead.
//!    at: GDScript::reload (res://scripts/broken.gd:7)
//! ```
//!
//! The first line is `<TYPE>: <details>`; the second is three spaces, `at: `, the
//! reporting function, then ` (<file>:<line>)`. The type is one of the engine's
//! own categories (`ERROR`, `WARNING`, `SCRIPT ERROR`, `SHADER ERROR`,
//! `USER ERROR`, `USER WARNING`, `USER SCRIPT ERROR`, `USER SHADER ERROR`).
//!
//! Two normalization rules carry the honesty of this module:
//!
//! 1. **A location is only a project location when it names a project resource.**
//!    The same `at:` line carries an engine C++ source position for an internal
//!    error (`core/io/resource_loader.cpp:283`). Reporting that as "the file you
//!    asked about, line 283" would fabricate a location in the user's project, so
//!    an engine-internal position is kept verbatim in
//!    [`Diagnostic::engine_location`] and [`Diagnostic::location`] stays `None`.
//! 2. **An unrecognised line is never silently dropped and never bucketed as
//!    "other".** A diagnostic whose type is known but whose subject is not gets
//!    [`DiagnosticCategory::EngineReported`], which says exactly that: the engine
//!    reported it and this layer did not classify it further.

use std::fmt;

use serde::{Deserialize, Serialize};

/// How serious the engine said a diagnostic is.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    /// The engine reported an error.
    #[default]
    Error,
    /// The engine reported a warning.
    Warning,
}

impl DiagnosticSeverity {
    /// Stable wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

impl fmt::Display for DiagnosticSeverity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What kind of problem a diagnostic describes. Each variant names a specific
/// defect; `EngineReported` is not a catch-all bucket but the explicit statement
/// "the engine reported this and this layer did not classify it further".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCategory {
    /// A GDScript parse or compile error.
    ScriptParse,
    /// A script could not be loaded at all.
    ScriptLoad,
    /// A shader failed to compile.
    ShaderCompile,
    /// A resource failed to import.
    ResourceImport,
    /// A resource failed to load.
    ResourceLoad,
    /// The engine reported a problem this layer did not classify further.
    EngineReported,
}

impl DiagnosticCategory {
    /// Every category. There are six.
    pub const ALL: [Self; 6] = [
        Self::ScriptParse,
        Self::ScriptLoad,
        Self::ShaderCompile,
        Self::ResourceImport,
        Self::ResourceLoad,
        Self::EngineReported,
    ];

    /// Stable wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ScriptParse => "script_parse",
            Self::ScriptLoad => "script_load",
            Self::ShaderCompile => "shader_compile",
            Self::ResourceImport => "resource_import",
            Self::ResourceLoad => "resource_load",
            Self::EngineReported => "engine_reported",
        }
    }
}

impl fmt::Display for DiagnosticCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A location inside the user's project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceLocation {
    /// The `res://` path, exactly as the engine spelled it.
    pub file: String,
    /// The 1-based line, when the engine reported one.
    pub line: Option<u32>,
}

/// One normalized engine diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// How serious it is.
    pub severity: DiagnosticSeverity,
    /// What kind of defect it describes.
    pub category: DiagnosticCategory,
    /// The engine's message, with its type prefix removed.
    pub message: String,
    /// Where in the project, when the engine named a project resource.
    pub location: Option<SourceLocation>,
    /// The engine's own `<file>:<line>` position when it is *not* a project
    /// resource (an engine C++ source position). Kept verbatim rather than
    /// promoted to a project location.
    pub engine_location: Option<String>,
    /// The reporting function from the `at:` line, when present.
    pub function: Option<String>,
    /// The exact first line the engine emitted, so a report can quote it.
    pub raw: String,
}

/// The engine error-type prefixes, exactly as `Logger::log_error` spells them.
/// Ordered longest-first so `USER SCRIPT ERROR` is matched before `USER`-less
/// `SCRIPT ERROR` and before `ERROR`.
const TYPE_PREFIXES: [(&str, DiagnosticSeverity, bool); 8] = [
    ("USER SCRIPT ERROR", DiagnosticSeverity::Error, true),
    ("USER SHADER ERROR", DiagnosticSeverity::Error, false),
    ("USER WARNING", DiagnosticSeverity::Warning, false),
    ("USER ERROR", DiagnosticSeverity::Error, false),
    ("SCRIPT ERROR", DiagnosticSeverity::Error, true),
    ("SHADER ERROR", DiagnosticSeverity::Error, false),
    ("WARNING", DiagnosticSeverity::Warning, false),
    ("ERROR", DiagnosticSeverity::Error, false),
];

/// `text` without the terminal colour sequences (`ESC [ … m`) some engine
/// builds write even when no terminal reads them — Godot 4.4's macOS logger
/// colours every error line it prints.
fn without_terminal_colours(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('\u{1b}') {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut plain = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' && characters.peek() == Some(&'[') {
            characters.next();
            // Parameters and intermediates, then the one final byte.
            for next in characters.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
            continue;
        }
        plain.push(character);
    }
    std::borrow::Cow::Owned(plain)
}

/// Parse every diagnostic out of one captured engine stream.
///
/// Lines that are not part of a diagnostic (the version banner, import progress,
/// framework output) are ignored rather than guessed at.
#[must_use]
pub fn parse_engine_diagnostics(text: &str) -> Vec<Diagnostic> {
    let text = without_terminal_colours(text);
    let lines: Vec<&str> = text.lines().collect();
    let mut diagnostics = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let Some((severity, script_type, message)) = split_type_prefix(lines[index]) else {
            index += 1;
            continue;
        };
        let raw = lines[index].trim_end().to_owned();
        // The `at:` continuation is always the immediately following line.
        let trailer = lines.get(index + 1).and_then(|line| parse_at_line(line));
        if trailer.is_some() {
            index += 1;
        }
        let (function, position) = trailer.map_or((None, None), |trailer| {
            (Some(trailer.function), Some(trailer.position))
        });
        let (mut location, engine_location) = split_position(position.as_deref());
        // When the `at:` line carries an engine C++ position, the subject resource
        // is instead named in the message itself ("Failed to import resource:
        // res://art/sprite.png"). Adopting it gives a file with `line: None` —
        // never a fabricated line number from the engine's own source position.
        if location.is_none()
            && let Some(subject) = subject_resource(&message)
        {
            location = Some(SourceLocation {
                file: subject,
                line: None,
            });
        }
        diagnostics.push(Diagnostic {
            severity,
            category: categorize(script_type, &message),
            message,
            location,
            engine_location,
            function,
            raw,
        });
        index += 1;
    }
    diagnostics
}

fn split_type_prefix(line: &str) -> Option<(DiagnosticSeverity, bool, String)> {
    let trimmed = line.trim_end();
    for (prefix, severity, script_type) in TYPE_PREFIXES {
        if let Some(rest) = trimmed.strip_prefix(prefix)
            && let Some(details) = rest.strip_prefix(": ")
        {
            return Some((severity, script_type, details.trim().to_owned()));
        }
    }
    None
}

struct AtLine {
    function: String,
    position: String,
}

/// Parse the engine's `   at: <function> (<file>:<line>)` continuation.
fn parse_at_line(line: &str) -> Option<AtLine> {
    let rest = line.trim_start().strip_prefix("at: ")?;
    let open = rest.rfind(" (")?;
    let close = rest.strip_suffix(')')?;
    if close.len() <= open + 2 {
        return None;
    }
    Some(AtLine {
        function: rest[..open].trim().to_owned(),
        position: close[open + 2..].to_owned(),
    })
}

/// The `res://` (or `user://`) resource a message names as its subject.
///
/// Only the *last* whitespace-delimited token is considered, so a message that
/// merely mentions a path in passing does not become a location.
fn subject_resource(message: &str) -> Option<String> {
    let candidate = message.split_whitespace().next_back()?;
    let trimmed = candidate.trim_end_matches(['.', ',', ':', ';', ')', '\'', '"']);
    (trimmed.starts_with("res://") || trimmed.starts_with("user://")).then(|| trimmed.to_owned())
}

/// Decide whether an `at:` position is a project location or an engine-internal
/// one. Only a `res://` (or `user://`) resource is a project location; anything
/// else — notably an engine C++ source path — is preserved verbatim instead.
fn split_position(position: Option<&str>) -> (Option<SourceLocation>, Option<String>) {
    let Some(position) = position else {
        return (None, None);
    };
    let (file, line) = match position.rsplit_once(':') {
        Some((file, number)) => match number.parse::<u32>() {
            Ok(parsed) => (file, Some(parsed)),
            Err(_) => (position, None),
        },
        None => (position, None),
    };
    if file.starts_with("res://") || file.starts_with("user://") {
        (
            Some(SourceLocation {
                file: file.to_owned(),
                line,
            }),
            None,
        )
    } else {
        (None, Some(position.to_owned()))
    }
}

fn categorize(script_type: bool, message: &str) -> DiagnosticCategory {
    if script_type {
        // GDScript spells these two exactly, in `GDScript::reload`.
        if message.starts_with("Parse Error:") || message.starts_with("Compile Error:") {
            return DiagnosticCategory::ScriptParse;
        }
        return DiagnosticCategory::EngineReported;
    }
    if message.starts_with("Can't load script:") {
        return DiagnosticCategory::ScriptLoad;
    }
    if message.starts_with("Failed to import resource:") || message.starts_with("Error importing:")
    {
        return DiagnosticCategory::ResourceImport;
    }
    if message.starts_with("Failed to load resource:")
        || message.starts_with("No loader found for resource:")
    {
        return DiagnosticCategory::ResourceLoad;
    }
    DiagnosticCategory::EngineReported
}

/// The subset of `diagnostics` that describes a project-located error.
#[must_use]
pub fn located_errors(diagnostics: &[Diagnostic]) -> Vec<&Diagnostic> {
    diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.severity == DiagnosticSeverity::Error && diagnostic.location.is_some()
        })
        .collect()
}

/// Whether any diagnostic is an error.
#[must_use]
pub fn has_error(diagnostics: &[Diagnostic]) -> bool {
    diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gdscript_parse_error_normalizes_to_a_project_file_and_line() {
        let text = concat!(
            "SCRIPT ERROR: Parse Error: Expected statement, found \"Identifier\" instead.\n",
            "   at: GDScript::reload (res://scripts/broken.gd:7)\n"
        );
        let diagnostics = parse_engine_diagnostics(text);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].severity, DiagnosticSeverity::Error);
        assert_eq!(diagnostics[0].category, DiagnosticCategory::ScriptParse);
        assert_eq!(
            diagnostics[0].location,
            Some(SourceLocation {
                file: "res://scripts/broken.gd".to_owned(),
                line: Some(7),
            })
        );
        assert_eq!(diagnostics[0].function.as_deref(), Some("GDScript::reload"));
        assert!(diagnostics[0].engine_location.is_none());
    }

    /// Godot 4.4's macOS terminal logger colours every line it writes, whether
    /// or not anything reads it on a terminal (`macos_terminal_logger.mm`): the
    /// same diagnostics must come out of the coloured text as out of the plain.
    #[test]
    fn a_coloured_macos_diagnostic_reads_as_the_plain_one() {
        let coloured = concat!(
            "\u{1b}[1;35mSCRIPT ERROR:\u{1b}[0;95m Parse Error: Expected statement, found ",
            "\"Identifier\" instead.\n",
            "\u{1b}[0;90m          at: GDScript::reload (res://scripts/broken.gd:7)\u{1b}[0m\n",
            "\u{1b}[1;31mERROR:\u{1b}[0;91m Failed loading scene: res://missing.tscn.\n",
            "\u{1b}[0;90m     at: start (main/main.cpp:4325)\u{1b}[0m\n"
        );
        let plain = concat!(
            "SCRIPT ERROR: Parse Error: Expected statement, found \"Identifier\" instead.\n",
            "          at: GDScript::reload (res://scripts/broken.gd:7)\n",
            "ERROR: Failed loading scene: res://missing.tscn.\n",
            "     at: start (main/main.cpp:4325)\n"
        );
        let from_coloured = parse_engine_diagnostics(coloured);
        assert_eq!(from_coloured.len(), 2, "{from_coloured:?}");
        let from_plain = parse_engine_diagnostics(plain);
        for (coloured, plain) in from_coloured.iter().zip(&from_plain) {
            assert_eq!(coloured.category, plain.category);
            assert_eq!(coloured.severity, plain.severity);
            assert_eq!(coloured.message, plain.message);
            assert_eq!(coloured.location, plain.location);
            assert_eq!(coloured.engine_location, plain.engine_location);
            assert_eq!(coloured.function, plain.function);
            assert!(!coloured.raw.contains('\u{1b}'), "{:?}", coloured.raw);
        }
    }

    #[test]
    fn an_engine_internal_position_never_becomes_a_project_location() {
        let text = concat!(
            "ERROR: Condition \"!f\" is true.\n",
            "   at: _load (core/io/resource_loader.cpp:283)\n"
        );
        let diagnostics = parse_engine_diagnostics(text);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].location.is_none());
        assert_eq!(
            diagnostics[0].engine_location.as_deref(),
            Some("core/io/resource_loader.cpp:283")
        );
        assert_eq!(diagnostics[0].category, DiagnosticCategory::EngineReported);
    }

    #[test]
    fn an_import_failure_names_the_resource_from_its_message_without_a_line() {
        let text = concat!(
            "ERROR: Failed to import resource: res://art/sprite.png\n",
            "   at: _reimport_file (editor/editor_file_system.cpp:2371)\n"
        );
        let diagnostics = parse_engine_diagnostics(text);
        assert_eq!(diagnostics[0].category, DiagnosticCategory::ResourceImport);
        assert_eq!(
            diagnostics[0].location,
            Some(SourceLocation {
                file: "res://art/sprite.png".to_owned(),
                line: None,
            }),
            "the subject resource comes from the message; the engine's own source \
             position is never borrowed as its line"
        );
        assert_eq!(
            diagnostics[0].engine_location.as_deref(),
            Some("editor/editor_file_system.cpp:2371")
        );
    }

    #[test]
    fn a_message_without_a_trailing_resource_gains_no_location() {
        let text = concat!(
            "ERROR: res://art/sprite.png could not be read for some other reason\n",
            "   at: _load (core/io/resource_loader.cpp:283)\n"
        );
        let diagnostics = parse_engine_diagnostics(text);
        assert!(
            diagnostics[0].location.is_none(),
            "a path mentioned mid-sentence is not a subject: {:?}",
            diagnostics[0].location
        );
    }

    #[test]
    fn a_missing_script_is_a_load_failure_not_a_parse_failure() {
        let text = concat!(
            "ERROR: Can't load script: res://scripts/absent.gd\n",
            "   at: main (main/main.cpp:3927)\n"
        );
        let diagnostics = parse_engine_diagnostics(text);
        assert_eq!(diagnostics[0].category, DiagnosticCategory::ScriptLoad);
    }

    #[test]
    fn non_diagnostic_output_produces_no_diagnostics() {
        let text = concat!(
            "Godot Engine v4.7.1.stable.official - https://godotengine.org\n",
            "godot-mcp-fake: started\n",
            "Import: begin\n"
        );
        assert!(parse_engine_diagnostics(text).is_empty());
        assert!(!has_error(&[]));
    }

    #[test]
    fn a_warning_is_not_an_error() {
        let text = "WARNING: The parameter \"delta\" is never used.\n";
        let diagnostics = parse_engine_diagnostics(text);
        assert_eq!(diagnostics[0].severity, DiagnosticSeverity::Warning);
        assert!(!has_error(&diagnostics));
        assert!(located_errors(&diagnostics).is_empty());
    }

    #[test]
    fn every_category_is_distinctly_named() {
        let mut names: Vec<&str> = DiagnosticCategory::ALL
            .iter()
            .map(|category| category.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), DiagnosticCategory::ALL.len());
    }
}
