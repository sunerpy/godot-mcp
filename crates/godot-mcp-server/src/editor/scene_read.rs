//! `editor.scene.read`: what the connected editor's engine answers about the
//! scene it is editing, unsaved edits included.
//!
//! The calls are those of `scene.read` ([`crate::engine_plane::read`]): the
//! read-only methods of the reading table, on nodes of the scene or on
//! resources they hold. `scene.read` makes them on the saved file in a
//! supervised headless Godot; this tool sends them to the add-on, which makes
//! them on the scene as the editor holds it and answers each value in Godot's
//! own text spelling, so both tools answer through the same parser.
//!
//! What the editor logged while a call ran is that call's diagnostics. Godot
//! 4.5.1 and later hand their log to the plugin; Godot 4.4.1 does not, so its
//! calls answer `diagnostics_complete: false`: nothing says such a call raised
//! nothing. An answer that does not answer exactly the calls asked is refused
//! whole rather than read as values.

use std::collections::BTreeMap;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_engine::validation::diagnostic::{
    Diagnostic, DiagnosticCategory, DiagnosticSeverity, parse_engine_diagnostics,
};
use serde_json::{Map, Value, json};

use crate::engine_plane::read::{ReadKind, call_answer, calls_schema, parse_calls};

/// The tool, and the bridge method the add-on serves it with.
pub(crate) const TOOL: &str = "editor.scene.read";
pub(crate) const BRIDGE_METHOD: &str = "editor.scene.read";

fn outcome_error(kind: ErrorKind, message: impl Into<String>) -> OutcomeError {
    OutcomeError::new(kind, message, false, BTreeMap::new())
}

/// The input schema's properties, beside the `scene` every editor read takes.
pub(crate) fn properties(scene: Value) -> Value {
    json!({ "scene": scene, "calls": calls_schema(ReadKind::Scene) })
}

/// What the editor is asked for: the `scene` (`""` for the scene being
/// edited) and the validated calls.
///
/// # Errors
/// Returns [`ErrorKind::InvalidInput`] for an unknown field, a scene that is
/// not a text scene, or calls `scene.read` would refuse.
pub(crate) fn request(
    arguments: &Map<String, Value>,
    scene: Option<String>,
) -> Result<Value, OutcomeError> {
    if let Some(unknown) = arguments
        .keys()
        .find(|key| !["scene", "calls"].contains(&key.as_str()))
    {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("{TOOL}: unknown field `{unknown}`, expected scene, calls"),
        ));
    }
    let calls = parse_calls(ReadKind::Scene, TOOL, arguments.get("calls"))?;
    Ok(json!({ "scene": scene.unwrap_or_default(), "calls": calls }))
}

fn malformed(what: &str) -> OutcomeError {
    outcome_error(
        ErrorKind::Internal,
        format!(
            "the editor's answer to {BRIDGE_METHOD} {what}, so none of its values is given; \
             update the add-on with editor.addon.update if it predates this server"
        ),
    )
}

/// The public answer from the editor's, held to the calls asked.
///
/// # Errors
/// Returns [`ErrorKind::Internal`] when the editor's answer does not name its
/// scene and digest, or does not answer each call asked, in order, with a
/// value or the reason it has none and the entries it logged.
pub(crate) fn answer(asked: &[Value], answer: &Value) -> Result<Value, OutcomeError> {
    let object = answer
        .as_object()
        .ok_or_else(|| malformed("is not an object"))?;
    let scene = object
        .get("scene")
        .and_then(Value::as_str)
        .filter(|scene| scene.starts_with("res://"))
        .ok_or_else(|| malformed("names no scene"))?;
    let digest = object
        .get("digest")
        .and_then(Value::as_str)
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| malformed("carries no semantic digest"))?;
    let unsaved = match object.get("unsaved") {
        Some(Value::Bool(unsaved)) => Value::Bool(*unsaved),
        None | Some(Value::Null) => Value::Null,
        Some(_) => {
            return Err(malformed(
                "says neither whether the scene is unsaved nor that it cannot tell",
            ));
        }
    };
    let calls = object
        .get("calls")
        .and_then(Value::as_array)
        .filter(|calls| calls.len() == asked.len())
        .ok_or_else(|| malformed("does not answer every call asked"))?;
    let mut answered = Vec::with_capacity(calls.len());
    for (index, (entry, call)) in calls.iter().zip(asked).enumerate() {
        if entry.get("index") != Some(&json!(index)) || entry.get("method") != call.get("method") {
            return Err(malformed(&format!("answers call {index} as another call")));
        }
        let valued = entry.get("literal").is_some_and(Value::is_string);
        let refused = entry.get("error_kind").is_some_and(Value::is_string)
            && entry.get("detail").is_some_and(Value::is_string);
        if valued == refused {
            return Err(malformed(&format!(
                "answers call {index} with neither a value nor the reason it has none"
            )));
        }
        let logged = entry
            .get("logged")
            .and_then(Value::as_array)
            .ok_or_else(|| malformed(&format!("lists no log entries for call {index}")))?;
        let complete = entry
            .get("logged_complete")
            .and_then(Value::as_bool)
            .ok_or_else(|| {
                malformed(&format!(
                    "does not say whether call {index}'s log entries are complete"
                ))
            })?;
        let diagnostics = logged
            .iter()
            .map(logged_diagnostic)
            .collect::<Result<Vec<_>, _>>()?;
        answered.push(call_answer(entry, &diagnostics, complete));
    }
    Ok(json!({
        "scene": scene,
        "digest": digest,
        "unsaved": unsaved,
        "backend": "editor",
        "calls": answered,
    }))
}

/// One entry the editor logged while a call ran, as the diagnostic a headless
/// read gives for the same report: the entry is spelled the way Godot prints
/// it to its output and read back with the same parser.
fn logged_diagnostic(entry: &Value) -> Result<Diagnostic, OutcomeError> {
    let (prefix, severity, category) = match entry.get("kind").and_then(Value::as_str) {
        Some("error") => (
            "ERROR",
            DiagnosticSeverity::Error,
            DiagnosticCategory::EngineReported,
        ),
        Some("warning") => (
            "WARNING",
            DiagnosticSeverity::Warning,
            DiagnosticCategory::EngineReported,
        ),
        Some("script") => (
            "SCRIPT ERROR",
            DiagnosticSeverity::Error,
            DiagnosticCategory::EngineReported,
        ),
        Some("shader") => (
            "SHADER ERROR",
            DiagnosticSeverity::Error,
            DiagnosticCategory::ShaderCompile,
        ),
        _ => return Err(malformed("logged an entry of no kind it reports")),
    };
    let text = |key: &str| {
        entry
            .get(key)
            .and_then(Value::as_str)
            .map(|text| text.replace(['\r', '\n'], " "))
            .filter(|text| !text.trim().is_empty())
    };
    let message = text("text").unwrap_or_else(|| "the editor logged no message".to_owned());
    let position = match (text("file"), entry.get("line").and_then(Value::as_u64)) {
        (Some(file), Some(line)) => Some(format!("{file}:{line}")),
        (Some(file), None) => Some(file),
        (None, _) => None,
    };
    let function = text("function");
    let mut printed = format!("{prefix}: {message}\n");
    if let (Some(function), Some(position)) = (&function, &position) {
        printed.push_str(&format!("   at: {function} ({position})\n"));
    }
    let mut parsed = parse_engine_diagnostics(&printed);
    if parsed.len() == 1 {
        return Ok(parsed.remove(0));
    }
    Ok(Diagnostic {
        severity,
        category,
        message: message.trim().to_owned(),
        location: None,
        engine_location: position,
        function,
        raw: format!("{prefix}: {message}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: &str, text: &str, function: &str, file: &str, line: Option<u64>) -> Value {
        json!({ "kind": kind, "text": text, "function": function, "file": file, "line": line,
                "sequence": 1, "unix_ms": 1, "stderr": true, "code": text, "rationale": "",
                "backtrace": [] })
    }

    #[test]
    fn a_logged_entry_reads_as_the_diagnostic_godot_would_print() {
        let engine = logged_diagnostic(&entry(
            "error",
            "Index p_track = 7 is out of bounds (tracks.size() = 1).",
            "track_get_key_count",
            "scene/resources/animation.cpp",
            Some(3150),
        ))
        .expect("an error");
        assert_eq!(engine.severity, DiagnosticSeverity::Error);
        assert_eq!(engine.category, DiagnosticCategory::EngineReported);
        assert_eq!(
            engine.engine_location.as_deref(),
            Some("scene/resources/animation.cpp:3150")
        );
        assert!(engine.location.is_none());

        let script = logged_diagnostic(&entry(
            "script",
            "Invalid call. Nonexistent function 'jump'.",
            "_process",
            "res://player.gd",
            Some(12),
        ))
        .expect("a script error");
        assert_eq!(script.severity, DiagnosticSeverity::Error);
        let location = script.location.expect("a project location");
        assert_eq!(location.file, "res://player.gd");
        assert_eq!(location.line, Some(12));

        let warning =
            logged_diagnostic(&entry("warning", "careful", "f", "a.cpp", None)).expect("a warning");
        assert_eq!(warning.severity, DiagnosticSeverity::Warning);
        assert_eq!(warning.engine_location.as_deref(), Some("a.cpp"));

        let shader = logged_diagnostic(&entry("shader", "Unknown identifier.", "", "", None))
            .expect("a shader error");
        assert_eq!(shader.category, DiagnosticCategory::ShaderCompile);

        let silent = logged_diagnostic(&entry("error", "", "f", "a.cpp", Some(1)))
            .expect("an entry without a message is still an error");
        assert_eq!(silent.severity, DiagnosticSeverity::Error);
        assert_eq!(silent.message, "the editor logged no message");

        assert!(logged_diagnostic(&entry("message", "printed", "", "", None)).is_err());
    }

    #[test]
    fn a_request_names_the_scene_and_the_checked_calls() {
        let arguments = json!({ "calls": [{ "node": "Layer", "method": "get_used_cells" }] });
        let asked = request(
            arguments.as_object().expect("an object"),
            Some("res://level.tscn".to_owned()),
        )
        .expect("a valid read");
        assert_eq!(
            asked,
            json!({ "scene": "res://level.tscn",
                    "calls": [{ "node": "Layer", "method": "get_used_cells" }] })
        );
        let unknown = json!({ "calls": [{ "method": "get_used_cells" }], "path": "a.tscn" });
        assert_eq!(
            request(unknown.as_object().expect("an object"), None)
                .expect_err("refused")
                .kind(),
            ErrorKind::InvalidInput
        );
    }
}
