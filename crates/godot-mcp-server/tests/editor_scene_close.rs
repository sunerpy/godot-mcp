//! Contract 0.3 acceptance at the MCP surface: closing a scene open in the
//! connected editor.
//!
//! What a real editor closes, and that it never closes a scene holding
//! unsaved changes, is checked by the live-editor suite; this pins what the
//! server owns against a scripted plugin: the scene it names, the checks it
//! makes before the editor is asked, and how it says that closing is missing
//! — from an editor too old to close a scene from a plugin, or from a plugin
//! that predates it.

#![cfg(target_os = "linux")]

mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use godot_mcp_bridge::protocol::mutation::MutationStatus;
use godot_mcp_domain::ErrorKind;
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

fn refused(response: &Value) -> bool {
    response.get("error").is_some() || response["result"]["isError"] == json!(true)
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

fn closing_plugin(tree: &ProjectTree, close: FakeAnswer) -> FakeEditorPlugin {
    FakeEditorPlugin::spawn(
        &rendezvous(tree),
        FakePluginScript::implementing(&["editor.state", "editor.scene.close"])
            .versions("0.3.0", "4.7.1.stable.fake")
            .describing("scene_close", json!(true))
            .answering("editor.scene.close", close),
    )
}

fn closed() -> Value {
    json!({
        "closed": "res://main.tscn",
        "open_scenes": [],
        "edited_scene": null,
        "action": "godot-mcp: editor.scene.close"
    })
}

#[test]
fn the_scene_to_close_is_named_by_its_res_path() {
    let tree = ProjectTree::new("m8-close");
    let mut session = open(&tree);
    let plugin = closing_plugin(&tree, FakeAnswer::Payload(closed()));
    let mut id = 1;
    wait_connected(&mut session, &mut id);

    for (scene, expected) in [
        (None, ""),
        (Some("levels/one.tscn"), "res://levels/one.tscn"),
        (Some("res://main.tscn"), "res://main.tscn"),
    ] {
        id += 1;
        let mut arguments = json!({ "idempotencyId": format!("close-{id}") });
        if let Some(scene) = scene {
            arguments["scene"] = json!(scene);
        }
        let answer = ok(&session.call_tool(id, "editor.scene.close", arguments));
        assert_eq!(answer["mutation_status"], "committed", "{answer}");
        assert_eq!(answer["closed"], "res://main.tscn", "{answer}");
        let asked = sent(&plugin, "editor.scene.close");
        assert_eq!(asked.last().expect("asked")["scene"], expected);
    }
    let asked_before = sent(&plugin, "editor.scene.close").len();
    for (scene, what) in [
        (json!("../outside.tscn"), "a path outside the project"),
        (json!("main.gd"), "a script"),
        (json!(""), "an empty scene"),
        (json!(3), "a number"),
    ] {
        id += 1;
        let response = session.call_tool(
            id,
            "editor.scene.close",
            json!({ "scene": scene, "idempotencyId": format!("close-bad-{id}") }),
        );
        assert_eq!(
            error_code(&response),
            json!("invalid_input"),
            "{what} is refused: {response}"
        );
    }
    id += 1;
    let without = session.call_tool(id, "editor.scene.close", json!({}));
    assert!(
        refused(&without),
        "closing needs an idempotencyId: {without}"
    );
    assert_eq!(sent(&plugin, "editor.scene.close").len(), asked_before);
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_scene_the_editor_will_not_close_stays_open() {
    let tree = ProjectTree::new("m8-close-unsaved");
    let mut session = open(&tree);
    let plugin = closing_plugin(
        &tree,
        FakeAnswer::Error(ErrorKind::Conflict, MutationStatus::NotStarted),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let response = session.call_tool(
        id,
        "editor.scene.close",
        json!({ "idempotencyId": "close-unsaved" }),
    );
    assert_eq!(error_code(&response), json!("conflict"), "{response}");
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn an_unanswered_close_is_uncertain_but_blocks_no_write() {
    let tree = ProjectTree::new("m8-close-lost");
    let mut session = open(&tree);
    let plugin = closing_plugin(&tree, FakeAnswer::Disconnect);
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let lost = session.call_tool(
        id,
        "editor.scene.close",
        json!({ "idempotencyId": "close-lost" }),
    );
    assert_eq!(error_code(&lost), json!("uncertain"), "{lost}");
    plugin.join().expect("the plugin dropped its connection");
    id += 1;
    ok(&session.call_tool(
        id,
        "file.create",
        json!({ "path": "notes/after.txt", "content": "x\n", "idempotencyId": "after-close" }),
    ));
    let _ = session.finish();
}

#[test]
fn an_editor_that_cannot_close_scenes_says_which_godot_it_runs() {
    let tree = ProjectTree::new("m8-close-44");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state"])
            .versions("0.3.0", "4.4.1.stable.official")
            .describing("scene_close", json!(false)),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let response = session.call_tool(
        id,
        "editor.scene.close",
        json!({ "idempotencyId": "close-44" }),
    );
    assert_eq!(error_code(&response), json!("unsupported"), "{response}");
    let text = response.to_string();
    assert!(
        text.contains("4.4.1.stable.official") && text.contains("4.5"),
        "the refusal names the editor's Godot and the release that can close scenes: {response}"
    );
    assert!(
        !text.contains("editor.addon.update"),
        "an update of the plugin would not help: {response}"
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_plugin_without_close_says_how_to_update_it() {
    let tree = ProjectTree::new("m8-close-old");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state"]),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let response = session.call_tool(
        id,
        "editor.scene.close",
        json!({ "idempotencyId": "close-old" }),
    );
    assert_eq!(error_code(&response), json!("unsupported"), "{response}");
    let text = response.to_string();
    assert!(
        text.contains("0.2.0") && text.contains("editor.addon.update"),
        "the refusal names the plugin's version and how to update it: {response}"
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}
