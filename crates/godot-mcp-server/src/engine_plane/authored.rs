//! Semantic scene and resource authoring: `scene.create`, `scene.batch.apply`,
//! `resource.create` and `resource.batch.apply`.
//!
//! One backend-neutral command model covers every edit: a batch of typed
//! operations — nodes, properties as tagged values, scripts, instances, signal
//! connections, groups and sub-resources. This module validates the batch's
//! *shape* (known operations, known fields, well-formed tagged values, project
//! paths); the bundled helper then applies it inside a supervised headless
//! Godot process against the engine's own class database and saves the
//! engine-canonical bytes to its staging area. The staged bytes are published
//! as one journaled file effect bound to a content precondition — absence for
//! a create, the caller's SHA-256 for a batch — so a stale or concurrently
//! changed target refuses without publishing anything.
//!
//! A node may be named by its path or by a revision-bound handle from a scene
//! tree answer. Handles are resolved against the scene's bytes before any
//! engine runs — a handle of older bytes refuses as a conflict — and the engine
//! then binds each one to its node object before the first operation, so a
//! handle keeps naming that node through renames and moves within the batch and
//! never names a replacement created at the same path.

use std::collections::{BTreeMap, BTreeSet};

use godot_mcp_analysis::project::ResourceTarget;
use godot_mcp_analysis::scene::{DocumentKind, ResourceRef, parse_document};
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{GodotValue, OutcomeError};
use godot_mcp_engine::supervision::journal::hex_sha256;
use godot_mcp_engine::validation::is_valid_project_relative_path;
use serde_json::{Map, Value, json};

use super::outcome_error;
use super::request::EngineOperation;
use crate::node_handle;

/// Most operations one batch may carry.
pub(super) const MAX_OPERATIONS: usize = 128;
/// Largest serialized batch passed to the engine.
const MAX_BATCH_BYTES: usize = 256 * 1024;
/// Longest accepted name, node path or class string.
const MAX_NAME_BYTES: usize = 512;
/// The operation fields that name a node of the edited scene.
const NODE_FIELDS: [&str; 4] = ["node", "parent", "from", "to"];

/// Which semantic authoring tool a request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AuthorKind {
    SceneCreate,
    SceneBatch,
    ResourceCreate,
    ResourceBatch,
}

impl AuthorKind {
    /// The engine operation that serves this kind of request.
    pub(super) const fn operation(self) -> EngineOperation {
        match self {
            Self::SceneCreate => EngineOperation::SceneCreate,
            Self::SceneBatch => EngineOperation::SceneBatch,
            Self::ResourceCreate => EngineOperation::ResourceCreate,
            Self::ResourceBatch => EngineOperation::ResourceBatch,
        }
    }

    /// The public name of the tool this kind of request is for.
    pub(super) fn tool_name(self) -> &'static str {
        self.operation().tool_name()
    }

    pub(super) const fn creates(self) -> bool {
        matches!(self, Self::SceneCreate | Self::ResourceCreate)
    }

    const fn is_scene(self) -> bool {
        matches!(self, Self::SceneCreate | Self::SceneBatch)
    }

    const fn extension(self) -> &'static str {
        if self.is_scene() { "tscn" } else { "tres" }
    }
}

/// One validated semantic authoring request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AuthorRequest {
    pub(super) kind: AuthorKind,
    /// Project-relative target, without `res://`.
    pub(super) target: String,
    pub(super) expected_sha256: Option<String>,
    pub(super) expected_revision: Option<u64>,
    pub(super) root_type: Option<String>,
    pub(super) root_name: Option<String>,
    pub(super) resource_class: Option<String>,
    /// The compact JSON array the engine applies.
    pub(super) batch: String,
    /// Every node handle the operations use, resolved before any engine runs.
    pub(super) handles: BTreeSet<String>,
    pub(super) dry_run: bool,
}

fn invalid(message: impl Into<String>) -> OutcomeError {
    outcome_error(ErrorKind::InvalidInput, message)
}

/// Every operation a scene batch accepts: its name, every field it declares,
/// and the fields it requires. The schema and the validator both read it.
const SCENE_OPERATIONS: &[(&str, &[&str], &[&str])] = &[
    ("add_node", &["parent", "name", "type"], &["name"]),
    (
        "instance_scene",
        &["parent", "name", "scene"],
        &["name", "scene"],
    ),
    ("remove_node", &["node"], &["node"]),
    ("rename_node", &["node", "name"], &["node", "name"]),
    (
        "move_node",
        &["node", "parent", "index"],
        &["node", "parent"],
    ),
    ("duplicate_node", &["node", "name"], &["node", "name"]),
    (
        "set_property",
        &["node", "property", "value"],
        &["property", "value"],
    ),
    ("clear_property", &["node", "property"], &["property"]),
    ("attach_script", &["node", "script"], &["script"]),
    ("detach_script", &["node"], &[]),
    ("replace_instance", &["node", "scene"], &["node", "scene"]),
    (
        "connect_signal",
        &["from", "signal", "to", "method"],
        &["from", "signal", "to", "method"],
    ),
    (
        "disconnect_signal",
        &["from", "signal", "to", "method"],
        &["from", "signal", "to", "method"],
    ),
    ("add_group", &["node", "group"], &["group"]),
    ("remove_group", &["node", "group"], &["group"]),
    (
        "set_subresource",
        &["node", "property", "class", "properties"],
        &["property", "class"],
    ),
];

/// Every operation a resource batch accepts, as [`SCENE_OPERATIONS`].
const RESOURCE_OPERATIONS: &[(&str, &[&str], &[&str])] = &[
    (
        "set_property",
        &["property", "value"],
        &["property", "value"],
    ),
    ("clear_property", &["property"], &["property"]),
    (
        "set_subresource",
        &["property", "class", "properties"],
        &["property", "class"],
    ),
];

const fn operations(
    scene: bool,
) -> &'static [(
    &'static str,
    &'static [&'static str],
    &'static [&'static str],
)] {
    if scene {
        SCENE_OPERATIONS
    } else {
        RESOURCE_OPERATIONS
    }
}

/// The schema of one operation field.
fn operation_field_schema(field: &str) -> Value {
    match field {
        "parent" | "node" | "from" | "to" => json!({
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_NAME_BYTES,
            "description": "A node path relative to the scene root (`.` is the root), or the \
                            handle a scene tree answer gave that node."
        }),
        "scene" => json!({
            "type": "string",
            "pattern": "^res://",
            "description": "A res:// path to a project .tscn or .scn scene."
        }),
        "script" => json!({
            "type": "string",
            "pattern": "^res://",
            "description": "A res:// path to a project .gd script."
        }),
        "index" => json!({ "type": "integer", "minimum": 0 }),
        "value" => crate::schema::writable_value(),
        "properties" => json!({
            "type": "object",
            "additionalProperties": crate::schema::writable_value(),
            "description": "Property names mapped to tagged values."
        }),
        _ => json!({ "type": "string", "minLength": 1, "maxLength": MAX_NAME_BYTES }),
    }
}

fn operations_schema(scene: bool) -> Value {
    let description = if scene {
        "Typed operations applied in order: add_node {parent?, name, type?}, instance_scene \
         {parent?, name, scene}, remove_node {node}, rename_node {node, name}, move_node {node, \
         parent, index?}, duplicate_node {node, name}, set_property {node?, property, value}, \
         clear_property {node?, property}, attach_script {node?, script}, detach_script {node?}, \
         replace_instance {node, scene}, connect_signal / disconnect_signal {from, signal, to, \
         method}, add_group / remove_group {node?, group}, set_subresource {node?, property, \
         class, properties?}. `node`, `parent`, `from` and `to` take a node path relative to the \
         scene root (`.` is the root) or the `handle` a scene tree answer gave that node; a \
         handle of older bytes refuses as a conflict. Values are tagged {type, value} objects."
    } else {
        "Typed operations applied in order: set_property {property, value}, clear_property \
         {property}, set_subresource {property, class, properties?}. Values are tagged {type, \
         value} objects."
    };
    json!({
        "type": "array",
        "minItems": 1,
        "maxItems": MAX_OPERATIONS,
        "description": description,
        "items": crate::schema::one_of(
            operations(scene)
                .iter()
                .map(|(op, fields, required)| {
                    let fields: Vec<(&str, Value)> = fields
                        .iter()
                        .map(|field| (*field, operation_field_schema(field)))
                        .collect();
                    crate::schema::variant("op", op, &fields, required)
                })
                .collect()
        )
    })
}

/// The strict input schema of one semantic authoring tool.
pub(super) fn input_schema(kind: AuthorKind) -> Map<String, Value> {
    let mut properties = Map::new();
    properties.insert(
        "path".to_owned(),
        json!({
            "type": "string",
            "minLength": 1,
            "maxLength": 1024,
            "description": if kind.is_scene() {
                "Project-relative .tscn path, optionally res://-prefixed."
            } else {
                "Project-relative .tres path, optionally res://-prefixed."
            }
        }),
    );
    let mut required = vec![json!("path"), json!("idempotencyId")];
    match kind {
        AuthorKind::SceneCreate => {
            properties.insert(
                "rootType".to_owned(),
                json!({
                    "type": "string",
                    "description": "Engine Node class of the root node (default Node)."
                }),
            );
            properties.insert(
                "rootName".to_owned(),
                json!({
                    "type": "string",
                    "description": "Name of the root node (default the file name)."
                }),
            );
            let mut operations = operations_schema(true);
            operations["minItems"] = json!(0);
            properties.insert("operations".to_owned(), operations);
        }
        AuthorKind::ResourceCreate => {
            properties.insert(
                "class".to_owned(),
                json!({
                    "type": "string",
                    "description": "Engine Resource class of the new resource (default Resource); \
                                    initial values are set_property operations."
                }),
            );
            let mut operations = operations_schema(false);
            operations["minItems"] = json!(0);
            properties.insert("operations".to_owned(), operations);
        }
        AuthorKind::SceneBatch | AuthorKind::ResourceBatch => {
            properties.insert(
                "expectedSha256".to_owned(),
                json!({
                    "type": "string",
                    "pattern": "^[0-9a-f]{64}$",
                    "description": "SHA-256 of the file's current bytes; a stale value refuses the batch."
                }),
            );
            properties.insert("operations".to_owned(), operations_schema(kind.is_scene()));
            required.push(json!("expectedSha256"));
            required.push(json!("operations"));
        }
    }
    properties.insert(
        "expectedRevision".to_owned(),
        json!({
            "type": "integer",
            "minimum": 0,
            "description": "Refuse unless the workspace is still at this revision."
        }),
    );
    properties.insert(
        "dryRun".to_owned(),
        json!({
            "type": "boolean",
            "description": "Run the engine and return the diff and tree without publishing."
        }),
    );
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": required,
        "$defs": crate::schema::value_definitions(),
    });
    schema
        .as_object()
        .cloned()
        .expect("authored tool schemas are JSON objects")
}

fn string_field<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<Option<&'a str>, OutcomeError> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::String(value)) if !value.is_empty() && value.len() <= MAX_NAME_BYTES => {
            if value.chars().any(char::is_control) {
                return Err(invalid(format!(
                    "{context}: `{key}` must not contain control characters"
                )));
            }
            Ok(Some(value))
        }
        Some(_) => Err(invalid(format!(
            "{context}: `{key}` must be a non-empty string of at most {MAX_NAME_BYTES} bytes"
        ))),
    }
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<&'a str, OutcomeError> {
    string_field(object, key, context)?
        .ok_or_else(|| invalid(format!("{context}: `{key}` is required")))
}

/// A node path relative to the scene root, or a node handle.
fn node_path(
    object: &Map<String, Value>,
    key: &str,
    context: &str,
    required: bool,
) -> Result<(), OutcomeError> {
    let Some(path) = string_field(object, key, context)? else {
        return if required {
            Err(invalid(format!("{context}: `{key}` is required")))
        } else {
            Ok(())
        };
    };
    if path.starts_with(node_handle::PREFIX) {
        if !node_handle::well_formed(path) {
            return Err(invalid(format!(
                "{context}: `{key}` is not a node handle a scene tree answer issued"
            )));
        }
        return Ok(());
    }
    if path.starts_with('/') || path.split('/').any(|part| part == ".." || part.is_empty()) {
        return Err(invalid(format!(
            "{context}: `{key}` must be a node path relative to the scene root"
        )));
    }
    Ok(())
}

/// A `res://` path to a project file with one of `extensions`.
fn resource_path(
    object: &Map<String, Value>,
    key: &str,
    context: &str,
    extensions: &[&str],
) -> Result<(), OutcomeError> {
    let path = required_string(object, key, context)?;
    let relative = path
        .strip_prefix("res://")
        .ok_or_else(|| invalid(format!("{context}: `{key}` must be a res:// path")))?;
    let extension = relative
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    if !is_valid_project_relative_path(relative) || !extensions.contains(&extension.as_str()) {
        return Err(invalid(format!(
            "{context}: `{key}` must name a project .{} file",
            extensions.join(" / .")
        )));
    }
    Ok(())
}

/// A writable tagged value: any contract value except a live object reference,
/// declaring no field beside `type` and `value` at any depth.
fn tagged_value(value: &Value, context: &str) -> Result<(), OutcomeError> {
    /// Whether a tagged value, or one nested in it, declares another field.
    fn undeclared(value: &Value) -> bool {
        let Some(object) = value.as_object() else {
            return false;
        };
        object.keys().any(|key| key != "type" && key != "value")
            || match (object.get("type"), object.get("value")) {
                (Some(kind), Some(Value::Array(items))) if kind == "array" => {
                    items.iter().any(undeclared)
                }
                (Some(kind), Some(Value::Object(entries))) if kind == "dictionary" => {
                    entries.values().any(undeclared)
                }
                _ => false,
            }
    }
    if undeclared(value) {
        return Err(invalid(format!(
            "{context}: a tagged value declares only `type` and `value`"
        )));
    }
    let parsed: GodotValue = serde_json::from_value(value.clone())
        .map_err(|error| invalid(format!("{context}: not a tagged value: {error}")))?;
    fn writable(value: &GodotValue) -> bool {
        match value {
            GodotValue::Object(_) => false,
            GodotValue::Array(items) => items.iter().all(writable),
            GodotValue::Dictionary(entries) => entries.values().all(writable),
            _ => true,
        }
    }
    if !writable(&parsed) {
        return Err(invalid(format!(
            "{context}: a live object reference cannot be written; name a resource by its res:// path"
        )));
    }
    Ok(())
}

fn property_map(object: &Map<String, Value>, key: &str, context: &str) -> Result<(), OutcomeError> {
    match object.get(key) {
        None => Ok(()),
        Some(Value::Object(properties)) => {
            for (name, value) in properties {
                if name.is_empty() || name.len() > MAX_NAME_BYTES {
                    return Err(invalid(format!(
                        "{context}: property names must be non-empty"
                    )));
                }
                tagged_value(value, &format!("{context}.{name}"))?;
            }
            Ok(())
        }
        Some(_) => Err(invalid(format!("{context}: `{key}` must be an object"))),
    }
}

fn validate_operation(scene: bool, operation: &Value, index: usize) -> Result<(), OutcomeError> {
    let context = format!("operation {index}");
    let object = operation
        .as_object()
        .ok_or_else(|| invalid(format!("{context} must be an object")))?;
    let op = required_string(object, "op", &context)?;
    let context = format!("operation {index} ({op})");
    let Some((_, allowed, _)) = operations(scene).iter().find(|(name, _, _)| *name == op) else {
        return Err(invalid(format!(
            "{context}: unknown {} operation",
            if scene { "scene" } else { "resource" }
        )));
    };
    if let Some(unknown) = object
        .keys()
        .find(|key| *key != "op" && !allowed.contains(&key.as_str()))
    {
        return Err(invalid(format!("{context} does not accept `{unknown}`")));
    }
    match op {
        "add_node" => {
            node_path(object, "parent", &context, false)?;
            required_string(object, "name", &context)?;
            string_field(object, "type", &context)?;
        }
        "instance_scene" => {
            node_path(object, "parent", &context, false)?;
            required_string(object, "name", &context)?;
            resource_path(object, "scene", &context, &["tscn", "scn"])?;
        }
        "remove_node" | "detach_script" => {
            node_path(object, "node", &context, op == "remove_node")?
        }
        "rename_node" | "duplicate_node" => {
            node_path(object, "node", &context, true)?;
            required_string(object, "name", &context)?;
        }
        "move_node" => {
            node_path(object, "node", &context, true)?;
            node_path(object, "parent", &context, true)?;
            if let Some(position) = object.get("index")
                && position.as_u64().is_none()
            {
                return Err(invalid(format!(
                    "{context}: `index` must be a non-negative integer"
                )));
            }
        }
        "set_property" => {
            if scene {
                node_path(object, "node", &context, false)?;
            }
            required_string(object, "property", &context)?;
            let value = object
                .get("value")
                .ok_or_else(|| invalid(format!("{context}: `value` is required")))?;
            tagged_value(value, &context)?;
        }
        "clear_property" => {
            if scene {
                node_path(object, "node", &context, false)?;
            }
            required_string(object, "property", &context)?;
        }
        "attach_script" => {
            node_path(object, "node", &context, false)?;
            resource_path(object, "script", &context, &["gd"])?;
        }
        "replace_instance" => {
            node_path(object, "node", &context, true)?;
            resource_path(object, "scene", &context, &["tscn", "scn"])?;
        }
        "connect_signal" | "disconnect_signal" => {
            node_path(object, "from", &context, true)?;
            node_path(object, "to", &context, true)?;
            required_string(object, "signal", &context)?;
            required_string(object, "method", &context)?;
        }
        "add_group" | "remove_group" => {
            node_path(object, "node", &context, false)?;
            required_string(object, "group", &context)?;
        }
        "set_subresource" => {
            if scene {
                node_path(object, "node", &context, false)?;
            }
            required_string(object, "property", &context)?;
            required_string(object, "class", &context)?;
            property_map(object, "properties", &context)?;
        }
        _ => unreachable!("every allowed operation is validated above"),
    }
    Ok(())
}

/// Parse and validate one semantic authoring request.
///
/// # Errors
/// Returns [`ErrorKind::InvalidInput`] for an unknown field, a malformed or
/// unknown operation, an invalid tagged value or a path that is not a project
/// text scene/resource of the tool's kind.
pub(super) fn parse(
    kind: AuthorKind,
    arguments: Map<String, Value>,
) -> Result<AuthorRequest, OutcomeError> {
    let tool = kind.tool_name();
    if let Some(key) = arguments
        .iter()
        .find_map(|(key, value)| value.is_null().then_some(key))
    {
        return Err(invalid(format!(
            "invalid engine request: `{key}` must not be null"
        )));
    }
    let accepted: &[&str] = match kind {
        AuthorKind::SceneCreate => &[
            "path",
            "rootType",
            "rootName",
            "operations",
            "expectedRevision",
            "dryRun",
        ],
        AuthorKind::ResourceCreate => {
            &["path", "class", "operations", "expectedRevision", "dryRun"]
        }
        AuthorKind::SceneBatch | AuthorKind::ResourceBatch => &[
            "path",
            "expectedSha256",
            "operations",
            "expectedRevision",
            "dryRun",
        ],
    };
    if let Some(unknown) = arguments
        .keys()
        .find(|key| !accepted.contains(&key.as_str()))
    {
        return Err(invalid(format!(
            "invalid engine request: unknown field `{unknown}`, expected one of {}",
            accepted.join(", ")
        )));
    }
    let raw_path = arguments
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("{tool}: `path` is required")))?;
    let target = raw_path
        .strip_prefix("res://")
        .unwrap_or(raw_path)
        .to_owned();
    let extension = target
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    if !is_valid_project_relative_path(&target) || target.starts_with('/') {
        return Err(invalid(format!("{tool}: `path` must be project-relative")));
    }
    if extension != kind.extension() {
        return Err(invalid(format!(
            "{tool}: `path` must end in .{}, not .{extension}",
            kind.extension()
        )));
    }
    let expected_sha256 = match arguments.get("expectedSha256") {
        None => None,
        Some(Value::String(value))
            if value.len() == 64
                && value
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')) =>
        {
            Some(value.clone())
        }
        Some(_) => {
            return Err(invalid(format!(
                "{tool}: `expectedSha256` must be a lowercase 64-character hex SHA-256"
            )));
        }
    };
    if !kind.creates() && expected_sha256.is_none() {
        return Err(invalid(format!("{tool}: `expectedSha256` is required")));
    }
    let expected_revision = match arguments.get("expectedRevision") {
        None => None,
        Some(value) => Some(
            crate::arguments::non_negative_whole_u64(value).ok_or_else(|| {
                invalid(format!(
                    "{tool}: `expectedRevision` must be a non-negative integer"
                ))
            })?,
        ),
    };
    let dry_run = match arguments.get("dryRun") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(invalid(format!("{tool}: `dryRun` must be a boolean"))),
    };

    let mut operations: Vec<Value> = Vec::new();
    let root_type = string_field(&arguments, "rootType", tool)?.map(str::to_owned);
    let mut root_name = string_field(&arguments, "rootName", tool)?.map(str::to_owned);
    let resource_class = string_field(&arguments, "class", tool)?.map(str::to_owned);
    if root_name.is_none() && kind == AuthorKind::SceneCreate {
        root_name = target
            .rsplit('/')
            .next()
            .and_then(|name| name.rsplit_once('.'))
            .map(|(stem, _)| stem.to_owned());
    }
    match arguments.get("operations") {
        None if kind.creates() => {}
        None => return Err(invalid(format!("{tool}: `operations` is required"))),
        Some(Value::Array(listed)) => {
            if (!kind.creates() && listed.is_empty()) || listed.len() > MAX_OPERATIONS {
                return Err(invalid(format!(
                    "{tool}: `operations` must hold between {} and {MAX_OPERATIONS} operations",
                    usize::from(!kind.creates())
                )));
            }
            for (index, operation) in listed.iter().enumerate() {
                validate_operation(kind.is_scene(), operation, index)?;
            }
            operations.extend(listed.iter().cloned());
        }
        Some(_) => return Err(invalid(format!("{tool}: `operations` must be an array"))),
    }
    let handles: BTreeSet<String> = operations
        .iter()
        .filter_map(Value::as_object)
        .flat_map(|operation| NODE_FIELDS.iter().filter_map(|key| operation.get(*key)))
        .filter_map(Value::as_str)
        .filter(|value| value.starts_with(node_handle::PREFIX))
        .map(str::to_owned)
        .collect();
    if kind.creates() && !handles.is_empty() {
        return Err(invalid(format!(
            "{tool}: a node handle names a node of an existing scene; refer to the nodes of a \
             scene being created by their paths"
        )));
    }
    if operations.len() > MAX_OPERATIONS {
        return Err(invalid(format!(
            "{tool}: at most {MAX_OPERATIONS} operations in total"
        )));
    }
    let batch = serde_json::to_string(&Value::Array(operations))
        .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
    if batch.len() > MAX_BATCH_BYTES {
        return Err(invalid(format!(
            "{tool}: the batch exceeds {MAX_BATCH_BYTES} bytes"
        )));
    }
    Ok(AuthorRequest {
        kind,
        target,
        expected_sha256,
        expected_revision,
        root_type,
        root_name,
        resource_class,
        batch,
        handles,
        dry_run,
    })
}

/// Resolve every handle in `used` to the path of the node it names in the
/// scene at `target` whose current bytes are `bytes`.
///
/// # Errors
/// Returns [`ErrorKind::Conflict`] for a handle that names no node of these
/// bytes: it was issued for an older revision of the scene, or for another
/// scene. The caller re-reads the scene tree for current handles.
pub(super) fn bind_handles(
    target: &str,
    bytes: &[u8],
    generation: u64,
    used: &BTreeSet<String>,
) -> Result<BTreeMap<String, String>, OutcomeError> {
    let sha256 = hex_sha256(bytes);
    let mut bound = BTreeMap::new();
    // Handles are only ever issued for a scene that parses as text.
    if let Some(document) = std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| parse_document(target, text, generation).ok())
    {
        for path in document.nodes().iter().filter_map(|node| node.node_path()) {
            let handle = node_handle::mint(target, path, &sha256);
            if used.contains(&handle) {
                bound.insert(handle, path.to_owned());
            }
        }
    }
    if let Some(stale) = used.iter().find(|handle| !bound.contains_key(*handle)) {
        let mut details = BTreeMap::new();
        details.insert("path".to_owned(), GodotValue::String(target.to_owned()));
        details.insert("handle".to_owned(), GodotValue::String(stale.clone()));
        details.insert(
            "current_sha256".to_owned(),
            GodotValue::String(sha256.clone()),
        );
        return Err(OutcomeError::new(
            ErrorKind::Conflict,
            format!(
                "the node handle {stale} is stale: no node of {target} as it is now (sha256 \
                 {sha256}) carries it; read the scene tree again for current handles"
            ),
            false,
            details,
        ));
    }
    Ok(bound)
}

fn ref_path(reference: Option<&ResourceRef>) -> Value {
    match reference.and_then(ResourceRef::target) {
        Some(ResourceTarget::ProjectRelative { res_uri, .. }) => Value::String(res_uri.clone()),
        Some(ResourceTarget::Uid(uid)) => Value::String(uid.clone()),
        Some(ResourceTarget::Unsupported(value)) => Value::String(value.clone()),
        None => Value::Null,
    }
}

/// The static parser's view of a text scene or resource: the semantic tree a
/// reader of the file sees, independent of the engine that wrote it.
pub(super) fn semantic_tree(path: &str, text: &str, generation: u64) -> Value {
    let document = match parse_document(path, text, generation) {
        Ok(document) => document,
        Err(error) => return json!({ "kind": "unparsed", "reason": error.to_string() }),
    };
    let sha256 = hex_sha256(text.as_bytes());
    match document.kind() {
        DocumentKind::Scene => json!({
            "kind": "scene",
            "uid": document.self_uid(),
            "nodes": document.nodes().iter().map(|node| json!({
                "path": node.node_path(),
                "handle": node.node_path().map(|node_path| node_handle::mint(path, node_path, &sha256)),
                "type": node.type_name(),
                "instance": ref_path(node.instance()),
                "script": ref_path(node.script()),
                "groups": node.groups().iter().map(|group| group.name()).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "connections": document.connections().iter().map(|connection| json!({
                "signal": connection.signal(),
                "from": connection.from_path(),
                "to": connection.to_path(),
                "method": connection.method(),
            })).collect::<Vec<_>>(),
            "sub_resources": document.sub_resources().iter().map(|sub| sub.type_name()).collect::<Vec<_>>(),
            "diagnostics": document.diagnostics().len(),
        }),
        DocumentKind::Resource => json!({
            "kind": "resource",
            "uid": document.self_uid(),
            "type": document.root_type(),
            "script": ref_path(document.resource_body().and_then(|body| body.script())),
            "properties": document
                .resource_body()
                .map(|body| body.properties().iter().map(|property| property.key()).collect::<Vec<_>>())
                .unwrap_or_default(),
            "sub_resources": document.sub_resources().iter().map(|sub| sub.type_name()).collect::<Vec<_>>(),
            "diagnostics": document.diagnostics().len(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The schema and the validator read one table: every operation with only
    /// its required fields validates, and no operation takes a field it does
    /// not declare.
    #[test]
    fn every_operation_the_schema_lists_validates_with_its_required_fields() {
        fn sample(field: &str) -> Value {
            match field {
                "parent" | "node" | "from" | "to" => json!("."),
                "scene" => json!("res://scenes/a.tscn"),
                "script" => json!("res://scripts/a.gd"),
                "index" => json!(0),
                "value" => json!({ "type": "integer", "value": 1 }),
                "properties" => json!({}),
                _ => json!("Name"),
            }
        }
        for scene in [true, false] {
            for (op, _, required) in operations(scene) {
                let mut operation = Map::new();
                operation.insert("op".to_owned(), json!(op));
                for field in *required {
                    operation.insert((*field).to_owned(), sample(field));
                }
                let operation = Value::Object(operation);
                assert!(
                    validate_operation(scene, &operation, 0).is_ok(),
                    "{operation}"
                );
                let mut extra = operation.clone();
                extra["colour"] = json!("red");
                assert!(validate_operation(scene, &extra, 0).is_err(), "{extra}");
            }
        }
    }

    /// The schema a batch advertises for one written value, as a standalone
    /// schema carrying the definitions it refers to.
    fn advertised_value_schema(kind: AuthorKind) -> Value {
        let root = Value::Object(input_schema(kind));
        let variants = root["properties"]["operations"]["items"]["oneOf"]
            .as_array()
            .expect("operation variants");
        let variant = |op: &str| {
            variants
                .iter()
                .find(|variant| variant["properties"]["op"]["const"] == json!(op))
                .unwrap_or_else(|| panic!("{op} is an operation"))
        };
        let value = variant("set_property")["properties"]["value"].clone();
        assert_eq!(
            variant("set_subresource")["properties"]["properties"]["additionalProperties"],
            value,
            "a sub-resource property takes the same values as set_property"
        );
        let mut schema = json!({ "allOf": [value] });
        if let Some(definitions) = root.get("$defs") {
            schema["$defs"] = definitions.clone();
        }
        schema
    }

    /// The value schema and the value check give the same verdict on every
    /// case, and it is the intended one: a closed set of variant names, each
    /// with its own payload shape, no undeclared field at any depth and never
    /// a live object reference, however deeply nested.
    #[test]
    fn the_value_schema_and_the_value_check_agree() {
        let object =
            json!({ "type": "object", "value": { "id": "object_1", "class_name": "Node" } });
        let identity = json!([[1, 0, 0, 0], [0, 1, 0, 0], [0, 0, 1, 0], [0, 0, 0, 1]]);
        let resource = |path: &str| json!({ "type": "resource", "value": { "resource": path, "resource_type": "Texture2D" } });
        let cases = [
            (json!({ "type": "null" }), true),
            (json!({ "type": "null", "value": null }), true),
            (json!({ "type": "bool", "value": true }), true),
            (json!({ "type": "integer", "value": -3 }), true),
            (json!({ "type": "float", "value": 1.5 }), true),
            (json!({ "type": "float", "value": 2 }), true),
            (json!({ "type": "string", "value": "text" }), true),
            (json!({ "type": "string_name", "value": "name" }), true),
            (
                json!({ "type": "node_path", "value": "Player/Sprite" }),
                true,
            ),
            (
                json!({ "type": "vector2", "value": { "x": 1, "y": 2.5 } }),
                true,
            ),
            (
                json!({ "type": "vector2i", "value": { "x": 1, "y": 2 } }),
                true,
            ),
            (
                json!({ "type": "rect2", "value": { "position": { "x": 0, "y": 0 }, "size": { "x": 1, "y": 1 } } }),
                true,
            ),
            (
                json!({ "type": "transform2_d", "value": { "x": { "x": 1, "y": 0 }, "y": { "x": 0, "y": 1 }, "origin": { "x": 0, "y": 0 } } }),
                true,
            ),
            (
                json!({ "type": "color", "value": { "red": 1, "green": 0, "blue": 0, "alpha": 1 } }),
                true,
            ),
            (
                json!({ "type": "projection", "value": { "matrix": { "rows": identity } } }),
                true,
            ),
            (
                json!({ "type": "array", "value": [{ "type": "integer", "value": 1 }] }),
                true,
            ),
            (
                json!({ "type": "dictionary", "value": { "key": { "type": "bool", "value": false } } }),
                true,
            ),
            (
                json!({ "type": "packed_byte_array", "value": [0, 255] }),
                true,
            ),
            (
                json!({ "type": "packed_int32_array", "value": [2_147_483_647, -2_147_483_648_i64] }),
                true,
            ),
            (
                json!({ "type": "packed_string_array", "value": ["a"] }),
                true,
            ),
            (
                json!({ "type": "packed_vector2_array", "value": [{ "x": 1, "y": 2 }] }),
                true,
            ),
            (resource("res://art/icon.png"), true),
            (
                json!({ "type": "resource", "value": { "resource": "res://a.tres", "resource_type": "Resource", "uid": null } }),
                true,
            ),
            (
                json!({ "type": "resource", "value": { "resource": "res://a.tres", "resource_type": "Resource", "uid": "uid://b1" } }),
                true,
            ),
            (json!({ "type": "widget", "value": 1 }), false),
            (json!({ "type": "transform2d", "value": {} }), false),
            (json!({ "value": 1 }), false),
            (json!({ "type": "integer" }), false),
            (json!({ "type": "integer", "value": "1" }), false),
            (
                json!({ "type": "integer", "value": 1, "extra": true }),
                false,
            ),
            (json!({ "type": "bool", "value": 1 }), false),
            (json!({ "type": "null", "value": 5 }), false),
            (json!({ "type": "vector2", "value": { "x": 1 } }), false),
            (
                json!({ "type": "vector2", "value": { "x": 1, "y": 2, "z": 3 } }),
                false,
            ),
            (
                json!({ "type": "vector2i", "value": { "x": 1.5, "y": 2 } }),
                false,
            ),
            (
                json!({ "type": "packed_byte_array", "value": [256] }),
                false,
            ),
            (
                json!({ "type": "packed_int32_array", "value": [2_147_483_648_i64] }),
                false,
            ),
            (
                json!({ "type": "projection", "value": { "matrix": { "rows": [[1, 0, 0], [0, 1, 0, 0], [0, 0, 1, 0], [0, 0, 0, 1]] } } }),
                false,
            ),
            (object.clone(), false),
            (json!({ "type": "array", "value": [object.clone()] }), false),
            (
                json!({ "type": "dictionary", "value": { "key": object.clone() } }),
                false,
            ),
            (
                json!({ "type": "array", "value": [{ "type": "integer", "value": 1, "extra": 1 }] }),
                false,
            ),
            (resource("/etc/passwd"), false),
            (resource("res://"), false),
            (resource("res://../outside.tres"), false),
            (resource("res://a//b.tres"), false),
            (resource("res://./a.tres"), false),
            (resource("res://a\\b.tres"), false),
            (resource("res:///abs.tres"), false),
            (
                json!({ "type": "resource", "value": { "resource": "res://a.tres" } }),
                false,
            ),
            (json!("integer"), false),
            (json!(5), false),
        ];
        for kind in [AuthorKind::SceneBatch, AuthorKind::ResourceBatch] {
            let schema = advertised_value_schema(kind);
            for (case, expected) in &cases {
                assert_eq!(
                    crate::schema::conformance::conforms(&schema, case),
                    *expected,
                    "the {kind:?} schema's verdict on {case}"
                );
                assert_eq!(
                    tagged_value(case, "case").is_ok(),
                    *expected,
                    "the {kind:?} handler's verdict on {case}"
                );
            }
        }
    }

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().cloned().expect("object")
    }

    #[test]
    fn a_scene_batch_is_validated_before_any_engine_runs() {
        let request = parse(
            AuthorKind::SceneBatch,
            arguments(json!({
                "path": "res://scenes/main.tscn",
                "expectedSha256": "0".repeat(64),
                "operations": [
                    { "op": "add_node", "parent": ".", "name": "Player", "type": "CharacterBody2D" },
                    { "op": "set_property", "node": "Player", "property": "position",
                      "value": { "type": "vector2", "value": { "x": 1.0, "y": 2.0 } } },
                    { "op": "connect_signal", "from": "Player", "signal": "ready", "to": ".", "method": "_on_ready" }
                ]
            })),
        )
        .expect("a valid batch");
        assert_eq!(request.target, "scenes/main.tscn");
        assert!(
            request.batch.contains("\"op\":\"add_node\""),
            "{}",
            request.batch
        );
        assert!(!request.batch.contains('\n'));

        for (operations, fragment) in [
            (json!([{ "op": "explode" }]), "unknown scene operation"),
            (
                json!([{ "op": "add_node", "name": "X", "colour": "red" }]),
                "does not accept `colour`",
            ),
            (
                json!([{ "op": "set_property", "property": "p", "value": 3 }]),
                "not a tagged value",
            ),
            (
                json!([{ "op": "remove_node", "node": "../escape" }]),
                "relative to the scene root",
            ),
            (
                json!([{ "op": "attach_script", "script": "scripts/a.gd" }]),
                "res:// path",
            ),
            (
                json!([{ "op": "attach_script", "script": "res://a.txt" }]),
                ".gd",
            ),
            (
                json!([{ "op": "set_property", "property": "p",
                      "value": { "type": "object", "value": { "id": "obj_x", "class_name": "Node" } } }]),
                "not a tagged value",
            ),
        ] {
            let error = parse(
                AuthorKind::SceneBatch,
                arguments(json!({
                    "path": "scenes/main.tscn",
                    "expectedSha256": "0".repeat(64),
                    "operations": operations
                })),
            )
            .expect_err("refused");
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
            assert!(error.message().contains(fragment), "{}", error.message());
        }
    }

    #[test]
    fn creation_defaults_and_kinds_are_enforced() {
        let scene = parse(
            AuthorKind::SceneCreate,
            arguments(json!({ "path": "levels/one.tscn", "rootType": "Node3D" })),
        )
        .expect("scene create");
        assert_eq!(scene.root_name.as_deref(), Some("one"));
        assert_eq!(scene.batch, "[]");
        let resource = parse(
            AuthorKind::ResourceCreate,
            arguments(json!({
                "path": "themes/box.tres",
                "class": "StyleBoxFlat",
                "operations": [{ "op": "set_property", "property": "bg_color", "value": {
                    "type": "color",
                    "value": { "red": 1.0, "green": 0.0, "blue": 0.0, "alpha": 1.0 } } }]
            })),
        )
        .expect("resource create");
        assert!(resource.batch.contains("\"op\":\"set_property\""));
        let wrong = parse(
            AuthorKind::ResourceCreate,
            arguments(json!({ "path": "themes/box.tscn" })),
        )
        .expect_err("a resource tool writes .tres");
        assert!(wrong.message().contains(".tres"));
        let missing = parse(
            AuthorKind::ResourceBatch,
            arguments(json!({ "path": "themes/box.tres", "operations": [] })),
        )
        .expect_err("a batch needs its precondition");
        assert!(missing.message().contains("expectedSha256"));
    }

    #[test]
    fn node_handles_are_collected_validated_and_refused_on_create() {
        let handle = crate::node_handle::mint("scenes/main.tscn", "Player", &"a".repeat(64));
        let request = parse(
            AuthorKind::SceneBatch,
            arguments(json!({
                "path": "scenes/main.tscn",
                "expectedSha256": "0".repeat(64),
                "operations": [
                    { "op": "rename_node", "node": handle, "name": "Hero" },
                    { "op": "connect_signal", "from": handle, "signal": "ready", "to": ".",
                      "method": "_on_ready" }
                ]
            })),
        )
        .expect("a handle names a node");
        assert_eq!(request.handles.iter().collect::<Vec<_>>(), vec![&handle]);

        let malformed = parse(
            AuthorKind::SceneBatch,
            arguments(json!({
                "path": "scenes/main.tscn",
                "expectedSha256": "0".repeat(64),
                "operations": [{ "op": "remove_node", "node": "nh1:xyz" }]
            })),
        )
        .expect_err("a malformed handle");
        assert_eq!(malformed.kind(), ErrorKind::InvalidInput);
        assert!(
            malformed.message().contains("node handle"),
            "{}",
            malformed.message()
        );

        let create = parse(
            AuthorKind::SceneCreate,
            arguments(json!({
                "path": "scenes/new.tscn",
                "operations": [{ "op": "add_group", "node": handle, "group": "g" }]
            })),
        )
        .expect_err("a new scene has no nodes a handle could name");
        assert_eq!(create.kind(), ErrorKind::InvalidInput);
        assert!(
            create.message().contains("node handle"),
            "{}",
            create.message()
        );
    }

    #[test]
    fn handles_bind_only_to_the_bytes_they_were_minted_from() {
        let text = "[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node2D\"]\n\n\
                    [node name=\"Player\" type=\"Node2D\" parent=\".\"]\n";
        let tree = semantic_tree("scenes/main.tscn", text, 1);
        let player = tree["nodes"][1]["handle"]
            .as_str()
            .expect("every node carries a handle")
            .to_owned();
        assert_eq!(
            player,
            crate::node_handle::mint(
                "scenes/main.tscn",
                "Player",
                &godot_mcp_engine::supervision::journal::hex_sha256(text.as_bytes())
            )
        );
        let used = std::collections::BTreeSet::from([player.clone()]);
        let bound = bind_handles("scenes/main.tscn", text.as_bytes(), 1, &used).expect("fresh");
        assert_eq!(bound.get(&player).map(String::as_str), Some("Player"));

        let changed = text.replace("Node2D\" parent", "Sprite2D\" parent");
        let stale =
            bind_handles("scenes/main.tscn", changed.as_bytes(), 1, &used).expect_err("stale");
        assert_eq!(stale.kind(), ErrorKind::Conflict);
        assert!(stale.message().contains("stale"), "{}", stale.message());
        let foreign =
            bind_handles("scenes/other.tscn", text.as_bytes(), 1, &used).expect_err("foreign");
        assert_eq!(foreign.kind(), ErrorKind::Conflict);
    }

    #[test]
    fn the_semantic_tree_reads_nodes_scripts_groups_and_connections() {
        let text = "[gd_scene load_steps=2 format=3 uid=\"uid://abc\"]\n\n\
            [ext_resource type=\"Script\" path=\"res://scripts/player.gd\" id=\"1\"]\n\n\
            [node name=\"World\" type=\"Node2D\"]\n\n\
            [node name=\"Player\" type=\"CharacterBody2D\" parent=\".\" groups=[\"players\"]]\n\
            script = ExtResource(\"1\")\n\n\
            [connection signal=\"ready\" from=\"Player\" to=\".\" method=\"_on_ready\"]\n";
        let tree = semantic_tree("scenes/world.tscn", text, 1);
        assert_eq!(tree["kind"], "scene");
        assert_eq!(tree["uid"], "uid://abc");
        assert_eq!(tree["nodes"][1]["path"], "Player");
        assert_eq!(tree["nodes"][1]["script"], "res://scripts/player.gd");
        assert_eq!(tree["nodes"][1]["groups"], json!(["players"]));
        assert_eq!(tree["connections"][0]["method"], "_on_ready");
    }
}
