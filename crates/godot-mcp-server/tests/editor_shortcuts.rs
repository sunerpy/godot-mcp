//! Contract 0.3 acceptance at the MCP surface: reading and changing the
//! connected editor's shortcuts, the ones its Editor Settings dialog lists on
//! its Shortcuts tab.
//!
//! What a real editor applies and saves is checked by the live-editor suite;
//! this pins what the server owns against a scripted plugin: the bounds and
//! the key-event shape it enforces before the editor is asked, the refusal
//! that names the Godot release an editor needs, and that a change is one
//! journaled editor-state transaction that holds no project file in doubt.

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

fn save_key() -> Value {
    json!({
        "key": "S",
        "match": "keycode",
        "shift": false,
        "ctrl": false,
        "alt": true,
        "meta": false,
        "command_or_control": true,
        "text": "Ctrl+Alt+S"
    })
}

fn save_page() -> Value {
    json!({
        "shortcuts": [{
            "name": "script_editor/save",
            "kind": "shortcut",
            "label": "Save",
            "events": [save_key()],
            "default": [save_key()],
            "changed": false
        }],
        "unknown": [],
        "total": 1,
        "offset": 0,
        "next_offset": null
    })
}

fn shortcuts_plugin(tree: &ProjectTree, set: FakeAnswer) -> FakeEditorPlugin {
    FakeEditorPlugin::spawn(
        &rendezvous(tree),
        FakePluginScript::implementing(&[
            "editor.state",
            "editor.shortcuts.read",
            "editor.shortcuts.set",
        ])
        .versions("0.3.0", "4.7.1.stable.fake")
        .describing("shortcuts", json!(true))
        .answering("editor.shortcuts.read", FakeAnswer::Payload(save_page()))
        .answering("editor.shortcuts.set", set),
    )
}

fn f9() -> Value {
    json!({ "key": "F9", "command_or_control": true })
}

fn changed() -> Value {
    json!({
        "shortcuts": [{
            "name": "script_editor/save",
            "kind": "shortcut",
            "before": [save_key()],
            "after": [{
                "key": "F9",
                "match": "keycode",
                "shift": false,
                "ctrl": false,
                "alt": false,
                "meta": false,
                "command_or_control": true,
                "text": "Ctrl+F9"
            }]
        }],
        "also_changed": { "settings": [], "shortcuts": [] },
        "saved": true,
        "action": "godot-mcp: editor.shortcuts.set"
    })
}

#[test]
fn shortcuts_are_read_by_name_or_page_by_page() {
    let tree = ProjectTree::new("m8-keys-read");
    let mut session = open(&tree);
    let plugin = shortcuts_plugin(&tree, FakeAnswer::Payload(changed()));
    let mut id = 1;
    wait_connected(&mut session, &mut id);

    id += 1;
    assert_eq!(
        ok(&session.call_tool(id, "editor.shortcuts.get", json!({}))),
        save_page()
    );
    for arguments in [
        json!({ "prefix": "script_editor/", "offset": 10, "limit": 20 }),
        json!({ "shortcuts": ["script_editor/save", "ui_accept", "no_such_shortcut"] }),
        json!({ "changedOnly": true, "limit": 500 }),
    ] {
        id += 1;
        ok(&session.call_tool(id, "editor.shortcuts.get", arguments));
    }
    assert_eq!(
        sent(&plugin, "editor.shortcuts.read"),
        vec![
            json!({
                "shortcuts": null, "prefix": "", "changed_only": false, "offset": 0, "limit": 100
            }),
            json!({
                "shortcuts": null,
                "prefix": "script_editor/",
                "changed_only": false,
                "offset": 10,
                "limit": 20
            }),
            json!({
                "shortcuts": ["script_editor/save", "ui_accept", "no_such_shortcut"],
                "prefix": "",
                "changed_only": false,
                "offset": 0,
                "limit": 100
            }),
            json!({
                "shortcuts": null, "prefix": "", "changed_only": true, "offset": 0, "limit": 500
            }),
        ],
        "every shortcut, or the named ones, under a prefix, 100 at a time from the first"
    );

    // Out-of-range input is refused before the editor is asked.
    let too_many: Vec<String> = (0..257)
        .map(|index| format!("a/shortcut_{index}"))
        .collect();
    for (arguments, what) in [
        (json!({ "limit": 0 }), "a zero limit"),
        (json!({ "limit": 501 }), "a limit above 500"),
        (json!({ "offset": -1 }), "a negative offset"),
        (json!({ "prefix": "x".repeat(257) }), "a prefix too long"),
        (
            json!({ "prefix": "a/\u{7}" }),
            "a prefix with a control character",
        ),
        (json!({ "shortcuts": [] }), "no shortcut named"),
        (json!({ "shortcuts": too_many }), "more than 256 shortcuts"),
        (json!({ "shortcuts": [""] }), "an empty name"),
        (
            json!({ "shortcuts": ["a/b\nc"] }),
            "a name with a control character",
        ),
        (
            json!({ "changedOnly": "yes" }),
            "a changedOnly that is no boolean",
        ),
    ] {
        id += 1;
        let response = session.call_tool(id, "editor.shortcuts.get", arguments);
        assert_eq!(
            error_code(&response),
            json!("invalid_input"),
            "{what} is refused: {response}"
        );
    }
    assert_eq!(sent(&plugin, "editor.shortcuts.read").len(), 4);
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_change_is_one_editor_state_transaction_sent_once() {
    let tree = ProjectTree::new("m8-keys-set");
    let mut session = open(&tree);
    let plugin = shortcuts_plugin(&tree, FakeAnswer::Payload(changed()));
    let mut id = 1;
    wait_connected(&mut session, &mut id);

    let shortcuts = json!({
        "script_editor/save": [f9(), save_key()],
        "ui_cancel": "default",
        "editor/close_scene": [],
        "spatial_editor/freelook_left": [{ "key": "J", "match": "physical", "shift": false }]
    });
    id += 1;
    let without = session.call_tool(
        id,
        "editor.shortcuts.set",
        json!({ "shortcuts": shortcuts.clone() }),
    );
    assert!(
        refused(&without),
        "a change needs an idempotencyId: {without}"
    );
    let arguments = json!({ "shortcuts": shortcuts.clone(), "idempotencyId": "save-f9" });
    id += 1;
    let first = ok(&session.call_tool(id, "editor.shortcuts.set", arguments.clone()));
    assert_eq!(first["mutation_status"], "committed", "{first}");
    assert_eq!(first["shortcuts"], changed()["shortcuts"], "{first}");
    assert_eq!(first["replayed"], json!(false), "{first}");
    id += 1;
    let again = ok(&session.call_tool(id, "editor.shortcuts.set", arguments));
    assert_eq!(again["replayed"], json!(true), "{again}");

    let asked = sent(&plugin, "editor.shortcuts.set");
    assert_eq!(
        asked.len(),
        1,
        "a replay is answered from the journal: {asked:?}"
    );
    assert_eq!(
        asked[0]["shortcuts"], shortcuts,
        "the change reaches the editor as it was given"
    );
    assert!(asked[0]["transaction_id"].is_string(), "{}", asked[0]);
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
    let recorded = recorded_transaction(&tree, &first);
    assert!(!recorded.quarantined, "{recorded:?}");
    assert!(
        recorded.touched_paths.is_empty(),
        "a shortcut is no project file: {recorded:?}"
    );
}

#[test]
fn a_key_event_is_checked_before_the_editor_is_asked() {
    let tree = ProjectTree::new("m8-keys-values");
    let mut session = open(&tree);
    let plugin = shortcuts_plugin(&tree, FakeAnswer::Payload(changed()));
    let mut id = 1;
    wait_connected(&mut session, &mut id);

    let too_many_shortcuts: serde_json::Map<String, Value> = (0..65)
        .map(|index| (format!("a/shortcut_{index}"), json!([])))
        .collect();
    let too_many_events: Vec<Value> = (0..17).map(|_| f9()).collect();
    for (shortcuts, what) in [
        (json!([]), "a list instead of shortcuts by name"),
        (json!({}), "no shortcut at all"),
        (Value::Object(too_many_shortcuts), "more than 64 shortcuts"),
        (json!({ "": [] }), "an empty name"),
        (json!({ "a/\u{1}": [] }), "a name with a control character"),
        (json!({ "a/b": "reset" }), "a word other than `default`"),
        (json!({ "a/b": 3 }), "a number"),
        (json!({ "a/b": too_many_events }), "more than 16 key events"),
        (json!({ "a/b": [{}] }), "an event without a key"),
        (json!({ "a/b": [{ "key": "" }] }), "an empty key"),
        (
            json!({ "a/b": [{ "key": "Ctrl+S" }] }),
            "modifiers inside the key name",
        ),
        (
            json!({ "a/b": [{ "key": "S\t" }] }),
            "a key with a control character",
        ),
        (
            json!({ "a/b": [{ "key": "x".repeat(65) }] }),
            "a key name too long",
        ),
        (
            json!({ "a/b": [{ "key": "S", "command_or_control": true, "ctrl": true }] }),
            "Ctrl beside command_or_control",
        ),
        (
            json!({ "a/b": [{ "key": "S", "command_or_control": true, "meta": true }] }),
            "Meta beside command_or_control",
        ),
        (
            json!({ "a/b": [{ "key": "S", "match": "position" }] }),
            "a match other than keycode, physical or label",
        ),
        (
            json!({ "a/b": [{ "key": "S", "physical": true }] }),
            "a flag that is not one",
        ),
        (
            json!({ "a/b": [{ "key": "S", "shift": "yes" }] }),
            "a modifier that is no boolean",
        ),
        (
            json!({ "a/b": [{ "key": "S", "text": 3 }] }),
            "a text that is no string",
        ),
    ] {
        id += 1;
        let response = session.call_tool(
            id,
            "editor.shortcuts.set",
            json!({ "shortcuts": shortcuts, "idempotencyId": format!("bad-{id}") }),
        );
        assert_eq!(
            error_code(&response),
            json!("invalid_input"),
            "{what} is refused: {response}"
        );
    }
    assert!(sent(&plugin, "editor.shortcuts.set").is_empty());
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn an_unanswered_change_is_uncertain_but_blocks_no_write() {
    let tree = ProjectTree::new("m8-keys-lost");
    let mut session = open(&tree);
    let plugin = shortcuts_plugin(&tree, FakeAnswer::Disconnect);
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let lost = session.call_tool(
        id,
        "editor.shortcuts.set",
        json!({ "shortcuts": { "script_editor/save": [f9()] }, "idempotencyId": "lost" }),
    );
    assert_eq!(error_code(&lost), json!("uncertain"), "{lost}");
    plugin.join().expect("the plugin dropped its connection");
    id += 1;
    ok(&session.call_tool(
        id,
        "file.create",
        json!({ "path": "notes/after.txt", "content": "x\n", "idempotencyId": "after-keys" }),
    ));
    let _ = session.finish();
}

#[test]
fn an_editor_before_godot_4_6_says_which_godot_it_runs() {
    let tree = ProjectTree::new("m8-keys-45");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state"])
            .versions("0.3.0", "4.5.1.stable.official")
            .describing("shortcuts", json!(false)),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    for (tool, arguments) in [
        ("editor.shortcuts.get", json!({})),
        (
            "editor.shortcuts.set",
            json!({ "shortcuts": { "script_editor/save": [f9()] }, "idempotencyId": "keys-45" }),
        ),
    ] {
        id += 1;
        let response = session.call_tool(id, tool, arguments);
        assert_eq!(
            error_code(&response),
            json!("unsupported"),
            "{tool}: {response}"
        );
        let text = response.to_string();
        assert!(
            text.contains("4.5.1.stable.official") && text.contains("4.6"),
            "{tool}: the refusal names the editor's Godot and the release that hands a plugin \
             its shortcuts: {response}"
        );
        assert!(
            !text.contains("editor.addon.update"),
            "{tool}: an update of the plugin would not help: {response}"
        );
    }
    assert!(
        plugin
            .received()
            .iter()
            .all(|(method, _)| !method.starts_with("editor.shortcuts"))
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_plugin_without_shortcuts_says_how_to_update_it() {
    let tree = ProjectTree::new("m8-keys-old");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state"]),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    for (tool, arguments) in [
        ("editor.shortcuts.get", json!({})),
        (
            "editor.shortcuts.set",
            json!({ "shortcuts": { "script_editor/save": [f9()] }, "idempotencyId": "old" }),
        ),
    ] {
        id += 1;
        let response = session.call_tool(id, tool, arguments);
        assert_eq!(
            error_code(&response),
            json!("unsupported"),
            "{tool}: {response}"
        );
        let text = response.to_string();
        assert!(
            text.contains("0.2.0") && text.contains("editor.addon.update"),
            "{tool}: the refusal names the plugin's version and how to update it: {response}"
        );
    }
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}
