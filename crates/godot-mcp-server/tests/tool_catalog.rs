//! M1 acceptance: discovery is implementation-backed and comes from one catalog.
//!
//! Finding F-08 (planned editor/runtime methods advertised without a production
//! handler) and F-10 (tool name, schema, policy and description maintained in
//! independent registries) are reproduced over the wire: every tool a profile
//! lists must have a strict schema, a specific description, MCP annotations and
//! a production handler, and the capability report must carry the current
//! contract version.

mod support;

use std::collections::BTreeSet;

use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession};

const CORE: [&str; 22] = [
    "capabilities.get",
    "gdscript.diagnostics.get",
    "gdscript.impact.get",
    "gdscript.references.get",
    "gdscript.symbols.get",
    "health.get",
    "project.dependencies.audit",
    "project.dependencies.get",
    "project.discover",
    "project.info.get",
    "project.metrics.get",
    "project.search",
    "project.signals.get",
    "project.uid.get",
    "project.validate",
    "resource.dependencies.get",
    "resource.tree.get",
    "scene.dependencies.get",
    "scene.exports.get",
    "scene.nodes.find",
    "scene.tree.get",
    "script.validate",
];
const EDITOR_ONLY: [&str; 4] = [
    "project.import.run",
    "project.uid.update",
    "resource.update",
    "scene.save",
];
const RUNTIME_ONLY: [&str; 3] = ["project.run", "project.test", "script.check"];
/// Contract 0.2 authoring reads: every profile except `core`.
const AUTHORING_READS: [&str; 4] = [
    "asset.status",
    "file.get",
    "project.files.list",
    "project.settings.get",
];
/// Contract 0.5 scene recipes: the write-capable profiles.
const RECIPES: [&str; 1] = ["scene.recipe.apply"];
/// Contract 0.2 authoring mutations: the write-capable profiles.
const AUTHORING_WRITES: [&str; 13] = [
    "asset.delete",
    "asset.put",
    "file.create",
    "file.delete",
    "file.move",
    "file.patch",
    "project.settings.patch",
    "resource.batch.apply",
    "resource.create",
    "scene.batch.apply",
    "scene.create",
    "script.create",
    "script.patch",
];

/// Live-editor methods and the add-on lifecycle: the editor profiles.
const EDITOR_PLANE: [&str; 26] = [
    "editor.addon.install",
    "editor.addon.reload",
    "editor.addon.remove",
    "editor.addon.status",
    "editor.addon.update",
    "editor.filesystem.scan",
    "editor.log.get",
    "editor.main_screen.set",
    "editor.node.get",
    "editor.preferences.get",
    "editor.preferences.set",
    "editor.redo",
    "editor.scene.close",
    "editor.scene.edit",
    "editor.scene.open",
    "editor.scene.save",
    "editor.scene.tree.get",
    "editor.screenshot.capture",
    "editor.script.open",
    "editor.selection.set",
    "editor.shortcuts.get",
    "editor.shortcuts.set",
    "editor.status.get",
    "editor.types.get",
    "editor.types.search",
    "editor.undo",
];

/// Contract 0.4 live-editor additions: the editor profiles.
const EDITOR_MORE: [&str; 9] = [
    "editor.camera.get",
    "editor.camera.set",
    "editor.dialog.respond",
    "editor.dialogs.get",
    "editor.performance.get",
    "editor.resource.preview",
    "editor.restart",
    "editor.scene.read",
    "editor.script.close",
];

/// Contract 0.4 declared script execution (ADR 0012): the editor profiles.
const SCRIPT_EXECUTION: [&str; 2] = ["editor.script.run", "script.run"];
/// Contract 0.6 commands a project declares (ADR 0016): the editor profiles.
const PROJECT_COMMANDS: [&str; 2] = ["editor.commands.call", "editor.commands.list"];
/// ADR 0012's script tool for a running game: the profiles that run the project.
const RUNTIME_SCRIPT: [&str; 1] = ["runtime.script.run"];
/// Contract 0.4 authoring mutations: the write-capable profiles.
const AUTHORING_WRITES_0_4: [&str; 1] = ["project.batch.apply"];
/// Contract 0.4 engine reads: a headless helper run starts the project's
/// autoloads, so the profiles that run the project.
const ENGINE_READS: [&str; 3] = ["resource.read", "scene.read", "shader.check"];

/// Running the project in the connected editor: the profiles that run it.
const EDITOR_PLAY: [&str; 2] = ["editor.play.start", "editor.play.stop"];
/// Contract 0.2 managed game sessions: the profiles that run the project.
const RUNTIME_PLANE: [&str; 15] = [
    "input.action.send",
    "input.key.send",
    "input.mouse.send",
    "input.sequence.send",
    "project.run.logs",
    "project.run.start",
    "project.run.status",
    "project.run.stop",
    "runtime.performance.get",
    "runtime.properties.get",
    "runtime.recording.capture",
    "runtime.screenshot.capture",
    "runtime.signals.get",
    "runtime.tree.get",
    "runtime.ui.get",
];
/// Contract 0.3 typed control and observation of a managed game session.
const RUNTIME_CONTROL: [&str; 11] = [
    "input.recording.start",
    "input.recording.stop",
    "input.replay",
    "runtime.navigation.path.get",
    "runtime.node.move",
    "runtime.nodes.find",
    "runtime.properties.batch.get",
    "runtime.properties.set",
    "runtime.sample",
    "runtime.ui.press",
    "runtime.wait",
];
/// Contract 0.4 game speed, image comparison and test scenarios of a managed
/// game session.
const RUNTIME_SCENARIO: [&str; 3] = [
    "runtime.image.compare",
    "runtime.time_scale.set",
    "test.scenario.run",
];
/// Contract 0.7 frame-level control of a managed game session (ADR 0017).
const FRAME_CONTROL: [&str; 1] = ["runtime.frames.step"];
/// Contract 0.2.0 managed editor sessions and their receipt-bound language
/// server and debug adapter.
const DEBUG_PLANE: [&str; 4] = [
    "debug.command",
    "debug.session.get",
    "editor.session.start",
    "editor.session.stop",
];
/// Contract 0.2.0 managed exports.
const EXPORT_PLANE: [&str; 4] = [
    "export.presets.list",
    "project.export",
    "project.export.cancel",
    "project.export.status",
];
/// Contract 0.4.0 Android devices: listing them and deploying an export.
const ANDROID_DEVICES: [&str; 2] = ["android.deploy", "android.devices.list"];

fn expected(profile: &str) -> BTreeSet<String> {
    let mut tools: BTreeSet<String> = CORE.iter().map(|tool| (*tool).to_owned()).collect();
    if matches!(profile, "editor" | "full") {
        tools.extend(EDITOR_ONLY.iter().map(|tool| (*tool).to_owned()));
        tools.extend(AUTHORING_WRITES.iter().map(|tool| (*tool).to_owned()));
        tools.extend(AUTHORING_WRITES_0_4.iter().map(|tool| (*tool).to_owned()));
        tools.extend(EDITOR_PLANE.iter().map(|tool| (*tool).to_owned()));
        tools.extend(EDITOR_MORE.iter().map(|tool| (*tool).to_owned()));
        tools.extend(SCRIPT_EXECUTION.iter().map(|tool| (*tool).to_owned()));
        tools.extend(RECIPES.iter().map(|tool| (*tool).to_owned()));
        tools.extend(PROJECT_COMMANDS.iter().map(|tool| (*tool).to_owned()));
    }
    if matches!(profile, "runtime" | "full") {
        tools.extend(RUNTIME_ONLY.iter().map(|tool| (*tool).to_owned()));
        tools.extend(RUNTIME_PLANE.iter().map(|tool| (*tool).to_owned()));
        tools.extend(RUNTIME_CONTROL.iter().map(|tool| (*tool).to_owned()));
        tools.extend(RUNTIME_SCENARIO.iter().map(|tool| (*tool).to_owned()));
        tools.extend(FRAME_CONTROL.iter().map(|tool| (*tool).to_owned()));
        tools.extend(RUNTIME_SCRIPT.iter().map(|tool| (*tool).to_owned()));
        tools.extend(ENGINE_READS.iter().map(|tool| (*tool).to_owned()));
        tools.extend(EDITOR_PLAY.iter().map(|tool| (*tool).to_owned()));
        tools.extend(DEBUG_PLANE.iter().map(|tool| (*tool).to_owned()));
        tools.extend(EXPORT_PLANE.iter().map(|tool| (*tool).to_owned()));
        tools.extend(ANDROID_DEVICES.iter().map(|tool| (*tool).to_owned()));
    }
    if profile != "core" {
        tools.extend(AUTHORING_READS.iter().map(|tool| (*tool).to_owned()));
    }
    tools
}

fn listed(session: &mut StdioSession, id: u64) -> Vec<Value> {
    let response = session.request(id, "tools/list", json!({}));
    response["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list must answer: {response}"))
        .clone()
}

/// F-08/F-10: each profile lists exactly its implemented tools, and every listed
/// tool is a real, strictly described operation.
#[test]
fn discovery_lists_only_implemented_tools_with_strict_descriptions() {
    let tree = ProjectTree::new("m1-catalog");
    for profile in ["core", "editor", "runtime", "full"] {
        let mut session = StdioSession::initialized(&Spawn {
            project: &tree.root,
            current_dir: None,
            profile,
            godot: None,
        });
        let tools = listed(&mut session, 2);
        let names: BTreeSet<String> = tools
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name").to_owned())
            .collect();
        assert_eq!(
            names,
            expected(profile),
            "{profile}: discovery must list exactly the implemented tools"
        );
        for (index, tool) in tools.iter().enumerate() {
            let name = tool["name"].as_str().expect("tool name");
            let description = tool["description"].as_str().unwrap_or_default();
            assert!(
                description.len() >= 40 && !description.starts_with("Godot MCP outcome"),
                "{name}: the description must say what the tool does: {description:?}"
            );
            assert_eq!(
                tool["inputSchema"]["additionalProperties"],
                json!(false),
                "{name}: the schema must reject unknown fields"
            );
            let annotations = &tool["annotations"];
            for hint in [
                "readOnlyHint",
                "destructiveHint",
                "idempotentHint",
                "openWorldHint",
            ] {
                assert!(
                    annotations[hint].is_boolean(),
                    "{name}: annotations must declare {hint}: {tool}"
                );
            }
            let response = session.call_tool(
                100 + index as u64,
                name,
                json!({ "not_a_declared_field": true }),
            );
            assert_eq!(
                response["error"]["code"],
                json!(-32602),
                "{name}: an undeclared field must be refused as invalid params: {response}"
            );
        }
        let _ = session.finish();
    }
}

/// The capability report carries the 0.7.0 contract and the same tool sets
/// discovery lists.
#[test]
fn the_capability_report_is_contract_0_7_0_and_matches_discovery() {
    let tree = ProjectTree::new("m1-capabilities");
    let mut session = StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
        godot: None,
    });
    let report = session.call_tool(2, "capabilities.get", json!({}));
    let manifests = &report["result"]["structuredContent"];
    for profile in ["core", "editor", "runtime", "full"] {
        let manifest = &manifests[profile];
        assert_eq!(
            manifest["schema_version"],
            json!("0.7.0"),
            "{profile} manifest version: {manifest}"
        );
        let tools: BTreeSet<String> = manifest["tools"]
            .as_array()
            .expect("tool list")
            .iter()
            .map(|tool| tool.as_str().expect("tool name").to_owned())
            .collect();
        assert_eq!(tools, expected(profile), "{profile} manifest tools");
    }
    let _ = session.finish();
}

/// Every object a discovered schema describes, at any depth, rejects fields it
/// does not declare: a nested operation item or value is as strict as the top
/// level (plan section 16, "every discovered schema rejects unknown fields").
/// A map of caller-chosen keys says what each value must be instead.
///
/// A member of `allOf`, `anyOf`, `oneOf` or `not` describes the same instance
/// as the object it belongs to. Such a member without a type of its own that
/// names only fields the closed object declares, like the recipe parameters'
/// "`mass` only when `body` is `rigid`", is a condition on that object: the
/// object's `additionalProperties: false` still refuses any other field, so the
/// condition is not an open object. A member naming an undeclared field, or
/// declaring an object of its own, is checked like any other.
#[test]
fn every_object_in_every_discovered_schema_rejects_unknown_fields() {
    /// `closed` holds the fields of the closed object whose instance `schema`
    /// still describes, when it describes one.
    fn open_objects(
        schema: &Value,
        pointer: &str,
        closed: Option<&BTreeSet<String>>,
        found: &mut Vec<String>,
    ) {
        let Value::Object(map) = schema else {
            return;
        };
        let declared = map.get("properties").and_then(Value::as_object);
        let condition = closed.filter(|closed| {
            !map.contains_key("type")
                && !map.contains_key("additionalProperties")
                && declared.is_none_or(|declared| declared.keys().all(|key| closed.contains(key)))
        });
        if condition.is_none()
            && (map.get("type") == Some(&json!("object")) || map.contains_key("properties"))
        {
            match map.get("additionalProperties") {
                Some(Value::Bool(false) | Value::Object(_)) => {}
                _ => found.push(pointer.to_owned()),
            }
        }
        let own: Option<BTreeSet<String>> =
            (map.get("additionalProperties") == Some(&json!(false))).then(|| {
                declared
                    .map(|declared| declared.keys().cloned().collect())
                    .unwrap_or_default()
            });
        let same_instance = own.as_ref().or(condition);
        for keyword in ["properties", "patternProperties", "$defs", "definitions"] {
            if let Some(Value::Object(named)) = map.get(keyword) {
                for (name, child) in named {
                    open_objects(child, &format!("{pointer}/{keyword}/{name}"), None, found);
                }
            }
        }
        for keyword in [
            "items",
            "additionalProperties",
            "if",
            "then",
            "else",
            "contains",
        ] {
            if let Some(child @ Value::Object(_)) = map.get(keyword) {
                open_objects(child, &format!("{pointer}/{keyword}"), None, found);
            }
        }
        if let Some(child @ Value::Object(_)) = map.get("not") {
            open_objects(child, &format!("{pointer}/not"), same_instance, found);
        }
        for keyword in ["oneOf", "anyOf", "allOf"] {
            if let Some(Value::Array(children)) = map.get(keyword) {
                for (index, child) in children.iter().enumerate() {
                    open_objects(
                        child,
                        &format!("{pointer}/{keyword}/{index}"),
                        same_instance,
                        found,
                    );
                }
            }
        }
        if let Some(Value::Array(children)) = map.get("prefixItems") {
            for (index, child) in children.iter().enumerate() {
                open_objects(
                    child,
                    &format!("{pointer}/prefixItems/{index}"),
                    None,
                    found,
                );
            }
        }
    }
    // The walker tells a condition on a closed object from an open object.
    let fixture = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": { "a": { "type": "string" }, "b": { "type": "integer" } },
        "anyOf": [
            { "required": ["a"], "properties": { "a": { "const": "x" } } },
            { "properties": { "c": { "type": "string" } } },
            { "type": "object", "properties": { "a": { "type": "string" } } },
            { "not": { "properties": { "b": { "const": 1 } }, "required": ["b"] } },
            { "allOf": [{ "properties": { "d": { "type": "string" } } }] },
        ],
    });
    let mut fixture_found = Vec::new();
    open_objects(&fixture, "fixture", None, &mut fixture_found);
    assert_eq!(
        fixture_found,
        [
            "fixture/anyOf/1",
            "fixture/anyOf/2",
            "fixture/anyOf/4/allOf/0"
        ],
        "only the members naming an undeclared field or declaring their own object are open"
    );
    let mut open_parent = Vec::new();
    open_objects(
        &json!({
            "type": "object",
            "properties": { "a": { "type": "string" } },
            "anyOf": [{ "properties": { "a": { "const": "x" } } }],
        }),
        "open",
        None,
        &mut open_parent,
    );
    assert_eq!(
        open_parent,
        ["open", "open/anyOf/0"],
        "an open object opens its members"
    );

    let tree = ProjectTree::new("m1-strict-schemas");
    let mut session = StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
        godot: None,
    });
    let mut found = Vec::new();
    for tool in listed(&mut session, 2) {
        let name = tool["name"].as_str().expect("tool name").to_owned();
        open_objects(&tool["inputSchema"], &name, None, &mut found);
    }
    let _ = session.finish();
    assert!(
        found.is_empty(),
        "these schema objects accept undeclared fields:\n{}",
        found.join("\n")
    );
}

/// Every value a discovered schema describes says what it must be: a type, a
/// constant, a closed set, a reference, or a union of those. A field described
/// only in prose accepts anything, which no handler does.
#[test]
fn every_value_in_every_discovered_schema_says_what_it_must_be() {
    fn says_what_it_must_be(schema: &Value) -> bool {
        let Value::Object(map) = schema else {
            return schema == &json!(false);
        };
        ["type", "const", "enum", "$ref"]
            .iter()
            .any(|keyword| map.contains_key(*keyword))
            || ["oneOf", "anyOf"].iter().any(|keyword| {
                map.get(*keyword)
                    .and_then(Value::as_array)
                    .is_some_and(|members| {
                        !members.is_empty() && members.iter().all(says_what_it_must_be)
                    })
            })
            || map
                .get("allOf")
                .and_then(Value::as_array)
                .is_some_and(|parts| parts.iter().any(says_what_it_must_be))
    }
    /// Check every value position below `schema`.
    fn walk(schema: &Value, pointer: &str, found: &mut Vec<String>) {
        let Value::Object(map) = schema else {
            return;
        };
        let value = |child: &Value, at: String, found: &mut Vec<String>| {
            if !says_what_it_must_be(child) {
                found.push(format!("{at}: {child}"));
            }
            walk(child, &at, found);
        };
        for keyword in ["properties", "$defs"] {
            if let Some(Value::Object(named)) = map.get(keyword) {
                for (name, child) in named {
                    value(child, format!("{pointer}/{keyword}/{name}"), found);
                }
            }
        }
        for keyword in ["items", "additionalProperties"] {
            if let Some(child @ Value::Object(_)) = map.get(keyword) {
                value(child, format!("{pointer}/{keyword}"), found);
            }
        }
        for keyword in ["oneOf", "anyOf", "allOf"] {
            if let Some(Value::Array(members)) = map.get(keyword) {
                for (index, member) in members.iter().enumerate() {
                    walk(member, &format!("{pointer}/{keyword}/{index}"), found);
                }
            }
        }
        if let Some(child) = map.get("not") {
            walk(child, &format!("{pointer}/not"), found);
        }
    }
    let tree = ProjectTree::new("m1-typed-schemas");
    let mut session = StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
        godot: None,
    });
    let mut found = Vec::new();
    for tool in listed(&mut session, 2) {
        let name = tool["name"].as_str().expect("tool name").to_owned();
        walk(&tool["inputSchema"], &name, &mut found);
    }
    let _ = session.finish();
    assert!(
        found.is_empty(),
        "these schema values accept anything:\n{}",
        found.join("\n")
    );
}

/// Every discovered schema is one plain object at its top level: no `oneOf`,
/// `anyOf`, `allOf`, `not` or conditional there. The Claude API refuses a tool
/// whose input schema combines shapes at its top level, and an MCP client that
/// sends the whole tool list then fails every request
/// (anthropics/claude-code#45106). A rule across top-level fields is therefore
/// stated in the fields' descriptions and enforced by the handler; nested values
/// carry their combinators.
#[test]
fn no_discovered_schema_combines_shapes_at_its_top_level() {
    let tree = ProjectTree::new("m1-plain-top-level");
    let mut session = StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
        godot: None,
    });
    let mut found = Vec::new();
    for tool in listed(&mut session, 2) {
        let name = tool["name"].as_str().expect("tool name").to_owned();
        let schema = &tool["inputSchema"];
        assert_eq!(schema["type"], json!("object"), "{name}");
        for keyword in ["oneOf", "anyOf", "allOf", "not", "if", "then", "else"] {
            if schema.get(keyword).is_some() {
                found.push(format!("{name}: {keyword}"));
            }
        }
    }
    let _ = session.finish();
    assert!(
        found.is_empty(),
        "these schemas combine shapes at their top level:\n{}",
        found.join("\n")
    );
}

/// Push every `$ref` inside `schema` onto `found`.
fn references(schema: &Value, found: &mut Vec<String>) {
    match schema {
        Value::Object(map) => {
            if let Some(reference) = map.get("$ref") {
                found.push(
                    reference
                        .as_str()
                        .unwrap_or_else(|| panic!("a reference is a string: {reference}"))
                        .to_owned(),
                );
            }
            map.values().for_each(|value| references(value, found));
        }
        Value::Array(items) => items.iter().for_each(|item| references(item, found)),
        _ => {}
    }
}

/// Every reference in a discovered schema, in every profile, resolves in the
/// tool's own `$defs`, and every definition the tool carries is one it refers
/// to: from its body, or from a definition its body refers to. A tool carries
/// the definitions it uses and no others, so a client resolves every reference
/// from the tool alone and pays for nothing it never reads.
#[test]
fn every_discovered_schema_carries_exactly_the_definitions_it_refers_to() {
    let tree = ProjectTree::new("w1-definitions");
    for profile in ["core", "editor", "runtime", "full"] {
        let mut session = StdioSession::initialized(&Spawn {
            project: &tree.root,
            current_dir: None,
            profile,
            godot: None,
        });
        for tool in listed(&mut session, 2) {
            let name = tool["name"].as_str().expect("tool name");
            let mut body = tool["inputSchema"].clone();
            let definitions = body
                .as_object_mut()
                .and_then(|schema| schema.remove("$defs"))
                .map(|definitions| {
                    definitions
                        .as_object()
                        .cloned()
                        .unwrap_or_else(|| panic!("{profile} {name}: $defs is an object"))
                })
                .unwrap_or_default();
            let mut pending = Vec::new();
            references(&body, &mut pending);
            let mut reached = BTreeSet::new();
            while let Some(reference) = pending.pop() {
                let definition = reference
                    .strip_prefix("#/$defs/")
                    .and_then(|definition| definitions.get(definition))
                    .unwrap_or_else(|| {
                        panic!("{profile} {name}: {reference} resolves to no definition of its own")
                    });
                if reached.insert(reference.clone()) {
                    references(definition, &mut pending);
                }
            }
            let carried: BTreeSet<String> = definitions
                .keys()
                .map(|definition| format!("#/$defs/{definition}"))
                .collect();
            assert_eq!(
                reached, carried,
                "{profile} {name}: the definitions it carries are exactly the ones it refers to"
            );
        }
        let _ = session.finish();
    }
}

/// The value definitions a tagged value roots in. A tool whose schema takes a
/// tagged value carries one of them.
const TAGGED_VALUES: [&str; 2] = ["WritableGodotValue", "PlainGodotValue"];

/// A tool carries `$defs` only when it takes a tagged value: every tool that
/// carries definitions carries a tagged value's. A tool that takes none writes
/// every field schema where it is used, even one it repeats.
#[test]
fn a_tool_carries_definitions_only_when_it_takes_a_tagged_value() {
    let tree = ProjectTree::new("w1-tagged-definitions");
    for profile in ["core", "editor", "runtime", "full"] {
        let mut session = StdioSession::initialized(&Spawn {
            project: &tree.root,
            current_dir: None,
            profile,
            godot: None,
        });
        for tool in listed(&mut session, 2) {
            let name = tool["name"].as_str().expect("tool name");
            if let Some(definitions) = tool["inputSchema"].get("$defs") {
                assert!(
                    TAGGED_VALUES
                        .iter()
                        .any(|value| definitions.get(*value).is_some()),
                    "{profile} {name}: it carries $defs but takes no tagged value"
                );
            }
        }
        let _ = session.finish();
    }
}

/// The bytes of `value` as the server writes it.
fn written(value: &Value) -> usize {
    serde_json::to_vec(value)
        .expect("a schema serializes")
        .len()
}

/// How many subschemas strictly inside `value` equal `target`.
fn occurrences_inside(value: &Value, target: &Value) -> usize {
    let items: Vec<&Value> = match value {
        Value::Object(map) => map.values().collect(),
        Value::Array(items) => items.iter().collect(),
        _ => Vec::new(),
    };
    items
        .into_iter()
        .map(|item| {
            if item == target {
                1
            } else {
                occurrences_inside(item, target)
            }
        })
        .sum()
}

/// Push every field schema strictly inside `value`, each subschema that carries
/// a `description`, onto `found`.
fn field_schemas_inside(value: &Value, found: &mut Vec<Value>) {
    let items: Vec<&Value> = match value {
        Value::Object(map) => map.values().collect(),
        Value::Array(items) => items.iter().collect(),
        _ => Vec::new(),
    };
    for item in items {
        if item.get("description").is_some_and(Value::is_string) {
            found.push(item.clone());
        }
        field_schemas_inside(item, found);
    }
}

/// A tool that carries `$defs` defines each field schema it repeats, a
/// subschema with a `description` that its properties hold more than once,
/// when a reference to it makes the schema shorter. The rule is the one the
/// server shares by, counted with a 24-character definition name. A top-level
/// property describes itself and is not counted.
#[test]
fn a_tool_with_definitions_writes_each_field_schema_it_repeats_once() {
    let name = "D".repeat(24);
    let reference = json!({ "$ref": format!("#/$defs/{name}") });
    let tree = ProjectTree::new("w1-repeated-fields");
    let mut repeated = Vec::new();
    for profile in ["core", "editor", "runtime", "full"] {
        let mut session = StdioSession::initialized(&Spawn {
            project: &tree.root,
            current_dir: None,
            profile,
            godot: None,
        });
        for tool in listed(&mut session, 2) {
            let tool_name = tool["name"].as_str().expect("tool name");
            let schema = &tool["inputSchema"];
            if schema.get("$defs").is_none() {
                continue;
            }
            let properties: Vec<&Value> = schema["properties"]
                .as_object()
                .expect("a tool schema declares its properties")
                .values()
                .collect();
            let mut fields = Vec::new();
            properties
                .iter()
                .for_each(|property| field_schemas_inside(property, &mut fields));
            for field in fields {
                let count: usize = properties
                    .iter()
                    .map(|property| occurrences_inside(property, &field))
                    .sum();
                let defined = name.len() + written(&field) + 4;
                if count >= 2 && count * written(&field) > count * written(&reference) + defined {
                    let entry = format!(
                        "{profile} {tool_name}: {count} x {}",
                        field["description"].as_str().unwrap_or_default()
                    );
                    if !repeated.contains(&entry) {
                        repeated.push(entry);
                    }
                }
            }
        }
        let _ = session.finish();
    }
    assert!(
        repeated.is_empty(),
        "these tools repeat a field schema a definition would shorten:\n{}",
        repeated.join("\n")
    );
}
