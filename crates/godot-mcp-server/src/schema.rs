//! JSON Schema pieces shared by the tool schemas.
//!
//! Every object a discovered schema describes rejects the fields it does not
//! declare, at any depth. A list of operations, steps or events is therefore a
//! discriminated union: each variant names its discriminator value and declares
//! exactly its own fields.

use std::sync::LazyLock;

use godot_mcp_domain::contracts::GodotValue;
use serde_json::{Map, Value, json};

#[cfg(any(test, feature = "test-hooks"))]
pub(crate) mod conformance;

/// One variant of a discriminated object: `discriminator` is the constant
/// `name`, the object declares exactly `fields`, and `required` must be present.
pub(crate) fn variant(
    discriminator: &str,
    name: &str,
    fields: &[(&str, Value)],
    required: &[&str],
) -> Value {
    let mut properties = Map::new();
    properties.insert(discriminator.to_owned(), json!({ "const": name }));
    for (field, schema) in fields {
        properties.insert((*field).to_owned(), schema.clone());
    }
    let mut needed = vec![Value::String(discriminator.to_owned())];
    needed.extend(
        required
            .iter()
            .map(|field| Value::String((*field).to_owned())),
    );
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": needed,
    })
}

/// A value that is exactly one of `variants`.
pub(crate) fn one_of(variants: Vec<Value>) -> Value {
    json!({ "oneOf": variants })
}

/// The `$defs` name of a value a tool may write.
const WRITABLE_VALUE: &str = "WritableGodotValue";
/// The definition [`plain_value`] refers to.
const PLAIN_VALUE: &str = "PlainGodotValue";
/// The `$defs` name of one argument of a batch's `call_method`.
pub(crate) const CALL_ARGUMENT: &str = "CallArgument";

/// One segment of a normalized project path: not empty, not `.` or `..`,
/// and without a slash or a backslash.
const PATH_SEGMENT: &str = r"(?:[^/\\.][^/\\]*|\.[^/\\.][^/\\]*|\.\.[^/\\]+)";

/// A normalized project resource, as `ProjectResource` parses one: `res://`
/// and one or more [`PATH_SEGMENT`]s.
fn project_resource_pattern() -> String {
    format!("^res://{PATH_SEGMENT}(?:/{PATH_SEGMENT})*$")
}

/// A project GDScript file, project-relative or `res://`: one or more
/// [`PATH_SEGMENT`]s ending in `.gd`.
pub(crate) fn gdscript_path_pattern() -> String {
    format!(r"^(?=.*\.gd$)(?:res://)?{PATH_SEGMENT}(?:/{PATH_SEGMENT})*$")
}

/// A project PNG image, project-relative or `res://`: [`PATH_SEGMENT`]s whose
/// last one ends in `.png` in any letter case, outside the engine cache
/// (`.godot`), the server's state root (`.godot-mcp`) and the headless
/// helper's scratch (`godot_mcp_helpers`).
pub(crate) fn project_png_pattern() -> String {
    format!(
        r"^(?!(?:res://)?(?:\.godot|\.godot-mcp|godot_mcp_helpers)/)(?:res://)?(?:{PATH_SEGMENT}/)*[^/\\]*\.[pP][nN][gG]$"
    )
}

/// A project scene, project-relative or `res://`: [`PATH_SEGMENT`]s whose last
/// one ends in `.tscn` or `.scn`.
pub(crate) fn project_scene_pattern() -> String {
    format!(r"^(?:res://)?(?:{PATH_SEGMENT}/)*[^/\\]*\.t?scn$")
}

/// A tagged contract value a tool may write, `{"type": <variant>, "value":
/// <payload>}`: any variant but a live object reference, at any depth. The
/// tool schema that uses it carries [`value_definitions`] as its `$defs`.
pub(crate) fn writable_value() -> Value {
    json!({ "$ref": format!("#/$defs/{WRITABLE_VALUE}") })
}

/// A tagged contract value that refers to nothing: any variant but a resource
/// or a live object reference, at any depth, as an editor setting holds. The
/// tool schema that uses it carries [`value_definitions`] as its `$defs`.
pub(crate) fn plain_value() -> Value {
    json!({ "$ref": format!("#/$defs/{PLAIN_VALUE}") })
}

/// The definitions [`writable_value`] and [`plain_value`] refer to.
///
/// They are generated from the [`GodotValue`] type the handler parses, so
/// each variant name and payload shape is the one the handler accepts. The
/// generated schema is then held to the handler's further rules: a tagged
/// value declares no field beside `type` and `value`, a unit variant's value
/// may only be null, no variant is a live object reference, a resource is a
/// normalized `res://` path and a 32-bit integer stays in range.
pub(crate) fn value_definitions() -> Map<String, Value> {
    static DEFINITIONS: LazyLock<Map<String, Value>> = LazyLock::new(generated_definitions);
    DEFINITIONS.clone()
}

fn generated_definitions() -> Map<String, Value> {
    let root = serde_json::to_value(
        schemars::generate::SchemaSettings::draft2020_12()
            .into_generator()
            .into_root_schema_for::<GodotValue>(),
    )
    .expect("a generated schema is JSON");
    let mut definitions = root
        .get("$defs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    // Only a live object reference uses these.
    definitions.remove("ObjectReference");
    definitions.remove("OpaqueObjectId");
    let resource = definitions
        .get_mut("ProjectResource")
        .and_then(Value::as_object_mut)
        .expect("a resource reference names a project resource");
    resource.insert("pattern".to_owned(), json!(project_resource_pattern()));
    let variants: Vec<Value> = root["oneOf"]
        .as_array()
        .expect("a tagged value is one of its variants")
        .iter()
        .filter(|variant| variant["properties"]["type"]["const"] != json!("object"))
        .cloned()
        .map(|mut variant| {
            variant["additionalProperties"] = json!(false);
            if variant["properties"].get("value").is_none() {
                variant["properties"]["value"] = json!({ "type": "null" });
            }
            variant
        })
        .collect();
    let mut plain: Vec<Value> = variants
        .iter()
        .filter(|variant| variant["properties"]["type"]["const"] != json!("resource"))
        .cloned()
        .collect();
    plain
        .iter_mut()
        .for_each(|variant| refer_root_to(variant, PLAIN_VALUE));
    definitions.insert(
        PLAIN_VALUE.to_owned(),
        json!({
            "description": "A tagged value {type, value} that refers to nothing: `type` names \
                            the variant and `value` is that variant's payload. Neither a \
                            resource nor a live object reference.",
            "oneOf": plain
        }),
    );
    definitions.insert(
        WRITABLE_VALUE.to_owned(),
        json!({
            "description": "A tagged value {type, value}: `type` names the variant and `value` \
                            is that variant's payload. A live object reference cannot be \
                            written; name a resource by its res:// path.",
            "oneOf": variants
        }),
    );
    // Only a `call_method` argument may also be a new embedded resource.
    definitions.insert(
        CALL_ARGUMENT.to_owned(),
        json!({
            "description": "A tagged value, or {type: \"new_resource\", value: {class, \
                            properties?}}: a new resource the edit embeds, of any engine or \
                            global script resource class but a script or a GDExtension, with \
                            initial properties as tagged values.",
            "oneOf": [
                { "$ref": format!("#/$defs/{WRITABLE_VALUE}") },
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["type", "value"],
                    "properties": {
                        "type": { "const": "new_resource" },
                        "value": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["class"],
                            "properties": {
                                "class": {
                                    "type": "string",
                                    "pattern": "^[A-Za-z_][A-Za-z0-9_]*$",
                                    "maxLength": godot_mcp_domain::editing_methods::MAX_CLASS_NAME_BYTES,
                                    "not": {
                                        "enum": godot_mcp_domain::editing_methods::NEW_RESOURCE_FORBIDDEN
                                    }
                                },
                                "properties": {
                                    "type": "object",
                                    "additionalProperties": { "$ref": format!("#/$defs/{WRITABLE_VALUE}") },
                                    "description": "Property names mapped to tagged values."
                                }
                            }
                        }
                    }
                }
            ]
        }),
    );
    let mut definitions = Value::Object(definitions);
    retarget(&mut definitions);
    match definitions {
        Value::Object(definitions) => definitions,
        _ => unreachable!("definitions stay an object"),
    }
}

/// Point the generated schema's references to its root at `definition`.
fn refer_root_to(schema: &mut Value, definition: &str) {
    match schema {
        Value::Object(map) => {
            if map.get("$ref") == Some(&json!("#")) {
                map.insert("$ref".to_owned(), json!(format!("#/$defs/{definition}")));
            }
            map.values_mut()
                .for_each(|value| refer_root_to(value, definition));
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| refer_root_to(item, definition)),
        _ => {}
    }
}

/// Point the generated schema's remaining references to its root at the
/// writable value definition, and bound every 32-bit integer.
fn retarget(schema: &mut Value) {
    match schema {
        Value::Object(map) => {
            if map.get("$ref") == Some(&json!("#")) {
                map.insert(
                    "$ref".to_owned(),
                    json!(format!("#/$defs/{WRITABLE_VALUE}")),
                );
            }
            if map.get("format") == Some(&json!("int32")) {
                map.insert("minimum".to_owned(), json!(i32::MIN));
                map.insert("maximum".to_owned(), json!(i32::MAX));
            }
            map.values_mut().for_each(retarget);
        }
        Value::Array(items) => items.iter_mut().for_each(retarget),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_variant_declares_exactly_its_fields_and_its_discriminator() {
        let schema = variant(
            "op",
            "rename",
            &[
                ("node", json!({ "type": "string" })),
                ("name", json!({ "type": "string" })),
            ],
            &["name"],
        );
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["properties"]["op"], json!({ "const": "rename" }));
        assert_eq!(schema["required"], json!(["op", "name"]));
        assert_eq!(
            schema["properties"].as_object().map(Map::len),
            Some(3),
            "{schema}"
        );
    }
}
