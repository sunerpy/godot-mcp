//! JSON Schema pieces shared by the tool schemas.
//!
//! Every object a discovered schema describes rejects the fields it does not
//! declare, at any depth. A list of operations, steps or events is therefore a
//! discriminated union: each variant names its discriminator value and declares
//! exactly its own fields.

use std::collections::BTreeSet;
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

/// The characters [`char::is_control`] refuses, U+0000–U+001F and
/// U+007F–U+009F, as the body of a regular-expression class.
pub(crate) const CONTROL: &str = r"\x00-\x1F\x7F-\x9F";

/// A string without a character [`char::is_control`] refuses.
pub(crate) const NO_CONTROL_PATTERN: &str = r"^[^\x00-\x1F\x7F-\x9F]*$";

/// A property path without control characters: property names separated by
/// `:`, none of them empty.
pub(crate) const PROPERTY_PATH_PATTERN: &str =
    r"^[^:\x00-\x1F\x7F-\x9F]+(?::[^:\x00-\x1F\x7F-\x9F]+)*$";

/// A name a handler takes as it is: 1 to `limit` characters (Unicode scalar
/// values, as `maxLength` counts them), none of them a control character.
pub(crate) fn name(limit: usize) -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": limit,
        "pattern": NO_CONTROL_PATTERN
    })
}

/// A node path relative to the scene root, unanchored: one or more segments
/// separated by `/`, none of them empty or `..` (so the path never starts
/// with `/`), holding no character of the class body `excluded` either.
pub(crate) fn relative_node_path(excluded: &str) -> String {
    let segment = format!(r"(?!\.\.(?:/|$))[^/{excluded}]+");
    format!("{segment}(?:/{segment})*")
}

/// `name` as a pattern fragment that matches it in any letter case.
pub(crate) fn caseless(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphabetic() {
                format!(
                    "[{}{}]",
                    character.to_ascii_uppercase(),
                    character.to_ascii_lowercase()
                )
            } else {
                character.to_string()
            }
        })
        .collect()
}

/// One segment of a normalized project path: not empty, not `.` or `..`,
/// and without a slash or a backslash.
const PATH_SEGMENT: &str = r"(?:[^/\\.][^/\\]*|\.[^/\\.][^/\\]*|\.\.[^/\\]+)";

/// A normalized project resource, as `ProjectResource` parses one: `res://`
/// and one or more [`PATH_SEGMENT`]s.
fn project_resource_pattern() -> String {
    format!("^res://{PATH_SEGMENT}(?:/{PATH_SEGMENT})*$")
}

/// A [`PATH_SEGMENT`] holding no character of the class body `excluded`
/// either.
pub(crate) fn path_segment(excluded: &str) -> String {
    format!(
        r"(?:[^/\\.{excluded}][^/\\{excluded}]*|\.[^/\\.{excluded}][^/\\{excluded}]*|\.\.[^/\\{excluded}]+)"
    )
}

/// A project file, project-relative or `res://`: one or more path segments,
/// none holding a character of the class body `excluded`.
pub(crate) fn project_path_pattern(excluded: &str) -> String {
    let segment = path_segment(excluded);
    format!("^(?:res://)?{segment}(?:/{segment})*$")
}

/// A project GDScript file, project-relative or `res://`: one or more path
/// segments, the last ending in `.gd`, none holding a character of the class
/// body `excluded`. The ending is spelled as the last segment rather than a
/// lookahead over `.*`: ECMA-262's `.` refuses line terminators, which a
/// segment takes.
pub(crate) fn gdscript_path_pattern(excluded: &str) -> String {
    let segment = path_segment(excluded);
    format!(r"^(?:res://)?(?:{segment}/)*[^/\\{excluded}]*\.gd$")
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
/// tool schema that uses it gets the definitions it refers to from
/// [`define_values`].
pub(crate) fn writable_value() -> Value {
    json!({ "$ref": format!("#/$defs/{WRITABLE_VALUE}") })
}

/// A tagged contract value that refers to nothing: any variant but a resource
/// or a live object reference, at any depth, as an editor setting holds. The
/// tool schema that uses it gets the definitions it refers to from
/// [`define_values`].
pub(crate) fn plain_value() -> Value {
    json!({ "$ref": format!("#/$defs/{PLAIN_VALUE}") })
}

/// The definitions [`writable_value`] and [`plain_value`] refer to, and the
/// ones those refer to in turn. A tool schema carries only those it names,
/// through [`define_values`].
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
                                    "propertyNames": property_name(),
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

/// Longest property name a batch's property map takes, in characters.
pub(crate) const MAX_PROPERTY_NAME_CHARS: usize = 512;

/// A key of a batch's property map: 1 to [`MAX_PROPERTY_NAME_CHARS`]
/// characters. The handler takes a control character in a property name, so
/// the schema does too.
pub(crate) fn property_name() -> Value {
    json!({ "type": "string", "minLength": 1, "maxLength": MAX_PROPERTY_NAME_CHARS })
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

/// Give the tool schema `schema` the `$defs` its body refers to and no others:
/// each of [`value_definitions`] it names, directly or through another
/// definition. A schema that names none carries no `$defs`.
pub(crate) fn define_values(schema: &mut Value) {
    define(schema, Map::new());
}

/// [`define_values`], after sharing each subschema of `shared` the body
/// repeats.
///
/// A shared subschema is defined once, under its name, and every occurrence
/// of it, a subschema equal to it inside a top-level property, refers to that
/// definition instead. A reference with nothing beside it takes exactly the
/// values the definition takes, so the schema still takes what it took: only
/// how it is written changes. A subschema the body holds once, or whose
/// sharing would not make the schema shorter, stays where it is, and so does
/// a top-level property, which describes itself.
pub(crate) fn define_shared(schema: &mut Value, shared: &[(&str, Value)]) {
    let mut definitions = Map::new();
    let properties = schema
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .expect("a tool schema declares its properties");
    let length = |value: &Value| serde_json::to_vec(value).map_or(0, |bytes| bytes.len());
    for (name, subschema) in shared {
        assert!(
            !definitions.contains_key(*name),
            "one tool shares one subschema as {name}"
        );
        let count: usize = properties
            .values()
            .map(|property| occurrences_within(property, subschema))
            .sum();
        let reference = json!({ "$ref": format!("#/$defs/{name}") });
        // The definition costs its name, its schema and the punctuation
        // around them; every occurrence then costs a reference.
        let defined = name.len() + length(subschema) + 4;
        if count < 2 || count * length(subschema) <= count * length(&reference) + defined {
            continue;
        }
        for property in properties.values_mut() {
            replace_within(property, subschema, &reference);
        }
        definitions.insert((*name).to_owned(), subschema.clone());
    }
    define(schema, definitions);
}

/// Give `schema` the definitions its body refers to, from `shared` and from
/// [`value_definitions`], whose names `shared` must not take.
fn define(schema: &mut Value, shared: Map<String, Value>) {
    let mut definitions = value_definitions();
    for (name, definition) in shared {
        assert!(
            !definitions.contains_key(&name),
            "a shared subschema takes the name of the value definition {name}"
        );
        definitions.insert(name, definition);
    }
    let object = schema
        .as_object_mut()
        .expect("a tool schema is a JSON object");
    assert!(
        !object.contains_key("$defs"),
        "a tool schema is given its definitions once"
    );
    let mut pending = Vec::new();
    object
        .values()
        .for_each(|value| references(value, &mut pending));
    let mut named = BTreeSet::new();
    while let Some(name) = pending.pop() {
        if named.contains(&name) {
            continue;
        }
        let definition = definitions
            .get(&name)
            .unwrap_or_else(|| panic!("#/$defs/{name} names no definition"));
        references(definition, &mut pending);
        named.insert(name);
    }
    if named.is_empty() {
        return;
    }
    let carried: Map<String, Value> = named
        .into_iter()
        .map(|name| {
            let definition = definitions.remove(&name).expect("a named definition");
            (name, definition)
        })
        .collect();
    object.insert("$defs".to_owned(), Value::Object(carried));
}

/// Push the name of every definition `schema` refers to onto `found`.
fn references(schema: &Value, found: &mut Vec<String>) {
    match schema {
        Value::Object(map) => {
            if let Some(reference) = map.get("$ref") {
                let name = reference
                    .as_str()
                    .and_then(|reference| reference.strip_prefix("#/$defs/"))
                    .unwrap_or_else(|| {
                        panic!("only a local #/$defs reference is defined: {reference}")
                    });
                found.push(name.to_owned());
            }
            map.values().for_each(|value| references(value, found));
        }
        Value::Array(items) => items.iter().for_each(|item| references(item, found)),
        _ => {}
    }
}

/// How many subschemas strictly inside `schema` equal `target`.
fn occurrences_within(schema: &Value, target: &Value) -> usize {
    let count = |value: &Value| {
        if value == target {
            1
        } else {
            occurrences_within(value, target)
        }
    };
    match schema {
        Value::Object(map) => map.values().map(count).sum(),
        Value::Array(items) => items.iter().map(count).sum(),
        _ => 0,
    }
}

/// Replace every subschema strictly inside `schema` that equals `target` with
/// `reference`.
fn replace_within(schema: &mut Value, target: &Value, reference: &Value) {
    let replace = |value: &mut Value| {
        if value == target {
            *value = reference.clone();
        } else {
            replace_within(value, target, reference);
        }
    };
    match schema {
        Value::Object(map) => map.values_mut().for_each(replace),
        Value::Array(items) => items.iter_mut().for_each(replace),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`CONTROL`] is exactly what [`char::is_control`] refuses, and the
    /// patterns built on it read it that way.
    #[test]
    fn the_control_class_is_what_is_control_refuses() {
        for character in (0..=0x10_FFFF).filter_map(char::from_u32) {
            let code = u32::from(character);
            assert_eq!(
                code <= 0x1F || (0x7F..=0x9F).contains(&code),
                character.is_control(),
                "U+{code:04X}"
            );
        }
        assert_eq!(NO_CONTROL_PATTERN, format!("^[^{CONTROL}]*$"));
        assert_eq!(
            PROPERTY_PATH_PATTERN,
            format!("^[^:{CONTROL}]+(?::[^:{CONTROL}]+)*$")
        );
        for character in (0..=0x2FF).filter_map(char::from_u32) {
            let text = format!("a{character}b");
            assert_eq!(
                conformance::pattern_matches(NO_CONTROL_PATTERN, &text),
                !character.is_control(),
                "U+{:04X}",
                u32::from(character)
            );
        }
    }

    /// A path segment that excludes nothing more is [`PATH_SEGMENT`], so the
    /// patterns built on either read a segment the same way.
    #[test]
    fn a_path_segment_excluding_nothing_is_the_shared_segment() {
        assert_eq!(path_segment(""), PATH_SEGMENT);
    }

    /// No advertised pattern spells "any character" as `.`: ECMA-262's `.`
    /// refuses the line terminators `\n`, `\r`, U+2028 and U+2029, which no
    /// handler here refuses for being a line terminator.
    #[test]
    fn no_advertised_pattern_uses_the_dot() {
        fn patterns<'a>(value: &'a Value, found: &mut Vec<&'a str>) {
            match value {
                Value::Object(keywords) => {
                    for (keyword, argument) in keywords {
                        match (keyword.as_str(), argument) {
                            ("pattern", Value::String(pattern)) => found.push(pattern),
                            // Instances, not schemas.
                            ("const" | "enum" | "default" | "examples", _) => {}
                            _ => patterns(argument, found),
                        }
                    }
                }
                Value::Array(items) => items.iter().for_each(|item| patterns(item, found)),
                _ => {}
            }
        }
        let mut checked = 0;
        for descriptor in godot_mcp_domain::catalog::TOOL_CATALOG {
            let schema = Value::Object(crate::kernel::tool_schema(
                descriptor.name,
                crate::core::CoreLimits::default(),
            ));
            let mut found = Vec::new();
            patterns(&schema, &mut found);
            for pattern in found {
                checked += 1;
                assert!(
                    !conformance::has_dot(pattern),
                    "{}: {pattern}",
                    descriptor.name
                );
            }
        }
        assert!(checked > 0, "the walk found no pattern at all");
    }

    /// A relative node path is one or more segments, none empty or `..`.
    #[test]
    fn a_relative_node_path_has_no_empty_or_parent_segment() {
        let pattern = format!("^{}$", relative_node_path(CONTROL));
        for (path, expected) in [
            (".", true),
            ("Player", true),
            ("Player/Sprite", true),
            ("..a", true),
            ("a/..b", true),
            ("a/b..", true),
            ("名/\u{1F600}", true),
            ("", false),
            ("/root", false),
            ("a/", false),
            ("a//b", false),
            ("..", false),
            ("../a", false),
            ("a/..", false),
            ("a/../b", false),
            ("a\u{1F}b", false),
        ] {
            assert_eq!(
                conformance::pattern_matches(&pattern, path),
                expected,
                "{path:?}"
            );
        }
        let open = format!("^{}$", relative_node_path(""));
        assert!(conformance::pattern_matches(&open, "a\u{1F}b"));
        assert!(!conformance::pattern_matches(&open, "a//b"));
    }

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

    /// A tool schema whose one property is `property`.
    fn tool(property: Value) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "a": property },
        })
    }

    /// The names of the definitions [`define_values`] gives `tool(property)`.
    fn defined(property: Value) -> BTreeSet<String> {
        let mut schema = tool(property);
        define_values(&mut schema);
        schema
            .get("$defs")
            .and_then(Value::as_object)
            .map(|definitions| definitions.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// A schema carries the value definitions its body names, and the ones
    /// those name in turn, and no others.
    #[test]
    fn a_schema_carries_exactly_the_value_definitions_it_refers_to() {
        let names = |list: &[&str]| -> BTreeSet<String> {
            list.iter().map(|name| (*name).to_owned()).collect()
        };
        let all: BTreeSet<String> = value_definitions().keys().cloned().collect();
        // What refers to nothing but the value structs: a plain value.
        let structs: BTreeSet<String> = all
            .difference(&names(&[
                PLAIN_VALUE,
                WRITABLE_VALUE,
                CALL_ARGUMENT,
                "ResourceReference",
                "ProjectResource",
            ]))
            .cloned()
            .collect();
        assert!(structs.contains("Vector2") && structs.contains("Projection"));

        let plain = defined(json!({ "type": "array", "items": plain_value() }));
        assert_eq!(plain, &structs | &names(&[PLAIN_VALUE]));
        let writable = defined(writable_value());
        assert_eq!(
            writable,
            &structs | &names(&[WRITABLE_VALUE, "ResourceReference", "ProjectResource"])
        );
        let argument = defined(json!({ "$ref": format!("#/$defs/{CALL_ARGUMENT}") }));
        assert_eq!(argument, &writable | &names(&[CALL_ARGUMENT]));
        assert_eq!(
            defined(json!({ "anyOf": [plain_value(), writable_value()] })),
            &plain | &writable
        );
        assert_eq!(&plain | &argument, all, "every definition is someone's");

        let mut none = tool(json!({ "type": "string" }));
        define_values(&mut none);
        assert_eq!(none, tool(json!({ "type": "string" })), "nothing to define");
    }

    /// A subschema a tool repeats is defined once and referred to, and the
    /// shared schema takes exactly what the repeated one took; one it holds
    /// once, and a top-level property, stay where they are.
    #[test]
    fn a_shared_subschema_is_defined_once_and_takes_what_it_replaced() {
        let path = json!({
            "type": "string",
            "minLength": 1,
            "maxLength": 8,
            "pattern": "^[a-z/]+$",
            "description": "A path of lower-case letters."
        });
        let once = json!({ "type": "integer", "minimum": 0, "description": "Only here." });
        let body = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": path,
                "operations": {
                    "type": "array",
                    "description": "Operations.",
                    "items": one_of(vec![
                        variant("op", "move", &[("from", path.clone()), ("to", path.clone())], &["from"]),
                        variant("op", "count", &[("index", once.clone()), ("value", writable_value())], &[]),
                    ])
                }
            },
            "required": ["path"],
        });
        let mut shared = body.clone();
        define_shared(
            &mut shared,
            &[("Path", path.clone()), ("Index", once.clone())],
        );
        let reference = json!({ "$ref": "#/$defs/Path" });
        let variants = &shared["properties"]["operations"]["items"]["oneOf"];
        assert_eq!(variants[0]["properties"]["from"], reference);
        assert_eq!(variants[0]["properties"]["to"], reference);
        assert_eq!(shared["$defs"]["Path"], path);
        assert_eq!(
            shared["properties"]["path"], path,
            "a top-level property stays"
        );
        assert_eq!(
            variants[1]["properties"]["index"], once,
            "a single use stays"
        );
        assert!(shared["$defs"].get("Index").is_none());
        assert!(shared["$defs"].get(WRITABLE_VALUE).is_some());

        let mut plain = body.clone();
        define_values(&mut plain);
        assert_eq!(
            conformance::inline_definitions(&shared, &["Path"]),
            plain,
            "only the writing changed"
        );
        for instance in [
            json!({ "path": "a", "operations": [{ "op": "move", "from": "a/b", "to": "c" }] }),
            json!({ "path": "a", "operations": [{ "op": "move", "from": "A" }] }),
            json!({ "path": "a", "operations": [{ "op": "move", "from": "abcdefghi" }] }),
            json!({ "path": "a", "operations": [{ "op": "move", "from": "" }] }),
            json!({ "path": "a", "operations": [{ "op": "move", "to": "a" }] }),
            json!({ "path": "a", "operations": [{ "op": "move", "from": "a", "x": 1 }] }),
            json!({ "path": "a", "operations": [{ "op": "count", "index": 2,
                                                 "value": { "type": "integer", "value": 1 } }] }),
            json!({ "path": "a", "operations": [{ "op": "count", "index": -1 }] }),
        ] {
            assert_eq!(
                conformance::conforms(&shared, &instance),
                conformance::conforms(&plain, &instance),
                "{instance}"
            );
        }
    }

    /// Sharing a subschema too small to pay for its definition would make
    /// the schema longer, so it stays repeated.
    #[test]
    fn a_subschema_too_small_to_share_stays_repeated() {
        let small = json!({ "type": "integer" });
        let body = tool(json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "x": small, "y": small }
        }));
        let mut shared = body.clone();
        define_shared(&mut shared, &[("Small", small)]);
        assert_eq!(shared, body);
    }

    #[test]
    #[should_panic(expected = "takes the name of the value definition")]
    fn a_shared_subschema_cannot_take_a_value_definition_name() {
        let word = json!({
            "type": "string",
            "pattern": "^[a-z]+$",
            "description": "A word of lower-case letters, which every field here takes."
        });
        let mut schema = tool(json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "w": word, "x": word, "y": word, "z": word }
        }));
        define_shared(&mut schema, &[(WRITABLE_VALUE, word)]);
    }
}
