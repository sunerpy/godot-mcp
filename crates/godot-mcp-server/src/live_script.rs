//! `editor.script.run` and `runtime.script.run` (ADR 0012): what the editor or
//! a game's probe is sent, and the answer made of what it reports and of the
//! project's files after the run.
//!
//! The add-on and the probe share one runner (`script_runner.gd`), which
//! compiles the source and calls its `run()` on the process's main thread.
//! Godot cannot stop a script part way through, so the deadline ends the
//! server's wait, not the code: a run not answered by then is `uncertain`, and
//! the editor or game may still be running it.

use std::time::Duration;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;
use serde_json::{Map, Value, json};

use crate::scripts::{Contents, MAX_SOURCE_CHARS, project_changes, returned_value, with_changes};

/// The deadline a call may set, in milliseconds.
const MIN_DEADLINE_MS: u64 = 1_000;
const DEFAULT_DEADLINE_MS: u64 = 60_000;
const MAX_DEADLINE_MS: u64 = 600_000;

/// Where a live run's code runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    /// The connected editor, through its add-on.
    Editor,
    /// A managed game session, through its runtime probe.
    Game,
}

impl Place {
    const fn source(self) -> &'static str {
        match self {
            Self::Editor => {
                "GDScript that defines `func run()`, called on the editor's main thread and \
                 awaited when it is a coroutine; what it returns is the answer's `result`. It \
                 runs as a tool script: `@tool` is written in front of its first line when it \
                 does not carry it, so its line numbers stay the same. A script extending Node \
                 is added under the plugin first."
            }
            Self::Game => {
                "GDScript that defines `func run()`, called on the game's main thread and \
                 awaited when it is a coroutine; what it returns is the answer's `result`. A \
                 script extending Node is added under the runtime probe first, in the running \
                 scene tree."
            }
        }
    }

    const fn deadline(self) -> &'static str {
        match self {
            Self::Editor => {
                "How long the server waits for the editor's answer, in milliseconds, with a \
                 further two seconds' grace. It ends the wait, not the code: a run not answered \
                 by then is `uncertain`, the editor may still be running it, and project writes \
                 stay blocked until the transaction is resolved."
            }
            Self::Game => {
                "How long the server waits for the game's answer, in milliseconds, with a \
                 further two seconds' grace. It ends the wait, not the code: a run not answered \
                 by then is `uncertain`, and the game may still be running it until its session \
                 stops."
            }
        }
    }

    const fn still_running(self) -> &'static str {
        match self {
            Self::Editor => {
                "The server stopped waiting before the editor answered, and the editor may still \
                 be running the code"
            }
            Self::Game => {
                "The server stopped waiting before the game answered, and the game may still be \
                 running the code until its session stops"
            }
        }
    }
}

/// The log kinds that make a run `failed`.
pub(crate) const ERROR_KINDS: [&str; 3] = ["error", "script", "shader"];

fn outcome_error(kind: ErrorKind, message: impl Into<String>) -> OutcomeError {
    OutcomeError::new(
        kind,
        message.into(),
        false,
        std::collections::BTreeMap::new(),
    )
}

/// The tool's own input properties, for code run in `place`.
pub(crate) fn properties(place: Place) -> Map<String, Value> {
    let properties = json!({
        "source": {
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_SOURCE_CHARS,
            "description": place.source()
        },
        "deadline_ms": {
            "type": "integer",
            "minimum": MIN_DEADLINE_MS,
            "maximum": MAX_DEADLINE_MS,
            "default": DEFAULT_DEADLINE_MS,
            "description": place.deadline()
        }
    });
    match properties {
        Value::Object(properties) => properties,
        _ => unreachable!("the properties are an object"),
    }
}

/// What one call asks the editor to run, and how long the server waits.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) payload: Value,
    pub(crate) deadline: Duration,
}

/// The request one call makes, checked as the schema states.
pub(crate) fn request(arguments: &Map<String, Value>) -> Result<Request, OutcomeError> {
    let source = arguments
        .get("source")
        .and_then(Value::as_str)
        .ok_or_else(|| outcome_error(ErrorKind::InvalidInput, "`source` is a string"))?;
    let characters = source.chars().count();
    if characters == 0 || characters > MAX_SOURCE_CHARS {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("`source` must be 1 to {MAX_SOURCE_CHARS} characters; it is {characters}"),
        ));
    }
    let deadline_ms = match arguments.get("deadline_ms") {
        None => DEFAULT_DEADLINE_MS,
        Some(value) => value
            .as_u64()
            .filter(|ms| (MIN_DEADLINE_MS..=MAX_DEADLINE_MS).contains(ms))
            .ok_or_else(|| {
                outcome_error(
                    ErrorKind::InvalidInput,
                    format!("`deadline_ms` is from {MIN_DEADLINE_MS} to {MAX_DEADLINE_MS}"),
                )
            })?,
    };
    Ok(Request {
        payload: json!({ "source": source }),
        deadline: Duration::from_millis(deadline_ms),
    })
}

/// The call's result from the routed command's result and the project's
/// contents before the run and after it.
pub(crate) fn finish(
    place: Place,
    result: Result<Value, OutcomeError>,
    before: Option<&Contents>,
    after: Result<Option<Contents>, OutcomeError>,
) -> Result<Value, OutcomeError> {
    match result {
        Ok(answer) => Ok(answer_of(answer, project_changes(before, after))),
        Err(error) if unresolved(&error) => {
            Err(with_changes(error, before, after, place.still_running()))
        }
        Err(error) => Err(error),
    }
}

/// Whether the editor may have started the run that `error` reports.
fn unresolved(error: &OutcomeError) -> bool {
    use godot_mcp_domain::contracts::GodotValue;

    error.kind() == ErrorKind::Uncertain
        || matches!(
            error.details().get("mutation_status"),
            Some(GodotValue::String(status)) if status == "unresolved"
        )
}

/// The answer of a run the editor or probe reported, from its report and the
/// project's changes.
pub(crate) fn answer_of(answer: Value, changes: Result<Value, String>) -> Value {
    let Value::Object(mut report) = answer else {
        return answer;
    };
    if report.get("replayed") == Some(&Value::Bool(true)) {
        report.insert(
            "result_unavailable".to_owned(),
            Value::String(
                "this request repeats one the journal already holds; the source was not run \
                 again, and the journal keeps the run's outcome, not its value or the files it \
                 changed"
                    .to_owned(),
            ),
        );
        return Value::Object(report);
    }
    let source_file = report
        .remove("source_file")
        .and_then(|file| file.as_str().map(str::to_owned));
    let captured = report.remove("log_capture") == Some(Value::Bool(true));
    let entries = match report.remove("log") {
        Some(Value::Array(entries)) => entries,
        _ => Vec::new(),
    };
    let (diagnostics, output) = sort_log(&entries, source_file.as_deref());
    let errors = entries
        .iter()
        .filter(|entry| ERROR_KINDS.contains(&entry["kind"].as_str().unwrap_or("")))
        .count();
    let stage = report.remove("stage");
    let given_reason = report
        .remove("reason")
        .and_then(|reason| reason.as_str().map(str::to_owned));
    let literal = report
        .remove("literal")
        .and_then(|literal| literal.as_str().map(str::to_owned));
    let length = report
        .remove("literal_length")
        .and_then(|length| length.as_u64());
    let (classification, reason, result, result_literal, unavailable) =
        match stage.as_ref().and_then(Value::as_str) {
            Some("compile") => (
                "compile_error",
                format!(
                    "{}; run() was not called",
                    given_reason
                        .as_deref()
                        .unwrap_or("the source did not compile")
                ),
                Value::Null,
                Value::Null,
                Some("run() was not called".to_owned()),
            ),
            Some("done") => {
                let (result, unavailable) = returned_value(literal.as_deref(), length);
                let literal = literal.map_or(Value::Null, Value::String);
                if errors > 0 {
                    let reason = format!(
                        "run() returned, and {errors} errors were logged while the source \
                         compiled and ran"
                    );
                    ("failed", reason, result, literal, unavailable)
                } else if captured {
                    (
                        "passed",
                        "run() returned".to_owned(),
                        result,
                        literal,
                        unavailable,
                    )
                } else {
                    let reason = "run() returned, but this Godot build gives no log to read \
                                  (log capture needs Godot 4.5 or later), so an error inside \
                                  run() cannot be seen"
                        .to_owned();
                    ("inconclusive", reason, result, literal, unavailable)
                }
            }
            _ => (
                "inconclusive",
                "the report names no stage the server knows".to_owned(),
                Value::Null,
                Value::Null,
                Some("the report is unreadable".to_owned()),
            ),
        };
    let (changes, changes_unavailable) = match changes {
        Ok(changes) => (changes, Value::Null),
        Err(reason) => (Value::Null, Value::String(reason)),
    };
    report.insert("classification".to_owned(), json!(classification));
    report.insert("reason".to_owned(), Value::String(reason));
    report.insert("result".to_owned(), result);
    report.insert("result_literal".to_owned(), result_literal);
    report.insert(
        "result_unavailable".to_owned(),
        unavailable.map_or(Value::Null, Value::String),
    );
    report.insert("diagnostics".to_owned(), Value::Array(diagnostics));
    report.insert("output".to_owned(), Value::Array(output));
    report.insert("log_capture".to_owned(), Value::Bool(captured));
    report.insert("changes".to_owned(), changes);
    report.insert("changes_unavailable".to_owned(), changes_unavailable);
    report.insert("applied".to_owned(), Value::Bool(true));
    report.insert("rolled_back".to_owned(), Value::Bool(false));
    Value::Object(report)
}

/// The run's log entries as diagnostics (errors and warnings) and output (what
/// it printed), each diagnostic with the line of the caller's source it names
/// when the entry or its backtrace is inside the compiled source.
pub(crate) fn sort_log(entries: &[Value], source_file: Option<&str>) -> (Vec<Value>, Vec<Value>) {
    let ours = |file: &Value| source_file.is_some() && file.as_str() == source_file;
    let mut diagnostics = Vec::new();
    let mut output = Vec::new();
    for entry in entries {
        let kind = entry["kind"].as_str().unwrap_or("error");
        if kind == "message" {
            output.push(json!({
                "text": entry["text"],
                "stderr": entry["stderr"].as_bool().unwrap_or(false),
            }));
            continue;
        }
        let source_line = if ours(&entry["file"]) {
            entry["line"].as_u64()
        } else {
            entry["backtrace"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|frame| ours(&frame["file"]))
                .and_then(|frame| frame["line"].as_u64())
        };
        let engine_location = match (entry["file"].as_str(), entry["line"].as_u64()) {
            (Some(file), Some(line)) if !ours(&entry["file"]) && !file.is_empty() => {
                Value::String(format!("{file}:{line}"))
            }
            _ => Value::Null,
        };
        diagnostics.push(json!({
            "severity": if kind == "warning" { "warning" } else { "error" },
            "kind": kind,
            "message": entry["text"],
            "function": entry["function"],
            "source_line": source_line,
            "engine_location": engine_location,
        }));
    }
    (diagnostics, output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().expect("object").clone()
    }

    #[test]
    fn a_request_carries_the_source_and_waits_as_long_as_asked() {
        let asked =
            request(&arguments(json!({ "source": "func run():\n\treturn 1\n" }))).expect("request");
        assert_eq!(
            asked.payload,
            json!({ "source": "func run():\n\treturn 1\n" })
        );
        assert_eq!(asked.deadline, Duration::from_millis(DEFAULT_DEADLINE_MS));
        let longer =
            request(&arguments(json!({ "source": "x", "deadline_ms": 120_000 }))).expect("request");
        assert_eq!(longer.deadline, Duration::from_secs(120));
        for refused in [
            json!({}),
            json!({ "source": "" }),
            json!({ "source": "x".repeat(MAX_SOURCE_CHARS + 1) }),
            json!({ "source": "x", "deadline_ms": 999 }),
            json!({ "source": "x", "deadline_ms": 600_001 }),
            json!({ "source": "x", "deadline_ms": "60000" }),
        ] {
            let error = request(&arguments(refused.clone())).expect_err("refused");
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "{refused}");
        }
    }

    fn reported(report: Value) -> Value {
        let mut answer = report;
        answer["transaction_id"] = json!("tx-1");
        answer["mutation_status"] = json!("committed");
        answer["replayed"] = json!(false);
        answer_of(answer, Ok(json!({ "total": 0 })))
    }

    #[test]
    fn a_returned_run_passes_only_when_the_editor_logged_no_error() {
        let passed = reported(json!({
            "stage": "done", "literal": "3", "source_file": "gdscript://7.gd",
            "log_capture": true,
            "log": [{ "kind": "message", "text": "hello", "stderr": false }],
        }));
        assert_eq!(passed["classification"], "passed", "{passed}");
        assert_eq!(passed["result"], json!({ "type": "integer", "value": 3 }));
        assert_eq!(
            passed["output"],
            json!([{ "text": "hello", "stderr": false }])
        );
        assert_eq!(passed["diagnostics"], json!([]));
        assert_eq!(passed["applied"], true);
        assert_eq!(passed["rolled_back"], false);
        for absent in ["stage", "literal", "log", "source_file"] {
            assert!(passed.get(absent).is_none(), "{absent}: {passed}");
        }

        let failed = reported(json!({
            "stage": "done", "literal": "null", "source_file": "gdscript://7.gd",
            "log_capture": true,
            "log": [{
                "kind": "script", "text": "Out of bounds get index '3'",
                "file": "gdscript://7.gd", "line": 5, "function": "run", "backtrace": [],
            }, {
                "kind": "error", "text": "from run", "function": "push_error",
                "file": "core/variant/variant_utility.cpp", "line": 1023,
                "backtrace": [{ "function": "run", "file": "gdscript://7.gd", "line": 4 }],
            }],
        }));
        assert_eq!(failed["classification"], "failed", "{failed}");
        let lines: Vec<_> = failed["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .map(|diagnostic| diagnostic["source_line"].clone())
            .collect();
        assert_eq!(lines, [json!(5), json!(4)]);
        assert_eq!(
            failed["diagnostics"][1]["engine_location"],
            "core/variant/variant_utility.cpp:1023"
        );
        assert_eq!(failed["diagnostics"][0]["engine_location"], Value::Null);

        let unseen = reported(json!({
            "stage": "done", "literal": "1", "log_capture": false, "log": [],
        }));
        assert_eq!(unseen["classification"], "inconclusive", "{unseen}");
        assert_eq!(unseen["result"], json!({ "type": "integer", "value": 1 }));
    }

    #[test]
    fn a_source_that_did_not_compile_is_a_compile_error_with_its_line() {
        let answer = reported(json!({
            "stage": "compile", "reason": "the source does not compile (Parse error)",
            "source_file": "gdscript://9.gd", "log_capture": true,
            "log": [{
                "kind": "script", "text": "Parse Error: Identifier not declared",
                "file": "gdscript://9.gd", "line": 4, "function": "GDScript::reload",
                "backtrace": [],
            }],
        }));
        assert_eq!(answer["classification"], "compile_error", "{answer}");
        assert_eq!(answer["result"], Value::Null);
        assert_eq!(answer["result_unavailable"], "run() was not called");
        assert_eq!(answer["diagnostics"][0]["source_line"], 4);
        assert!(
            answer["reason"]
                .as_str()
                .is_some_and(|reason| reason.ends_with("run() was not called")),
            "{answer}"
        );
    }

    #[test]
    fn a_replay_answers_from_the_journal_without_the_value() {
        let replay = answer_of(
            json!({ "replayed": true, "status": "committed", "transaction_id": "tx-1" }),
            Ok(json!({ "total": 3 })),
        );
        assert_eq!(replay["replayed"], true);
        assert!(replay.get("changes").is_none(), "{replay}");
        assert!(replay["result_unavailable"].is_string(), "{replay}");
    }

    #[test]
    fn an_unanswered_run_says_the_code_may_still_be_running() {
        let mut details = std::collections::BTreeMap::new();
        details.insert(
            "mutation_status".to_owned(),
            godot_mcp_domain::contracts::GodotValue::String("unresolved".to_owned()),
        );
        let error = OutcomeError::new(
            ErrorKind::Uncertain,
            "whether the editor made the change is not decidable".to_owned(),
            false,
            details,
        );
        let before = Contents::new();
        let mut after = Contents::new();
        after.insert(
            "note.txt".to_owned(),
            crate::scripts::Fingerprint {
                sha256: "a".repeat(64),
                bytes: 4,
            },
        );
        let reported = finish(Place::Editor, Err(error), Some(&before), Ok(Some(after)))
            .expect_err("uncertain");
        assert_eq!(reported.kind(), ErrorKind::Uncertain);
        assert!(
            reported.message().contains("may still be running"),
            "{reported:?}"
        );
        assert_eq!(
            reported.details().get("changed_paths"),
            Some(&godot_mcp_domain::contracts::GodotValue::Array(vec![
                godot_mcp_domain::contracts::GodotValue::String("note.txt".to_owned())
            ]))
        );

        let refused = outcome_error(ErrorKind::PermissionDenied, "not offered");
        let passed_on =
            finish(Place::Game, Err(refused), Some(&before), Ok(None)).expect_err("refused");
        assert_eq!(passed_on.kind(), ErrorKind::PermissionDenied);
        assert!(passed_on.details().get("changed_paths").is_none());
    }
}
