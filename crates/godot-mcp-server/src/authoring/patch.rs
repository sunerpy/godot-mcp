//! Structured text changes: whole replacement, bounded line edits, or a strict
//! unified diff.
//!
//! Every form is applied in memory to the exact bytes whose SHA-256 the caller
//! named, and refuses rather than guesses: line edits must not overlap, and a
//! unified diff must match its stated lines exactly, with no fuzz and no
//! search for a better offset.

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;
use serde_json::{Map, Value};

use super::outcome_error;

/// Most line edits one request may carry.
const MAX_EDITS: usize = 200;

/// One requested change to a text file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TextChange {
    /// Replace the whole file.
    Replace(String),
    /// Replace line ranges of the current file.
    Edits(Vec<LineEdit>),
    /// Apply a unified diff to the current file.
    UnifiedDiff(String),
}

/// Replace lines `start_line..=end_line` (1-based) with `text`. An
/// `end_line` of `start_line - 1` inserts before `start_line`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LineEdit {
    pub(super) start_line: usize,
    pub(super) end_line: usize,
    pub(super) text: String,
}

impl TextChange {
    /// Read the one change form of a tool's `change` argument.
    ///
    /// # Errors
    /// Returns [`ErrorKind::InvalidInput`] unless `change` holds exactly one
    /// of `content`, `edits` and `unifiedDiff`, well formed, and nothing else.
    pub(super) fn from_arguments(arguments: &Map<String, Value>) -> Result<Self, OutcomeError> {
        let change = arguments
            .get("change")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                invalid("`change` must be an object holding one of `content`, `edits` and `unifiedDiff`")
            })?;
        if let Some(unknown) = change
            .keys()
            .find(|key| !matches!(key.as_str(), "content" | "edits" | "unifiedDiff"))
        {
            return Err(invalid(&format!("a change does not accept `{unknown}`")));
        }
        let present: Vec<&str> = ["content", "edits", "unifiedDiff"]
            .into_iter()
            .filter(|key| change.contains_key(*key))
            .collect();
        if present.len() != 1 {
            return Err(invalid(
                "a change holds exactly one of `content`, `edits` and `unifiedDiff`",
            ));
        }
        let arguments = change;
        match present[0] {
            "content" => arguments
                .get("content")
                .and_then(Value::as_str)
                .map(|text| Self::Replace(text.to_owned()))
                .ok_or_else(|| invalid("`content` must be a string")),
            "unifiedDiff" => arguments
                .get("unifiedDiff")
                .and_then(Value::as_str)
                .map(|text| Self::UnifiedDiff(text.to_owned()))
                .ok_or_else(|| invalid("`unifiedDiff` must be a string")),
            _ => {
                let edits = arguments
                    .get("edits")
                    .and_then(Value::as_array)
                    .ok_or_else(|| invalid("`edits` must be an array"))?;
                if edits.is_empty() || edits.len() > MAX_EDITS {
                    return Err(invalid(&format!(
                        "`edits` must hold between 1 and {MAX_EDITS} edits"
                    )));
                }
                edits
                    .iter()
                    .map(LineEdit::from_value)
                    .collect::<Result<_, _>>()
                    .map(Self::Edits)
            }
        }
    }

    /// Apply the change to `original`.
    ///
    /// # Errors
    /// Returns [`ErrorKind::InvalidInput`] for an out-of-range or overlapping
    /// edit, and [`ErrorKind::Conflict`] for a diff whose lines do not match.
    pub(super) fn apply(&self, original: &str) -> Result<String, OutcomeError> {
        match self {
            Self::Replace(text) => Ok(text.clone()),
            Self::Edits(edits) => apply_edits(original, edits),
            Self::UnifiedDiff(diff) => apply_unified(original, diff),
        }
    }
}

impl LineEdit {
    fn from_value(value: &Value) -> Result<Self, OutcomeError> {
        let object = value
            .as_object()
            .ok_or_else(|| invalid("every edit must be an object"))?;
        if let Some(unknown) = object
            .keys()
            .find(|key| !matches!(key.as_str(), "startLine" | "endLine" | "text"))
        {
            return Err(invalid(&format!("an edit does not accept `{unknown}`")));
        }
        let line = |key: &str| {
            object
                .get(key)
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| invalid(&format!("every edit needs a whole-number `{key}`")))
        };
        let start_line = line("startLine")?;
        let end_line = line("endLine")?;
        let text = object
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("every edit needs a string `text`"))?
            .to_owned();
        if start_line == 0 || end_line + 1 < start_line {
            return Err(invalid(
                "an edit needs startLine >= 1 and endLine >= startLine - 1",
            ));
        }
        Ok(Self {
            start_line,
            end_line,
            text,
        })
    }
}

/// The file's line terminator: `\r\n` when every terminated line uses it.
fn terminator(text: &str) -> &'static str {
    let lines = text.matches('\n').count();
    if lines > 0 && text.matches("\r\n").count() == lines {
        "\r\n"
    } else {
        "\n"
    }
}

/// Split `text` into lines that keep their terminators.
fn lines(text: &str) -> Vec<String> {
    text.split_inclusive('\n').map(str::to_owned).collect()
}

/// `text` as whole lines in the file's terminator, the last one terminated
/// unless `terminate_last` is false.
fn replacement_lines(text: &str, eol: &str, terminate_last: bool) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let normalized = text.replace("\r\n", "\n");
    let mut parts: Vec<&str> = normalized.split('\n').collect();
    let ends_with_newline = normalized.ends_with('\n');
    if ends_with_newline {
        parts.pop();
    }
    let count = parts.len();
    parts
        .into_iter()
        .enumerate()
        .map(|(index, part)| {
            let last = index + 1 == count;
            if last && !ends_with_newline && !terminate_last {
                part.to_owned()
            } else {
                format!("{part}{eol}")
            }
        })
        .collect()
}

fn apply_edits(original: &str, edits: &[LineEdit]) -> Result<String, OutcomeError> {
    let eol = terminator(original);
    let current = lines(original);
    let total = current.len();
    let mut ordered: Vec<&LineEdit> = edits.iter().collect();
    ordered.sort_by_key(|edit| (edit.start_line, edit.end_line));
    for pair in ordered.windows(2) {
        if pair[1].start_line <= pair[0].end_line
            || (pair[1].start_line == pair[0].start_line && pair[0].end_line < pair[0].start_line)
        {
            return Err(invalid("line edits must not overlap"));
        }
    }
    let mut result: Vec<String> = Vec::with_capacity(total + edits.len());
    let mut next = 1_usize;
    let original_ends_with_newline = original.is_empty() || original.ends_with('\n');
    for edit in ordered {
        if edit.start_line > total + 1 || edit.end_line > total {
            return Err(invalid(&format!(
                "an edit at lines {}-{} is outside the file's {total} lines",
                edit.start_line, edit.end_line
            )));
        }
        result.extend(current[next - 1..edit.start_line - 1].iter().cloned());
        let reaches_end = edit.end_line == total;
        let mut replacement =
            replacement_lines(&edit.text, eol, !reaches_end || original_ends_with_newline);
        // An insertion right after an unterminated last line terminates it, so
        // the inserted text starts on a line of its own.
        if let Some(previous) = result.last_mut()
            && !previous.ends_with('\n')
            && !replacement.is_empty()
        {
            previous.push_str(eol);
        }
        result.append(&mut replacement);
        next = edit.end_line + 1;
    }
    if next <= total {
        if let Some(previous) = result.last_mut()
            && !previous.ends_with('\n')
        {
            previous.push_str(eol);
        }
        result.extend(current[next - 1..].iter().cloned());
    }
    Ok(result.concat())
}

/// One parsed hunk: its 1-based old start and its lines with their markers.
struct Hunk {
    old_start: usize,
    old_count: usize,
    new_count: usize,
    lines: Vec<(char, String, bool)>,
}

fn parse_unified(diff: &str) -> Result<Vec<Hunk>, OutcomeError> {
    let mut hunks: Vec<Hunk> = Vec::new();
    for raw in diff.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(header) = line.strip_prefix("@@ ") {
            let ranges = header
                .split(" @@")
                .next()
                .ok_or_else(|| invalid("malformed hunk header"))?;
            let mut parts = ranges.split(' ');
            let old = parts
                .next()
                .and_then(|part| part.strip_prefix('-'))
                .ok_or_else(|| invalid("a hunk header needs an old range"))?;
            let new = parts
                .next()
                .and_then(|part| part.strip_prefix('+'))
                .ok_or_else(|| invalid("a hunk header needs a new range"))?;
            let (old_start, old_count) = parse_range(old)?;
            let (_, new_count) = parse_range(new)?;
            hunks.push(Hunk {
                old_start,
                old_count,
                new_count,
                lines: Vec::new(),
            });
            continue;
        }
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            if hunks.last().is_some_and(|hunk| !hunk_complete(hunk)) {
                // Inside a hunk these are ordinary removed/added lines.
            } else {
                continue;
            }
        }
        let Some(hunk) = hunks.last_mut() else {
            if line.is_empty() || line.starts_with("diff ") || line.starts_with("index ") {
                continue;
            }
            return Err(invalid(
                "a unified diff must start its changes with an @@ header",
            ));
        };
        if line == "\\ No newline at end of file" {
            if let Some(last) = hunk.lines.last_mut() {
                last.2 = false;
            }
            continue;
        }
        if hunk_complete(hunk) {
            if line.is_empty() {
                continue;
            }
            return Err(invalid("a hunk holds more lines than its header declares"));
        }
        let mut characters = line.chars();
        match characters.next() {
            Some(marker @ (' ' | '-' | '+')) => {
                hunk.lines
                    .push((marker, characters.as_str().to_owned(), true));
            }
            // A context line whose leading space was stripped by a transport.
            None => hunk.lines.push((' ', String::new(), true)),
            Some(_) => return Err(invalid(&format!("unexpected diff line `{line}`"))),
        }
    }
    if hunks.is_empty() {
        return Err(invalid("a unified diff needs at least one @@ hunk"));
    }
    if let Some(hunk) = hunks.iter().find(|hunk| !hunk_complete(hunk)) {
        return Err(invalid(&format!(
            "the hunk at old line {} holds fewer lines than its header declares",
            hunk.old_start
        )));
    }
    Ok(hunks)
}

fn hunk_complete(hunk: &Hunk) -> bool {
    let old = hunk
        .lines
        .iter()
        .filter(|(marker, _, _)| *marker != '+')
        .count();
    let new = hunk
        .lines
        .iter()
        .filter(|(marker, _, _)| *marker != '-')
        .count();
    old >= hunk.old_count && new >= hunk.new_count
}

fn parse_range(range: &str) -> Result<(usize, usize), OutcomeError> {
    let (start, count) = range.split_once(',').unwrap_or((range, "1"));
    let start = start
        .parse()
        .map_err(|_| invalid("a hunk range start must be a number"))?;
    let count = count
        .parse()
        .map_err(|_| invalid("a hunk range count must be a number"))?;
    Ok((start, count))
}

fn strip_terminator(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

fn apply_unified(original: &str, diff: &str) -> Result<String, OutcomeError> {
    let eol = terminator(original);
    let current = lines(original);
    let mut result: Vec<String> = Vec::with_capacity(current.len());
    let mut next = 0_usize;
    for hunk in parse_unified(diff)? {
        // A zero-length old side is numbered by the line before it.
        let start = if hunk.old_count == 0 {
            hunk.old_start
        } else {
            hunk.old_start
                .checked_sub(1)
                .ok_or_else(|| invalid("a hunk with old lines must start at line 1 or later"))?
        };
        if start < next || start > current.len() {
            return Err(outcome_error(
                ErrorKind::Conflict,
                format!(
                    "the hunk at old line {} does not apply: hunks must be ordered and inside \
                     the file",
                    hunk.old_start
                ),
            ));
        }
        result.extend(current[next..start].iter().cloned());
        let mut cursor = start;
        for (marker, text, terminated) in &hunk.lines {
            match marker {
                ' ' | '-' => {
                    let matches = current
                        .get(cursor)
                        .is_some_and(|line| strip_terminator(line) == text);
                    if !matches {
                        return Err(outcome_error(
                            ErrorKind::Conflict,
                            format!(
                                "the diff does not match the file at line {}; it was made \
                                 against other content",
                                cursor + 1
                            ),
                        ));
                    }
                    if *marker == ' ' {
                        result.push(current[cursor].clone());
                    }
                    cursor += 1;
                }
                _ => result.push(if *terminated {
                    format!("{text}{eol}")
                } else {
                    text.clone()
                }),
            }
        }
        next = cursor;
    }
    result.extend(current[next..].iter().cloned());
    Ok(result.concat())
}

fn invalid(message: &str) -> OutcomeError {
    outcome_error(ErrorKind::InvalidInput, message.to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Both patch schemas and the change parser give the same verdict on every
    /// change, and it is the intended one: `change` holds exactly one of
    /// `content`, `edits` and `unifiedDiff`, each well formed.
    #[test]
    fn the_patch_schemas_and_the_change_parser_agree() {
        let edit = json!({ "startLine": 1, "endLine": 1, "text": "x\n" });
        let forms = [
            ("content", json!("text")),
            ("edits", json!([edit])),
            ("unifiedDiff", json!("@@ -1 +1 @@\n-a\n+b\n")),
        ];
        // (the change, or None for none at all; the verdict)
        let mut cases = Vec::new();
        for mask in 0..8_u8 {
            let mut change = Map::new();
            for (bit, (name, value)) in forms.iter().enumerate() {
                if mask & (1 << bit) != 0 {
                    change.insert((*name).to_owned(), value.clone());
                }
            }
            cases.push((Some(Value::Object(change)), mask.count_ones() == 1));
        }
        for change in [
            json!({ "content": 5 }),
            json!({ "edits": [] }),
            json!({ "edits": [{ "startLine": 1, "endLine": 1 }] }),
            json!({ "edits": [{ "startLine": 0, "endLine": 0, "text": "" }] }),
            json!({ "edits": [{ "startLine": 1, "endLine": 1, "text": "", "x": 1 }] }),
            json!({ "unifiedDiff": ["@@"] }),
            json!({ "content": "x", "colour": "red" }),
            json!("text"),
        ] {
            cases.push((Some(change), false));
        }
        cases.push((None, false));
        for tool in ["file.patch", "script.patch"] {
            let schema = Value::Object(crate::kernel::tool_schema(
                tool,
                crate::core::CoreLimits::default(),
            ));
            for (change, expected) in &cases {
                let mut arguments = json!({
                    "path": "res://a.gd",
                    "expectedSha256": "0".repeat(64),
                    "idempotencyId": "patch-1"
                });
                if let Some(change) = change {
                    arguments["change"] = change.clone();
                }
                assert_eq!(
                    crate::schema::conformance::conforms(&schema, &arguments),
                    *expected,
                    "the {tool} schema's verdict on {arguments}"
                );
                // The kernel takes the idempotency id out before a handler runs.
                let mut handed = arguments.as_object().cloned().expect("an object");
                handed.remove("idempotencyId");
                assert_eq!(
                    TextChange::from_arguments(&handed).is_ok(),
                    *expected,
                    "the parser's verdict on {arguments}"
                );
            }
        }
    }

    fn edit(start_line: usize, end_line: usize, text: &str) -> LineEdit {
        LineEdit {
            start_line,
            end_line,
            text: text.to_owned(),
        }
    }

    #[test]
    fn line_edits_replace_insert_and_delete() {
        let original = "one\ntwo\nthree\n";
        let changed = TextChange::Edits(vec![
            edit(1, 0, "zero"),
            edit(2, 2, "TWO\nTWO-AND-A-HALF"),
            edit(3, 3, ""),
        ])
        .apply(original)
        .expect("edits apply");
        assert_eq!(changed, "zero\none\nTWO\nTWO-AND-A-HALF\n");
    }

    #[test]
    fn line_edits_append_at_the_end_and_keep_crlf() {
        let original = "a\r\nb\r\n";
        let changed = TextChange::Edits(vec![edit(3, 2, "c")])
            .apply(original)
            .expect("append");
        assert_eq!(changed, "a\r\nb\r\nc\r\n");
    }

    #[test]
    fn overlapping_or_out_of_range_edits_are_refused() {
        let original = "a\nb\nc\n";
        for edits in [
            vec![edit(1, 2, "x"), edit(2, 3, "y")],
            vec![edit(5, 5, "x")],
            vec![edit(1, 4, "x")],
        ] {
            let error = TextChange::Edits(edits)
                .apply(original)
                .expect_err("refused");
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn a_unified_diff_applies_exactly_or_conflicts() {
        let original = "extends Node\n\nfunc a() -> void:\n\tpass\n";
        let diff = "--- a/x.gd\n+++ b/x.gd\n@@ -3,2 +3,3 @@\n func a() -> void:\n-\tpass\n+\tprint(\"a\")\n+\treturn\n";
        let changed = TextChange::UnifiedDiff(diff.to_owned())
            .apply(original)
            .expect("diff applies");
        assert_eq!(
            changed,
            "extends Node\n\nfunc a() -> void:\n\tprint(\"a\")\n\treturn\n"
        );

        let stale = TextChange::UnifiedDiff(diff.to_owned())
            .apply("extends Node\n\nfunc b() -> void:\n\tpass\n")
            .expect_err("a stale diff conflicts");
        assert_eq!(stale.kind(), ErrorKind::Conflict);
    }

    #[test]
    fn a_rendered_diff_round_trips_through_the_applier() {
        let before = "a\nb\nc\nd\ne\nf\ng\n";
        let after = "a\nB\nc\nd\ne\nf\ng\nh\n";
        let diff = super::super::diff::unified(Some("x"), Some("x"), before, after);
        assert!(!diff.truncated);
        let applied = TextChange::UnifiedDiff(diff.text)
            .apply(before)
            .expect("own diff applies");
        assert_eq!(applied, after);
    }

    #[test]
    fn exactly_one_change_form_is_required() {
        let with = |change: Value| {
            let mut arguments = Map::new();
            arguments.insert("change".to_owned(), change);
            arguments
        };
        assert!(
            TextChange::from_arguments(&with(json!({ "content": "x", "unifiedDiff": "y" })))
                .is_err()
        );
        assert!(TextChange::from_arguments(&with(json!({}))).is_err());
        assert!(TextChange::from_arguments(&with(json!({ "content": "x" }))).is_ok());
        assert!(TextChange::from_arguments(&Map::new()).is_err());
    }
}
