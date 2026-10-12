use std::collections::{BTreeMap, BTreeSet};

use assert_cmd::Command;
use serde_json::{Map, Value, json};

const TOOLS_LIST_EXCHANGE: &str = concat!(
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"tool-schema-test","version":"0.1.0"}}}"#,
    "\n",
    r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
    "\n",
    r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    "\n",
);

const STDIO_HANDSHAKE: &str = concat!(
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"tool-schema-test","version":"0.1.0"}}}"#,
    "\n",
    r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
    "\n",
);

const LIVE_TOOLS: [&str; 73] = [
    "asset.delete",
    "asset.put",
    "asset.status",
    "capabilities.get",
    "debug.command",
    "debug.session.get",
    "editor.addon.install",
    "editor.addon.reload",
    "editor.addon.remove",
    "editor.addon.status",
    "editor.addon.update",
    "editor.redo",
    "editor.scene.open",
    "editor.scene.tree.get",
    "editor.selection.set",
    "editor.session.start",
    "editor.session.stop",
    "editor.status.get",
    "editor.types.get",
    "editor.undo",
    "export.presets.list",
    "file.create",
    "file.delete",
    "file.get",
    "file.move",
    "file.patch",
    "gdscript.diagnostics.get",
    "gdscript.impact.get",
    "gdscript.references.get",
    "gdscript.symbols.get",
    "health.get",
    "input.action.send",
    "input.key.send",
    "input.mouse.send",
    "input.sequence.send",
    "project.dependencies.get",
    "project.discover",
    "project.export",
    "project.export.cancel",
    "project.export.status",
    "project.files.list",
    "project.import.run",
    "project.info.get",
    "project.run",
    "project.run.logs",
    "project.run.start",
    "project.run.status",
    "project.run.stop",
    "project.settings.get",
    "project.settings.patch",
    "project.test",
    "project.uid.update",
    "project.validate",
    "resource.batch.apply",
    "resource.create",
    "resource.dependencies.get",
    "resource.tree.get",
    "resource.update",
    "runtime.properties.get",
    "runtime.recording.capture",
    "runtime.screenshot.capture",
    "runtime.signals.get",
    "runtime.tree.get",
    "runtime.ui.get",
    "scene.batch.apply",
    "scene.create",
    "scene.dependencies.get",
    "scene.save",
    "scene.tree.get",
    "script.check",
    "script.create",
    "script.patch",
    "script.validate",
];

fn cli() -> Command {
    let mut command = Command::cargo_bin("godot-mcp").expect("godot-mcp binary");
    for (name, _) in std::env::vars().filter(|(name, _)| name.starts_with("GODOT_MCP_")) {
        command.env_remove(name);
    }
    command
}

fn empty_project(root: &std::path::Path) -> std::path::PathBuf {
    let project = root.join("project");
    std::fs::create_dir_all(&project).expect("create project directory");
    std::fs::write(project.join("project.godot"), b"config_version=5\n")
        .expect("write project.godot");
    project
}

fn listed_schemas(stdout: &[u8]) -> BTreeMap<String, Map<String, Value>> {
    let response = String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|frame| frame["id"] == json!(2))
        .unwrap_or_else(|| {
            panic!(
                "tools/list response was absent from stdout: {}",
                String::from_utf8_lossy(stdout)
            )
        });
    response["result"]["tools"]
        .as_array()
        .expect("tools/list result contains tools")
        .iter()
        .map(|tool| {
            let name = tool["name"].as_str().expect("tool name").to_owned();
            let schema = tool["inputSchema"]
                .as_object()
                .unwrap_or_else(|| panic!("{name} inputSchema must be an object"))
                .clone();
            (name, schema)
        })
        .collect()
}

fn property_keys(schemas: &BTreeMap<String, Map<String, Value>>, name: &str) -> BTreeSet<String> {
    schemas[name]["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("{name} properties must be an object"))
        .keys()
        .cloned()
        .collect()
}

fn idempotency_property() -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "description": "Non-empty identifier that makes this call replay-safe."
    })
}

fn call_tool(project: &std::path::Path, name: &str, arguments: Value) -> Value {
    let request = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    });
    let exchange = format!("{STDIO_HANDSHAKE}{request}\n");
    let output = cli()
        .args(["serve", "--stdio", "--project"])
        .arg(project)
        .args(["--profile", "full"])
        .write_stdin(exchange)
        .output()
        .expect("run the shipped CLI");
    assert!(
        output.status.success(),
        "CLI failed: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|frame| frame["id"] == json!(2))
        .unwrap_or_else(|| {
            panic!(
                "tools/call response was absent from stdout: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
}

#[test]
fn tools_list_from_shipped_binary_advertises_live_argument_schemas() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = empty_project(temp.path());
    let output = cli()
        .args(["serve", "--stdio", "--project"])
        .arg(&project)
        .args(["--profile", "full"])
        .write_stdin(TOOLS_LIST_EXCHANGE)
        .output()
        .expect("run the shipped CLI");
    assert!(
        output.status.success(),
        "CLI failed: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let schemas = listed_schemas(&output.stdout);
    // Contract 0.2: the full profile lists exactly the implemented catalog. The
    // 27 version 0.1 planned methods without handlers are no longer listed.
    assert_eq!(
        schemas.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        LIVE_TOOLS.into_iter().collect::<BTreeSet<_>>(),
        "full profile must list exactly the implemented tools"
    );
    for (name, schema) in &schemas {
        assert_eq!(
            schema["properties"]["idempotencyId"],
            idempotency_property(),
            "{name} must advertise the kernel-owned idempotency argument"
        );
    }
    for name in LIVE_TOOLS {
        let schema = schemas
            .get(name)
            .unwrap_or_else(|| panic!("live tool {name} must be advertised"));
        assert_eq!(schema["type"], json!("object"), "{name} object type");
        assert_eq!(
            schema["additionalProperties"],
            json!(false),
            "{name} must refuse unknown properties"
        );
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{name} must advertise properties"));
        if name != "capabilities.get" {
            assert!(
                schema["required"].is_array(),
                "{name} required must be explicit"
            );
            for (key, property) in properties {
                assert!(property["type"].is_string(), "{name}.{key} needs a type");
                assert!(
                    property["description"].is_string(),
                    "{name}.{key} needs a description"
                );
            }
        }
    }

    for (name, expected) in [
        (
            "gdscript.symbols.get",
            &["idempotencyId", "limit", "offset", "script"][..],
        ),
        (
            "gdscript.references.get",
            &["idempotencyId", "limit", "offset", "script", "symbol"][..],
        ),
        (
            "scene.tree.get",
            &["idempotencyId", "limit", "offset", "scene"][..],
        ),
        (
            "resource.tree.get",
            &["idempotencyId", "limit", "offset", "resource"][..],
        ),
    ] {
        assert_eq!(
            property_keys(&schemas, name),
            expected.iter().map(|key| (*key).to_owned()).collect(),
            "{name} must expose the handler's path key"
        );
    }
    for (name, key) in [
        ("gdscript.symbols.get", "script"),
        ("scene.tree.get", "scene"),
        ("resource.tree.get", "resource"),
        ("project.run", "main_scene"),
        ("resource.update", "target"),
    ] {
        assert_eq!(
            schemas[name]["properties"][key]["minLength"],
            json!(1),
            "{name}.{key} must advertise its non-empty path constraint"
        );
    }
    assert_eq!(
        schemas["gdscript.references.get"]["properties"]["symbol"]["description"],
        json!("Optional function, signal, or script-class name declared by the script.")
    );
    assert_eq!(
        schemas["gdscript.impact.get"]["properties"]["limit"]["description"],
        json!("Maximum number of entries returned in each result set.")
    );

    // Contract 0.2 lists `debug.session.get` now that a managed editor
    // session's launch receipt answers it: its one argument names that session.
    assert_eq!(
        property_keys(&schemas, "debug.session.get"),
        ["idempotencyId", "session"]
            .iter()
            .map(|key| (*key).to_owned())
            .collect(),
        "debug.session.get takes only the editor session it reports"
    );
    assert_eq!(schemas["debug.session.get"]["required"], json!([]));
    // A debug command is a journaled mutation: its handler refuses a call
    // without an idempotency id, and its schema says so.
    assert_eq!(
        schemas["debug.command"]["required"],
        json!(["session", "command", "idempotencyId"])
    );
    assert_eq!(schemas["gdscript.diagnostics.get"]["required"], json!([]));

    let live: BTreeSet<_> = LIVE_TOOLS.into_iter().collect();
    let stubs: Vec<_> = schemas
        .iter()
        .filter(|(name, _)| !live.contains(name.as_str()))
        .collect();
    assert_eq!(
        stubs.len(),
        0,
        "contract 0.2 lists no handlerless stub: {stubs:?}"
    );
    for (name, schema) in stubs {
        assert_eq!(
            schema,
            &json!({
                "type": "object",
                "properties": { "idempotencyId": idempotency_property() }
            })
            .as_object()
            .unwrap()
            .clone(),
            "unimplemented tool {name} must advertise only the kernel-owned argument"
        );
    }
}

#[test]
fn shipped_binary_rejects_invalid_main_scene_before_engine_discovery() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = empty_project(temp.path());

    for main_scene in ["", "../outside.tscn", "/etc/passwd", "--editor"] {
        let response = call_tool(&project, "project.run", json!({ "main_scene": main_scene }));
        let rendered = response.to_string();
        assert_eq!(response["error"]["data"]["code"], json!("invalid_input"));
        assert!(
            rendered.contains(
                "path must be non-empty, project-relative without parent traversal, and not option-shaped"
            ),
            "{main_scene:?} must name the path constraint: {response}"
        );
        assert!(
            !rendered.contains("no Godot executable was found"),
            "{main_scene:?} reached engine discovery: {response}"
        );
    }
}
