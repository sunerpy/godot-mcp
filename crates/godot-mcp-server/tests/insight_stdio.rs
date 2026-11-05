//! The project-wide static views over the production stdio assembly.
//!
//! One `tools/call` per view against a copy of the analysis crate's `insight`
//! fixture project, sent exactly as an MCP client sends it, plus the `glob`
//! filter of `project.files.list`. The advertised input schemas are read from
//! `tools/list` and every call uses only what they declare. No Godot process is
//! started.

mod support;

use std::path::Path;

use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, error_code, structured};

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("godot-mcp-analysis")
        .join("tests")
        .join("fixtures")
        .join("insight")
        .join("project")
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create directory");
    for entry in std::fs::read_dir(from).expect("read fixture directory") {
        let entry = entry.expect("fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("entry type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}

fn project() -> ProjectTree {
    let tree = ProjectTree::new("insight");
    copy_tree(&fixture(), &tree.root);
    tree
}

/// The answer of a successful call, refusing a tool error too.
fn ok(response: &Value) -> &Value {
    assert_ne!(
        response["result"]["isError"],
        json!(true),
        "the call must succeed: {response}"
    );
    structured(response)
}

#[test]
fn every_project_wide_view_answers_over_stdio() {
    let tree = project();
    let mut session = StdioSession::initialized(&Spawn::core(&tree.root));

    let listed = session.request(2, "tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().expect("tools");
    let schema = |name: &str| -> Value {
        tools
            .iter()
            .find(|tool| tool["name"] == json!(name))
            .unwrap_or_else(|| panic!("{name} is advertised in the core profile"))["inputSchema"]
            .clone()
    };
    assert_eq!(
        schema("project.dependencies.audit")["properties"]["view"]["enum"],
        json!(["unused", "cycles"])
    );
    assert_eq!(schema("project.search")["required"], json!(["query"]));
    assert_eq!(schema("scene.exports.get")["required"], json!(["scene"]));
    assert_eq!(
        schema("project.signals.get")["properties"]["view"]["enum"],
        json!(["signals", "problems", "undeclared_connections"])
    );

    let audit = session.call_tool(3, "project.dependencies.audit", json!({ "limit": 2 }));
    let audit = ok(&audit);
    assert_eq!(audit["data"]["summary"]["unused"], json!(10), "{audit}");
    assert_eq!(audit["data"]["page"]["next_offset"], json!(2), "{audit}");
    assert_eq!(
        audit["provenance"]["confidence"],
        json!("inferred"),
        "{audit}"
    );

    let metrics = session.call_tool(
        4,
        "project.metrics.get",
        json!({ "view": "complexity", "limit": 1 }),
    );
    let metrics = ok(&metrics);
    assert_eq!(
        metrics["data"]["functions"][0]["name"],
        json!("take_hit"),
        "{metrics}"
    );

    let search = session.call_tool(
        5,
        "project.search",
        json!({ "query": ".emit(", "include": ["scripts/**"] }),
    );
    let search = ok(&search);
    assert_eq!(search["data"]["page"]["total"], json!(4), "{search}");
    assert_eq!(
        search["data"]["matches"][0]["path"],
        json!("scripts/game_state.gd"),
        "{search}"
    );

    let signals = session.call_tool(6, "project.signals.get", json!({ "view": "problems" }));
    let signals = ok(&signals);
    assert_eq!(
        signals["data"]["problems"][0]["kind"],
        json!("missing_handler"),
        "{signals}"
    );

    let uid = session.call_tool(
        7,
        "project.uid.get",
        json!({ "uid": "uid://bmainscene001" }),
    );
    let uid = ok(&uid);
    assert_eq!(uid["data"]["value"], json!("main.tscn"), "{uid}");

    let exports = session.call_tool(
        8,
        "scene.exports.get",
        json!({ "scene": "res://main.tscn", "node": "Player" }),
    );
    let exports = ok(&exports);
    assert_eq!(
        exports["data"]["exports"][0]["node_path"],
        json!("Player"),
        "{exports}"
    );

    let nodes = session.call_tool(9, "scene.nodes.find", json!({ "group": "foes" }));
    let nodes = ok(&nodes);
    assert_eq!(nodes["data"]["page"]["total"], json!(3), "{nodes}");

    let refused = session.call_tool(10, "project.search", json!({ "query": "a", "context": 11 }));
    assert_eq!(error_code(&refused), json!("invalid_input"), "{refused}");
    let undeclared = session.call_tool(11, "project.uid.get", json!({ "uuid": "x" }));
    assert_eq!(
        error_code(&undeclared),
        json!("invalid_input"),
        "{undeclared}"
    );

    assert!(
        !session.transcript.contains(&*tree.root.to_string_lossy()),
        "no answer names the local project path"
    );
    let exited = session.finish();
    assert_eq!(
        exited.code,
        Some(0),
        "the server exits cleanly: {}",
        exited.stderr
    );
}

#[test]
fn files_list_filters_by_glob() {
    let tree = project();
    let mut session = StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "editor",
        godot: None,
    });
    let listed = session.call_tool(
        2,
        "project.files.list",
        json!({ "glob": "scripts/*.gd.uid" }),
    );
    let listed = ok(&listed);
    let paths: Vec<&str> = listed["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|file| file["path"].as_str().expect("path"))
        .collect();
    assert_eq!(
        paths,
        vec![
            "scripts/boss.gd.uid",
            "scripts/enemy.gd.uid",
            "scripts/game_state.gd.uid",
            "scripts/player.gd.uid",
        ],
        "{listed}"
    );
    let basename = session.call_tool(
        3,
        "project.files.list",
        json!({ "glob": "*.import", "prefix": "art" }),
    );
    let basename = ok(&basename);
    assert_eq!(
        basename["files"][0]["path"],
        json!("art/unused.png.import"),
        "{basename}"
    );
    let refused = session.call_tool(4, "project.files.list", json!({ "glob": "[" }));
    assert_eq!(error_code(&refused), json!("invalid_input"), "{refused}");
    let exited = session.finish();
    assert_eq!(
        exited.code,
        Some(0),
        "the server exits cleanly: {}",
        exited.stderr
    );
}
