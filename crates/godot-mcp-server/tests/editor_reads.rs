//! Contract 0.3 acceptance at the MCP surface: the live editor's node and
//! class reads.
//!
//! What the add-on reads in a real editor is checked by the live-editor suite;
//! this pins what the server owns against a scripted plugin: what each tool
//! asks of the editor, its answer without an editor, and its refusal when the
//! connected plugin predates the method.

#![cfg(target_os = "linux")]

mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use godot_mcp_server::test_support::fake_editor_plugin::{
    FakeAnswer, FakeEditorPlugin, FakePluginScript,
};
use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, error_code};

fn rendezvous(tree: &ProjectTree) -> PathBuf {
    tree.root
        .join(".godot-mcp")
        .join("editor-bridge.rendezvous")
}

fn open(tree: &ProjectTree) -> StdioSession {
    StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "editor",
        godot: None,
    })
}

fn ok(response: &Value) -> Value {
    assert!(
        response.get("error").is_none() && response["result"]["isError"] != json!(true),
        "the call must succeed: {response}"
    );
    response["result"]["structuredContent"].clone()
}

fn wait_connected(session: &mut StdioSession, next_id: &mut u64) {
    let started = Instant::now();
    loop {
        *next_id += 1;
        let status = ok(&session.call_tool(*next_id, "editor.status.get", json!({})));
        if status["connected"] == json!(true) {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the plugin never connected: {status}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn sent(plugin: &FakeEditorPlugin, method: &str) -> Vec<Value> {
    plugin
        .received()
        .into_iter()
        .filter(|(received, _)| received == method)
        .map(|(_, payload)| payload)
        .collect()
}

#[test]
fn node_and_class_reads_are_answered_by_the_editor() {
    let tree = ProjectTree::new("m8-reads");
    let mut session = open(&tree);
    let node = json!({
        "scene": "res://main.tscn",
        "digest": "d".repeat(64),
        "node": { "path": "Player", "class": "CharacterBody2D", "owned": true },
        "properties": [],
        "signals": [],
        "truncated": false
    });
    let classes = json!({
        "classes": [{ "name": "CharacterBody2D", "kind": "engine", "parent": "PhysicsBody2D",
                      "instantiable": true, "path": null }],
        "total": 1,
        "truncated": false
    });
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state", "editor.node", "editor.types.search"])
            .answering(
                "editor.state",
                FakeAnswer::Payload(json!({ "open_scenes": [], "open_scripts": [] })),
            )
            .answering("editor.node", FakeAnswer::Payload(node.clone()))
            .answering("editor.types.search", FakeAnswer::Payload(classes.clone())),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);

    id += 1;
    let read = ok(&session.call_tool(
        id,
        "editor.node.get",
        json!({ "scene": "main.tscn", "node": "Player", "properties": ["position", "velocity"] }),
    ));
    assert_eq!(read, node);
    assert_eq!(
        sent(&plugin, "editor.node"),
        vec![json!({
            "scene": "res://main.tscn",
            "node": "Player",
            "properties": ["position", "velocity"]
        })]
    );
    id += 1;
    ok(&session.call_tool(id, "editor.node.get", json!({})));
    assert_eq!(
        sent(&plugin, "editor.node")[1],
        json!({ "scene": "", "node": ".", "properties": null }),
        "the edited scene's root, every property"
    );

    id += 1;
    let found = ok(&session.call_tool(
        id,
        "editor.types.search",
        json!({ "query": "Body", "base": "Node2D", "instantiable": true, "limit": 20 }),
    ));
    assert_eq!(found, classes);
    assert_eq!(
        sent(&plugin, "editor.types.search"),
        vec![json!({ "query": "Body", "base": "Node2D", "instantiable": true, "limit": 20 })]
    );
    id += 1;
    ok(&session.call_tool(id, "editor.types.search", json!({})));
    assert_eq!(
        sent(&plugin, "editor.types.search")[1],
        json!({ "query": "", "base": null, "instantiable": null, "limit": 100 })
    );

    // Out-of-range input is refused before the editor is asked.
    id += 1;
    let refused = session.call_tool(id, "editor.types.search", json!({ "limit": 501 }));
    assert!(refused.get("error").is_some(), "{refused}");
    id += 1;
    let refused = session.call_tool(id, "editor.node.get", json!({ "node": "../Outside" }));
    assert_eq!(error_code(&refused), json!("invalid_input"), "{refused}");
    assert_eq!(sent(&plugin, "editor.node").len(), 2);
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_plugin_without_the_reads_says_how_to_update_it() {
    let tree = ProjectTree::new("m8-reads-old");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state"]).answering(
            "editor.state",
            FakeAnswer::Payload(json!({ "open_scenes": [], "open_scripts": [] })),
        ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    for tool in ["editor.node.get", "editor.types.search"] {
        id += 1;
        let refused = session.call_tool(id, tool, json!({}));
        assert_eq!(
            error_code(&refused),
            json!("unsupported"),
            "{tool}: {refused}"
        );
        let text = refused.to_string();
        assert!(
            text.contains("0.2.0") && text.contains("editor.addon.update"),
            "{tool}: the refusal names the plugin's version and how to update it: {refused}"
        );
    }
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn without_an_editor_the_reads_say_so() {
    let tree = ProjectTree::new("m8-reads-absent");
    let mut session = open(&tree);
    for (id, tool) in [(2, "editor.node.get"), (3, "editor.types.search")] {
        let refused = session.call_tool(id, tool, json!({}));
        assert_eq!(
            error_code(&refused),
            json!("unavailable"),
            "{tool}: {refused}"
        );
    }
    let _ = session.finish();
}

#[test]
fn the_status_says_whether_the_connected_plugin_is_current() {
    for (version, update_available) in [
        ("0.2.0", true),
        (godot_mcp_bridge::addon::ADDON_VERSION, false),
    ] {
        let tree = ProjectTree::new("m8-status-addon");
        let mut session = open(&tree);
        let plugin = FakeEditorPlugin::spawn(
            &rendezvous(&tree),
            FakePluginScript::implementing(&["editor.state"])
                .versions(version, "4.7.1.stable.fake")
                .answering(
                    "editor.state",
                    FakeAnswer::Payload(json!({ "open_scenes": [], "open_scripts": [] })),
                ),
        );
        let mut id = 1;
        wait_connected(&mut session, &mut id);
        id += 1;
        let status = ok(&session.call_tool(id, "editor.status.get", json!({})));
        assert_eq!(
            status["editor"]["addon"],
            json!({
                "version": version,
                "bundled_version": godot_mcp_bridge::addon::ADDON_VERSION,
                "update_available": update_available,
            }),
            "{status}"
        );
        assert_eq!(
            status["editor"]["open_scenes"],
            json!([]),
            "the editor's own state stays: {status}"
        );
        let _ = session.finish();
        plugin.join().expect("the plugin ended");
    }
}
