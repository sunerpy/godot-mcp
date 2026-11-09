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
use godot_mcp_domain::editing_methods::{
    EDITING_METHODS, MAX_CALL_ARGUMENTS, MAX_CLASS_NAME_BYTES, MAX_REPEATED_CALLS,
    NEW_RESOURCE_FORBIDDEN, is_class_name, method_names,
};
use godot_mcp_engine::supervision::journal::hex_sha256;
use godot_mcp_engine::validation::is_valid_project_relative_path;
use serde_json::{Map, Value, json};

use super::outcome_error;
use super::request::EngineOperation;
use crate::node_handle;

/// Most operations one batch may carry.
pub(super) const MAX_OPERATIONS: usize = 128;
/// Largest serialized batch passed to the engine: a `repeat` of 4096 calls
/// with three tagged arguments each fits.
const MAX_BATCH_BYTES: usize = 1024 * 1024;
/// Longest accepted name, node path, property path, class string or
/// `res://` path, in characters: Unicode scalar values, as a schema's
/// `maxLength` counts them.
const MAX_NAME_CHARS: usize = 512;
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
    /// The recipe a `scene.recipe.apply` request expanded into this batch, or
    /// a `resource.create` request into this new resource.
    pub(super) recipe: Option<RecipeApplied>,
}

/// What a recipe adds to the answer of the request it expanded into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RecipeApplied {
    /// The answer's `recipe`: the recipe, its parent for a scene recipe, and
    /// its resolved parameters.
    pub(super) described: Value,
    /// The answer's `operations`: the operations the recipe expanded into.
    pub(super) operations: Value,
}

impl AuthorRequest {
    /// The public tool this request came through: the authoring tool of its
    /// kind, or `scene.recipe.apply` for a scene batch a recipe made. A
    /// resource recipe comes through `resource.create` itself. The journal,
    /// the idempotency record, the transaction id and the editor's undo label
    /// all name it.
    pub(super) fn tool(&self) -> &'static str {
        if self.recipe.is_some() && self.kind == AuthorKind::SceneBatch {
            godot_mcp_domain::recipes::RECIPE_TOOL
        } else {
            self.kind.tool_name()
        }
    }

    /// Add what the request's recipe contributes to its answer.
    pub(super) fn annotate(&self, result: &mut Map<String, Value>) {
        if let Some(recipe) = &self.recipe {
            result.insert("recipe".to_owned(), recipe.described.clone());
            result.insert("operations".to_owned(), recipe.operations.clone());
        }
    }
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
    (
        "call_method",
        &["node", "resource", "method", "arguments", "repeat"],
        &["method"],
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
    (
        "call_method",
        &["resource", "method", "arguments", "repeat"],
        &["method"],
    ),
];

/// The resource a scene's `call_method` calls, instead of the node itself.
const RESOURCE_SELECTORS: [&str; 3] = ["property", "animation", "library"];
/// The scene files a scene batch instances: text and binary scenes, and the
/// 3D model formats Godot imports as scenes.
const SCENE_EXTENSIONS: [&str; 8] = ["tscn", "scn", "glb", "gltf", "fbx", "blend", "dae", "obj"];
/// A property path: property names separated by `:`, none of them empty,
/// without a control character.
const PROPERTY_PATH_PATTERN: &str = crate::schema::PROPERTY_PATH_PATTERN;

/// The classes of the editing-method table, for descriptions.
fn editing_classes() -> String {
    let classes: Vec<&str> = EDITING_METHODS.iter().map(|class| class.class).collect();
    match classes.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
        _ => classes.join(""),
    }
}

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

/// A `res://` path to a project file with one of `extensions`, in any letter
/// case, as [`resource_path`] checks one: what follows `res://` is a
/// project-relative path by the rules of
/// [`godot_mcp_engine::validation::PROJECT_RELATIVE_PATH_PATTERN`], and no
/// character is a control character. The pattern spells "any character" as
/// "any character but a control character", so a client reads it as the
/// check does whatever line separators the path holds.
fn resource_path_pattern(extensions: &[&str]) -> String {
    let any = format!("[^{}]", crate::schema::CONTROL);
    let extensions: Vec<String> = extensions
        .iter()
        .map(|extension| crate::schema::caseless(extension))
        .collect();
    format!(
        r"^res://(?={any}*\.(?:{})$)(?:res://)?(?!(?:/|-|[A-Za-z]:))(?!(?:\.\.(?:/|$)|{any}*/\.\.(?:/|$)))(?!{any}*(?:\\|://)){any}+$",
        extensions.join("|")
    )
}

/// The schema of one field of operation `op` of a scene (`scene`) or
/// resource batch; a `paths_only` batch (a live edit, or a new scene, whose
/// nodes no scene tree answer has handed out yet) names nodes by path only.
fn operation_field_schema(op: &str, field: &str, scene: bool, paths_only: bool) -> Value {
    match field {
        "parent" | "node" | "from" | "to" => {
            let path = crate::schema::relative_node_path(crate::schema::CONTROL);
            let prefix = node_handle::PREFIX;
            let (pattern, description) = if paths_only {
                (
                    format!("^(?!{prefix}){path}$"),
                    "A node path relative to the scene root (`.` is the root).",
                )
            } else {
                (
                    format!("^(?:{}|(?!{prefix}){path})$", node_handle::pattern()),
                    "A node path relative to the scene root (`.` is the root), or the handle a \
                     scene tree answer gave that node.",
                )
            };
            json!({
                "type": "string",
                "minLength": 1,
                "maxLength": MAX_NAME_CHARS,
                "pattern": pattern,
                "description": description
            })
        }
        "scene" => json!({
            "type": "string",
            "maxLength": MAX_NAME_CHARS,
            "pattern": resource_path_pattern(&SCENE_EXTENSIONS),
            "description": "A res:// path to a project scene: a .tscn or .scn file, or a 3D \
                            model Godot imports as a scene (.glb, .gltf, .fbx, .blend, .dae, \
                            .obj), instanced whole."
        }),
        "property" => json!({
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_NAME_CHARS,
            "pattern": PROPERTY_PATH_PATTERN,
            "description": "A property, or a property path: names separated by `:` pass \
                            through properties holding resources the file embeds \
                            (`material:albedo_color`, `tree_root:states/Run/node`), and \
                            what follows the last one may name a component of its value \
                            (`position:x`)."
        }),
        "script" => json!({
            "type": "string",
            "maxLength": MAX_NAME_CHARS,
            "pattern": resource_path_pattern(&["gd"]),
            "description": "A res:// path to a project .gd script."
        }),
        "index" => json!({ "type": "integer", "minimum": 0 }),
        "value" => crate::schema::writable_value(),
        "properties" => json!({
            "type": "object",
            "propertyNames": crate::schema::property_name(),
            "additionalProperties": crate::schema::writable_value(),
            "description": "Property names mapped to tagged values."
        }),
        "resource" if !scene => json!({
            "type": "object",
            "minProperties": 1,
            "maxProperties": 1,
            "additionalProperties": false,
            "properties": {
                "property": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_NAME_CHARS,
                    "pattern": PROPERTY_PATH_PATTERN,
                    "description": "The embedded resource a property, or a property path \
                                    through embedded resources, of the edited resource holds."
                }
            },
            "required": ["property"],
            "description": "Call an embedded resource of the edited resource instead of the \
                            resource itself."
        }),
        "resource" => json!({
            "type": "object",
            "minProperties": 1,
            "maxProperties": 1,
            "additionalProperties": false,
            "properties": {
                "property": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_NAME_CHARS,
                    "pattern": PROPERTY_PATH_PATTERN,
                    "description": "The resource the node's property holds, or a property \
                                    path through embedded resources such as \
                                    `tree_root:states/Locomotion/node`."
                },
                "animation": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_NAME_CHARS,
                    "pattern": crate::schema::NO_CONTROL_PATTERN,
                    "description": "The animation an AnimationPlayer or AnimationTree holds, as \
                                    `library/name` or `name`."
                },
                "library": {
                    "type": "string",
                    "maxLength": MAX_NAME_CHARS,
                    "pattern": crate::schema::NO_CONTROL_PATTERN,
                    "description": "The animation library an AnimationPlayer or AnimationTree \
                                    holds; \"\" is its default library."
                }
            },
            "description": "Call a resource of the node instead of the node: one the scene \
                            embeds, never a separate resource file."
        }),
        "method" if op == "call_method" => json!({
            "type": "string",
            "enum": method_names().into_iter().collect::<Vec<_>>(),
            "description": format!(
                "An editing method of the receiver's class, from the table in \
                 editor-bridge.md: {}.",
                editing_classes()
            )
        }),
        "method" => {
            let mut method = crate::schema::name(MAX_NAME_CHARS);
            method["description"] =
                json!("The method of `to` the signal calls, such as `_on_body_entered`.");
            method
        }
        "arguments" => json!({
            "type": "array",
            "maxItems": MAX_CALL_ARGUMENTS,
            "items": { "$ref": format!("#/$defs/{}", crate::schema::CALL_ARGUMENT) },
            "description": "The call's arguments, in order, as tagged values; \
                            {type: \"new_resource\", value: {class, properties?}} passes a new \
                            embedded resource."
        }),
        "repeat" => json!({
            "type": "array",
            "minItems": 1,
            "maxItems": MAX_REPEATED_CALLS,
            "items": {
                "type": "array",
                "maxItems": MAX_CALL_ARGUMENTS,
                "items": { "$ref": format!("#/$defs/{}", crate::schema::CALL_ARGUMENT) }
            },
            "description": "Call the method once per argument list, in order, instead of once \
                            with `arguments`: one change, one undo step in the editor, and one \
                            entry in `results` whose `values` are the calls' return values."
        }),
        _ => crate::schema::name(MAX_NAME_CHARS),
    }
}

pub(super) fn operations_schema(scene: bool) -> Value {
    operations_schema_for(scene, false)
}

/// The `$defs` name a tool shares the schema of `field` of `op` under, when
/// it repeats it. A `value` is a reference already, and a `call_method`'s
/// `resource` is a different schema in a scene's operations and in a
/// resource's, so neither is shared.
fn shared_name(op: &str, field: &str) -> Option<&'static str> {
    Some(match field {
        "parent" | "node" | "from" | "to" => "NodeReference",
        "property" => "PropertyPath",
        "scene" => "ScenePath",
        "script" => "ScriptPath",
        "index" => "ChildIndex",
        "properties" => "PropertyValues",
        "method" if op == "call_method" => "EditingMethod",
        "method" => "SignalMethod",
        "arguments" => "CallArguments",
        "repeat" => "RepeatedCalls",
        "value" | "resource" => return None,
        _ => "Name",
    })
}

/// The operation field schemas a tool advertising operations of `shapes`,
/// each a (`scene`, `paths_only`) pair as [`operations_schema_for`] takes
/// it, shares when it repeats them, each under its [`shared_name`].
pub(super) fn shared_fields(shapes: &[(bool, bool)]) -> Vec<(&'static str, Value)> {
    let mut shared: Vec<(&'static str, Value)> = Vec::new();
    for &(scene, paths_only) in shapes {
        for (op, fields, _) in operations(scene) {
            for field in *fields {
                let Some(name) = shared_name(op, field) else {
                    continue;
                };
                let schema = operation_field_schema(op, field, scene, paths_only);
                match shared.iter().find(|(known, _)| *known == name) {
                    Some((_, known)) => assert_eq!(
                        known, &schema,
                        "every field shared as {name} has one schema in a tool"
                    ),
                    None => shared.push((name, schema)),
                }
            }
        }
    }
    shared
}

/// The operation field schemas `editor.scene.edit` shares: those of the
/// scene batch model, with every node named by its path.
pub(crate) fn live_shared_fields() -> Vec<(&'static str, Value)> {
    shared_fields(&[(true, true)])
}

/// The operations an `editor.scene.edit` applies to the scene the editor
/// holds: the scene batch model, with every node named by its path.
pub(crate) fn live_operations_schema() -> Value {
    operations_schema_for(true, true)
}

/// A top-level name a creation takes, as [`string_field`] checks it.
fn name_property(description: &str) -> Value {
    let mut name = crate::schema::name(MAX_NAME_CHARS);
    name["description"] = json!(description);
    name
}

/// One operation of `editor.scene.edit`: a valid scene batch operation that
/// names no node handle, since a handle is bound to file bytes the editor's
/// copy may no longer hold.
///
/// # Errors
/// Returns [`ErrorKind::InvalidInput`] for an invalid operation or a handle.
pub(crate) fn validate_live_operation(operation: &Value, index: usize) -> Result<(), OutcomeError> {
    validate_operation(true, operation, index)?;
    for field in ["parent", "node", "from", "to"] {
        if operation
            .get(field)
            .and_then(Value::as_str)
            .is_some_and(|reference| reference.starts_with(node_handle::PREFIX))
        {
            return Err(invalid(format!(
                "operation {index}: `{field}` is a node handle, which names a node of a file's \
                 bytes; name the node in the editor's copy by its path"
            )));
        }
    }
    Ok(())
}

fn operations_schema_for(scene: bool, paths_only: bool) -> Value {
    let description = if paths_only {
        "Typed operations applied in order: add_node {parent?, name, type?}, instance_scene \
         {parent?, name, scene}, remove_node {node}, rename_node {node, name}, move_node {node, \
         parent, index?}, duplicate_node {node, name}, set_property {node?, property, value}, \
         clear_property {node?, property}, attach_script {node?, script}, detach_script {node?}, \
         replace_instance {node, scene}, connect_signal / disconnect_signal {from, signal, to, \
         method}, add_group / remove_group {node?, group}, set_subresource {node?, property, \
         class, properties?}, call_method {node?, resource?, method, arguments? | repeat?}. \
         `node`, `parent`, `from` and `to` take a node path relative to the scene root (`.` is \
         the root). `property` may be a property path through embedded resources. Values are \
         tagged {type, value} objects."
    } else if scene {
        "Typed operations applied in order: add_node {parent?, name, type?}, instance_scene \
         {parent?, name, scene}, remove_node {node}, rename_node {node, name}, move_node {node, \
         parent, index?}, duplicate_node {node, name}, set_property {node?, property, value}, \
         clear_property {node?, property}, attach_script {node?, script}, detach_script {node?}, \
         replace_instance {node, scene}, connect_signal / disconnect_signal {from, signal, to, \
         method}, add_group / remove_group {node?, group}, set_subresource {node?, property, \
         class, properties?}, call_method {node?, resource?, method, arguments? | repeat?}. \
         `node`, `parent`, `from` and `to` take a node path relative to the scene root (`.` is \
         the root) or the `handle` a scene tree answer gave that node; a handle of older bytes \
         refuses as a conflict. `property` may be a property path through embedded resources. \
         Values are tagged {type, value} objects."
    } else {
        "Typed operations applied in order: set_property {property, value}, clear_property \
         {property}, set_subresource {property, class, properties?}, call_method {resource?, \
         method, arguments? | repeat?} on the resource itself, or on an embedded resource a \
         `resource` property path names. `property` may be a property path through embedded \
         resources. Values are tagged {type, value} objects."
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
                        .map(|field| {
                            (*field, operation_field_schema(op, field, scene, paths_only))
                        })
                        .collect();
                    let mut variant = crate::schema::variant("op", op, &fields, required);
                    if *op == "call_method" {
                        variant["not"] = json!({ "required": ["arguments", "repeat"] });
                    }
                    variant
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
                name_property("Engine Node class of the root node (default Node)."),
            );
            properties.insert(
                "rootName".to_owned(),
                name_property("Name of the root node (default the file name)."),
            );
            // No scene tree answer has handed out a handle to a node of a
            // scene that does not exist yet.
            let mut operations = operations_schema_for(true, true);
            operations["minItems"] = json!(0);
            properties.insert("operations".to_owned(), operations);
        }
        AuthorKind::ResourceCreate => {
            properties.insert(
                "class".to_owned(),
                name_property(
                    "Engine Resource class of the new resource (default Resource); initial \
                     values are set_property and call_method operations.",
                ),
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
    let mut schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": required,
    });
    // The shape of the operations above, as `operations_schema_for` takes
    // it: of the four, only a new scene names its nodes by path alone.
    let shape = (kind.is_scene(), kind == AuthorKind::SceneCreate);
    crate::schema::define_shared(&mut schema, &shared_fields(&[shape]));
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
        Some(Value::String(value))
            if !value.is_empty() && value.chars().count() <= MAX_NAME_CHARS =>
        {
            if value.chars().any(char::is_control) {
                return Err(invalid(format!(
                    "{context}: `{key}` must not contain control characters"
                )));
            }
            Ok(Some(value))
        }
        Some(_) => Err(invalid(format!(
            "{context}: `{key}` must be a non-empty string of at most {MAX_NAME_CHARS} characters"
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

/// A property path: one or more non-empty names separated by `:`.
fn property_path<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<&'a str, OutcomeError> {
    let path = required_string(object, key, context)?;
    if path.split(':').any(str::is_empty) {
        return Err(invalid(format!(
            "{context}: `{key}` must be a property or a property path of non-empty names \
             separated by `:`"
        )));
    }
    Ok(path)
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
pub(crate) fn tagged_value(value: &Value, context: &str) -> Result<(), OutcomeError> {
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

/// One argument of a `call_method`: a writable tagged value, or a new
/// embedded resource of an engine or global script class that is not a
/// script or a GDExtension, with optional initial properties as tagged values.
/// The engine that applies the edit checks that the class exists, is a
/// resource and can be created.
fn call_argument(value: &Value, context: &str) -> Result<(), OutcomeError> {
    let Some(object) = value.as_object() else {
        return tagged_value(value, context);
    };
    if object.get("type") != Some(&json!("new_resource")) {
        return tagged_value(value, context);
    }
    let shape = || {
        invalid(format!(
            "{context}: a new resource is {{type: \"new_resource\", value: {{class, \
             properties?}}}}"
        ))
    };
    let described = object
        .get("value")
        .and_then(Value::as_object)
        .filter(|described| {
            object.len() == 2
                && described.contains_key("class")
                && described
                    .keys()
                    .all(|key| key == "class" || key == "properties")
        })
        .ok_or_else(shape)?;
    let class = described
        .get("class")
        .and_then(Value::as_str)
        .ok_or_else(shape)?;
    if !is_class_name(class) {
        return Err(invalid(format!(
            "{context}: a new resource's class is a class name of at most \
             {MAX_CLASS_NAME_BYTES} letters, digits and underscores"
        )));
    }
    if NEW_RESOURCE_FORBIDDEN.contains(&class) {
        return Err(invalid(format!(
            "{context}: a new resource cannot be a {class}"
        )));
    }
    property_map(described, "properties", context)
}

/// A `call_method` operation: a listed method, its receiver, and its
/// arguments or the argument lists of its `repeat`.
fn call_operation(
    scene: bool,
    object: &Map<String, Value>,
    context: &str,
) -> Result<(), OutcomeError> {
    let method = required_string(object, "method", context)?;
    if !method_names().contains(method) {
        return Err(invalid(format!(
            "{context}: `{method}` is not an editing method; see the table in editor-bridge.md"
        )));
    }
    if scene {
        node_path(object, "node", context, false)?;
    }
    if let Some(selector) = object.get("resource") {
        let selectors: &[&str] = if scene {
            &RESOURCE_SELECTORS
        } else {
            &["property"]
        };
        let selector = selector
            .as_object()
            .filter(|selector| selector.len() == 1)
            .ok_or_else(|| {
                invalid(format!(
                    "{context}: `resource` names one of {}",
                    selectors.join(", ")
                ))
            })?;
        let (kind, name) = selector.iter().next().expect("one selector");
        let name = name
            .as_str()
            .filter(|name| {
                name.chars().count() <= MAX_NAME_CHARS && !name.chars().any(char::is_control)
            })
            .ok_or_else(|| invalid(format!("{context}: `resource.{kind}` must be a name")))?;
        if !selectors.contains(&kind.as_str())
            || (kind != "library" && name.is_empty())
            || (kind == "property" && name.split(':').any(str::is_empty))
        {
            return Err(invalid(format!(
                "{context}: `resource` names one of {}",
                selectors.join(", ")
            )));
        }
    }
    let arguments = |list: &Value, context: &str| match list {
        Value::Array(arguments) if arguments.len() <= MAX_CALL_ARGUMENTS => {
            for (position, argument) in arguments.iter().enumerate() {
                call_argument(argument, &format!("{context} argument {position}"))?;
            }
            Ok(())
        }
        _ => Err(invalid(format!(
            "{context}: a call takes a list of at most {MAX_CALL_ARGUMENTS} tagged values"
        ))),
    };
    match (object.get("arguments"), object.get("repeat")) {
        (None, None) => Ok(()),
        (Some(list), None) => arguments(list, context),
        (None, Some(Value::Array(lists)))
            if !lists.is_empty() && lists.len() <= MAX_REPEATED_CALLS =>
        {
            for (call, list) in lists.iter().enumerate() {
                arguments(list, &format!("{context} call {call}"))?;
            }
            Ok(())
        }
        (None, Some(_)) => Err(invalid(format!(
            "{context}: `repeat` lists 1 to {MAX_REPEATED_CALLS} argument lists"
        ))),
        (Some(_), Some(_)) => Err(invalid(format!(
            "{context}: a call takes `arguments` or `repeat`, not both"
        ))),
    }
}

fn property_map(object: &Map<String, Value>, key: &str, context: &str) -> Result<(), OutcomeError> {
    match object.get(key) {
        None => Ok(()),
        Some(Value::Object(properties)) => {
            for (name, value) in properties {
                if name.is_empty() || name.chars().count() > crate::schema::MAX_PROPERTY_NAME_CHARS
                {
                    return Err(invalid(format!(
                        "{context}: property names are 1 to {} characters",
                        crate::schema::MAX_PROPERTY_NAME_CHARS
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
            resource_path(object, "scene", &context, &SCENE_EXTENSIONS)?;
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
            property_path(object, "property", &context)?;
            let value = object
                .get("value")
                .ok_or_else(|| invalid(format!("{context}: `value` is required")))?;
            tagged_value(value, &context)?;
        }
        "clear_property" => {
            if scene {
                node_path(object, "node", &context, false)?;
            }
            property_path(object, "property", &context)?;
        }
        "attach_script" => {
            node_path(object, "node", &context, false)?;
            resource_path(object, "script", &context, &["gd"])?;
        }
        "replace_instance" => {
            node_path(object, "node", &context, true)?;
            resource_path(object, "scene", &context, &SCENE_EXTENSIONS)?;
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
            property_path(object, "property", &context)?;
            required_string(object, "class", &context)?;
            property_map(object, "properties", &context)?;
        }
        "call_method" => call_operation(scene, object, &context)?,
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
    parse_as(kind, arguments, kind.tool_name())
}

/// [`parse`], naming the request `tool` in every refusal: the tool, or the
/// file of a multi-file batch.
///
/// # Errors
/// The refusals of [`parse`].
pub(super) fn parse_as(
    kind: AuthorKind,
    arguments: Map<String, Value>,
    tool: &str,
) -> Result<AuthorRequest, OutcomeError> {
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
        recipe: None,
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
    use crate::schema::conformance::{NameRule, conforms, name_cases};

    /// The rule of every name-like string field of an operation.
    const NAME: NameRule = NameRule {
        limit: MAX_NAME_CHARS,
        empty: false,
        control: true,
    };

    /// Every way an operation is advertised, as (`scene`, `live`): in a scene
    /// batch, in a resource batch and in a live edit of the editor's scene.
    const SHAPES: [(bool, bool); 3] = [(true, false), (false, false), (true, true)];

    /// A valid value of `field` in `op`: a callback name for a signal
    /// operation's `method`, an editing method for `call_method`'s.
    fn sample(op: &str, field: &str) -> Value {
        match field {
            "parent" | "node" | "from" | "to" => json!("."),
            "scene" => json!("res://scenes/a.tscn"),
            "script" => json!("res://scripts/a.gd"),
            "index" => json!(0),
            "value" => json!({ "type": "integer", "value": 1 }),
            "properties" => json!({}),
            "method" if op == "call_method" => json!("clear"),
            "method" => json!("_on_body_entered"),
            _ => json!("Name"),
        }
    }

    /// `op` with only its `required` fields, each a valid sample.
    fn with_required(op: &str, required: &[&str]) -> Value {
        let mut operation = Map::new();
        operation.insert("op".to_owned(), json!(op));
        for field in required {
            operation.insert((*field).to_owned(), sample(op, field));
        }
        Value::Object(operation)
    }

    /// The advertised schema of one operation of a scene (`scene`) or
    /// resource batch, or of a live edit (`live`), as a standalone schema
    /// carrying the definitions it refers to.
    fn advertised_operation_schema(scene: bool, live: bool) -> Value {
        json!({
            "allOf": [operations_schema_for(scene, live)["items"].clone()],
            "$defs": crate::schema::value_definitions(),
        })
    }

    /// Hold the advertised `schema` of an operation and the check behind it
    /// to `expected` on `operation`, which `case` names.
    fn agree(
        (scene, live): (bool, bool),
        schema: &Value,
        operation: &Value,
        expected: bool,
        case: &str,
    ) {
        assert_eq!(
            conforms(schema, operation),
            expected,
            "the schema's verdict (scene {scene}, live {live}) on {case}"
        );
        let accepted = if live {
            validate_live_operation(operation, 0).is_ok()
        } else {
            validate_operation(scene, operation, 0).is_ok()
        };
        assert_eq!(
            accepted, expected,
            "the handler's verdict (scene {scene}, live {live}) on {case}"
        );
    }

    /// The schema and the validator read one table: every operation with only
    /// its required fields conforms and validates, and no operation takes a
    /// field it does not declare.
    #[test]
    fn every_operation_the_schema_lists_validates_with_its_required_fields() {
        for shape in SHAPES {
            let schema = advertised_operation_schema(shape.0, shape.1);
            for (op, _, required) in operations(shape.0) {
                let operation = with_required(op, required);
                agree(shape, &schema, &operation, true, &operation.to_string());
                let mut extra = operation.clone();
                extra["colour"] = json!("red");
                agree(shape, &schema, &extra, false, &extra.to_string());
            }
        }
    }

    /// Every name-like string field of every operation follows one rule in
    /// the schema and the check: 1 to 512 characters, whatever the bytes
    /// each takes, and no control character. A scene or script path is a
    /// `res://` path of that length ending in its extension.
    #[test]
    fn every_name_field_follows_one_rule_in_the_schema_and_the_check() {
        for shape in SHAPES {
            let schema = advertised_operation_schema(shape.0, shape.1);
            for (op, fields, required) in operations(shape.0) {
                for field in *fields {
                    let (prefix, suffix) = match *field {
                        "scene" => ("res://", ".tscn"),
                        "script" => ("res://", ".gd"),
                        "method" if *op == "call_method" => continue,
                        "index" | "value" | "properties" | "resource" | "arguments" | "repeat" => {
                            continue;
                        }
                        _ => ("", ""),
                    };
                    for (index, (value, expected)) in
                        name_cases(NAME, prefix, suffix).into_iter().enumerate()
                    {
                        let mut operation = with_required(op, required);
                        operation[*field] = json!(value);
                        let case = format!(
                            "{op}.{field}, case {index} ({} characters)",
                            value.chars().count()
                        );
                        agree(shape, &schema, &operation, expected, &case);
                    }
                }
            }
        }
    }

    /// A `call_method` resource selector names its resource by the same
    /// rule; only the default animation library's name may be empty.
    #[test]
    fn every_selector_name_follows_one_rule_in_the_schema_and_the_check() {
        let library = NameRule {
            empty: true,
            ..NAME
        };
        for shape in SHAPES {
            let schema = advertised_operation_schema(shape.0, shape.1);
            let selectors: &[(&str, NameRule)] = if shape.0 {
                &[
                    ("property", NAME),
                    ("animation", NAME),
                    ("library", library),
                ]
            } else {
                &[("property", NAME)]
            };
            for (selector, rule) in selectors {
                for (index, (value, expected)) in name_cases(*rule, "", "").into_iter().enumerate()
                {
                    let mut resource = Map::new();
                    resource.insert((*selector).to_owned(), json!(value));
                    let operation =
                        json!({ "op": "call_method", "method": "clear", "resource": resource });
                    let case = format!(
                        "resource.{selector}, case {index} ({} characters)",
                        value.chars().count()
                    );
                    agree(shape, &schema, &operation, expected, &case);
                }
            }
        }
    }

    /// A property map's keys, of `set_subresource` and of a new resource a
    /// call passes, are 1 to 512 characters. The check takes a control
    /// character in a property name, so the schema does too.
    #[test]
    fn every_property_name_follows_one_rule_in_the_schema_and_the_check() {
        let rule = NameRule {
            limit: crate::schema::MAX_PROPERTY_NAME_CHARS,
            empty: false,
            control: false,
        };
        let tagged = json!({ "type": "integer", "value": 1 });
        for shape in SHAPES {
            let schema = advertised_operation_schema(shape.0, shape.1);
            for (index, (key, expected)) in name_cases(rule, "", "").into_iter().enumerate() {
                let mut properties = Map::new();
                properties.insert(key.clone(), tagged.clone());
                let set = json!({ "op": "set_subresource", "property": "material",
                                  "class": "StandardMaterial3D", "properties": properties });
                let call = json!({ "op": "call_method", "method": "add_animation", "arguments": [
                    { "type": "new_resource",
                      "value": { "class": "Animation", "properties": properties } }
                ] });
                for operation in [set, call] {
                    let case = format!(
                        "{} property name, case {index} ({} characters)",
                        operation["op"],
                        key.chars().count()
                    );
                    agree(shape, &schema, &operation, expected, &case);
                }
            }
        }
    }

    /// A new scene's root type and name, and a new resource's class, follow
    /// the same rule in the tool schema and the check.
    #[test]
    fn the_names_a_creation_takes_follow_one_rule() {
        for (kind, path, field) in [
            (AuthorKind::SceneCreate, "a.tscn", "rootType"),
            (AuthorKind::SceneCreate, "a.tscn", "rootName"),
            (AuthorKind::ResourceCreate, "a.tres", "class"),
        ] {
            let schema = Value::Object(crate::kernel::tool_schema(
                kind.tool_name(),
                crate::core::CoreLimits::default(),
            ));
            for (index, (value, expected)) in name_cases(NAME, "", "").into_iter().enumerate() {
                let mut call = json!({ "path": path, "idempotencyId": "call-1" });
                call[field] = json!(value);
                let case = format!(
                    "{field}, case {index} ({} characters)",
                    value.chars().count()
                );
                assert_eq!(
                    conforms(&schema, &call),
                    expected,
                    "the schema's verdict on {case}"
                );
                let mut arguments = call.as_object().cloned().expect("an object");
                arguments.remove("idempotencyId");
                assert_eq!(
                    parse(kind, arguments).is_ok(),
                    expected,
                    "the handler's verdict on {case}"
                );
            }
        }
    }

    /// A node field takes a node path relative to the scene root, or in a
    /// batch a handle a scene tree answer issued; a live edit and a new scene
    /// take paths only.
    #[test]
    fn the_node_schemas_and_the_node_checks_agree() {
        let handle = format!("{}{}", node_handle::PREFIX, "0123456789abcdef".repeat(2));
        // (value, verdict in a batch, verdict in a live edit or a creation)
        let cases = [
            (".".to_owned(), true, true),
            ("Player/Sprite".to_owned(), true, true),
            ("..a/b..".to_owned(), true, true),
            ("\u{540D}/\u{1F600}".to_owned(), true, true),
            (handle.clone(), true, false),
            (format!("{}xyz", node_handle::PREFIX), false, false),
            (
                format!("{}{}", node_handle::PREFIX, "A".repeat(32)),
                false,
                false,
            ),
            (format!("{handle}/a"), false, false),
            ("/root".to_owned(), false, false),
            ("a/".to_owned(), false, false),
            ("a//b".to_owned(), false, false),
            ("..".to_owned(), false, false),
            ("../a".to_owned(), false, false),
            ("a/../b".to_owned(), false, false),
        ];
        for shape in [(true, false), (true, true)] {
            let schema = advertised_operation_schema(shape.0, shape.1);
            for (op, fields, required) in operations(true) {
                for field in fields.iter().filter(|field| NODE_FIELDS.contains(field)) {
                    for (value, in_batch, in_live) in &cases {
                        let expected = if shape.1 { *in_live } else { *in_batch };
                        let mut operation = with_required(op, required);
                        operation[*field] = json!(value);
                        agree(
                            shape,
                            &schema,
                            &operation,
                            expected,
                            &format!("{op}.{field} {value:?}"),
                        );
                    }
                }
            }
        }
        let schema = Value::Object(crate::kernel::tool_schema(
            AuthorKind::SceneCreate.tool_name(),
            crate::core::CoreLimits::default(),
        ));
        for (op, fields, required) in operations(true) {
            for field in fields.iter().filter(|field| NODE_FIELDS.contains(field)) {
                for (value, _, expected) in &cases {
                    let mut operation = with_required(op, required);
                    operation[*field] = json!(value);
                    let call = json!({ "path": "a.tscn", "idempotencyId": "call-1",
                                       "operations": [operation] });
                    let case = format!("scene.create {op}.{field} {value:?}");
                    assert_eq!(
                        conforms(&schema, &call),
                        *expected,
                        "the schema's verdict on {case}"
                    );
                    let mut arguments = call.as_object().cloned().expect("an object");
                    arguments.remove("idempotencyId");
                    assert_eq!(
                        parse(AuthorKind::SceneCreate, arguments).is_ok(),
                        *expected,
                        "the handler's verdict on {case}"
                    );
                }
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

    /// The `call_method` schema each batch advertises and the call check give
    /// the same verdict: a listed method, one owned receiver, at most 16
    /// arguments or a `repeat` of 1 to 4096 argument lists, and a new resource
    /// only as an argument, of a class name that is not a script or a
    /// GDExtension, with tagged initial properties.
    #[test]
    fn the_call_schema_and_the_call_check_agree() {
        let integer = |value: i64| json!({ "type": "integer", "value": value });
        let cell = |x: i64, y: i64| json!({ "type": "vector2i", "value": { "x": x, "y": y } });
        let new_resource =
            |class: &str| json!({ "type": "new_resource", "value": { "class": class } });
        let object =
            json!({ "type": "object", "value": { "id": "object_1", "class_name": "Node" } });
        let call = |fields: Value| {
            let mut operation = json!({ "op": "call_method" });
            for (key, value) in fields.as_object().expect("fields") {
                operation[key] = value.clone();
            }
            operation
        };
        // (operation, verdict in a scene batch, verdict in a resource batch)
        let cases = [
            (call(json!({ "method": "clear" })), true, true),
            (
                call(
                    json!({ "method": "set_point_value", "arguments": [integer(0), { "type": "float", "value": 0.5 }] }),
                ),
                true,
                true,
            ),
            (
                call(
                    json!({ "method": "add_animation", "arguments": [{ "type": "string_name", "value": "run" }, new_resource("Animation")] }),
                ),
                true,
                true,
            ),
            // Whether a class exists and is a resource is the engine's to say.
            (
                call(
                    json!({ "method": "add_node", "arguments": [new_resource("AnimationNodeAnimation")] }),
                ),
                true,
                true,
            ),
            (
                call(json!({ "method": "add_node", "arguments": [new_resource("Node")] })),
                true,
                true,
            ),
            (
                call(
                    json!({ "method": "add_node", "arguments": [{ "type": "string_name", "value": "Run" }, { "type": "new_resource", "value": { "class": "AnimationNodeAnimation", "properties": { "animation": { "type": "string_name", "value": "run" } } } }] }),
                ),
                true,
                true,
            ),
            (call(json!({ "method": "queue_free" })), false, false),
            (call(json!({ "method": "set_script" })), false, false),
            (
                call(json!({ "method": "clear", "arguments": vec![integer(1); 17] })),
                false,
                false,
            ),
            (
                call(json!({ "method": "add_animation", "arguments": [new_resource("Script")] })),
                false,
                false,
            ),
            (
                call(
                    json!({ "method": "add_animation", "arguments": [new_resource("GDExtension")] }),
                ),
                false,
                false,
            ),
            (
                call(
                    json!({ "method": "add_animation", "arguments": [new_resource("Style Box")] }),
                ),
                false,
                false,
            ),
            (
                call(json!({ "method": "add_animation", "arguments": [new_resource("3D")] })),
                false,
                false,
            ),
            (
                call(
                    json!({ "method": "add_animation", "arguments": [new_resource(&"A".repeat(129))] }),
                ),
                false,
                false,
            ),
            (
                call(
                    json!({ "method": "add_animation", "arguments": [{ "type": "new_resource", "value": { "class": "Animation", "path": "res://a.tres" } }] }),
                ),
                false,
                false,
            ),
            (
                call(
                    json!({ "method": "add_animation", "arguments": [{ "type": "new_resource", "value": { "class": "Animation" }, "extra": 1 }] }),
                ),
                false,
                false,
            ),
            (
                call(
                    json!({ "method": "add_animation", "arguments": [{ "type": "new_resource", "value": { "class": "Animation", "properties": { "length": 2 } } }] }),
                ),
                false,
                false,
            ),
            (
                call(
                    json!({ "method": "add_animation", "arguments": [{ "type": "new_resource", "value": { "class": "Animation", "properties": { "inner": new_resource("Animation") } } }] }),
                ),
                false,
                false,
            ),
            (
                call(json!({ "method": "add_point", "arguments": [object.clone()] })),
                false,
                false,
            ),
            (
                call(json!({ "method": "add_point", "arguments": "point" })),
                false,
                false,
            ),
            (
                call(json!({ "method": "clear", "colour": "red" })),
                false,
                false,
            ),
            (
                call(
                    json!({ "method": "set_cell", "repeat": [[cell(0, 0), integer(0)], [cell(1, 0), integer(0)]] }),
                ),
                true,
                true,
            ),
            (
                call(json!({ "method": "set_cell", "repeat": vec![json!([]); 4096] })),
                true,
                true,
            ),
            (
                call(json!({ "method": "set_cell", "repeat": vec![json!([]); 4097] })),
                false,
                false,
            ),
            (
                call(json!({ "method": "set_cell", "repeat": [] })),
                false,
                false,
            ),
            (
                call(json!({ "method": "set_cell", "repeat": [[cell(0, 0)]], "arguments": [] })),
                false,
                false,
            ),
            (
                call(json!({ "method": "set_cell", "repeat": [vec![integer(1); 17]] })),
                false,
                false,
            ),
            (
                call(json!({ "method": "set_cell", "repeat": [cell(0, 0)] })),
                false,
                false,
            ),
            (
                call(
                    json!({ "node": "Ground", "method": "set_cell", "arguments": [cell(0, 0), integer(0), cell(1, 0)] }),
                ),
                true,
                false,
            ),
            (
                call(json!({ "resource": { "property": "curve" }, "method": "add_point" })),
                true,
                true,
            ),
            (
                call(
                    json!({ "resource": { "property": "tree_root:states/Locomotion/node" }, "method": "add_blend_point" }),
                ),
                true,
                true,
            ),
            (
                call(json!({ "resource": { "property": "tree_root::node" }, "method": "clear" })),
                false,
                false,
            ),
            (
                call(json!({ "resource": { "property": "tree_root:" }, "method": "clear" })),
                false,
                false,
            ),
            (
                call(
                    json!({ "node": "Player", "resource": { "animation": "run" }, "method": "add_track", "arguments": [integer(0)] }),
                ),
                true,
                false,
            ),
            (
                call(json!({ "resource": { "library": "" }, "method": "add_animation" })),
                true,
                false,
            ),
            (
                call(json!({ "resource": {} , "method": "clear" })),
                false,
                false,
            ),
            (
                call(
                    json!({ "resource": { "property": "a", "animation": "b" }, "method": "clear" }),
                ),
                false,
                false,
            ),
            (
                call(json!({ "resource": { "texture": "a" }, "method": "clear" })),
                false,
                false,
            ),
            (
                call(json!({ "resource": { "property": "" }, "method": "clear" })),
                false,
                false,
            ),
            (
                call(json!({ "resource": "curve", "method": "clear" })),
                false,
                false,
            ),
        ];
        for kind in [AuthorKind::SceneBatch, AuthorKind::ResourceBatch] {
            let root = Value::Object(input_schema(kind));
            let mut schema =
                json!({ "allOf": [root["properties"]["operations"]["items"].clone()] });
            schema["$defs"] = root["$defs"].clone();
            let scene = kind == AuthorKind::SceneBatch;
            for (case, in_scene, in_resource) in &cases {
                let expected = if scene { *in_scene } else { *in_resource };
                assert_eq!(
                    crate::schema::conformance::conforms(&schema, case),
                    expected,
                    "the {kind:?} schema's verdict on {case}"
                );
                assert_eq!(
                    validate_operation(scene, case, 0).is_ok(),
                    expected,
                    "the {kind:?} handler's verdict on {case}"
                );
            }
        }
        // A new resource is only ever a call's argument.
        let set =
            json!({ "op": "set_property", "property": "curve", "value": new_resource("Curve") });
        assert!(validate_operation(true, &set, 0).is_err());
        let root = Value::Object(input_schema(AuthorKind::SceneBatch));
        let mut schema = json!({ "allOf": [root["properties"]["operations"]["items"].clone()] });
        schema["$defs"] = root["$defs"].clone();
        assert!(!crate::schema::conformance::conforms(&schema, &set));
    }

    /// A property is a property path of non-empty names, in every operation
    /// that writes one, and an instanced scene may be an imported 3D model.
    #[test]
    fn the_path_schemas_and_the_path_checks_agree() {
        let float = json!({ "type": "float", "value": 1.0 });
        // (operation, verdict in a scene batch, verdict in a resource batch)
        let cases = [
            (
                json!({ "op": "set_property", "property": "position:x", "value": float }),
                true,
                true,
            ),
            (
                json!({ "op": "set_property", "property": "material:albedo_color:r", "value": float }),
                true,
                true,
            ),
            (
                json!({ "op": "set_property", "property": "tree_root:states/Run/position", "value": float }),
                true,
                true,
            ),
            (
                json!({ "op": "set_property", "property": "a::b", "value": float }),
                false,
                false,
            ),
            (
                json!({ "op": "set_property", "property": ":a", "value": float }),
                false,
                false,
            ),
            (
                json!({ "op": "set_property", "property": "a:", "value": float }),
                false,
                false,
            ),
            (
                json!({ "op": "clear_property", "property": "material:roughness" }),
                true,
                true,
            ),
            (
                json!({ "op": "clear_property", "property": "material:" }),
                false,
                false,
            ),
            (
                json!({ "op": "set_subresource", "property": "process_material:color_ramp", "class": "GradientTexture1D" }),
                true,
                true,
            ),
            (
                json!({ "op": "set_subresource", "property": ":color_ramp", "class": "GradientTexture1D" }),
                false,
                false,
            ),
            (
                json!({ "op": "instance_scene", "name": "Ship", "scene": "res://models/ship.glb" }),
                true,
                false,
            ),
            (
                json!({ "op": "instance_scene", "name": "Ship", "scene": "res://models/ship.gltf" }),
                true,
                false,
            ),
            (
                json!({ "op": "instance_scene", "name": "Ship", "scene": "res://models/Ship.BLEND" }),
                true,
                false,
            ),
            (
                json!({ "op": "replace_instance", "node": "Ship", "scene": "res://levels/A.TSCN" }),
                true,
                false,
            ),
            (
                json!({ "op": "attach_script", "script": "res://scripts/Player.GD" }),
                true,
                false,
            ),
        ];
        for kind in [AuthorKind::SceneBatch, AuthorKind::ResourceBatch] {
            let root = Value::Object(input_schema(kind));
            let mut schema =
                json!({ "allOf": [root["properties"]["operations"]["items"].clone()] });
            schema["$defs"] = root["$defs"].clone();
            let scene = kind == AuthorKind::SceneBatch;
            for (case, in_scene, in_resource) in &cases {
                let expected = if scene { *in_scene } else { *in_resource };
                assert_eq!(
                    crate::schema::conformance::conforms(&schema, case),
                    expected,
                    "the {kind:?} schema's verdict on {case}"
                );
                assert_eq!(
                    validate_operation(scene, case, 0).is_ok(),
                    expected,
                    "the {kind:?} handler's verdict on {case}"
                );
            }
        }
        // A scene or script path is a project path after `res://` and ends in
        // one of its extensions, in both the schema and the check.
        let schema = advertised_operation_schema(true, false);
        for (field, path, expected) in [
            ("scene", "res://a.txt", false),
            ("scene", "res://a.tres", false),
            ("scene", "res://a.png", false),
            ("scene", "res://a", false),
            ("scene", "res://a.tscn/", false),
            ("scene", "res://a.tscn.b/c", false),
            ("scene", "res://a.tscn ", false),
            ("scene", "res://../a.tscn", false),
            ("scene", "res://a/../b.tscn", false),
            ("scene", "res:///a.tscn", false),
            ("scene", "res://C:/a.tscn", false),
            ("scene", "res://-a.tscn", false),
            ("scene", "res://a\\b.tscn", false),
            ("scene", "res://x://a.tscn", false),
            ("scene", "a.tscn", false),
            ("scene", "res://a/./b.scn", true),
            ("scene", "res://.tscn", true),
            ("scene", "res://\u{2028}a.obj", true),
            ("script", "res://a.gdc", false),
            ("script", "res://a.tscn", false),
            ("script", "res://a/b.gd", true),
        ] {
            let operation = if field == "scene" {
                json!({ "op": "instance_scene", "name": "A", "scene": path })
            } else {
                json!({ "op": "attach_script", "script": path })
            };
            agree(
                (true, false),
                &schema,
                &operation,
                expected,
                &format!("{field} {path:?}"),
            );
        }
    }

    /// Sharing changes how a tool writes its operations, not what they take:
    /// every tool that takes operations defines each field schema it shares
    /// once and refers to it more than once, and with those definitions put
    /// back, every field of every operation it advertises is that field's own
    /// schema.
    #[test]
    fn a_shared_definition_stands_for_exactly_the_fields_it_replaces() {
        /// Where a tool advertises an operation list, and the (`scene`,
        /// `paths_only`) shape the list has.
        type List = (&'static str, (bool, bool));
        let operations = "/properties/operations";
        let tools: [(&str, &[List]); 6] = [
            ("scene.create", &[(operations, (true, true))]),
            ("scene.batch.apply", &[(operations, (true, false))]),
            ("resource.create", &[(operations, (false, false))]),
            ("resource.batch.apply", &[(operations, (false, false))]),
            ("editor.scene.edit", &[(operations, (true, true))]),
            (
                "project.batch.apply",
                &[
                    (
                        "/properties/files/items/oneOf/0/properties/operations",
                        (true, false),
                    ),
                    (
                        "/properties/files/items/oneOf/1/properties/operations",
                        (false, false),
                    ),
                ],
            ),
        ];
        let values = crate::schema::value_definitions();
        for (tool, lists) in tools {
            let schema = Value::Object(crate::kernel::tool_schema(
                tool,
                crate::core::CoreLimits::default(),
            ));
            let shared: Vec<&str> = schema["$defs"]
                .as_object()
                .expect("an authoring tool carries definitions")
                .keys()
                .map(String::as_str)
                .filter(|name| !values.contains_key(*name))
                .collect();
            assert!(!shared.is_empty(), "{tool} shares what it repeats");
            let mut body = schema.clone();
            if let Some(object) = body.as_object_mut() {
                object.remove("$defs");
            }
            let written = body.to_string();
            for name in &shared {
                let uses = written
                    .matches(&format!("{{\"$ref\":\"#/$defs/{name}\"}}"))
                    .count();
                assert!(uses > 1, "{tool} refers to {name} {uses} times");
            }
            let inlined = crate::schema::conformance::inline_definitions(&schema, &shared);
            for &(pointer, (in_scene, paths_only)) in lists {
                let list = inlined
                    .pointer(pointer)
                    .unwrap_or_else(|| panic!("{tool} advertises operations at {pointer}"));
                for variant in list["items"]["oneOf"]
                    .as_array()
                    .expect("operation variants")
                {
                    let op = variant["properties"]["op"]["const"]
                        .as_str()
                        .expect("an operation name");
                    for (field, advertised) in
                        variant["properties"].as_object().expect("operation fields")
                    {
                        if field == "op" {
                            continue;
                        }
                        assert_eq!(
                            advertised,
                            &operation_field_schema(op, field, in_scene, paths_only),
                            "{tool} {op}.{field}"
                        );
                    }
                }
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
