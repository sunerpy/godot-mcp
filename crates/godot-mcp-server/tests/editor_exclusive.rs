//! Acceptance at the MCP surface: a headless writer of the editor's import and
//! UID caches never runs beside a connected editor.
//!
//! `project.import.run` and `project.uid.update` start Godot against the
//! project, and a running editor rewrites the same `.godot/` caches. While an
//! editor is connected both refuse before anything starts; while either one
//! runs, an editor that dials in is kept out until it has finished. A retry of
//! a change that already committed still answers from the journal.

#![cfg(target_os = "linux")]

mod support;

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use godot_mcp_server::test_support::fake_editor_plugin::{
    FakeAnswer, FakeEditorPlugin, FakePluginScript,
};
use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, error_code, fake_godot};

fn canonical_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../godot-mcp-engine/tests/fixtures/canonical/binaries/unix/godot-canonical.sh")
        .canonicalize()
        .expect("the engine crate's canonical fixture")
}

/// A fake Godot build that runs `before`, then the canonical fixture: an
/// import exits 0 and a canonical helper run answers as the real helper does.
fn engine(tree: &ProjectTree, before: &str) -> (PathBuf, PathBuf) {
    fake_godot(
        &tree.base,
        &format!(
            "{before}\nexec /bin/sh '{}' \"$@\"",
            canonical_fixture().display()
        ),
    )
}

/// Shell that waits until `marker` exists, at most 30 seconds.
fn wait_for(marker: &Path) -> String {
    format!(
        "waited=0\nwhile [ ! -f '{}' ] && [ \"$waited\" -lt 600 ]; do sleep 0.05; \
         waited=$((waited + 1)); done",
        marker.display()
    )
}

fn rendezvous(tree: &ProjectTree) -> PathBuf {
    tree.root
        .join(".godot-mcp")
        .join("editor-bridge.rendezvous")
}

fn open(tree: &ProjectTree, godot: &Path) -> StdioSession {
    StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
        godot: Some(godot),
    })
}

fn ok(response: &Value) -> Value {
    assert!(
        response.get("error").is_none() && response["result"]["isError"] != json!(true),
        "the call must succeed: {response}"
    );
    response["result"]["structuredContent"].clone()
}

fn plugin(tree: &ProjectTree) -> FakeEditorPlugin {
    FakeEditorPlugin::spawn(
        &rendezvous(tree),
        FakePluginScript::implementing(&["editor.state"]).answering(
            "editor.state",
            FakeAnswer::Payload(json!({ "open_scenes": [], "open_scripts": [] })),
        ),
    )
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

/// Wait for `path` to exist, at most 20 seconds.
fn wait_exists(path: &Path) {
    let started = Instant::now();
    while !path.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// How many times the fake engine was launched (capability probes excluded).
fn launches(argv_log: &Path) -> usize {
    std::fs::read_to_string(argv_log)
        .unwrap_or_default()
        .matches("--end--")
        .count()
}

/// The plugin must be described and then closed by the server, within 20
/// seconds: a plugin the server admitted would stay connected.
fn kept_out(plugin: FakeEditorPlugin) {
    let started = Instant::now();
    while !plugin
        .received()
        .iter()
        .any(|(method, _)| method == "bridge.describe_session")
    {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the plugin never reached the server"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(plugin.join());
    });
    receiver
        .recv_timeout(Duration::from_secs(20))
        .expect("the server closed the plugin's session instead of serving it")
        .expect("the plugin saw its session closed");
}

#[test]
fn headless_cache_writers_refuse_while_an_editor_is_connected() {
    let tree = ProjectTree::new("m8-exclusive-refused");
    tree.write(
        "data.tres",
        "[gd_resource type=\"Resource\" format=3]\n\n[resource]\n",
    );
    let (godot, argv_log) = engine(&tree, "");
    let mut session = open(&tree, &godot);
    let editor = plugin(&tree);
    let mut id = 1;
    wait_connected(&mut session, &mut id);

    for (tool, arguments) in [
        (
            "project.import.run",
            json!({ "idempotencyId": "import-while-connected" }),
        ),
        (
            "project.uid.update",
            json!({ "target": "data.tres", "idempotencyId": "uid-while-connected" }),
        ),
        (
            "project.uid.update",
            json!({ "target": "data.tres", "dry_run": true, "idempotencyId": "uid-dry-run" }),
        ),
    ] {
        id += 1;
        let refused = session.call_tool(id, tool, arguments);
        assert_eq!(error_code(&refused), json!("conflict"), "{tool}: {refused}");
        assert!(
            refused.to_string().contains("close the editor first"),
            "{tool}: the refusal says how to run it: {refused}"
        );
    }
    assert_eq!(launches(&argv_log), 0, "no Godot was started");
    let _ = session.finish();
    editor.join().expect("the plugin ended");
}

#[test]
fn an_editor_dialling_in_during_an_import_is_kept_out_until_it_ends() {
    let tree = ProjectTree::new("m8-exclusive-import");
    let marker = tree.base.join("release-import");
    let (godot, argv_log) = engine(&tree, &wait_for(&marker));
    let mut session = open(&tree, &godot);
    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "project.import.run",
            "arguments": { "deadline_ms": 60_000, "idempotencyId": "import-running" }
        }
    }));
    wait_exists(&argv_log);
    kept_out(plugin(&tree));

    // Let the import end and answer.
    std::fs::write(&marker, b"go").expect("release the import");
    let _ = session.await_response(10, "tools/call");
    let status = ok(&session.call_tool(11, "editor.status.get", json!({})));
    assert_eq!(status["connected"], json!(false), "{status}");
    assert_eq!(status["sessions_opened"], json!(0), "{status}");

    let editor = plugin(&tree);
    let mut id = 11;
    wait_connected(&mut session, &mut id);
    let _ = session.finish();
    editor.join().expect("the plugin ended");
}

#[test]
fn an_editor_dialling_in_during_a_uid_update_is_kept_out_until_it_ends() {
    let tree = ProjectTree::new("m8-exclusive-uid");
    tree.write(
        "data.tres",
        "[gd_resource type=\"Resource\" format=3]\n\n[resource]\n",
    );
    let marker = tree.base.join("release-helper");
    let (godot, argv_log) = engine(&tree, &wait_for(&marker));
    let mut session = open(&tree, &godot);
    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "project.uid.update",
            "arguments": { "target": "data.tres", "idempotencyId": "uid-running" }
        }
    }));
    wait_exists(&argv_log);
    kept_out(plugin(&tree));

    std::fs::write(&marker, b"go").expect("release the helper");
    let _ = session.await_response(10, "tools/call");
    let status = ok(&session.call_tool(11, "editor.status.get", json!({})));
    assert_eq!(status["connected"], json!(false), "{status}");
    assert_eq!(status["sessions_opened"], json!(0), "{status}");

    let editor = plugin(&tree);
    let mut id = 11;
    wait_connected(&mut session, &mut id);
    let _ = session.finish();
    editor.join().expect("the plugin ended");
}

#[test]
fn a_committed_import_or_uid_update_replays_while_an_editor_is_connected() {
    let tree = ProjectTree::new("m8-exclusive-replay");
    tree.write(
        "data.tres",
        "[gd_resource type=\"Resource\" format=3]\n\n[resource]\n",
    );
    let (godot, argv_log) = engine(&tree, "");
    let mut session = open(&tree, &godot);
    let import = json!({ "idempotencyId": "import-replay" });
    let uid = json!({ "target": "data.tres", "idempotencyId": "uid-replay" });
    let first_import = session.call_tool(2, "project.import.run", import.clone());
    assert!(first_import.get("error").is_none(), "{first_import}");
    ok(&session.call_tool(3, "project.uid.update", uid.clone()));
    let launched = launches(&argv_log);
    assert!(launched >= 2, "both ran a Godot process: {launched}");

    let editor = plugin(&tree);
    let mut id = 3;
    wait_connected(&mut session, &mut id);
    id += 1;
    let replayed_import = session.call_tool(id, "project.import.run", import);
    assert!(
        replayed_import.get("error").is_none()
            && !replayed_import.to_string().contains("\"conflict\""),
        "a committed import answers from the journal: {replayed_import}"
    );
    id += 1;
    let replayed_uid = session.call_tool(id, "project.uid.update", uid);
    assert!(
        replayed_uid.get("error").is_none() && !replayed_uid.to_string().contains("\"conflict\""),
        "a committed UID update answers from the journal: {replayed_uid}"
    );
    assert_eq!(launches(&argv_log), launched, "no Godot ran again");
    let _ = session.finish();
    editor.join().expect("the plugin ended");
}
