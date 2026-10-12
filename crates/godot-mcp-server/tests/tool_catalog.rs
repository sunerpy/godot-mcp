//! M1 acceptance: discovery is implementation-backed and comes from one catalog.
//!
//! Finding F-08 (planned editor/runtime methods advertised without a production
//! handler) and F-10 (tool name, schema, policy and description maintained in
//! independent registries) are reproduced over the wire: every tool a profile
//! lists must have a strict schema, a specific description, MCP annotations and
//! a production handler, and the contract must be version 0.2.0.

mod support;

use std::collections::BTreeSet;

use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession};

const CORE: [&str; 15] = [
    "capabilities.get",
    "gdscript.diagnostics.get",
    "gdscript.impact.get",
    "gdscript.references.get",
    "gdscript.symbols.get",
    "health.get",
    "project.dependencies.get",
    "project.discover",
    "project.info.get",
    "project.validate",
    "resource.dependencies.get",
    "resource.tree.get",
    "scene.dependencies.get",
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

/// Contract 0.2 live-editor methods and the add-on lifecycle: the editor
/// profiles.
const EDITOR_PLANE: [&str; 12] = [
    "editor.addon.install",
    "editor.addon.reload",
    "editor.addon.remove",
    "editor.addon.status",
    "editor.addon.update",
    "editor.redo",
    "editor.scene.open",
    "editor.scene.tree.get",
    "editor.selection.set",
    "editor.status.get",
    "editor.types.get",
    "editor.undo",
];

/// Contract 0.2 managed game sessions: the profiles that run the project.
const RUNTIME_PLANE: [&str; 14] = [
    "input.action.send",
    "input.key.send",
    "input.mouse.send",
    "input.sequence.send",
    "project.run.logs",
    "project.run.start",
    "project.run.status",
    "project.run.stop",
    "runtime.properties.get",
    "runtime.recording.capture",
    "runtime.screenshot.capture",
    "runtime.signals.get",
    "runtime.tree.get",
    "runtime.ui.get",
];
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

fn expected(profile: &str) -> BTreeSet<String> {
    let mut tools: BTreeSet<String> = CORE.iter().map(|tool| (*tool).to_owned()).collect();
    if matches!(profile, "editor" | "full") {
        tools.extend(EDITOR_ONLY.iter().map(|tool| (*tool).to_owned()));
        tools.extend(AUTHORING_WRITES.iter().map(|tool| (*tool).to_owned()));
        tools.extend(EDITOR_PLANE.iter().map(|tool| (*tool).to_owned()));
    }
    if matches!(profile, "runtime" | "full") {
        tools.extend(RUNTIME_ONLY.iter().map(|tool| (*tool).to_owned()));
        tools.extend(RUNTIME_PLANE.iter().map(|tool| (*tool).to_owned()));
        tools.extend(DEBUG_PLANE.iter().map(|tool| (*tool).to_owned()));
        tools.extend(EXPORT_PLANE.iter().map(|tool| (*tool).to_owned()));
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

/// The capability report carries the 0.2.0 contract and the same tool sets
/// discovery lists.
#[test]
fn the_capability_report_is_contract_0_2_0_and_matches_discovery() {
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
            json!("0.2.0"),
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
#[test]
fn every_object_in_every_discovered_schema_rejects_unknown_fields() {
    fn open_objects(schema: &Value, pointer: &str, found: &mut Vec<String>) {
        let Value::Object(map) = schema else {
            return;
        };
        if map.get("type") == Some(&json!("object")) || map.contains_key("properties") {
            match map.get("additionalProperties") {
                Some(Value::Bool(false) | Value::Object(_)) => {}
                _ => found.push(pointer.to_owned()),
            }
        }
        for keyword in ["properties", "patternProperties", "$defs", "definitions"] {
            if let Some(Value::Object(named)) = map.get(keyword) {
                for (name, child) in named {
                    open_objects(child, &format!("{pointer}/{keyword}/{name}"), found);
                }
            }
        }
        for keyword in [
            "items",
            "additionalProperties",
            "not",
            "if",
            "then",
            "else",
            "contains",
        ] {
            if let Some(child @ Value::Object(_)) = map.get(keyword) {
                open_objects(child, &format!("{pointer}/{keyword}"), found);
            }
        }
        for keyword in ["oneOf", "anyOf", "allOf", "prefixItems"] {
            if let Some(Value::Array(children)) = map.get(keyword) {
                for (index, child) in children.iter().enumerate() {
                    open_objects(child, &format!("{pointer}/{keyword}/{index}"), found);
                }
            }
        }
    }
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
        open_objects(&tool["inputSchema"], &name, &mut found);
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
