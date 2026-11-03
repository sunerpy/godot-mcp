//! Contract 0.3 acceptance at the MCP surface: the editor's file-system scan
//! and opening a script in its script editor.
//!
//! A scan lets the editor import whatever changed and write `.import`, `.uid`
//! and cache files nobody can list in advance, so its transaction covers the
//! whole project: an answer that never arrives blocks every write until it is
//! resolved. What a real editor imports and reloads is checked by the
//! live-editor suite.

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

fn scanned() -> Value {
    json!({
        "reloaded_scripts": ["res://scripts/a.gd"],
        "stale_scripts": [],
        "script_reload": "done",
        "diagnostics": null,
        "action": "godot-mcp: editor.filesystem.scan"
    })
}

fn scanning_plugin(tree: &ProjectTree, answer: FakeAnswer) -> FakeEditorPlugin {
    FakeEditorPlugin::spawn(
        &rendezvous(tree),
        FakePluginScript::implementing(&["editor.state", "editor.filesystem.scan"])
            .answering("editor.filesystem.scan", answer),
    )
}

#[test]
fn a_scan_is_journaled_for_the_whole_project() {
    let tree = ProjectTree::new("m8-scan");
    let mut session = open(&tree);
    let plugin = scanning_plugin(&tree, FakeAnswer::Payload(scanned()));
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let answer = ok(&session.call_tool(
        id,
        "editor.filesystem.scan",
        json!({ "idempotencyId": "scan-1" }),
    ));
    assert_eq!(answer["mutation_status"], "committed", "{answer}");
    assert_eq!(
        answer["reloaded_scripts"],
        json!(["res://scripts/a.gd"]),
        "{answer}"
    );
    assert!(answer["workspace"]["revision"].is_u64(), "{answer}");
    let asked = &sent(&plugin, "editor.filesystem.scan")[0];
    assert_eq!(asked["reimport"], json!([]), "{asked}");
    assert_eq!(asked["timeout_ms"], json!(300_000), "{asked}");
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
    let recorded = recorded_transaction(&tree, &answer);
    assert_eq!(recorded.touched_paths, vec!["*".to_owned()]);
    assert!(!recorded.quarantined, "{recorded:?}");
}

#[test]
fn reimported_assets_are_checked_and_answered_with_their_import_state() {
    let tree = ProjectTree::new("m8-scan-reimport");
    tree.write("art/icon.png", "not really a png\n");
    let mut session = open(&tree);
    let plugin = scanning_plugin(&tree, FakeAnswer::Payload(scanned()));
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    for (reimport, what) in [
        (json!(["../outside.png"]), "a path outside the project"),
        (json!(["/etc/passwd"]), "an absolute path"),
        (json!([""]), "an empty path"),
    ] {
        id += 1;
        let response = session.call_tool(
            id,
            "editor.filesystem.scan",
            json!({ "reimport": reimport, "idempotencyId": format!("scan-bad-{id}") }),
        );
        assert!(refused(&response), "{what} is refused: {response}");
    }
    assert!(sent(&plugin, "editor.filesystem.scan").is_empty());
    id += 1;
    let answer = ok(&session.call_tool(
        id,
        "editor.filesystem.scan",
        json!({ "reimport": ["art/icon.png"], "timeoutMs": 60_000, "idempotencyId": "scan-reimport" }),
    ));
    let asked = &sent(&plugin, "editor.filesystem.scan")[0];
    assert_eq!(asked["reimport"], json!(["res://art/icon.png"]), "{asked}");
    assert_eq!(asked["timeout_ms"], json!(60_000), "{asked}");
    assert_eq!(answer["assets"][0]["path"], "art/icon.png", "{answer}");
    assert_eq!(
        answer["assets"][0]["import"]["state"], "not_imported",
        "{answer}"
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_scan_without_an_answer_blocks_every_write() {
    let tree = ProjectTree::new("m8-scan-lost");
    let mut session = open(&tree);
    let plugin = scanning_plugin(&tree, FakeAnswer::Disconnect);
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let lost = session.call_tool(
        id,
        "editor.filesystem.scan",
        json!({ "idempotencyId": "scan-lost" }),
    );
    assert_eq!(error_code(&lost), json!("uncertain"), "{lost}");
    plugin.join().expect("the plugin dropped its connection");
    id += 1;
    let blocked = session.call_tool(
        id,
        "file.create",
        json!({ "path": "notes/after.txt", "content": "x\n", "idempotencyId": "after-scan" }),
    );
    assert!(refused(&blocked), "{blocked}");
    let _ = session.finish();
    let recorded = recorded_transaction(&tree, &lost);
    assert!(recorded.quarantined, "{recorded:?}");
    assert_eq!(recorded.touched_paths, vec!["*".to_owned()]);
}

#[test]
fn a_scan_the_editor_could_not_finish_blocks_every_write() {
    let tree = ProjectTree::new("m8-scan-unfinished");
    let mut session = open(&tree);
    let plugin = scanning_plugin(
        &tree,
        FakeAnswer::Error(ErrorKind::Unavailable, MutationStatus::Unresolved),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let unfinished = session.call_tool(
        id,
        "editor.filesystem.scan",
        json!({ "idempotencyId": "scan-unfinished" }),
    );
    assert_eq!(error_code(&unfinished), json!("uncertain"), "{unfinished}");
    id += 1;
    let blocked = session.call_tool(
        id,
        "file.create",
        json!({ "path": "notes/after.txt", "content": "x\n", "idempotencyId": "after-unfinished" }),
    );
    assert!(refused(&blocked), "{blocked}");
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn the_caller_chooses_how_long_a_scan_may_take() {
    let tree = ProjectTree::new("m8-scan-deadline");
    let mut session = open(&tree);
    let plugin = scanning_plugin(&tree, FakeAnswer::Ignore);
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let started = Instant::now();
    let late = session.call_tool(
        id,
        "editor.filesystem.scan",
        json!({ "timeoutMs": 1000, "idempotencyId": "scan-late" }),
    );
    let waited = started.elapsed();
    assert_eq!(error_code(&late), json!("uncertain"), "{late}");
    assert!(
        waited >= Duration::from_secs(1) && waited < Duration::from_secs(15),
        "the server waits for the caller's deadline, not its own 30 s: {waited:?}"
    );
    id += 1;
    let short = session.call_tool(
        id,
        "editor.filesystem.scan",
        json!({ "timeoutMs": 999, "idempotencyId": "scan-too-short" }),
    );
    assert!(refused(&short), "{short}");
    let _ = session.finish();
    let _ = plugin.join();
}

#[test]
fn a_script_is_opened_where_the_caller_asks() {
    let tree = ProjectTree::new("m8-script-open");
    tree.write("scripts/a.gd", "extends Node\n");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state", "editor.script.open"]).answering(
            "editor.script.open",
            FakeAnswer::Payload(json!({
                "path": "res://scripts/a.gd", "line": 3, "column": 5,
                "action": "godot-mcp: editor.script.open"
            })),
        ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let opened = ok(&session.call_tool(
        id,
        "editor.script.open",
        json!({ "path": "scripts/a.gd", "line": 3, "column": 5, "idempotencyId": "open-a" }),
    ));
    assert_eq!(opened["line"], json!(3), "{opened}");
    id += 1;
    ok(&session.call_tool(
        id,
        "editor.script.open",
        json!({ "path": "res://scripts/a.gd", "idempotencyId": "open-a-top" }),
    ));
    assert_eq!(
        sent(&plugin, "editor.script.open")
            .iter()
            .map(|payload| (
                payload["path"].clone(),
                payload["line"].clone(),
                payload["column"].clone()
            ))
            .collect::<Vec<_>>(),
        vec![
            (json!("res://scripts/a.gd"), json!(3), json!(5)),
            (json!("res://scripts/a.gd"), json!(1), json!(1)),
        ]
    );
    for (arguments, what) in [
        (
            json!({ "path": "main.tscn", "idempotencyId": "open-scene" }),
            "a scene",
        ),
        (
            json!({ "path": "scripts/a.gd", "line": 0, "idempotencyId": "open-line-0" }),
            "line 0",
        ),
        (
            json!({ "path": "scripts/a.gd", "column": 0, "idempotencyId": "open-column-0" }),
            "column 0",
        ),
        (
            json!({ "path": "../a.gd", "idempotencyId": "open-outside" }),
            "a path outside the project",
        ),
    ] {
        id += 1;
        let response = session.call_tool(id, "editor.script.open", arguments);
        assert!(refused(&response), "{what} is refused: {response}");
    }
    assert_eq!(sent(&plugin, "editor.script.open").len(), 2);
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_plugin_without_scans_or_script_opening_says_how_to_update_it() {
    let tree = ProjectTree::new("m8-scan-old");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state"]),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    for (tool, arguments) in [
        (
            "editor.filesystem.scan",
            json!({ "idempotencyId": "old-scan" }),
        ),
        (
            "editor.script.open",
            json!({ "path": "a.gd", "idempotencyId": "old-open" }),
        ),
    ] {
        id += 1;
        let refused = session.call_tool(id, tool, arguments);
        assert_eq!(
            error_code(&refused),
            json!("unsupported"),
            "{tool}: {refused}"
        );
        assert!(
            refused.to_string().contains("editor.addon.update"),
            "{tool}: {refused}"
        );
    }
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}
