//! `scene.read`, `resource.read` and `shader.check`: what the engine itself
//! answers about a text scene, a text resource or a shader.
//!
//! Some facts a file holds are not one stored property a parser can show:
//! which cells of a `TileMapLayer` are painted, the keys of an animation
//! track, the states of an animation state machine, the polygons a baked
//! navigation mesh holds, the uniforms a shader declares. These tools ask the
//! engine, by calling the read-only methods of the reading table
//! ([`godot_mcp_domain::reading_methods`]) on nodes of the scene, on the
//! resource, or on resources either holds, and answer each value as a tagged
//! value.
//!
//! The headless helper makes the calls in a supervised Godot process and
//! answers each value in Godot's own text spelling. Nothing is staged,
//! published or saved. Like every helper run, the process starts the
//! project's autoloads, and instancing a scene runs its scripts' `_init` and
//! property setters; the scene never enters the tree, so `_ready` never runs.

use std::time::Duration;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{GodotValue, OutcomeError};
use godot_mcp_domain::editing_methods::MAX_CALL_ARGUMENTS;
use godot_mcp_domain::reading_methods::{MAX_READ_CALLS, READING_METHODS, method_names};
use godot_mcp_engine::canonical::request::CanonicalRequest;
use godot_mcp_engine::canonical::stage::{self, EngineContext};
use godot_mcp_engine::supervision::journal::{
    OperationFamily as EngineOperationFamily, ProjectIdentityClaim, TransactionEnvelope, hex_sha256,
};
use godot_mcp_engine::validation::diagnostic::{
    Diagnostic, DiagnosticCategory, DiagnosticSeverity, SourceLocation,
};
use godot_mcp_engine::validation::is_valid_project_relative_path;
use serde_json::{Map, Value, json};

use super::outcome_error;
use super::request::EngineOperation;
use super::runtime::{EngineRuntime, engine_transport};
use crate::kernel::AccessContext;

/// Longest accepted node path, property path or selector name, in
/// characters: Unicode scalar values, as a schema's `maxLength` counts them.
const MAX_NAME_CHARS: usize = 512;
/// Largest serialized list of calls passed to the engine.
const MAX_CALLS_BYTES: usize = 256 * 1024;
/// How long the helper may take to load the target and make every call.
const READ_DEADLINE: Duration = Duration::from_secs(60);

/// Which read a request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadKind {
    /// `scene.read`: calls on nodes of a `.tscn` scene.
    Scene,
    /// `resource.read`: calls on a `.tres` resource.
    Resource,
}

impl ReadKind {
    /// The engine operation that serves this read.
    pub(super) const fn operation(self) -> EngineOperation {
        match self {
            Self::Scene => EngineOperation::SceneRead,
            Self::Resource => EngineOperation::ResourceRead,
        }
    }

    /// The public tool name.
    pub(super) fn tool_name(self) -> &'static str {
        self.operation().tool_name()
    }

    const fn extension(self) -> &'static str {
        match self {
            Self::Scene => "tscn",
            Self::Resource => "tres",
        }
    }

    /// The resources a call may select instead of its node or resource.
    const fn selectors(self) -> &'static [&'static str] {
        match self {
            Self::Scene => &["property", "animation", "library"],
            Self::Resource => &["property", "animation"],
        }
    }
}

/// One validated read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReadRequest {
    pub(super) kind: ReadKind,
    /// The project-relative target, without a `res://` prefix.
    pub(super) target: String,
    /// The calls, as the helper reads them.
    pub(super) calls: Vec<Value>,
}

fn invalid(message: impl Into<String>) -> OutcomeError {
    outcome_error(ErrorKind::InvalidInput, message)
}

/// The classes of the reading table, for descriptions.
fn reading_classes() -> String {
    let classes: Vec<&str> = READING_METHODS.iter().map(|class| class.class).collect();
    match classes.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
        _ => classes.join(""),
    }
}

fn selector_schema(kind: ReadKind) -> Value {
    let mut properties = Map::new();
    properties.insert(
        "property".to_owned(),
        json!({
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_NAME_CHARS,
            "pattern": crate::schema::PROPERTY_PATH_PATTERN,
            "description": "The resource a property holds, or a property path through the \
                            resources each property holds, embedded or saved in its own file \
                            (`navigation_mesh`, `tree_root:states/Run/node`)."
        }),
    );
    properties.insert(
        "animation".to_owned(),
        json!({
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_NAME_CHARS,
            "pattern": crate::schema::NO_CONTROL_PATTERN,
            "description": match kind {
                ReadKind::Scene => "The animation an AnimationPlayer or AnimationTree holds, as \
                                    `library/name` or `name`.",
                ReadKind::Resource => "The animation an AnimationLibrary holds.",
            }
        }),
    );
    if kind == ReadKind::Scene {
        properties.insert(
            "library".to_owned(),
            json!({
                "type": "string",
                "maxLength": MAX_NAME_CHARS,
                "pattern": crate::schema::NO_CONTROL_PATTERN,
                "description": "The animation library an AnimationPlayer or AnimationTree \
                                holds; \"\" is its default library."
            }),
        );
    }
    json!({
        "type": "object",
        "minProperties": 1,
        "maxProperties": 1,
        "additionalProperties": false,
        "properties": properties,
        "description": "Call a resource instead of the node or resource itself."
    })
}

/// The schema of one read's `calls`: what `editor.scene.read` takes too.
pub(crate) fn calls_schema(kind: ReadKind) -> Value {
    let mut call = Map::new();
    if kind == ReadKind::Scene {
        call.insert(
            "node".to_owned(),
            json!({
                "type": "string",
                "minLength": 1,
                "maxLength": MAX_NAME_CHARS,
                "pattern": format!(
                    "^{}$",
                    crate::schema::relative_node_path(crate::schema::CONTROL)
                ),
                "description": "A node path relative to the scene root (`.`, the default, is \
                                the root)."
            }),
        );
    }
    call.insert("resource".to_owned(), selector_schema(kind));
    call.insert(
        "method".to_owned(),
        json!({
            "type": "string",
            "enum": method_names().into_iter().collect::<Vec<_>>(),
            "description": format!(
                "A reading method of the receiver's class, from the table in editor-bridge.md: \
                 {}.",
                reading_classes()
            )
        }),
    );
    call.insert(
        "arguments".to_owned(),
        json!({
            "type": "array",
            "maxItems": MAX_CALL_ARGUMENTS,
            "items": crate::schema::plain_value(),
            "description": "The call's arguments, in order, as tagged values."
        }),
    );
    json!({
        "type": "array",
        "minItems": 1,
        "maxItems": MAX_READ_CALLS,
        "items": {
            "type": "object",
            "additionalProperties": false,
            "properties": call,
            "required": ["method"]
        },
        "description": "The calls to make, in order. A call that cannot be made says why, and \
                        the others are still made."
    })
}

/// The strict input schema of one read tool.
pub(super) fn input_schema(kind: ReadKind) -> Map<String, Value> {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "path": {
                "type": "string",
                "minLength": 1,
                "maxLength": 1024,
                "description": match kind {
                    ReadKind::Scene => "Project-relative .tscn path, optionally res://-prefixed.",
                    ReadKind::Resource => "Project-relative .tres path, optionally res://-prefixed.",
                }
            },
            "calls": calls_schema(kind)
        },
        "required": ["path", "calls"],
        "$defs": crate::schema::value_definitions(),
    });
    schema
        .as_object()
        .cloned()
        .expect("read tool schemas are JSON objects")
}

/// A name a call carries: a string of at most [`MAX_NAME_CHARS`] characters
/// without control characters, empty only when `empty` allows it.
fn name<'a>(value: &'a Value, what: &str, empty: bool) -> Result<&'a str, OutcomeError> {
    value
        .as_str()
        .filter(|text| {
            (empty || !text.is_empty())
                && text.chars().count() <= MAX_NAME_CHARS
                && !text.chars().any(char::is_control)
        })
        .ok_or_else(|| {
            invalid(format!(
                "{what} must be a string of at most {MAX_NAME_CHARS} characters without \
                 control characters"
            ))
        })
}

/// A tagged value that refers to nothing: no resource and no live object, at
/// any depth.
fn plain_argument(value: &Value, context: &str) -> Result<(), OutcomeError> {
    super::authored::tagged_value(value, context)?;
    fn plain(value: &GodotValue) -> bool {
        match value {
            GodotValue::Resource(_) | GodotValue::Object(_) => false,
            GodotValue::Array(items) => items.iter().all(plain),
            GodotValue::Dictionary(entries) => entries.values().all(plain),
            _ => true,
        }
    }
    let parsed: GodotValue = serde_json::from_value(value.clone())
        .map_err(|error| invalid(format!("{context}: not a tagged value: {error}")))?;
    if !plain(&parsed) {
        return Err(invalid(format!(
            "{context}: a read's argument is a plain value, never a resource"
        )));
    }
    Ok(())
}

fn call(kind: ReadKind, value: &Value, context: &str) -> Result<(), OutcomeError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid(format!("{context}: a call is an object")))?;
    let accepted: &[&str] = match kind {
        ReadKind::Scene => &["node", "resource", "method", "arguments"],
        ReadKind::Resource => &["resource", "method", "arguments"],
    };
    if let Some(unknown) = object.keys().find(|key| !accepted.contains(&key.as_str())) {
        return Err(invalid(format!(
            "{context}: unknown field `{unknown}`, expected one of {}",
            accepted.join(", ")
        )));
    }
    let method = object
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("{context}: `method` is required")))?;
    if !method_names().contains(method) {
        return Err(invalid(format!(
            "{context}: `{method}` is not a reading method; see the table in editor-bridge.md"
        )));
    }
    if let Some(node) = object.get("node") {
        let node = name(node, &format!("{context}: `node`"), false)?;
        if node.starts_with('/') || node.split('/').any(|part| part == ".." || part.is_empty()) {
            return Err(invalid(format!(
                "{context}: `node` must be a node path relative to the scene root"
            )));
        }
    }
    if let Some(selector) = object.get("resource") {
        let selectors = kind.selectors();
        let shape = || {
            invalid(format!(
                "{context}: `resource` names one of {}",
                selectors.join(", ")
            ))
        };
        let selector = selector
            .as_object()
            .filter(|selector| selector.len() == 1)
            .ok_or_else(shape)?;
        let (key, value) = selector.iter().next().expect("one selector");
        if !selectors.contains(&key.as_str()) {
            return Err(shape());
        }
        let text = name(
            value,
            &format!("{context}: `resource.{key}`"),
            key == "library",
        )?;
        if key == "property" && text.split(':').any(str::is_empty) {
            return Err(invalid(format!(
                "{context}: `resource.property` is a property or a property path of non-empty \
                 names separated by `:`"
            )));
        }
    }
    match object.get("arguments") {
        None => Ok(()),
        Some(Value::Array(arguments)) if arguments.len() <= MAX_CALL_ARGUMENTS => {
            for (position, argument) in arguments.iter().enumerate() {
                plain_argument(argument, &format!("{context} argument {position}"))?;
            }
            Ok(())
        }
        Some(_) => Err(invalid(format!(
            "{context}: `arguments` is a list of at most {MAX_CALL_ARGUMENTS} tagged values"
        ))),
    }
}

/// Validate one read's arguments.
///
/// # Errors
/// Returns [`ErrorKind::InvalidInput`] for an unknown field, a path that is
/// not a project text scene or resource of the tool's kind, or a malformed
/// call.
pub(super) fn parse(
    kind: ReadKind,
    arguments: Map<String, Value>,
) -> Result<ReadRequest, OutcomeError> {
    let tool = kind.tool_name();
    if let Some(key) = arguments
        .iter()
        .find_map(|(key, value)| value.is_null().then_some(key))
    {
        return Err(invalid(format!(
            "invalid engine request: `{key}` must not be null"
        )));
    }
    if let Some(unknown) = arguments
        .keys()
        .find(|key| !["path", "calls"].contains(&key.as_str()))
    {
        return Err(invalid(format!(
            "invalid engine request: unknown field `{unknown}`, expected one of path, calls"
        )));
    }
    let raw_path = arguments
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("{tool}: `path` is required")))?;
    let target = raw_path.strip_prefix("res://").unwrap_or(raw_path);
    if target.len() > 1024 || !is_valid_project_relative_path(target) || target.starts_with('/') {
        return Err(invalid(format!("{tool}: `path` must be project-relative")));
    }
    let extension = target
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    if extension != kind.extension() {
        return Err(invalid(format!(
            "{tool}: `path` must end in .{}, not .{extension}",
            kind.extension()
        )));
    }
    Ok(ReadRequest {
        kind,
        target: target.to_owned(),
        calls: parse_calls(kind, tool, arguments.get("calls"))?,
    })
}

/// Validate one read's `calls`, as `tool` takes them.
///
/// # Errors
/// Returns [`ErrorKind::InvalidInput`] for a missing or empty list, more than
/// [`MAX_READ_CALLS`] calls, a malformed call, or calls that take more than
/// [`MAX_CALLS_BYTES`] of compact JSON.
pub(crate) fn parse_calls(
    kind: ReadKind,
    tool: &str,
    calls: Option<&Value>,
) -> Result<Vec<Value>, OutcomeError> {
    let calls = match calls {
        Some(Value::Array(calls)) if !calls.is_empty() && calls.len() <= MAX_READ_CALLS => calls,
        Some(_) => {
            return Err(invalid(format!(
                "{tool}: `calls` lists 1 to {MAX_READ_CALLS} calls"
            )));
        }
        None => return Err(invalid(format!("{tool}: `calls` is required"))),
    };
    for (index, value) in calls.iter().enumerate() {
        call(kind, value, &format!("{tool} call {index}"))?;
    }
    let serialized = serde_json::to_string(calls)
        .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
    if serialized.len() > MAX_CALLS_BYTES {
        return Err(invalid(format!(
            "{tool}: the calls take {} bytes, more than the {MAX_CALLS_BYTES} a read accepts",
            serialized.len()
        )));
    }
    Ok(calls.clone())
}

/// One call's public answer from the helper's entry for it.
fn call_value(answered: stage::ReadCall) -> Value {
    call_answer(
        &answered.result,
        &answered.diagnostics,
        answered.diagnostics_complete,
    )
}

/// One call's public answer from the entry the helper or the editor gave for
/// it: `{index, method, literal}` for a value in Godot's text spelling, or
/// `{index, method, error_kind, detail}` and perhaps `value_bytes` for a call
/// that could not be made, with the diagnostics it raised.
pub(crate) fn call_answer(entry: &Value, diagnostics: &[Diagnostic], complete: bool) -> Value {
    let mut value = Map::new();
    value.insert("index".to_owned(), entry["index"].clone());
    value.insert("method".to_owned(), entry["method"].clone());
    if let Some(literal) = entry.get("literal").and_then(Value::as_str) {
        let (parsed, reason) = crate::scripts::returned_value(Some(literal), None);
        value.insert("value".to_owned(), parsed);
        if let Some(reason) = reason {
            value.insert("literal".to_owned(), Value::String(literal.to_owned()));
            value.insert("value_reason".to_owned(), Value::String(reason));
        }
    } else {
        value.insert("value".to_owned(), Value::Null);
        value.insert(
            "error".to_owned(),
            json!({
                "kind": entry
                    .get("error_kind")
                    .and_then(Value::as_str)
                    .unwrap_or("internal"),
                "detail": entry
                    .get("detail")
                    .and_then(Value::as_str)
                    .unwrap_or("the helper gave no reason"),
            }),
        );
        if let Some(bytes) = entry.get("value_bytes") {
            value.insert("value_bytes".to_owned(), bytes.clone());
        }
    }
    value.insert(
        "diagnostics".to_owned(),
        serde_json::to_value(diagnostics).unwrap_or(Value::Array(Vec::new())),
    );
    value.insert("diagnostics_complete".to_owned(), Value::Bool(complete));
    Value::Object(value)
}

/// One validated `shader.check`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShaderCheckRequest {
    /// The project-relative `.gdshader`, without a `res://` prefix.
    pub(super) target: String,
}

/// The strict input schema of `shader.check`.
pub(super) fn shader_check_schema() -> Map<String, Value> {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "path": {
                "type": "string",
                "minLength": 1,
                "maxLength": 1024,
                "description": "Project-relative .gdshader path, optionally res://-prefixed."
            }
        },
        "required": ["path"],
    })
    .as_object()
    .cloned()
    .expect("the shader check schema is a JSON object")
}

/// Validate `shader.check`'s arguments.
///
/// # Errors
/// Returns [`ErrorKind::InvalidInput`] for an unknown field or a path that is
/// not a project `.gdshader`.
pub(super) fn parse_shader_check(
    arguments: Map<String, Value>,
) -> Result<ShaderCheckRequest, OutcomeError> {
    let tool = EngineOperation::ShaderCheck.tool_name();
    if let Some(key) = arguments
        .iter()
        .find_map(|(key, value)| value.is_null().then_some(key))
    {
        return Err(invalid(format!(
            "invalid engine request: `{key}` must not be null"
        )));
    }
    if let Some(unknown) = arguments.keys().find(|key| key.as_str() != "path") {
        return Err(invalid(format!(
            "invalid engine request: unknown field `{unknown}`, expected path"
        )));
    }
    let raw_path = arguments
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("{tool}: `path` is required")))?;
    let target = raw_path.strip_prefix("res://").unwrap_or(raw_path);
    if target.len() > 1024 || !is_valid_project_relative_path(target) || target.starts_with('/') {
        return Err(invalid(format!("{tool}: `path` must be project-relative")));
    }
    if !target.to_ascii_lowercase().ends_with(".gdshader") {
        return Err(invalid(format!(
            "{tool}: `path` must be a .gdshader file; a .gdshaderinc include is checked through \
             a shader that includes it"
        )));
    }
    Ok(ShaderCheckRequest {
        target: target.to_owned(),
    })
}

/// `Shader.Mode`, by value, as the `shader_type` keyword spells it.
const SHADER_MODES: [&str; 5] = ["spatial", "canvas_item", "particles", "sky", "fog"];

/// `Variant.Type`, by value, as Godot's `type_string` spells it: identical on
/// 4.4.1, 4.5.1, 4.6.1 and 4.7.1, measured.
const VARIANT_TYPES: [&str; 39] = [
    "Nil",
    "bool",
    "int",
    "float",
    "String",
    "Vector2",
    "Vector2i",
    "Rect2",
    "Rect2i",
    "Vector3",
    "Vector3i",
    "Transform2D",
    "Vector4",
    "Vector4i",
    "Plane",
    "Quaternion",
    "AABB",
    "Basis",
    "Transform3D",
    "Projection",
    "Color",
    "StringName",
    "NodePath",
    "RID",
    "Object",
    "Callable",
    "Signal",
    "Dictionary",
    "Array",
    "PackedByteArray",
    "PackedInt32Array",
    "PackedInt64Array",
    "PackedFloat32Array",
    "PackedFloat64Array",
    "PackedStringArray",
    "PackedVector2Array",
    "PackedVector3Array",
    "PackedColorArray",
    "PackedVector4Array",
];

/// A shader compile error whose position names no file is the checked
/// shader's own: Godot leaves the main shader's file unnamed and names an
/// included one.
fn locate_shader_error(mut diagnostic: Diagnostic, target: &str) -> Diagnostic {
    if diagnostic.category == DiagnosticCategory::ShaderCompile
        && diagnostic.location.is_none()
        && let Some(line) = diagnostic
            .engine_location
            .as_deref()
            .and_then(|position| position.strip_prefix(':'))
            .and_then(|line| line.parse::<u32>().ok())
    {
        diagnostic.location = Some(SourceLocation {
            file: format!("res://{target}"),
            line: Some(line),
        });
        diagnostic.engine_location = None;
    }
    diagnostic
}

/// One uniform of `get_shader_uniform_list`, from its tagged dictionary.
fn uniform_value(uniform: &Value) -> Value {
    let field = |name: &str| &uniform["value"][name]["value"];
    let variant = field("type").as_i64();
    json!({
        "name": field("name"),
        "type": variant
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| VARIANT_TYPES.get(index))
            .copied(),
        "variant_type": variant,
        "hint": field("hint"),
        "hint_string": field("hint_string"),
        "usage": field("usage"),
    })
}

/// The `shader.check` answer from the read of its two calls.
fn shader_answer(
    target: &str,
    answer: stage::ReadAnswer,
    sha256: String,
) -> Result<Value, OutcomeError> {
    let tool = EngineOperation::ShaderCheck.tool_name();
    if answer.calls.len() != 2 || answer.calls.iter().any(|call| !call.diagnostics_complete) {
        return Err(outcome_error(
            ErrorKind::Uncertain,
            format!(
                "{tool}: the engine's output was cut off before {target} finished compiling, so \
                 whether it compiles is not known"
            ),
        ));
    }
    if let Some(refused) = answer
        .calls
        .iter()
        .find(|call| call.result.get("literal").is_none())
    {
        return Err(outcome_error(
            ErrorKind::Internal,
            format!(
                "{tool}: the engine did not answer {}: {}",
                refused.result["method"], refused.result["detail"]
            ),
        ));
    }
    let compile: Vec<Diagnostic> = answer
        .calls
        .iter()
        .flat_map(|call| call.diagnostics.iter().cloned())
        .map(|diagnostic| locate_shader_error(diagnostic, target))
        .collect();
    let compiled = !compile
        .iter()
        .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error);
    let errors: Vec<Value> = compile
        .iter()
        .filter(|diagnostic| diagnostic.category == DiagnosticCategory::ShaderCompile)
        .map(|diagnostic| {
            json!({
                "message": diagnostic.message,
                "file": diagnostic.location.as_ref().map(|location| location.file.clone()),
                "line": diagnostic.location.as_ref().and_then(|location| location.line),
            })
        })
        .collect();
    let mode =
        crate::scripts::returned_value(answer.calls[0].result["literal"].as_str(), None).0["value"]
            .as_i64()
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| SHADER_MODES.get(index))
            .copied();
    let uniforms: Vec<Value> =
        crate::scripts::returned_value(answer.calls[1].result["literal"].as_str(), None).0["value"]
            .as_array()
            .map(|uniforms| uniforms.iter().map(uniform_value).collect())
            .unwrap_or_default();
    let mut diagnostics = compile;
    diagnostics.extend(answer.diagnostics);
    Ok(json!({
        "path": target,
        "sha256": sha256,
        "compiled": compiled,
        "mode": mode,
        "uniforms": uniforms,
        "errors": errors,
        "diagnostics": diagnostics,
        "output_truncated": answer.output_truncated,
        "backend": "headless",
        "transaction_id": answer.launch_transaction_id,
        "argv": answer.argv,
    }))
}

impl EngineRuntime {
    /// Make one read's calls through the headless helper, and answer them
    /// with the target's bytes as they were read.
    ///
    /// The target's bytes are read before and after the run: an answer is
    /// given only when they did not change while the engine read them, and
    /// only when the helper's launch is a provably clean terminal process.
    async fn read_through_engine(
        &self,
        tool: &str,
        target: &str,
        calls: &[Value],
        arguments: &Value,
        access: &AccessContext,
        idempotency_id: Option<&str>,
    ) -> Result<(stage::ReadAnswer, Vec<u8>), OutcomeError> {
        let calls = serde_json::to_string(calls)
            .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
        let request = CanonicalRequest::read_calls(target, calls);
        let kind = request
            .validate()
            .map_err(|error| outcome_error(error.error_kind(), error.to_string()))?;
        let root = self.root()?;
        let before = stage::read_optional(root, target)
            .map_err(|error| outcome_error(error.error_kind(), error.to_string()))?
            .ok_or_else(|| {
                outcome_error(ErrorKind::NotFound, format!("{target} does not exist"))
            })?;
        let resolved = self.resolve_engine().await?;
        let observed_at = access.observed_at_unix_ms();
        let transaction_id = self.router.next_transaction_id(tool, observed_at);
        let canonical_bytes = serde_json::to_vec(arguments)
            .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
        let envelope = TransactionEnvelope {
            transaction_id: transaction_id.clone(),
            project: ProjectIdentityClaim {
                object_identity: self.held.object_identity.clone(),
                root_object: self.held.root_object.clone(),
                file_object: self.held.file_object.clone(),
                generation: self.held.generation,
            },
            transport_mode: engine_transport(access.transport_mode()),
            principal: Some(access.mode_principal().to_journal_string()),
            session: Some(access.connection_session().to_owned()),
            operation_family: EngineOperationFamily::DiagnosticProbe,
            tool_name: tool.to_owned(),
            idempotency_id: idempotency_id
                .map_or_else(|| format!("read-{transaction_id}"), str::to_owned),
            canonical_request_hash: hex_sha256(&canonical_bytes),
            canonical_request_bytes: canonical_bytes.len() as u64,
            opened_at_unix_ms: observed_at,
        };
        let answer = {
            let context = EngineContext {
                launch_journal: self.journal.as_ref(),
                provider: resolved.provider.as_ref(),
                binding: resolved.binding,
                deadline: READ_DEADLINE,
            };
            stage::read(&context, root, &request, kind, &envelope)
                .map_err(|error| outcome_error(error.error_kind(), error.to_string()))?
        };
        if !answer
            .launch_proof
            .as_ref()
            .is_some_and(|proof| proof.is_clean_terminal_process())
        {
            return Err(outcome_error(
                ErrorKind::Uncertain,
                format!(
                    "the helper that read {target} is not provably a clean terminal process, so \
                     its answer is not given (launch transaction {})",
                    answer.launch_transaction_id
                ),
            ));
        }
        let after = stage::read_optional(root, target)
            .map_err(|error| outcome_error(error.error_kind(), error.to_string()))?;
        if after.as_deref() != Some(before.as_slice()) {
            return Err(outcome_error(
                ErrorKind::Conflict,
                format!(
                    "{target} changed while the engine read it, so the answer may describe \
                     either version; read it again"
                ),
            ));
        }
        Ok((answer, before))
    }

    /// Make one `scene.read` or `resource.read`.
    pub(super) async fn execute_read(
        &self,
        read: ReadRequest,
        arguments: &Value,
        access: &AccessContext,
        idempotency_id: Option<&str>,
    ) -> Result<Value, OutcomeError> {
        let (answer, before) = self
            .read_through_engine(
                read.kind.tool_name(),
                &read.target,
                &read.calls,
                arguments,
                access,
                idempotency_id,
            )
            .await?;
        Ok(json!({
            "path": read.target,
            "sha256": hex_sha256(&before),
            "backend": "headless",
            "calls": answer.calls.into_iter().map(call_value).collect::<Vec<_>>(),
            "diagnostics": answer.diagnostics,
            "output_truncated": answer.output_truncated,
            "transaction_id": answer.launch_transaction_id,
            "argv": answer.argv,
        }))
    }

    /// Make one `shader.check`: compile the shader in the engine's own shader
    /// language front end, as a headless build runs it, and answer whether it
    /// compiles, its errors and its uniforms.
    pub(super) async fn execute_shader_check(
        &self,
        check: ShaderCheckRequest,
        arguments: &Value,
        access: &AccessContext,
        idempotency_id: Option<&str>,
    ) -> Result<Value, OutcomeError> {
        let calls = [
            json!({ "method": "get_mode" }),
            json!({ "method": "get_shader_uniform_list" }),
        ];
        let (answer, before) = self
            .read_through_engine(
                EngineOperation::ShaderCheck.tool_name(),
                &check.target,
                &calls,
                arguments,
                access,
                idempotency_id,
            )
            .await?;
        shader_answer(&check.target, answer, hex_sha256(&before))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::conformance::{NameRule, conforms, name_cases};

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().cloned().expect("an object")
    }

    /// The advertised schema of one call of a `kind` read, as a standalone
    /// schema carrying the definitions it refers to.
    fn advertised_call_schema(kind: ReadKind) -> Value {
        json!({
            "allOf": [calls_schema(kind)["items"].clone()],
            "$defs": crate::schema::value_definitions(),
        })
    }

    /// Hold the advertised call schema of a `kind` read and the call check
    /// to `expected` on `value`, which `case` names.
    fn agree(kind: ReadKind, value: &Value, expected: bool, case: &str) {
        assert_eq!(
            conforms(&advertised_call_schema(kind), value),
            expected,
            "the {kind:?} schema's verdict on {case}"
        );
        assert_eq!(
            call(kind, value, "call 0").is_ok(),
            expected,
            "the {kind:?} handler's verdict on {case}"
        );
    }

    /// Every name a call carries follows one rule in the schema and the
    /// check: 1 to 512 characters, whatever the bytes each takes, and no
    /// control character; only the default animation library's name is
    /// empty.
    #[test]
    fn every_name_a_call_carries_follows_one_rule_in_the_schema_and_the_check() {
        let rule = NameRule {
            limit: MAX_NAME_CHARS,
            empty: false,
            control: true,
        };
        let library = NameRule {
            empty: true,
            ..rule
        };
        for kind in [ReadKind::Scene, ReadKind::Resource] {
            let mut fields = vec![("property", rule), ("animation", rule)];
            if kind == ReadKind::Scene {
                fields.extend([("node", rule), ("library", library)]);
            }
            for (field, rule) in fields {
                for (index, (text, expected)) in name_cases(rule, "", "").into_iter().enumerate() {
                    let value = if field == "node" {
                        json!({ "method": "get_used_cells", "node": text })
                    } else {
                        let mut resource = Map::new();
                        resource.insert(field.to_owned(), json!(text));
                        json!({ "method": "get_length", "resource": resource })
                    };
                    let case = format!(
                        "{field}, case {index} ({} characters)",
                        text.chars().count()
                    );
                    agree(kind, &value, expected, &case);
                }
            }
        }
    }

    /// A call's node is a node path relative to the scene root, and a
    /// selected property a property path of non-empty names, in both the
    /// schema and the check.
    #[test]
    fn the_path_shapes_of_a_call_agree_in_the_schema_and_the_check() {
        for (node, expected) in [
            (".", true),
            ("Player/Sprite", true),
            ("..a/b..", true),
            ("nh1:abc", true),
            ("/root", false),
            ("a/", false),
            ("a//b", false),
            ("..", false),
            ("../a", false),
            ("a/../b", false),
        ] {
            let value = json!({ "method": "get_used_cells", "node": node });
            agree(ReadKind::Scene, &value, expected, &format!("node {node:?}"));
        }
        for (property, expected) in [
            ("navigation_mesh", true),
            ("tree_root:states/Run/node", true),
            ("a::b", false),
            (":a", false),
            ("a:", false),
        ] {
            let value = json!({ "method": "get_length", "resource": { "property": property } });
            for kind in [ReadKind::Scene, ReadKind::Resource] {
                agree(kind, &value, expected, &format!("property {property:?}"));
            }
        }
    }

    #[test]
    fn a_scene_read_takes_nodes_selectors_and_plain_arguments() {
        let read = parse(
            ReadKind::Scene,
            arguments(json!({
                "path": "res://levels/one.tscn",
                "calls": [
                    { "node": "Ground", "method": "get_used_cells" },
                    { "node": "Ground", "method": "get_cell_source_id",
                      "arguments": [{ "type": "vector2i", "value": { "x": 1, "y": 2 } }] },
                    { "node": "Player/AnimationPlayer", "resource": { "animation": "walk" },
                      "method": "track_get_key_count",
                      "arguments": [{ "type": "integer", "value": 0 }] },
                    { "node": "Region", "resource": { "property": "navigation_mesh" },
                      "method": "get_polygon_count" },
                    { "node": "Player/AnimationPlayer", "resource": { "library": "" },
                      "method": "get_animation_list" }
                ]
            })),
        )
        .expect("a valid scene read");
        assert_eq!(read.target, "levels/one.tscn");
        assert_eq!(read.calls.len(), 5);
    }

    #[test]
    fn a_read_refuses_what_its_schema_does_not_declare() {
        let refused = |kind: ReadKind, value: Value| {
            parse(kind, arguments(value)).expect_err("refused before any engine runs")
        };
        for (kind, value) in [
            (
                ReadKind::Scene,
                json!({ "path": "a.tres", "calls": [{ "method": "get_used_cells" }] }),
            ),
            (
                ReadKind::Resource,
                json!({ "path": "a.tscn", "calls": [{ "method": "get_length" }] }),
            ),
            (
                ReadKind::Scene,
                json!({ "path": "../a.tscn", "calls": [{ "method": "get_used_cells" }] }),
            ),
            (ReadKind::Scene, json!({ "path": "a.tscn", "calls": [] })),
            (
                ReadKind::Scene,
                json!({ "path": "a.tscn", "calls": [{ "method": "set_cell" }] }),
            ),
            (
                ReadKind::Scene,
                json!({ "path": "a.tscn", "calls": [{ "method": "get_used_cells", "node": "/root" }] }),
            ),
            (
                ReadKind::Scene,
                json!({ "path": "a.tscn", "calls": [{ "method": "get_used_cells", "node": "a/../b" }] }),
            ),
            (
                ReadKind::Resource,
                json!({ "path": "a.tres", "calls": [{ "method": "get_length", "node": "A" }] }),
            ),
            (
                ReadKind::Resource,
                json!({ "path": "a.tres", "calls": [{ "method": "get_length", "resource": { "library": "" } }] }),
            ),
            (
                ReadKind::Scene,
                json!({ "path": "a.tscn", "calls": [{ "method": "get_length", "resource": { "property": "a::b" } }] }),
            ),
            (
                ReadKind::Scene,
                json!({ "path": "a.tscn", "calls": [{ "method": "get_length", "resource": { "property": "a", "animation": "b" } }] }),
            ),
            (
                ReadKind::Scene,
                json!({ "path": "a.tscn", "calls": [{ "method": "get_length",
                "arguments": [{ "type": "resource", "value": { "resource": "res://a.tres", "resource_type": "Resource" } }] }] }),
            ),
            (
                ReadKind::Scene,
                json!({ "path": "a.tscn", "calls": [{ "method": "get_length", "extra": 1 }] }),
            ),
            (
                ReadKind::Scene,
                json!({ "path": "a.tscn", "calls": [{ "method": "get_length" }], "dryRun": true }),
            ),
        ] {
            let error = refused(kind, value.clone());
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "{value}");
        }
        let too_many = (0..=MAX_READ_CALLS)
            .map(|_| json!({ "method": "get_length" }))
            .collect::<Vec<_>>();
        assert_eq!(
            refused(
                ReadKind::Resource,
                json!({ "path": "a.tres", "calls": too_many })
            )
            .kind(),
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn the_schema_lists_every_reading_method_and_no_editing_one() {
        let schema = Value::Object(input_schema(ReadKind::Scene));
        let methods: Vec<&str> =
            schema["properties"]["calls"]["items"]["properties"]["method"]["enum"]
                .as_array()
                .expect("an enum")
                .iter()
                .filter_map(Value::as_str)
                .collect();
        assert_eq!(methods.len(), method_names().len());
        let editing = godot_mcp_domain::editing_methods::method_names();
        assert!(methods.iter().all(|method| !editing.contains(method)));
        let resource = Value::Object(input_schema(ReadKind::Resource));
        let call = &resource["properties"]["calls"]["items"]["properties"];
        assert!(call.get("node").is_none(), "a resource has no nodes");
        assert!(call["resource"]["properties"].get("library").is_none());
    }

    #[test]
    fn a_call_answers_its_value_or_why_it_has_none() {
        let answered = call_value(stage::ReadCall {
            result: json!({ "index": 0, "method": "get_point_count", "literal": "3" }),
            diagnostics: Vec::new(),
            diagnostics_complete: true,
        });
        assert_eq!(answered["value"], json!({ "type": "integer", "value": 3 }));
        assert!(answered.get("error").is_none());
        assert_eq!(answered["diagnostics_complete"], json!(true));

        let refused = call_value(stage::ReadCall {
            result: json!({ "index": 1, "method": "get_node_list",
                            "error_kind": "unsupported",
                            "detail": "this Godot build has no AnimationNodeBlendTree.get_node_list" }),
            diagnostics: Vec::new(),
            diagnostics_complete: false,
        });
        assert_eq!(refused["value"], Value::Null);
        assert_eq!(refused["error"]["kind"], json!("unsupported"));
        assert_eq!(refused["diagnostics_complete"], json!(false));

        let unparsed = call_value(stage::ReadCall {
            result: json!({ "index": 2, "method": "get_aabb", "literal": "Object(Node,\"x\":1)" }),
            diagnostics: Vec::new(),
            diagnostics_complete: true,
        });
        assert_eq!(unparsed["value"], Value::Null);
        assert!(unparsed["value_reason"].is_string());
        assert_eq!(unparsed["literal"], json!("Object(Node,\"x\":1)"));
    }

    fn read_call(
        index: usize,
        method: &str,
        literal: &str,
        diagnostics: Vec<Diagnostic>,
    ) -> stage::ReadCall {
        stage::ReadCall {
            result: json!({ "index": index, "method": method, "literal": literal }),
            diagnostics,
            diagnostics_complete: true,
        }
    }

    fn shader_read(calls: Vec<stage::ReadCall>) -> stage::ReadAnswer {
        stage::ReadAnswer {
            calls,
            diagnostics: Vec::new(),
            output_truncated: false,
            launch_transaction_id: "transaction-1".to_owned(),
            launch_proof: None,
            argv: Vec::new(),
        }
    }

    #[test]
    fn a_compiled_shader_answers_its_mode_and_uniforms() {
        let uniforms = "[{\n\"class_name\": &\"\",\n\"hint\": 1,\n\"hint_string\": \"0.0,2.0,0.0010000000475\",\n\"name\": \"strength\",\n\"type\": 3,\n\"usage\": 6\n}]";
        let answer = shader_answer(
            "fx/glow.gdshader",
            shader_read(vec![
                read_call(0, "get_mode", "0", Vec::new()),
                read_call(1, "get_shader_uniform_list", uniforms, Vec::new()),
            ]),
            "a".repeat(64),
        )
        .expect("an answer");
        assert_eq!(answer["compiled"], json!(true));
        assert_eq!(answer["mode"], json!("spatial"));
        assert_eq!(answer["errors"], json!([]));
        assert_eq!(
            answer["uniforms"],
            json!([{ "name": "strength", "type": "float", "variant_type": 3, "hint": 1,
                     "hint_string": "0.0,2.0,0.0010000000475", "usage": 6 }])
        );
    }

    #[test]
    fn a_failed_shader_names_each_error_at_its_file_and_line() {
        let raised = godot_mcp_engine::validation::diagnostic::parse_engine_diagnostics(
            "SHADER ERROR: Unknown identifier in expression: 'undefined_name'.\n          \
             at: (null) (:5)\n\
             SHADER ERROR: Unknown identifier in expression: 'missing_factor'.\n          \
             at: (null) (res://inc.gdshaderinc:2)\n\
             ERROR: Shader compilation failed.\n   \
             at: shader_set_code (servers/rendering/dummy/storage/material_storage.cpp:190)\n",
        );
        let answer = shader_answer(
            "fx/broken.gdshader",
            shader_read(vec![
                read_call(0, "get_mode", "1", Vec::new()),
                read_call(1, "get_shader_uniform_list", "[]", raised),
            ]),
            "a".repeat(64),
        )
        .expect("an answer");
        assert_eq!(answer["compiled"], json!(false));
        assert_eq!(answer["mode"], json!("canvas_item"));
        assert_eq!(
            answer["errors"],
            json!([
                { "message": "Unknown identifier in expression: 'undefined_name'.",
                  "file": "res://fx/broken.gdshader", "line": 5 },
                { "message": "Unknown identifier in expression: 'missing_factor'.",
                  "file": "res://inc.gdshaderinc", "line": 2 }
            ])
        );
        assert_eq!(answer["uniforms"], json!([]));
    }

    #[test]
    fn a_cut_off_compile_is_uncertain() {
        let mut cut = read_call(1, "get_shader_uniform_list", "[]", Vec::new());
        cut.diagnostics_complete = false;
        let error = shader_answer(
            "fx/glow.gdshader",
            shader_read(vec![read_call(0, "get_mode", "0", Vec::new()), cut]),
            "a".repeat(64),
        )
        .expect_err("whether it compiles is not known");
        assert_eq!(error.kind(), ErrorKind::Uncertain);
    }

    #[test]
    fn a_shader_check_takes_one_gdshader_path() {
        assert_eq!(
            parse_shader_check(arguments(json!({ "path": "res://fx/glow.gdshader" })))
                .expect("a shader")
                .target,
            "fx/glow.gdshader"
        );
        for value in [
            json!({ "path": "fx/glow.gdshaderinc" }),
            json!({ "path": "fx/glow.tres" }),
            json!({ "path": "../glow.gdshader" }),
            json!({ "path": "fx/glow.gdshader", "calls": [] }),
            json!({}),
        ] {
            assert_eq!(
                parse_shader_check(arguments(value.clone()))
                    .expect_err("refused")
                    .kind(),
                ErrorKind::InvalidInput,
                "{value}"
            );
        }
    }
}
