//! M4 acceptance at the MCP surface: the live-editor plane through the
//! production stdio assembly, against a scripted editor plugin that speaks the
//! shipped bridge protocol.
//!
//! What a real editor does is exercised against exact Godot builds by
//! `scripts/real_client/m4_smoke.py`; this suite pins what the server owns:
//! backend selection for a scene open in the editor, the journal and replay
//! around an editor change, an unanswered change that is never resent, the
//! refusal of a headless write to a file the editor holds, and the tools'
//! answers while no editor is connected.

#![cfg(target_os = "linux")]

mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use godot_mcp_server::test_support::fake_editor_plugin::{
    FakeAnswer, FakeEditorPlugin, FakePluginScript,
};
use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, error_code, sha256_hex};

const SCENE: &str =
    "[gd_scene format=3 uid=\"uid://b7rq2x1yb4kfl\"]\n\n[node name=\"Main\" type=\"Node2D\"]\n";

fn rendezvous(tree: &ProjectTree) -> PathBuf {
    tree.root
        .join(".godot-mcp")
        .join("editor-bridge.rendezvous")
}

fn open(tree: &ProjectTree) -> StdioSession {
    StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
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

fn editor_state_with_open_scene() -> Value {
    json!({
        "open_scenes": [{ "path": "res://scenes/main.tscn", "edited": true, "unsaved": false }],
        "open_scripts": [],
        "edited_scene": "res://scenes/main.tscn",
        "selection": [],
        "history": { "version": 1, "has_undo": false, "has_redo": false, "current_action": "" },
        "playing": false
    })
}

/// Call `editor.status.get` until the plugin is connected.
fn wait_connected(session: &mut StdioSession, next_id: &mut u64) -> Value {
    let started = Instant::now();
    loop {
        *next_id += 1;
        let status = ok(&session.call_tool(*next_id, "editor.status.get", json!({})));
        if status["connected"] == json!(true) {
            return status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the plugin never connected: {status}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn methods(plugin: &FakeEditorPlugin, method: &str) -> usize {
    plugin
        .received()
        .iter()
        .filter(|(received, _)| received == method)
        .count()
}

#[test]
fn a_scene_open_in_the_editor_is_edited_there_as_one_journaled_change() {
    let tree = ProjectTree::new("m4-editor-batch");
    tree.write("scenes/main.tscn", SCENE);
    let mut session = open(&tree);
    let saved = format!("{SCENE}\n[node name=\"Batched\" type=\"Node\" parent=\".\"]\n");
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state", "editor.scene.batch"])
            .answering(
                "editor.state",
                FakeAnswer::Payload(editor_state_with_open_scene()),
            )
            .answering(
                "editor.scene.batch",
                FakeAnswer::SaveThenAnswer {
                    path: tree.root.join("scenes/main.tscn"),
                    contents: saved.clone().into_bytes(),
                    payload: json!({
                        "path": "res://scenes/main.tscn",
                        "saved": true,
                        "action": "godot-mcp: scene.batch.apply",
                        "version": 2,
                        "digest": "live-digest"
                    }),
                },
            ),
    );
    let mut id = 1;
    let status = wait_connected(&mut session, &mut id);
    assert_eq!(status["editor"]["edited_scene"], "res://scenes/main.tscn");

    let arguments = json!({
        "path": "scenes/main.tscn",
        "expectedSha256": sha256_hex(SCENE.as_bytes()),
        "operations": [{ "op": "add_node", "parent": ".", "name": "Batched", "type": "Node" }],
        "idempotencyId": "editor-batch-1"
    });
    id += 1;
    let applied = ok(&session.call_tool(id, "scene.batch.apply", arguments.clone()));
    assert_eq!(applied["backend"], "editor", "{applied}");
    assert_eq!(applied["conclusion"], "proven_post_state", "{applied}");
    assert_eq!(applied["before_sha256"], sha256_hex(SCENE.as_bytes()));
    assert_eq!(applied["after_sha256"], sha256_hex(saved.as_bytes()));
    assert_eq!(applied["editor"]["action"], "godot-mcp: scene.batch.apply");
    assert!(
        applied["tree"]["nodes"].to_string().contains("Batched"),
        "the answer carries the saved file's tree: {applied}"
    );
    let (_, sent) = plugin
        .received()
        .into_iter()
        .find(|(method, _)| method == "editor.scene.batch")
        .expect("the editor received the batch");
    assert_eq!(sent["path"], "res://scenes/main.tscn");
    assert_eq!(sent["expected_sha256"], sha256_hex(SCENE.as_bytes()));
    assert_eq!(sent["operations"][0]["name"], "Batched");

    // An exact retry answers from the journal; the editor is not asked again.
    id += 1;
    let replay = ok(&session.call_tool(id, "scene.batch.apply", arguments));
    assert_eq!(replay["replayed"], json!(true), "{replay}");
    assert_eq!(methods(&plugin, "editor.scene.batch"), 1);
    let _ = session.finish();
    plugin.join().expect("the plugin saw the server stop");
}

#[test]
fn a_lost_editor_batch_is_uncertain_blocks_writes_and_is_never_resent() {
    let tree = ProjectTree::new("m4-editor-lost");
    tree.write("scenes/main.tscn", SCENE);
    let mut session = open(&tree);
    let first = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state", "editor.scene.batch"])
            .answering(
                "editor.state",
                FakeAnswer::Payload(editor_state_with_open_scene()),
            )
            .answering("editor.scene.batch", FakeAnswer::Disconnect),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    let arguments = json!({
        "path": "scenes/main.tscn",
        "expectedSha256": sha256_hex(SCENE.as_bytes()),
        "operations": [{ "op": "add_node", "parent": ".", "name": "Lost", "type": "Node" }],
        "idempotencyId": "editor-lost-1"
    });
    id += 1;
    let lost = session.call_tool(id, "scene.batch.apply", arguments.clone());
    assert_eq!(error_code(&lost), json!("uncertain"), "{lost}");
    first
        .join()
        .expect("the first plugin dropped its connection");

    // Whether the editor wrote the scene is not decidable: writes stay blocked.
    id += 1;
    let blocked = session.call_tool(
        id,
        "file.create",
        json!({ "path": "notes/after.txt", "content": "x\n", "idempotencyId": "after-lost" }),
    );
    assert!(
        blocked.get("error").is_some() || blocked["result"]["isError"] == json!(true),
        "a quarantined editor change blocks every later write: {blocked}"
    );
    assert!(!tree.root.join("notes/after.txt").exists());

    // The editor reconnects: it is described afresh and sent nothing else, and
    // the same request answers from the journal without reaching it.
    let second = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state", "editor.scene.batch"]).answering(
            "editor.state",
            FakeAnswer::Payload(editor_state_with_open_scene()),
        ),
    );
    wait_connected(&mut session, &mut id);
    id += 1;
    let retried = session.call_tool(id, "scene.batch.apply", arguments);
    assert!(
        retried.get("error").is_some() || retried["result"]["isError"] == json!(true),
        "the retry answers the recorded outcome: {retried}"
    );
    assert_eq!(
        methods(&second, "editor.scene.batch"),
        0,
        "an unresolved editor change is never sent again"
    );
    assert_eq!(
        std::fs::read_to_string(tree.root.join("scenes/main.tscn")).expect("scene"),
        SCENE
    );
    let _ = session.finish();
    second.join().expect("the second plugin ended");
}

#[test]
fn a_headless_write_to_a_file_the_editor_holds_is_refused() {
    let tree = ProjectTree::new("m4-editor-held");
    tree.write("scenes/main.tscn", SCENE);
    tree.write("notes/free.txt", "free\n");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state"]).answering(
            "editor.state",
            FakeAnswer::Payload(editor_state_with_open_scene()),
        ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let held = session.call_tool(
        id,
        "file.patch",
        json!({
            "path": "scenes/main.tscn",
            "expectedSha256": sha256_hex(SCENE.as_bytes()),
            "change": { "content": format!("{SCENE}; edited around the editor\n") },
            "idempotencyId": "held-1"
        }),
    );
    assert_eq!(error_code(&held), json!("conflict"), "{held}");
    assert!(
        held.to_string().contains("open in the connected editor"),
        "{held}"
    );
    assert_eq!(
        std::fs::read_to_string(tree.root.join("scenes/main.tscn")).expect("scene"),
        SCENE
    );
    id += 1;
    let free = session.call_tool(
        id,
        "file.patch",
        json!({
            "path": "notes/free.txt",
            "expectedSha256": sha256_hex(b"free\n"),
            "change": { "content": "still free\n" },
            "idempotencyId": "free-1"
        }),
    );
    ok(&free);
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn an_unanswered_editor_state_change_is_uncertain_but_blocks_no_write() {
    let tree = ProjectTree::new("m4-editor-state");
    tree.write("scenes/main.tscn", SCENE);
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state", "editor.selection.set"])
            .answering(
                "editor.state",
                FakeAnswer::Payload(json!({ "open_scenes": [], "open_scripts": [] })),
            )
            .answering("editor.selection.set", FakeAnswer::Disconnect),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let lost = session.call_tool(
        id,
        "editor.selection.set",
        json!({ "nodes": ["."], "idempotencyId": "select-1" }),
    );
    assert_eq!(error_code(&lost), json!("uncertain"), "{lost}");
    plugin.join().expect("the plugin dropped its connection");
    // No project file was in scope, so nothing blocks the next write.
    id += 1;
    ok(&session.call_tool(
        id,
        "file.create",
        json!({ "path": "notes/after.txt", "content": "x\n", "idempotencyId": "after-select" }),
    ));
    let _ = session.finish();
}

#[test]
fn without_an_editor_the_editor_tools_say_so() {
    let tree = ProjectTree::new("m4-editor-absent");
    tree.write("scenes/main.tscn", SCENE);
    let mut session = open(&tree);
    let status = ok(&session.call_tool(2, "editor.status.get", json!({})));
    assert_eq!(status["connected"], json!(false), "{status}");
    assert_eq!(status["editor"], Value::Null);
    let tree_read = session.call_tool(3, "editor.scene.tree.get", json!({}));
    assert_eq!(error_code(&tree_read), json!("unavailable"), "{tree_read}");
    assert!(tree_read.to_string().contains("editor"), "{tree_read}");
    let undo = session.call_tool(4, "editor.undo", json!({ "idempotencyId": "undo-1" }));
    assert_eq!(error_code(&undo), json!("unavailable"), "{undo}");
    // A scene batch without an editor stays on the headless path.
    let batch = session.call_tool(
        5,
        "scene.batch.apply",
        json!({
            "path": "scenes/main.tscn",
            "expectedSha256": sha256_hex(SCENE.as_bytes()),
            "operations": [{ "op": "add_node", "parent": ".", "name": "X", "type": "Node" }],
            "idempotencyId": "headless-1"
        }),
    );
    assert_eq!(error_code(&batch), json!("not_found"), "{batch}");
    assert!(
        batch.to_string().contains("no Godot executable was found"),
        "no Godot binary is configured, so the headless helper cannot run: {batch}"
    );
    let _ = session.finish();
}
