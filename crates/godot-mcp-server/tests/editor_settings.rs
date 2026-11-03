//! Contract 0.3 acceptance at the MCP surface: while a live editor is
//! connected, `project.godot` belongs to it.
//!
//! A running Godot editor keeps the project settings in memory and writes them
//! back whenever it saves them, so a headless write underneath it is silently
//! lost. These cases pin what the server owns against a scripted plugin that
//! speaks the shipped bridge protocol: a settings patch is carried out by the
//! editor, its answer is computed from the bytes the editor saved, and no
//! headless writer touches `project.godot` while an editor is connected. What
//! a real editor does is exercised against exact Godot builds by the live
//! editor suite.

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
use support::{ProjectTree, Spawn, StdioSession, error_code, sha256_hex};

const PROJECT: &str = "; keep this comment\nconfig_version=5\n\n[application]\n\n\
                       config/name=\"Demo\"\n";

/// What a Godot editor writes when it saves the patched settings: its own
/// writer drops the comment, and it also saves a value it held in memory.
const SAVED_BY_EDITOR: &str = "config_version=5\n\n[application]\n\n\
                               config/name=\"Renamed\"\n\n[rendering]\n\n\
                               textures/canvas_textures/default_texture_filter=0\n";

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

fn editor_state() -> Value {
    json!({
        "open_scenes": [],
        "open_scripts": [],
        "edited_scene": null,
        "selection": [],
        "history": null,
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

fn sent(plugin: &FakeEditorPlugin, method: &str) -> Vec<Value> {
    plugin
        .received()
        .into_iter()
        .filter(|(received, _)| received == method)
        .map(|(_, payload)| payload)
        .collect()
}

fn manifest(tree: &ProjectTree) -> String {
    std::fs::read_to_string(tree.root.join("project.godot")).expect("project.godot")
}

fn rename(sha: &str, idempotency_id: &str) -> Value {
    json!({
        "expectedSha256": sha,
        "operations": [
            { "op": "set_value", "setting": "application/config/name", "value": "Renamed" }
        ],
        "idempotencyId": idempotency_id
    })
}

#[test]
fn a_settings_patch_is_carried_out_by_the_connected_editor() {
    let tree = ProjectTree::new("m8-settings-editor");
    tree.write("project.godot", PROJECT);
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state", "editor.settings.apply"])
            .answering("editor.state", FakeAnswer::Payload(editor_state()))
            .answering(
                "editor.settings.apply",
                FakeAnswer::SaveThenAnswer {
                    path: tree.root.join("project.godot"),
                    contents: SAVED_BY_EDITOR.as_bytes().to_vec(),
                    payload: json!({
                        "saved": true,
                        "action": "godot-mcp: project.settings.patch"
                    }),
                },
            ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    let before = sha256_hex(PROJECT.as_bytes());
    let arguments = rename(&before, "settings-editor-1");
    id += 1;
    let patched = ok(&session.call_tool(id, "project.settings.patch", arguments.clone()));

    // The editor received the typed change, not a file to write.
    let requests = sent(&plugin, "editor.settings.apply");
    assert_eq!(requests.len(), 1, "{:?}", plugin.received());
    assert_eq!(requests[0]["expected_sha256"], json!(before));
    assert_eq!(
        requests[0]["changes"],
        json!([{ "key": "application/config/name", "old": "\"Demo\"", "new": "\"Renamed\"" }])
    );
    assert_eq!(requests[0]["plugins"], json!([]));

    // The answer describes the bytes the editor saved, not the server's
    // rendering of them.
    assert_eq!(manifest(&tree), SAVED_BY_EDITOR);
    assert_eq!(patched["backend"], "editor", "{patched}");
    assert_eq!(patched["before_sha256"], json!(before));
    assert_eq!(
        patched["after_sha256"],
        json!(sha256_hex(SAVED_BY_EDITOR.as_bytes())),
        "{patched}"
    );
    assert_eq!(patched["changed"], json!(true));
    assert!(
        patched["diff"].to_string().contains("keep this comment"),
        "the diff shows the comment the editor's writer dropped: {patched}"
    );
    assert_eq!(
        patched["changed_keys"],
        json!([
            "application/config/name",
            "rendering/textures/canvas_textures/default_texture_filter"
        ]),
        "{patched}"
    );
    assert_eq!(
        patched["unrequested_keys"],
        json!(["rendering/textures/canvas_textures/default_texture_filter"]),
        "{patched}"
    );
    assert_eq!(patched["settings"]["name"], "Renamed", "{patched}");
    assert_eq!(patched["conclusion"], "proven_post_state", "{patched}");

    // An exact retry answers from the journal; the editor is not asked again.
    id += 1;
    let replay = ok(&session.call_tool(id, "project.settings.patch", arguments));
    assert_eq!(replay["replayed"], json!(true), "{replay}");
    assert_eq!(sent(&plugin, "editor.settings.apply").len(), 1);
    let _ = session.finish();
    plugin.join().expect("the plugin saw the server stop");
}

#[test]
fn plugin_switches_are_sent_as_switches_and_never_as_a_raw_setting() {
    let tree = ProjectTree::new("m8-settings-plugins");
    tree.write("project.godot", PROJECT);
    tree.write(
        "addons/other/plugin.cfg",
        "[plugin]\nname=\"other\"\nscript=\"plugin.gd\"\n",
    );
    let mut session = open(&tree);
    let saved = format!(
        "{PROJECT}\n[editor_plugins]\n\nenabled=PackedStringArray(\"res://addons/other/plugin.cfg\")\n"
    );
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state", "editor.settings.apply"])
            .answering("editor.state", FakeAnswer::Payload(editor_state()))
            .answering(
                "editor.settings.apply",
                FakeAnswer::SaveThenAnswer {
                    path: tree.root.join("project.godot"),
                    contents: saved.clone().into_bytes(),
                    payload: json!({ "saved": true }),
                },
            ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let patched = ok(&session.call_tool(
        id,
        "project.settings.patch",
        json!({
            "expectedSha256": sha256_hex(PROJECT.as_bytes()),
            "operations": [{ "op": "enable_plugin", "plugin": "res://addons/other/plugin.cfg" }],
            "idempotencyId": "settings-plugin-1"
        }),
    ));
    let requests = sent(&plugin, "editor.settings.apply");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]["plugins"],
        json!([{ "path": "res://addons/other/plugin.cfg", "enabled": true }])
    );
    assert_eq!(
        requests[0]["changes"],
        json!([]),
        "the enabled list is the editor's to rewrite: {}",
        requests[0]
    );
    assert_eq!(patched["backend"], "editor");
    assert_eq!(patched["unrequested_keys"], json!([]), "{patched}");
    assert_eq!(manifest(&tree), saved);
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn no_headless_writer_touches_project_godot_while_an_editor_is_connected() {
    let tree = ProjectTree::new("m8-settings-held");
    tree.write("project.godot", PROJECT);
    let mut session = open(&tree);
    // A plugin released before contract 0.3 cannot apply settings.
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state"])
            .answering("editor.state", FakeAnswer::Payload(editor_state())),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);

    // Enabling the add-on writes project.godot, so the whole install refuses.
    id += 1;
    let installed = session.call_tool(
        id,
        "editor.addon.install",
        json!({ "enable": true, "idempotencyId": "held-manifest-1" }),
    );
    assert_eq!(error_code(&installed), json!("conflict"), "{installed}");
    assert!(
        installed.to_string().contains("project.settings.patch"),
        "the refusal names the tool the editor carries out: {installed}"
    );
    assert!(
        !tree.root.join("addons/godot_mcp").exists(),
        "a refused install writes none of its files"
    );

    id += 1;
    let unsupported = session.call_tool(
        id,
        "project.settings.patch",
        rename(&sha256_hex(PROJECT.as_bytes()), "settings-old-plugin-1"),
    );
    assert_eq!(
        error_code(&unsupported),
        json!("unsupported"),
        "{unsupported}"
    );
    assert!(
        unsupported.to_string().contains("editor.addon.update"),
        "the refusal says how to update the plugin: {unsupported}"
    );
    assert_eq!(manifest(&tree), PROJECT, "nothing was written");
    assert!(sent(&plugin, "editor.settings.apply").is_empty());
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn an_editor_holding_unsaved_settings_refuses_and_nothing_is_written() {
    let tree = ProjectTree::new("m8-settings-refused");
    tree.write("project.godot", PROJECT);
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state", "editor.settings.apply"])
            .answering("editor.state", FakeAnswer::Payload(editor_state()))
            .answering(
                "editor.settings.apply",
                FakeAnswer::Error(ErrorKind::Conflict, MutationStatus::NotStarted),
            ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let refused = session.call_tool(
        id,
        "project.settings.patch",
        rename(&sha256_hex(PROJECT.as_bytes()), "settings-refused-1"),
    );
    assert_eq!(error_code(&refused), json!("conflict"), "{refused}");
    assert_eq!(manifest(&tree), PROJECT);
    // Nothing was in doubt, so later writes go ahead.
    id += 1;
    ok(&session.call_tool(
        id,
        "file.create",
        json!({ "path": "notes/after.txt", "content": "x\n", "idempotencyId": "after-refused" }),
    ));
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_lost_settings_change_is_uncertain_and_blocks_writes() {
    let tree = ProjectTree::new("m8-settings-lost");
    tree.write("project.godot", PROJECT);
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state", "editor.settings.apply"])
            .answering("editor.state", FakeAnswer::Payload(editor_state()))
            .answering("editor.settings.apply", FakeAnswer::Disconnect),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let lost = session.call_tool(
        id,
        "project.settings.patch",
        rename(&sha256_hex(PROJECT.as_bytes()), "settings-lost-1"),
    );
    assert_eq!(error_code(&lost), json!("uncertain"), "{lost}");
    plugin.join().expect("the plugin dropped its connection");
    // Whether the editor saved project.godot is not decidable.
    id += 1;
    let blocked = session.call_tool(
        id,
        "file.create",
        json!({ "path": "notes/after.txt", "content": "x\n", "idempotencyId": "after-lost" }),
    );
    assert!(
        blocked.get("error").is_some() || blocked["result"]["isError"] == json!(true),
        "an unresolved settings change blocks every later write: {blocked}"
    );
    assert!(!tree.root.join("notes/after.txt").exists());
    let _ = session.finish();
    let recorded = recorded_transaction(&tree, &lost);
    assert!(recorded.quarantined, "{recorded:?}");
    assert_eq!(recorded.touched_paths, vec!["project.godot".to_owned()]);
}

/// Switching a plugin runs that plugin's code in the editor, so a lost answer
/// leaves every project file in doubt, not just project.godot.
#[test]
fn a_lost_plugin_switch_is_scoped_to_the_whole_project() {
    let tree = ProjectTree::new("m8-settings-lost-plugin");
    tree.write("project.godot", PROJECT);
    tree.write(
        "addons/other/plugin.cfg",
        "[plugin]\nname=\"other\"\nscript=\"plugin.gd\"\n",
    );
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state", "editor.settings.apply"])
            .answering("editor.state", FakeAnswer::Payload(editor_state()))
            .answering("editor.settings.apply", FakeAnswer::Disconnect),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let lost = session.call_tool(
        id,
        "project.settings.patch",
        json!({
            "expectedSha256": sha256_hex(PROJECT.as_bytes()),
            "operations": [{ "op": "enable_plugin", "plugin": "res://addons/other/plugin.cfg" }],
            "idempotencyId": "settings-lost-plugin-1"
        }),
    );
    assert_eq!(error_code(&lost), json!("uncertain"), "{lost}");
    plugin.join().expect("the plugin dropped its connection");
    let _ = session.finish();
    let recorded = recorded_transaction(&tree, &lost);
    assert!(recorded.quarantined, "{recorded:?}");
    assert_eq!(
        recorded.touched_paths,
        vec!["project.godot".to_owned(), "*".to_owned()]
    );
}

/// The durable record of the transaction an uncertain answer names.
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

/// An expected revision is rechecked inside the effect lane on the editor path
/// too: a stale workspace proves no revision, so the editor is never asked.
#[test]
fn a_stale_workspace_proves_no_expected_revision_on_the_editor_path() {
    let tree = ProjectTree::new("m8-settings-stale");
    tree.write("project.godot", PROJECT);
    tree.write("scripts/ok.gd", "extends Node\n");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state", "editor.settings.apply"])
            .answering("editor.state", FakeAnswer::Payload(editor_state()))
            .answering(
                "editor.settings.apply",
                FakeAnswer::SaveThenAnswer {
                    path: tree.root.join("project.godot"),
                    contents: SAVED_BY_EDITOR.as_bytes().to_vec(),
                    payload: json!({ "saved": true }),
                },
            ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let info = ok(&session.call_tool(id, "project.info.get", json!({})));
    let current = info["workspace"]["revision"].clone();
    std::fs::write(
        tree.root.join("scripts/broken.gd"),
        [0xff, 0xfe, 0x00, 0x80],
    )
    .expect("a script that is not UTF-8 cannot be indexed");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        id += 1;
        let info = ok(&session.call_tool(id, "project.info.get", json!({})));
        if info["workspace"]["coherent"] == json!(false) {
            assert_eq!(info["workspace"]["revision"], current, "{info}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the workspace never went stale: {info}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut arguments = rename(&sha256_hex(PROJECT.as_bytes()), "settings-stale-1");
    arguments["expectedRevision"] = current;
    id += 1;
    let refused = session.call_tool(id, "project.settings.patch", arguments);
    assert_eq!(error_code(&refused), json!("conflict"), "{refused}");
    assert!(
        sent(&plugin, "editor.settings.apply").is_empty(),
        "the editor is never asked to apply a change the lane refused"
    );
    assert_eq!(manifest(&tree), PROJECT);
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}
