//! Contract 0.3 acceptance at the MCP surface: reading and changing the
//! connected editor's own settings, the ones its Editor Settings dialog shows.
//!
//! What a real editor applies and saves is checked by the live-editor suite;
//! this pins what the server owns against a scripted plugin: the bounds it
//! enforces before the editor is asked, the settings it refuses by name, and
//! that a change is one journaled editor-state transaction that holds no
//! project file in doubt.

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

fn integer(value: i64) -> Value {
    json!({ "type": "integer", "value": value })
}

fn indent_page() -> Value {
    json!({
        "settings": [{
            "name": "text_editor/behavior/indent/size",
            "type": "integer",
            "value": integer(4),
            "default": integer(4),
            "changed": false,
            "hint": "range",
            "hint_string": "1,64,1",
            "restart_required": false,
            "changeable": true
        }],
        "unknown": [],
        "total": 1,
        "offset": 0,
        "next_offset": null
    })
}

fn preferences_plugin(tree: &ProjectTree, set: FakeAnswer) -> FakeEditorPlugin {
    FakeEditorPlugin::spawn(
        &rendezvous(tree),
        FakePluginScript::implementing(&[
            "editor.state",
            "editor.preferences.read",
            "editor.preferences.set",
        ])
        .versions("0.3.0", "4.7.1.stable.fake")
        .answering(
            "editor.preferences.read",
            FakeAnswer::Payload(indent_page()),
        )
        .answering("editor.preferences.set", set),
    )
}

fn changed() -> Value {
    json!({
        "settings": [{
            "name": "text_editor/behavior/indent/size",
            "before": integer(4),
            "after": integer(2),
            "restart_required": false
        }],
        "saved": true,
        "restart_required": false,
        "action": "godot-mcp: editor.preferences.set"
    })
}

#[test]
fn settings_are_read_by_name_or_page_by_page() {
    let tree = ProjectTree::new("m8-prefs-read");
    let mut session = open(&tree);
    let plugin = preferences_plugin(&tree, FakeAnswer::Payload(changed()));
    let mut id = 1;
    wait_connected(&mut session, &mut id);

    id += 1;
    assert_eq!(
        ok(&session.call_tool(id, "editor.preferences.get", json!({}))),
        indent_page()
    );
    for arguments in [
        json!({ "prefix": "text_editor/", "offset": 10, "limit": 20 }),
        json!({ "settings": ["text_editor/behavior/indent/size", "no/such_setting"] }),
        json!({ "settings": ["a/b", "c/d"], "prefix": "c/", "limit": 1 }),
    ] {
        id += 1;
        ok(&session.call_tool(id, "editor.preferences.get", arguments));
    }
    assert_eq!(
        sent(&plugin, "editor.preferences.read"),
        vec![
            json!({ "settings": null, "prefix": "", "offset": 0, "limit": 100 }),
            json!({ "settings": null, "prefix": "text_editor/", "offset": 10, "limit": 20 }),
            json!({
                "settings": ["text_editor/behavior/indent/size", "no/such_setting"],
                "prefix": "",
                "offset": 0,
                "limit": 100
            }),
            json!({ "settings": ["a/b", "c/d"], "prefix": "c/", "offset": 0, "limit": 1 }),
        ],
        "every setting, or the named ones, under a prefix, 100 at a time from the first"
    );

    // Out-of-range input is refused before the editor is asked.
    let too_many: Vec<String> = (0..257).map(|index| format!("a/setting_{index}")).collect();
    for (arguments, what) in [
        (json!({ "limit": 0 }), "a zero limit"),
        (json!({ "limit": 501 }), "a limit above 500"),
        (json!({ "offset": -1 }), "a negative offset"),
        (json!({ "prefix": "x".repeat(257) }), "a prefix too long"),
        (json!({ "settings": [] }), "no setting named"),
        (json!({ "settings": too_many }), "more than 256 settings"),
        (
            json!({ "settings": ["no_slash"] }),
            "a name that is not a setting's",
        ),
        (
            json!({ "settings": ["a/b\nc"] }),
            "a name with a control character",
        ),
        (
            json!({ "prefix": "a/\u{7}" }),
            "a prefix with a control character",
        ),
    ] {
        id += 1;
        let response = session.call_tool(id, "editor.preferences.get", arguments);
        assert_eq!(
            error_code(&response),
            json!("invalid_input"),
            "{what} is refused: {response}"
        );
    }
    assert_eq!(sent(&plugin, "editor.preferences.read").len(), 4);
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_change_is_one_editor_state_transaction_sent_once() {
    let tree = ProjectTree::new("m8-prefs-set");
    let mut session = open(&tree);
    let plugin = preferences_plugin(&tree, FakeAnswer::Payload(changed()));
    let mut id = 1;
    wait_connected(&mut session, &mut id);

    id += 1;
    let without = session.call_tool(
        id,
        "editor.preferences.set",
        json!({ "settings": { "text_editor/behavior/indent/size": integer(2) } }),
    );
    assert!(
        refused(&without),
        "a change needs an idempotencyId: {without}"
    );
    let arguments = json!({
        "settings": { "text_editor/behavior/indent/size": integer(2) },
        "idempotencyId": "indent-2"
    });
    id += 1;
    let first = ok(&session.call_tool(id, "editor.preferences.set", arguments.clone()));
    assert_eq!(first["mutation_status"], "committed", "{first}");
    assert_eq!(first["settings"], changed()["settings"], "{first}");
    assert_eq!(first["replayed"], json!(false), "{first}");
    id += 1;
    let again = ok(&session.call_tool(id, "editor.preferences.set", arguments));
    assert_eq!(again["replayed"], json!(true), "{again}");

    let asked = sent(&plugin, "editor.preferences.set");
    assert_eq!(
        asked.len(),
        1,
        "a replay is answered from the journal: {asked:?}"
    );
    assert_eq!(
        asked[0]["settings"],
        json!({ "text_editor/behavior/indent/size": integer(2) })
    );
    assert!(asked[0]["transaction_id"].is_string(), "{}", asked[0]);
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
    let recorded = recorded_transaction(&tree, &first);
    assert!(!recorded.quarantined, "{recorded:?}");
    assert!(
        recorded.touched_paths.is_empty(),
        "an editor setting is no project file: {recorded:?}"
    );
}

#[test]
fn settings_that_reach_programs_paths_hosts_or_credentials_are_refused_unasked() {
    let tree = ProjectTree::new("m8-prefs-refused");
    let mut session = open(&tree);
    let plugin = preferences_plugin(&tree, FakeAnswer::Payload(changed()));
    let mut id = 1;
    wait_connected(&mut session, &mut id);

    for name in [
        "export/android/debug_keystore_pass",
        "filesystem/file_server/password",
        "filesystem/file_server/port",
        "network/debug/remote_host",
        "version_control/username",
        "asset_library/use_threads",
        "asset_store/use_threads",
        "filesystem/external_programs/terminal_emulator",
        "filesystem/import/blender/enabled",
        "filesystem/tools/oidn/oidn_denoise_path",
        "text_editor/external/use_external_editor",
        "docks/filesystem/textfile_extensions",
        "docks/filesystem/other_file_extensions",
        "interface/editor/import_resources_when_unfocused",
        "interface/editor/behavior/import_resources_when_unfocused",
    ] {
        id += 1;
        // An allowed setting beside a refused one changes nothing either.
        let response = session.call_tool(
            id,
            "editor.preferences.set",
            json!({
                "settings": {
                    "text_editor/behavior/indent/size": integer(2),
                    name: { "type": "bool", "value": true }
                },
                "idempotencyId": format!("refused-{id}")
            }),
        );
        assert_eq!(
            error_code(&response),
            json!("permission_denied"),
            "{name}: {response}"
        );
        assert!(
            response.to_string().contains(name),
            "the refusal names the setting: {response}"
        );
    }
    assert!(
        sent(&plugin, "editor.preferences.set").is_empty(),
        "the editor is never asked"
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_value_is_checked_before_the_editor_is_asked() {
    let tree = ProjectTree::new("m8-prefs-values");
    let mut session = open(&tree);
    let plugin = preferences_plugin(&tree, FakeAnswer::Payload(changed()));
    let mut id = 1;
    wait_connected(&mut session, &mut id);

    let too_many: serde_json::Map<String, Value> = (0..65)
        .map(|index| (format!("a/setting_{index}"), integer(1)))
        .collect();
    for (settings, what) in [
        (json!([]), "a list instead of settings by name"),
        (json!({}), "no setting at all"),
        (Value::Object(too_many), "more than 64 settings"),
        (
            json!({ "text_editor/behavior/indent/size": 2 }),
            "an untagged value",
        ),
        (
            json!({ "text_editor/behavior/indent/size": { "type": "integer", "value": 2, "x": 1 } }),
            "a tagged value with another field",
        ),
        (
            json!({ "interface/theme/preset": { "type": "resource", "value": {
                "resource": "res://theme.tres", "resource_type": "Theme", "uid": null
            } } }),
            "a resource",
        ),
        (
            json!({ "text_editor/behavior/indent/size": { "type": "array", "value": [
                { "type": "resource", "value": {
                    "resource": "res://theme.tres", "resource_type": "Theme", "uid": null
                } }
            ] } }),
            "a resource inside a value",
        ),
    ] {
        id += 1;
        let response = session.call_tool(
            id,
            "editor.preferences.set",
            json!({ "settings": settings, "idempotencyId": format!("bad-{id}") }),
        );
        assert_eq!(
            error_code(&response),
            json!("invalid_input"),
            "{what} is refused: {response}"
        );
        if what.starts_with("a resource") {
            assert!(
                response.to_string().contains("never a resource"),
                "{what} is refused for what it is: {response}"
            );
        }
    }
    assert!(sent(&plugin, "editor.preferences.set").is_empty());
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn an_unanswered_change_is_uncertain_but_blocks_no_write() {
    let tree = ProjectTree::new("m8-prefs-lost");
    let mut session = open(&tree);
    let plugin = preferences_plugin(&tree, FakeAnswer::Disconnect);
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let lost = session.call_tool(
        id,
        "editor.preferences.set",
        json!({
            "settings": { "text_editor/behavior/indent/size": integer(2) },
            "idempotencyId": "lost"
        }),
    );
    assert_eq!(error_code(&lost), json!("uncertain"), "{lost}");
    plugin.join().expect("the plugin dropped its connection");
    id += 1;
    ok(&session.call_tool(
        id,
        "file.create",
        json!({ "path": "notes/after.txt", "content": "x\n", "idempotencyId": "after-prefs" }),
    ));
    let _ = session.finish();
}

#[test]
fn a_plugin_without_settings_access_says_how_to_update_it() {
    let tree = ProjectTree::new("m8-prefs-old");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state"]),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    for (tool, arguments) in [
        ("editor.preferences.get", json!({})),
        (
            "editor.preferences.set",
            json!({
                "settings": { "text_editor/behavior/indent/size": integer(2) },
                "idempotencyId": "old"
            }),
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
