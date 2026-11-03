//! Contract 0.3 acceptance at the MCP surface: reading the live editor's log.
//!
//! What a real editor captures is checked by the live-editor suite; this pins
//! what the server owns against a scripted plugin: the cursor it forwards, the
//! bounds it enforces before the editor is asked, and how it says that log
//! capture is missing — from an editor too old to have it, or from a plugin
//! that predates it.

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
fn the_log_is_read_from_the_cursor_the_caller_holds() {
    let tree = ProjectTree::new("m8-log");
    let mut session = open(&tree);
    let page = json!({
        "entries": [{
            "sequence": 7, "kind": "error", "text": "boom", "stderr": true,
            "function": "push_error", "file": "core/variant/variant_utility.cpp", "line": 1023,
            "code": "boom", "rationale": "", "unix_ms": 1_700_000_000_000_u64,
            "backtrace": [{ "function": "_ready", "file": "res://main.gd", "line": 4 }]
        }],
        "next": 7, "oldest": 1, "dropped": 0, "skipped": 0, "capacity": 2000,
        "epoch": "e1", "reset": false, "truncated": false
    });
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state", "editor.log.read"])
            .versions("0.3.0", "4.7.1.stable.fake")
            .describing("log_capture", json!(true))
            .answering("editor.log.read", FakeAnswer::Payload(page.clone())),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);

    id += 1;
    let read = ok(&session.call_tool(
        id,
        "editor.log.get",
        json!({ "after": 6, "epoch": "e1", "limit": 20, "kinds": ["error", "warning"] }),
    ));
    assert_eq!(read, page);
    id += 1;
    ok(&session.call_tool(id, "editor.log.get", json!({})));
    assert_eq!(
        sent(&plugin, "editor.log.read"),
        vec![
            json!({ "after": 6, "epoch": "e1", "limit": 20, "kinds": ["error", "warning"] }),
            json!({ "after": 0, "epoch": null, "limit": 100, "kinds": null }),
        ],
        "the caller's cursor, or every entry from the oldest, 100 at a time"
    );

    // Out-of-range input is refused before the editor is asked.
    for (arguments, what) in [
        (json!({ "limit": 0 }), "a zero limit"),
        (json!({ "limit": 501 }), "a limit above 500"),
        (json!({ "after": -1 }), "a negative cursor"),
        (json!({ "kinds": ["fatal"] }), "an unknown kind"),
        (json!({ "kinds": [] }), "no kind at all"),
    ] {
        id += 1;
        let refused = session.call_tool(id, "editor.log.get", arguments);
        assert!(
            refused.get("error").is_some() || refused["result"]["isError"] == json!(true),
            "{what} is refused: {refused}"
        );
    }
    assert_eq!(sent(&plugin, "editor.log.read").len(), 2);
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn an_editor_without_a_logger_says_which_godot_it_runs() {
    let tree = ProjectTree::new("m8-log-44");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state"])
            .versions("0.3.0", "4.4.1.stable.official")
            .describing("log_capture", json!(false)),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let refused = session.call_tool(id, "editor.log.get", json!({}));
    assert_eq!(error_code(&refused), json!("unsupported"), "{refused}");
    let text = refused.to_string();
    assert!(
        text.contains("4.4.1.stable.official") && text.contains("4.5"),
        "the refusal names the editor's Godot and the release that added log capture: {refused}"
    );
    assert!(
        !text.contains("editor.addon.update"),
        "an update of the plugin would not help: {refused}"
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_plugin_without_log_capture_says_how_to_update_it() {
    let tree = ProjectTree::new("m8-log-old");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state"]),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let refused = session.call_tool(id, "editor.log.get", json!({}));
    assert_eq!(error_code(&refused), json!("unsupported"), "{refused}");
    let text = refused.to_string();
    assert!(
        text.contains("0.2.0") && text.contains("editor.addon.update"),
        "the refusal names the plugin's version and how to update it: {refused}"
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn without_an_editor_the_log_read_says_so() {
    let tree = ProjectTree::new("m8-log-absent");
    let mut session = open(&tree);
    let refused = session.call_tool(2, "editor.log.get", json!({}));
    assert_eq!(error_code(&refused), json!("unavailable"), "{refused}");
    let _ = session.finish();
}
