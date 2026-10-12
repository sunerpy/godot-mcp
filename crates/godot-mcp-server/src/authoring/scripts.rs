//! `script.create` and `script.patch`: GDScript authoring behind a static
//! parse gate.
//!
//! Authoring never implies execution. The proposed bytes are parsed by the
//! same static GDScript grammar the analysis layer uses; parse errors refuse
//! the write unless the caller explicitly accepts a damaged intermediate file,
//! and either way the result carries the diagnostics and says that no engine
//! ran. `script.check` remains the engine verdict.

use godot_mcp_analysis::gdscript::GdScriptParser;
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::catalog::AuthoringOperation;
use godot_mcp_domain::contracts::{GodotValue, OutcomeError};
use serde_json::{Map, Value, json};

use super::files::{self, MAX_TEXT_BYTES, change_property};
use super::patch::TextChange;
use super::paths::{self, WriteOwner};
use super::{
    AuthoringRuntime, CallContext, Envelope, object_schema, optional_bool, outcome_error,
    outcome_error_with, path_property, required_str, sha256_argument, sha256_property,
};

/// Diagnostics quoted in a refusal message.
const QUOTED_DIAGNOSTICS: usize = 5;

fn allow_property() -> Value {
    json!({
        "type": "boolean",
        "description": "Publish even when the static parse reports errors (default false)."
    })
}

pub(super) fn create_schema() -> Value {
    object_schema(
        json!({
            "path": path_property("Project-relative path of the new .gd script; it must be absent."),
            "content": {
                "type": "string",
                "maxLength": MAX_TEXT_BYTES,
                "description": "The complete GDScript source."
            },
            "allowParseErrors": allow_property()
        }),
        &["path", "content"],
        true,
    )
}

pub(super) fn patch_schema() -> Value {
    object_schema(
        json!({
            "path": path_property("Project-relative path of the .gd script to change."),
            "expectedSha256": sha256_property("SHA-256 of the script's current bytes."),
            "change": change_property(),
            "allowParseErrors": allow_property()
        }),
        &["path", "expectedSha256", "change"],
        true,
    )
}

fn script_path(arguments: &Map<String, Value>) -> Result<String, OutcomeError> {
    let path = paths::normalize(required_str(arguments, "path")?, "path")?;
    match paths::writable(&path)? {
        WriteOwner::Script => Ok(path),
        WriteOwner::Text | WriteOwner::Asset => Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("{path} is not a .gd script; the script tools write GDScript sources only"),
        )),
    }
}

/// The static diagnostics of `text`, rendered for the public result.
fn static_diagnostics(path: &str, text: &str, generation: u64) -> Result<Vec<Value>, OutcomeError> {
    let mut parser = GdScriptParser::new().map_err(|error| {
        outcome_error(
            error.kind(),
            format!("the GDScript parser is unavailable: {}", error.message()),
        )
    })?;
    let script = parser
        .parse(path, text, generation)
        .map_err(|error| outcome_error(error.kind(), error.message().to_owned()))?;
    Ok(script
        .diagnostics()
        .iter()
        .map(|diagnostic| {
            json!({
                "code": diagnostic.kind().code(),
                "message": diagnostic.message(),
                "line": diagnostic.span().start_line(),
                "column": diagnostic.span().start_column(),
            })
        })
        .collect())
}

/// Refuse a script with parse errors unless they were explicitly accepted.
fn gate(path: &str, diagnostics: &[Value], allow: bool) -> Result<(), OutcomeError> {
    if diagnostics.is_empty() || allow {
        return Ok(());
    }
    let quoted: Vec<String> = diagnostics
        .iter()
        .take(QUOTED_DIAGNOSTICS)
        .map(|diagnostic| {
            format!(
                "line {}: {}",
                diagnostic["line"],
                diagnostic["message"].as_str().unwrap_or_default()
            )
        })
        .collect();
    Err(outcome_error_with(
        ErrorKind::InvalidInput,
        format!(
            "{path} does not parse ({} diagnostic(s): {}); nothing was written. Fix the source, \
             or pass allowParseErrors to publish a damaged intermediate file",
            diagnostics.len(),
            quoted.join("; ")
        ),
        [
            ("path", GodotValue::String(path.to_owned())),
            (
                "diagnostic_count",
                GodotValue::Integer(i64::try_from(diagnostics.len()).unwrap_or(i64::MAX)),
            ),
        ],
    ))
}

fn static_check_value(diagnostics: Vec<Value>, allowed: bool) -> Value {
    json!({
        "verdict": if diagnostics.is_empty() { "valid_static" } else { "static_errors" },
        "published_with_errors": allowed && !diagnostics.is_empty(),
        "diagnostics": diagnostics,
        "engine_checked": false,
        "limitation": "a static parse only; no Godot process ran. Use script.check for the \
                       engine's own verdict.",
    })
}

pub(super) async fn create(
    runtime: &AuthoringRuntime,
    arguments: &Map<String, Value>,
    envelope: &Envelope,
    canonical: &Value,
    context: &CallContext<'_>,
) -> Result<Value, OutcomeError> {
    let path = script_path(arguments)?;
    let content = required_str(arguments, "content")?;
    let allow = optional_bool(arguments, "allowParseErrors")?.unwrap_or(false);
    let plan = files::plan_create(runtime, &path, content, envelope)?;
    let diagnostics = static_diagnostics(&path, &plan.after, runtime.generation)?;
    gate(&path, &diagnostics, allow)?;
    let mut result = Map::new();
    result.insert(
        "static_check".to_owned(),
        static_check_value(diagnostics, allow),
    );
    files::publish_text(
        runtime,
        AuthoringOperation::ScriptCreate.tool_name(),
        plan,
        result,
        envelope,
        canonical,
        context,
    )
    .await
}

pub(super) async fn patch(
    runtime: &AuthoringRuntime,
    arguments: &Map<String, Value>,
    envelope: &Envelope,
    canonical: &Value,
    context: &CallContext<'_>,
) -> Result<Value, OutcomeError> {
    let path = script_path(arguments)?;
    let expected = sha256_argument(arguments, "expectedSha256")?;
    let allow = optional_bool(arguments, "allowParseErrors")?.unwrap_or(false);
    let change = TextChange::from_arguments(arguments)?;
    let plan = files::plan_patch(runtime, &path, &expected, &change, envelope)?;
    let diagnostics = static_diagnostics(&path, &plan.after, runtime.generation)?;
    gate(&path, &diagnostics, allow)?;
    let mut result = Map::new();
    result.insert(
        "static_check".to_owned(),
        static_check_value(diagnostics, allow),
    );
    files::publish_text(
        runtime,
        AuthoringOperation::ScriptPatch.tool_name(),
        plan,
        result,
        envelope,
        canonical,
        context,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_valid_script_has_no_static_diagnostics() {
        let diagnostics = static_diagnostics(
            "scripts/ok.gd",
            "extends Node\n\nfunc jump() -> void:\n\tpass\n",
            1,
        )
        .expect("parser");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        gate("scripts/ok.gd", &diagnostics, false).expect("a clean script passes");
    }

    #[test]
    fn a_broken_script_is_refused_unless_allowed() {
        let diagnostics = static_diagnostics(
            "scripts/broken.gd",
            "extends Node\n\nfunc jump( -> void:\n\tpass\n",
            1,
        )
        .expect("parser");
        assert!(
            !diagnostics.is_empty(),
            "the parser must flag the syntax error"
        );
        let error = gate("scripts/broken.gd", &diagnostics, false).expect_err("refused");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(error.message().contains("nothing was written"));
        gate("scripts/broken.gd", &diagnostics, true).expect("explicitly allowed");
    }
}
