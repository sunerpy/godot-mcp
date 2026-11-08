//! Commands a project declares for MCP clients to call (ADR 0016).
//!
//! `editor.commands.list` reads `godot_mcp_commands.json` and the provider
//! scripts it names, and runs nothing. `editor.commands.call` checks a call
//! against the declaration on disk, sends the connected editor the provider's
//! script and the arguments as tagged values, and answers what the provider's
//! `mcp_call(command, args)` returned, what the editor logged meanwhile and every
//! project file that differs from before the call. Godot cannot stop a script
//! part way through, so the deadline ends the server's wait, not the code: a
//! call not answered by then is `uncertain`, and the editor may still be
//! running it.

use std::path::Path;
use std::time::Duration;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_domain::project_commands::{
    DECLARATION_FILE, Declaration, prepare_arguments, valid_name,
};
use godot_mcp_domain::safe_root::{SafeRoot, SafeRootError};
use godot_mcp_engine::supervision::journal::hex_sha256;
use serde_json::{Map, Value, json};

use crate::scripts::{Contents, project_changes, returned_value, with_changes};

/// The deadline a call may set, in milliseconds.
const MIN_DEADLINE_MS: u64 = 1_000;
const DEFAULT_DEADLINE_MS: u64 = 60_000;
const MAX_DEADLINE_MS: u64 = 600_000;
/// The longest error text a command's answer carries, in characters.
pub(crate) const MAX_ERROR_CHARS: usize = 4_096;

/// How an answer that ended without the editor's answer says the code may go on.
const STILL_RUNNING: &str = "The server stopped waiting before the editor answered, and the \
     editor may still be running the command";

/// Why a replayed answer carries no value.
const REPLAYED: &str = "this request repeats one the journal already holds; the command was not \
     called again, and the journal keeps the call's outcome, not its value or the files it \
     changed";

fn outcome_error(kind: ErrorKind, message: impl Into<String>) -> OutcomeError {
    OutcomeError::new(
        kind,
        message.into(),
        false,
        std::collections::BTreeMap::new(),
    )
}

/// Why no project command is called on this server.
const OFF_REASON: &str = "project commands are off on this server: the operator allows them \
     with `project_commands = true` in the --config file, or GODOT_MCP_PROJECT_COMMANDS=true. \
     They are on by default except for a proxied OIDC principal.";

/// The refusal of a call on a server whose operator does not allow project
/// commands.
pub(crate) fn commands_off() -> OutcomeError {
    outcome_error(
        ErrorKind::PermissionDenied,
        format!("{OFF_REASON} Nothing was called."),
    )
}

/// What one provider script holds, as the server reads it from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ScriptState {
    /// No file is at the path.
    Missing,
    /// The file exists but the held project root refused to read it.
    Unreadable(String),
    /// A GDScript without a `@tool` line, which the editor does not run.
    NotTool { sha256: String },
    /// A tool script the editor may load.
    Tool { sha256: String },
}

impl ScriptState {
    fn sha256(&self) -> Option<&str> {
        match self {
            Self::NotTool { sha256 } | Self::Tool { sha256 } => Some(sha256),
            Self::Missing | Self::Unreadable(_) => None,
        }
    }

    /// Why a command of this provider cannot be called, if it cannot.
    fn reason(&self, script: &str) -> Option<String> {
        match self {
            Self::Tool { .. } => None,
            Self::Missing => Some(format!("the provider's script {script} does not exist")),
            Self::Unreadable(reason) => Some(format!(
                "the provider's script {script} cannot be read: {reason}"
            )),
            Self::NotTool { .. } => Some(format!(
                "the provider's script {script} has no @tool line, so the editor cannot run it"
            )),
        }
    }
}

/// Read `script`, a `res://` path the declaration accepted, through the held
/// project root.
fn script_state(root: &SafeRoot, script: &str) -> ScriptState {
    let relative = script.trim_start_matches("res://");
    match root.read(Path::new(relative)) {
        Ok(bytes) => {
            let sha256 = hex_sha256(&bytes);
            if String::from_utf8_lossy(&bytes)
                .lines()
                .any(is_tool_annotation)
            {
                ScriptState::Tool { sha256 }
            } else {
                ScriptState::NotTool { sha256 }
            }
        }
        Err(SafeRootError::StalePreimage { .. }) => ScriptState::Missing,
        Err(error) => ScriptState::Unreadable(error.to_string()),
    }
}

/// Whether `line` is GDScript's `@tool` annotation, alone or in front of the
/// line's code, as the add-on's script runner recognizes it.
fn is_tool_annotation(line: &str) -> bool {
    line.trim_start_matches([' ', '\t'])
        .strip_prefix("@tool")
        .is_some_and(|rest| {
            !rest
                .chars()
                .next()
                .is_some_and(|next| next.is_ascii_alphanumeric() || next == '_')
        })
}

/// What the connected editor says about project commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EditorView {
    /// Whether an editor running the godot_mcp plugin is connected.
    pub(crate) connected: bool,
    /// Whether its session carries `editor.commands.call`: its plugin
    /// implements the method and this server offered it.
    pub(crate) serves_commands: bool,
}

/// The `editor.commands.list` answer for the declaration's bytes (`None`
/// when the file is absent).
pub(crate) fn list_value(
    bytes: Option<&[u8]>,
    root: &SafeRoot,
    enabled: bool,
    editor: EditorView,
    workspace: Value,
) -> Value {
    let declaration = bytes.map(Declaration::parse);
    let status = match &declaration {
        None => "absent",
        Some(declaration) if declaration.is_valid() => "valid",
        Some(_) => "invalid",
    };
    let valid = declaration.as_ref().is_some_and(Declaration::is_valid);
    let providers: Vec<Value> = declaration
        .iter()
        .flat_map(|declaration| &declaration.providers)
        .map(|provider| {
            let state = script_state(root, &provider.script);
            let commands: Vec<Value> = provider
                .commands
                .iter()
                .map(|command| {
                    let reason = if !enabled {
                        Some(OFF_REASON.to_owned())
                    } else if !valid {
                        Some(
                            "the declaration breaks a rule (see `problems`), so none of its \
                             commands can be called until it is fixed"
                                .to_owned(),
                        )
                    } else {
                        command
                            .unavailable
                            .clone()
                            .or_else(|| state.reason(&provider.script))
                    };
                    json!({
                        "name": command.name,
                        "title": command.title,
                        "description": command.description,
                        "input_schema": command.input_schema,
                        "mutates": command.mutates,
                        "available": reason.is_none(),
                        "unavailable_reason": reason,
                    })
                })
                .collect();
            json!({
                "id": provider.id,
                "script": provider.script,
                "script_sha256": state.sha256(),
                "commands": commands,
            })
        })
        .collect();
    let problems: Vec<Value> = declaration
        .iter()
        .flat_map(|declaration| &declaration.problems)
        .map(|problem| json!({ "pointer": problem.pointer, "message": problem.message }))
        .collect();
    json!({
        "file": DECLARATION_FILE,
        "status": status,
        "sha256": bytes.map(hex_sha256),
        "bytes": bytes.map(<[u8]>::len),
        "enabled": enabled,
        "disabled_reason": (!enabled).then_some(OFF_REASON),
        "editor": {
            "connected": editor.connected,
            "serves_commands": editor.serves_commands,
        },
        "commands": declaration.as_ref().map_or(0, Declaration::command_count),
        "providers": providers,
        "problems": problems,
        "workspace": workspace,
    })
}

/// The call tool's own input properties.
pub(crate) fn properties() -> Map<String, Value> {
    let name = |description: &str| {
        json!({
            "type": "string",
            "minLength": 1,
            "maxLength": godot_mcp_domain::project_commands::MAX_NAME_CHARS,
            "description": description
        })
    };
    let properties = json!({
        "provider": name("The provider's `id` in godot_mcp_commands.json, as editor.commands.list names it."),
        "command": name("The command's `name` under that provider."),
        "arguments": {
            "type": "object",
            "additionalProperties": {
                "type": ["null", "boolean", "integer", "number", "string", "array", "object"]
            },
            "description": "The command's arguments: an object its declared `input_schema` \
                            accepts, which the server checks before the editor is asked. An \
                            empty object when omitted."
        },
        "deadline_ms": {
            "type": "integer",
            "minimum": MIN_DEADLINE_MS,
            "maximum": MAX_DEADLINE_MS,
            "default": DEFAULT_DEADLINE_MS,
            "description": "How long the server waits for the editor's answer, in \
                            milliseconds, with a further two seconds' grace. It ends the wait, \
                            not the code: a call not answered by then is `uncertain`, the \
                            editor may still be running it, and project writes stay blocked \
                            until the transaction is resolved."
        }
    });
    match properties {
        Value::Object(properties) => properties,
        _ => unreachable!("the properties are an object"),
    }
}

/// One call checked against the declaration: what the editor is sent, how
/// long the server waits, and the command it calls.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Call {
    pub(crate) payload: Value,
    pub(crate) deadline: Duration,
    pub(crate) provider: String,
    pub(crate) command: String,
    pub(crate) mutates: bool,
}

fn name_argument<'a>(
    arguments: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a str, OutcomeError> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|name| valid_name(name))
        .ok_or_else(|| {
            outcome_error(
                ErrorKind::InvalidInput,
                format!(
                    "`{key}` must be a provider id or command name of godot_mcp_commands.json: \
                     1 to 64 characters, a lowercase ASCII letter, then lowercase letters, \
                     digits, `_`, `.` or `-`"
                ),
            )
        })
}

/// Check one call against the declaration and the provider's script on disk.
pub(crate) fn prepare(
    root: &SafeRoot,
    arguments: &Map<String, Value>,
) -> Result<Call, OutcomeError> {
    let provider_id = name_argument(arguments, "provider")?;
    let command_name = name_argument(arguments, "command")?;
    let empty = Map::new();
    let given = match arguments.get("arguments") {
        None => &empty,
        Some(Value::Object(given)) => given,
        Some(_) => {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                "`arguments` must be an object",
            ));
        }
    };
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
    let bytes = match root.read(Path::new(DECLARATION_FILE)) {
        Ok(bytes) => bytes,
        Err(SafeRootError::StalePreimage { .. }) => {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                format!(
                    "the project declares no commands: {DECLARATION_FILE} does not exist at its \
                     root. Nothing was called."
                ),
            ));
        }
        Err(error) => {
            return Err(outcome_error(
                error.error_kind(),
                format!("{DECLARATION_FILE} cannot be read: {error}"),
            ));
        }
    };
    let declaration = Declaration::parse(&bytes);
    if !declaration.is_valid() {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!(
                "{DECLARATION_FILE} breaks {} rules, so none of its commands can be called; \
                 editor.commands.list shows them. Nothing was called.",
                declaration.problems.len()
            ),
        ));
    }
    let Some(provider) = declaration
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
    else {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("{DECLARATION_FILE} declares no provider `{provider_id}`. Nothing was called."),
        ));
    };
    let Some(command) = provider
        .commands
        .iter()
        .find(|command| command.name == command_name)
    else {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!(
                "provider `{provider_id}` declares no command `{command_name}`. Nothing was called."
            ),
        ));
    };
    if let Some(reason) = &command.unavailable {
        return Err(outcome_error(
            ErrorKind::Unsupported,
            format!("{provider_id}/{command_name} cannot be called: {reason}. Nothing was called."),
        ));
    }
    if let Some(reason) = script_state(root, &provider.script).reason(&provider.script) {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("{reason}. Nothing was called."),
        ));
    }
    let tagged = prepare_arguments(command, given).map_err(|reason| {
        outcome_error(
            ErrorKind::InvalidInput,
            format!("{reason}. Nothing was called."),
        )
    })?;
    Ok(Call {
        payload: json!({
            "provider": provider.id,
            "command": command.name,
            "script": provider.script,
            "arguments": tagged,
        }),
        deadline: Duration::from_millis(deadline_ms),
        provider: provider.id.clone(),
        command: command.name.clone(),
        mutates: command.mutates,
    })
}

/// The call's result from the routed command's result and the project's
/// contents before the call and after it.
pub(crate) fn finish(
    call: &Call,
    result: Result<Value, OutcomeError>,
    before: Option<&Contents>,
    after: Result<Option<Contents>, OutcomeError>,
) -> Result<Value, OutcomeError> {
    match result {
        Ok(answer) => Ok(answer_of(call, answer, project_changes(before, after))),
        Err(error) if unresolved(&error) => Err(with_changes(error, before, after, STILL_RUNNING)),
        Err(error) => Err(error),
    }
}

/// Whether the editor may have started the call that `error` reports.
fn unresolved(error: &OutcomeError) -> bool {
    use godot_mcp_domain::contracts::GodotValue;

    error.kind() == ErrorKind::Uncertain
        || matches!(
            error.details().get("mutation_status"),
            Some(GodotValue::String(status)) if status == "unresolved"
        )
}

/// The answer of an exact retry the journal already holds: its recorded
/// outcome, the command the call names, and why the value is absent.
pub(crate) fn replayed(answer: Value, arguments: &Map<String, Value>) -> Value {
    let Value::Object(mut report) = answer else {
        return answer;
    };
    report.insert(
        "command".to_owned(),
        json!({
            "provider": arguments.get("provider").cloned().unwrap_or(Value::Null),
            "name": arguments.get("command").cloned().unwrap_or(Value::Null),
        }),
    );
    report.insert(
        "result_unavailable".to_owned(),
        Value::String(REPLAYED.to_owned()),
    );
    Value::Object(report)
}

/// The answer of a call the editor reported, from its report and the
/// project's changes.
pub(crate) fn answer_of(call: &Call, answer: Value, changes: Result<Value, String>) -> Value {
    if answer.get("replayed") == Some(&Value::Bool(true)) {
        let mut named = Map::new();
        named.insert("provider".to_owned(), Value::String(call.provider.clone()));
        named.insert("command".to_owned(), Value::String(call.command.clone()));
        return replayed(answer, &named);
    }
    let Value::Object(mut report) = answer else {
        return answer;
    };
    report.insert(
        "command".to_owned(),
        json!({ "provider": call.provider, "name": call.command, "mutates": call.mutates }),
    );
    let source_file = report
        .remove("source_file")
        .and_then(|file| file.as_str().map(str::to_owned));
    let captured = report.remove("log_capture") == Some(Value::Bool(true));
    let entries = match report.remove("log") {
        Some(Value::Array(entries)) => entries,
        _ => Vec::new(),
    };
    let (diagnostics, output) = crate::live_script::sort_log(&entries, source_file.as_deref());
    let errors = entries
        .iter()
        .filter(|entry| {
            crate::live_script::ERROR_KINDS.contains(&entry["kind"].as_str().unwrap_or(""))
        })
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
    let reported_ok = report.remove("ok").and_then(|ok| ok.as_bool());
    let command_error = report.remove("error").map(|error| {
        let text = error
            .as_str()
            .map_or_else(|| error.to_string(), str::to_owned);
        text.chars().take(MAX_ERROR_CHARS).collect::<String>()
    });
    let not_called = |default: &str| {
        format!(
            "{}; mcp_call() was not called",
            given_reason.as_deref().unwrap_or(default)
        )
    };
    let none = || {
        (
            Value::Null,
            Value::Null,
            Some("mcp_call() was not called".to_owned()),
        )
    };
    let (classification, reason, (result, result_literal, unavailable)) =
        match stage.as_ref().and_then(Value::as_str) {
            Some("compile") => (
                "compile_error",
                not_called("the provider's script did not compile"),
                none(),
            ),
            Some("provider") => (
                "failed",
                not_called("the provider's script defines no mcp_call(command, args)"),
                none(),
            ),
            Some("returned") => {
                let (result, unavailable) = returned_value(literal.as_deref(), length);
                (
                    "failed",
                    "mcp_call() returned a value that is not a Dictionary with a boolean `ok`; \
                     `result` is what it returned"
                        .to_owned(),
                    (
                        result,
                        literal.map_or(Value::Null, Value::String),
                        unavailable,
                    ),
                )
            }
            Some("done") if reported_ok == Some(false) => (
                "failed",
                format!(
                    "the command reported an error: {}",
                    command_error.as_deref().unwrap_or("it gave no error text")
                ),
                (
                    Value::Null,
                    Value::Null,
                    Some("the command reported an error instead of a value".to_owned()),
                ),
            ),
            Some("done") if reported_ok == Some(true) => {
                let (result, unavailable) = returned_value(literal.as_deref(), length);
                let returned = (
                    result,
                    literal.map_or(Value::Null, Value::String),
                    unavailable,
                );
                if errors > 0 {
                    (
                        "failed",
                        format!(
                            "mcp_call() returned, and {errors} errors were logged while the \
                             script loaded and ran"
                        ),
                        returned,
                    )
                } else if captured {
                    ("passed", "mcp_call() returned".to_owned(), returned)
                } else {
                    (
                        "inconclusive",
                        "mcp_call() returned, but this Godot build gives no log to read (log \
                         capture needs Godot 4.5 or later), so an error inside it cannot be seen"
                            .to_owned(),
                        returned,
                    )
                }
            }
            _ => (
                "inconclusive",
                "the report names no stage the server knows".to_owned(),
                (
                    Value::Null,
                    Value::Null,
                    Some("the report is unreadable".to_owned()),
                ),
            ),
        };
    let mismatch = match &changes {
        Ok(changes) if !call.mutates => changes["total"]
            .as_u64()
            .filter(|total| *total > 0)
            .map(|total| {
                Value::String(format!(
                    "the command is declared with mutates false, but {total} project files \
                     differ from before the call"
                ))
            })
            .unwrap_or(Value::Null),
        _ => Value::Null,
    };
    let (changes, changes_unavailable) = match changes {
        Ok(changes) => (changes, Value::Null),
        Err(reason) => (Value::Null, Value::String(reason)),
    };
    report.insert("classification".to_owned(), json!(classification));
    report.insert("reason".to_owned(), Value::String(reason));
    report.insert(
        "error".to_owned(),
        command_error.map_or(Value::Null, Value::String),
    );
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
    report.insert("declaration_mismatch".to_owned(), mismatch);
    report.insert("applied".to_owned(), Value::Bool(true));
    report.insert("rolled_back".to_owned(), Value::Bool(false));
    Value::Object(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(mutates: bool) -> Call {
        Call {
            payload: json!({}),
            deadline: Duration::from_millis(DEFAULT_DEADLINE_MS),
            provider: "levels".to_owned(),
            command: "rebuild".to_owned(),
            mutates,
        }
    }

    fn reported(report: Value, changes: Value) -> Value {
        let mut answer = report;
        answer["transaction_id"] = json!("tx-1");
        answer["mutation_status"] = json!("committed");
        answer["replayed"] = json!(false);
        answer_of(&call(true), answer, Ok(changes))
    }

    fn no_changes() -> Value {
        json!({ "added": [], "modified": [], "removed": [], "total": 0, "truncated": false })
    }

    #[test]
    fn the_tool_annotation_is_found_where_gdscript_reads_it() {
        for line in [
            "@tool",
            "  @tool",
            "\t@tool extends Node",
            "@tool # comment",
        ] {
            assert!(is_tool_annotation(line), "{line:?}");
        }
        for line in ["# @tool", "@toolkit", "@tool_x", "var a = \"@tool\"", ""] {
            assert!(!is_tool_annotation(line), "{line:?}");
        }
    }

    #[test]
    fn a_returned_command_passes_only_when_it_said_ok_and_nothing_was_logged() {
        let passed = reported(
            json!({
                "stage": "done", "ok": true, "literal": "3",
                "source_file": "res://tools/c.gd", "log_capture": true,
                "log": [{ "kind": "message", "text": "hi", "stderr": false }],
            }),
            no_changes(),
        );
        assert_eq!(passed["classification"], "passed", "{passed}");
        assert_eq!(passed["result"], json!({ "type": "integer", "value": 3 }));
        assert_eq!(passed["error"], Value::Null);
        assert_eq!(passed["declaration_mismatch"], Value::Null);
        for absent in ["stage", "literal", "log", "source_file", "ok"] {
            assert!(passed.get(absent).is_none(), "{absent}: {passed}");
        }

        let failed = reported(
            json!({
                "stage": "done", "ok": true, "literal": "null",
                "source_file": "res://tools/c.gd", "log_capture": true,
                "log": [{
                    "kind": "script", "text": "Invalid access", "file": "res://tools/c.gd",
                    "line": 9, "function": "mcp_call", "backtrace": [],
                }],
            }),
            no_changes(),
        );
        assert_eq!(failed["classification"], "failed", "{failed}");
        assert_eq!(failed["diagnostics"][0]["source_line"], 9, "{failed}");

        let unseen = reported(
            json!({ "stage": "done", "ok": true, "literal": "1", "log_capture": false, "log": [] }),
            no_changes(),
        );
        assert_eq!(unseen["classification"], "inconclusive", "{unseen}");
    }

    #[test]
    fn a_command_that_did_not_load_or_answer_well_says_where_it_stopped() {
        let compile = reported(
            json!({
                "stage": "compile", "reason": "the script did not load (Parse error)",
                "source_file": "res://tools/c.gd", "log_capture": true,
                "log": [{
                    "kind": "script", "text": "Parse Error: Expected expression",
                    "file": "res://tools/c.gd", "line": 3, "function": "", "backtrace": [],
                }],
            }),
            no_changes(),
        );
        assert_eq!(compile["classification"], "compile_error", "{compile}");
        assert_eq!(compile["diagnostics"][0]["source_line"], 3);
        assert_eq!(compile["result_unavailable"], "mcp_call() was not called");

        let provider = reported(
            json!({ "stage": "provider", "log_capture": true, "log": [] }),
            no_changes(),
        );
        assert_eq!(provider["classification"], "failed", "{provider}");
        assert!(
            provider["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("mcp_call(command, args)")),
            "{provider}"
        );

        let shapeless = reported(
            json!({ "stage": "returned", "literal": "42", "log_capture": true, "log": [] }),
            no_changes(),
        );
        assert_eq!(shapeless["classification"], "failed", "{shapeless}");
        assert_eq!(
            shapeless["result"],
            json!({ "type": "integer", "value": 42 })
        );

        let refused = reported(
            json!({
                "stage": "done", "ok": false, "error": "x".repeat(MAX_ERROR_CHARS + 10),
                "literal": null, "log_capture": true, "log": [],
            }),
            no_changes(),
        );
        assert_eq!(refused["classification"], "failed", "{refused}");
        assert_eq!(
            refused["error"].as_str().map(|text| text.chars().count()),
            Some(MAX_ERROR_CHARS)
        );
    }

    #[test]
    fn a_command_declared_not_to_mutate_that_changed_files_is_flagged() {
        let changed = json!({
            "added": [{ "path": "a.txt", "sha256": "0", "bytes": 1 }],
            "modified": [], "removed": [], "total": 1, "truncated": false
        });
        let report =
            json!({ "stage": "done", "ok": true, "literal": "1", "log_capture": true, "log": [] });
        let flagged = answer_of(&call(false), report.clone(), Ok(changed.clone()));
        assert!(
            flagged["declaration_mismatch"]
                .as_str()
                .is_some_and(|text| text.contains("mutates false")),
            "{flagged}"
        );
        let declared = answer_of(&call(true), report.clone(), Ok(changed));
        assert_eq!(declared["declaration_mismatch"], Value::Null);
        let quiet = answer_of(&call(false), report, Ok(no_changes()));
        assert_eq!(quiet["declaration_mismatch"], Value::Null);
    }

    #[test]
    fn a_replay_answers_from_the_journal_without_the_value() {
        let replay = answer_of(
            &call(true),
            json!({ "replayed": true, "status": "committed", "transaction_id": "tx-1" }),
            Ok(no_changes()),
        );
        assert_eq!(replay["replayed"], true);
        assert!(replay.get("changes").is_none(), "{replay}");
        assert!(replay["result_unavailable"].is_string(), "{replay}");
        assert_eq!(replay["command"]["name"], "rebuild");
    }
}
