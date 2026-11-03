//! Contract 0.3 acceptance at the MCP surface: editing the scene the editor
//! holds, and saving it.
//!
//! `editor.scene.edit` changes the editor's copy of a scene — unsaved changes
//! included — as one undoable action and saves nothing, so it is editor state
//! only. `editor.scene.save` writes scene files, so the files it saves are in
//! its transaction's scope, and so is everything else the editor may write
//! with them. What a real editor does is checked by the live-editor suite.

#![cfg(target_os = "linux")]

mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use godot_mcp_server::test_support::fake_editor_plugin::{
    FakeAnswer, FakeEditorPlugin, FakePluginScript,
};
use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, error_code, sha256_hex};

const MAIN: &str = "[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node2D\"]\n";
const OTHER: &str = "[gd_scene format=3]\n\n[node name=\"Other\" type=\"Node3D\"]\n";

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

fn editor_state() -> Value {
    json!({
        "open_scenes": [
            { "path": "res://main.tscn", "edited": true, "unsaved": true },
            { "path": "res://other.tscn", "edited": false, "unsaved": null }
        ],
        "open_scripts": [],
        "edited_scene": "res://main.tscn",
        "selection": [],
        "history": { "version": 3, "has_undo": true, "has_redo": false, "current_action": "" },
        "playing": false
    })
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

/// The durable record of the transaction an answer names.
fn recorded_transaction(
    tree: &ProjectTree,
    answer: &Value,
) -> godot_mcp_server::operator::RepairTarget {
    let text = answer.to_string();
    let transaction_id = text
        .split(|character: char| !(character.is_ascii_hexdigit() || character == '-'))
        .find(|token| token.len() == 36 && token.matches('-').count() == 4)
        .unwrap_or_else(|| panic!("the answer names its transaction: {text}"))
        .to_owned();
    godot_mcp_server::operator::OperatorSession::open(&tree.root)
        .expect("an operator session over the stopped server's journal")
        .load(&transaction_id)
        .expect("the transaction is journaled")
}

fn edit(digest: &str, idempotency_id: &str) -> Value {
    json!({
        "expectedDigest": digest,
        "operations": [{ "op": "add_node", "parent": ".", "name": "Live", "type": "Node2D" }],
        "idempotencyId": idempotency_id
    })
}

#[test]
fn a_live_edit_changes_the_editor_copy_and_writes_no_file() {
    let tree = ProjectTree::new("m8-live-edit");
    tree.write("main.tscn", MAIN);
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state", "editor.scene.edit"])
            .answering("editor.state", FakeAnswer::Payload(editor_state()))
            .answering(
                "editor.scene.edit",
                FakeAnswer::Payload(json!({
                    "scene": "res://main.tscn",
                    "action": "godot-mcp: add a node",
                    "history_version": 4,
                    "digest": "e".repeat(64),
                    "unsaved": true
                })),
            ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    let digest = "a".repeat(64);
    let mut arguments = edit(&digest, "live-edit-1");
    arguments["label"] = json!("add a node");
    id += 1;
    let edited = ok(&session.call_tool(id, "editor.scene.edit", arguments.clone()));
    assert_eq!(edited["digest"], json!("e".repeat(64)), "{edited}");
    assert_eq!(edited["mutation_status"], "committed", "{edited}");
    assert_eq!(
        sent(&plugin, "editor.scene.edit")[0]
            .as_object()
            .map(|payload| {
                let mut payload = payload.clone();
                payload.remove("transaction_id");
                Value::Object(payload)
            }),
        Some(json!({
            "scene": "",
            "expected_digest": digest,
            "operations": [{ "op": "add_node", "parent": ".", "name": "Live", "type": "Node2D" }],
            "label": "godot-mcp: add a node"
        }))
    );
    assert_eq!(
        std::fs::read_to_string(tree.root.join("main.tscn")).expect("scene"),
        MAIN
    );

    // An exact retry answers from the journal.
    id += 1;
    let replay = ok(&session.call_tool(id, "editor.scene.edit", arguments));
    assert_eq!(replay["replayed"], json!(true), "{replay}");
    assert_eq!(sent(&plugin, "editor.scene.edit").len(), 1);

    // A node handle names file bytes, not the editor's copy.
    id += 1;
    let handle = session.call_tool(
        id,
        "editor.scene.edit",
        json!({
            "expectedDigest": digest,
            "operations": [{ "op": "remove_node", "node": "nh1:abc" }],
            "idempotencyId": "live-edit-handle"
        }),
    );
    assert_eq!(error_code(&handle), json!("invalid_input"), "{handle}");
    // The digest is required and exact.
    id += 1;
    let short = session.call_tool(id, "editor.scene.edit", edit("abc", "live-edit-short"));
    assert!(refused(&short), "{short}");
    assert_eq!(sent(&plugin, "editor.scene.edit").len(), 1);
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_lost_live_edit_is_uncertain_and_blocks_no_write() {
    let tree = ProjectTree::new("m8-live-edit-lost");
    tree.write("main.tscn", MAIN);
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state", "editor.scene.edit"])
            .answering("editor.state", FakeAnswer::Payload(editor_state()))
            .answering("editor.scene.edit", FakeAnswer::Disconnect),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let lost = session.call_tool(
        id,
        "editor.scene.edit",
        edit(&"a".repeat(64), "live-edit-lost"),
    );
    assert_eq!(error_code(&lost), json!("uncertain"), "{lost}");
    plugin.join().expect("the plugin dropped its connection");
    // The edit wrote no file, so an unrelated write goes ahead.
    id += 1;
    ok(&session.call_tool(
        id,
        "file.create",
        json!({ "path": "notes/after.txt", "content": "x\n", "idempotencyId": "after-edit" }),
    ));
    let _ = session.finish();
}

#[test]
fn saving_declares_every_scene_it_writes_and_everything_else_in_doubt() {
    let tree = ProjectTree::new("m8-live-save");
    tree.write("main.tscn", MAIN);
    tree.write("other.tscn", OTHER);
    let mut session = open(&tree);
    let saved = format!("{MAIN}\n[node name=\"Live\" type=\"Node2D\" parent=\".\"]\n");
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state", "editor.scene.save"])
            .answering("editor.state", FakeAnswer::Payload(editor_state()))
            .answering(
                "editor.scene.save",
                FakeAnswer::SaveThenAnswer {
                    path: tree.root.join("main.tscn"),
                    contents: saved.clone().into_bytes(),
                    payload: json!({
                        "saved": [{ "path": "res://main.tscn" }],
                        "also_saved": [{ "path": "res://materials/shared.tres" }],
                        "action": "godot-mcp: editor.scene.save"
                    }),
                },
            ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let before = ok(&session.call_tool(id, "project.info.get", json!({})))["workspace"]["revision"]
        .as_u64()
        .expect("revision");

    id += 1;
    let answer = ok(&session.call_tool(
        id,
        "editor.scene.save",
        json!({ "idempotencyId": "live-save-1" }),
    ));
    assert_eq!(
        sent(&plugin, "editor.scene.save")[0]["scenes"],
        json!([{ "path": "res://main.tscn", "sha256": sha256_hex(MAIN.as_bytes()) }]),
        "the edited scene, with the bytes its file holds now"
    );
    assert_eq!(sent(&plugin, "editor.scene.save")[0]["all"], json!(false));
    assert_eq!(
        answer["also_saved"],
        json!([{ "path": "res://materials/shared.tres" }]),
        "{answer}"
    );
    assert!(
        answer["workspace"]["revision"].as_u64().expect("revision") > before,
        "the saved file is in the answer's revision: {answer}"
    );
    assert_eq!(
        std::fs::read_to_string(tree.root.join("main.tscn")).expect("scene"),
        saved
    );

    id += 1;
    ok(&session.call_tool(
        id,
        "editor.scene.save",
        json!({ "all": true, "idempotencyId": "live-save-all" }),
    ));
    assert_eq!(
        sent(&plugin, "editor.scene.save")[1]["scenes"],
        json!([
            { "path": "res://main.tscn", "sha256": sha256_hex(saved.as_bytes()) },
            { "path": "res://other.tscn", "sha256": sha256_hex(OTHER.as_bytes()) }
        ])
    );
    assert_eq!(sent(&plugin, "editor.scene.save")[1]["all"], json!(true));

    id += 1;
    let both = session.call_tool(
        id,
        "editor.scene.save",
        json!({ "scene": "main.tscn", "all": true, "idempotencyId": "live-save-both" }),
    );
    assert_eq!(error_code(&both), json!("invalid_input"), "{both}");
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_lost_save_quarantines_the_whole_project() {
    let tree = ProjectTree::new("m8-live-save-lost");
    tree.write("main.tscn", MAIN);
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state", "editor.scene.save"])
            .answering("editor.state", FakeAnswer::Payload(editor_state()))
            .answering("editor.scene.save", FakeAnswer::Disconnect),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let lost = session.call_tool(
        id,
        "editor.scene.save",
        json!({ "idempotencyId": "live-save-lost" }),
    );
    assert_eq!(error_code(&lost), json!("uncertain"), "{lost}");
    plugin.join().expect("the plugin dropped its connection");
    id += 1;
    let blocked = session.call_tool(
        id,
        "file.create",
        json!({ "path": "notes/after.txt", "content": "x\n", "idempotencyId": "after-save" }),
    );
    assert!(refused(&blocked), "{blocked}");
    let _ = session.finish();
    let recorded = recorded_transaction(&tree, &lost);
    assert!(recorded.quarantined, "{recorded:?}");
    assert_eq!(
        recorded.touched_paths,
        vec!["main.tscn".to_owned(), "*".to_owned()]
    );
}
