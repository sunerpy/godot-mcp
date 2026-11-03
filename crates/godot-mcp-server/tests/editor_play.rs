//! Contract 0.3 acceptance at the MCP surface: running the project in the
//! connected editor, as its Play buttons do.
//!
//! The editor starts the game as its own child process, so nothing about it is
//! inside a boundary this server holds: an untrusted project never runs this
//! way. The editor saves the open scenes first, so a start declares every open
//! scene and may write files nobody can list in advance. The `runtime`
//! profile lists the two play tools and no other editor tool, and serves the
//! editor bridge for them. What a real editor runs and stops is checked by the
//! live-editor suite.

#![cfg(target_os = "linux")]

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use godot_mcp_server::test_support::fake_editor_plugin::{
    FakeAnswer, FakeEditorPlugin, FakePluginScript,
};
use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, error_code, sha256_hex};

const PLAY: [&str; 2] = ["editor.play.start", "editor.play.stop"];

fn rendezvous(tree: &ProjectTree) -> PathBuf {
    tree.root
        .join(".godot-mcp")
        .join("editor-bridge.rendezvous")
}

fn open(tree: &ProjectTree, profile: &str) -> StdioSession {
    StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile,
        godot: None,
    })
}

/// A server whose configuration marks the project untrusted.
fn open_untrusted(tree: &ProjectTree, profile: &str) -> StdioSession {
    let config = tree.base.join("serve.toml");
    std::fs::write(&config, "[process]\ntrust = \"untrusted\"\n").expect("configuration");
    let mut command = Command::new(env!("CARGO_BIN_EXE_qa-journal-foundation"));
    command
        .arg("--project")
        .arg(&tree.root)
        .arg("--profile")
        .arg(profile)
        .arg("--config")
        .arg(&config)
        .env_remove("GODOT_MCP_TEST_LAUNCH_CRASH_STAGE");
    StdioSession::handshake(StdioSession::start_command(command)).0
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

fn listed(session: &mut StdioSession, id: u64) -> Vec<String> {
    let response = session.request(id, "tools/list", json!({}));
    response["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list must answer: {response}"))
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect()
}

/// Stop until the plugin answers: in the `runtime` profile no editor read is
/// listed, and stopping a game that does not run changes nothing.
fn wait_connected(session: &mut StdioSession, next_id: &mut u64) -> Value {
    let started = Instant::now();
    loop {
        *next_id += 1;
        let response = session.call_tool(
            *next_id,
            "editor.play.stop",
            json!({ "idempotencyId": format!("connect-{next_id}") }),
        );
        if !refused(&response) {
            return ok(&response);
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the plugin never connected: {response}"
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

fn playing() -> Value {
    json!({
        "playing": true,
        "scene": "res://main.tscn",
        "saved": [],
        "also_saved": [],
        "action": "godot-mcp: editor.play.start"
    })
}

fn stopped(was_playing: bool) -> Value {
    json!({ "was_playing": was_playing, "playing": false, "action": "godot-mcp: editor.play.stop" })
}

fn playing_plugin(rendezvous: &Path, start: FakeAnswer, open_scenes: Value) -> FakeEditorPlugin {
    FakeEditorPlugin::spawn(
        rendezvous,
        FakePluginScript::implementing(&["editor.state", "editor.play.start", "editor.play.stop"])
            .versions("0.3.0", "4.7.1.stable.fake")
            .answering(
                "editor.state",
                FakeAnswer::Payload(json!({ "open_scenes": open_scenes })),
            )
            .answering("editor.play.start", start)
            .answering("editor.play.stop", FakeAnswer::Payload(stopped(false))),
    )
}

#[test]
fn only_the_profiles_that_run_the_project_list_the_play_tools() {
    for (profile, lists_play, serves_bridge) in [
        ("core", false, false),
        ("editor", false, true),
        ("runtime", true, true),
        ("full", true, true),
    ] {
        let tree = ProjectTree::new(&format!("m8-play-{profile}"));
        let mut session = open(&tree, profile);
        let tools = listed(&mut session, 2);
        for tool in PLAY {
            assert_eq!(
                tools.iter().any(|listed| listed == tool),
                lists_play,
                "{profile} lists {tool}: {tools:?}"
            );
        }
        if profile == "runtime" {
            // editor.session.* start managed editor sessions, which the
            // bridge does not serve.
            let editor_tools: Vec<&String> = tools
                .iter()
                .filter(|tool| tool.starts_with("editor.") && !tool.starts_with("editor.session."))
                .collect();
            assert_eq!(
                editor_tools,
                ["editor.play.start", "editor.play.stop"],
                "the runtime profile lists no other live-editor tool"
            );
        }
        assert_eq!(
            rendezvous(&tree).exists(),
            serves_bridge,
            "{profile} serves the editor bridge exactly when it lists an editor tool"
        );
        let _ = session.finish();
    }
}

#[test]
fn the_runtime_profile_starts_and_stops_the_game_in_the_editor() {
    let tree = ProjectTree::new("m8-play-runtime");
    tree.write(
        "main.tscn",
        "[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node2D\"]\n",
    );
    let mut session = open(&tree, "runtime");
    let plugin = playing_plugin(
        &rendezvous(&tree),
        FakeAnswer::Payload(playing()),
        json!([{ "path": "res://main.tscn" }]),
    );
    let mut id = 1;
    let first = wait_connected(&mut session, &mut id);
    assert_eq!(first["was_playing"], json!(false), "{first}");
    id += 1;
    let started = ok(&session.call_tool(
        id,
        "editor.play.start",
        json!({ "idempotencyId": "play-main" }),
    ));
    assert_eq!(started["playing"], json!(true), "{started}");
    assert_eq!(started["mutation_status"], "committed", "{started}");
    let asked = &sent(&plugin, "editor.play.start")[0];
    assert_eq!(asked["scene"], "main", "{asked}");
    assert_eq!(
        asked["scenes"],
        json!([{
            "path": "res://main.tscn",
            "sha256": sha256_hex(b"[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node2D\"]\n")
        }]),
        "{asked}"
    );
    for (scene, expected) in [
        ("current", "current"),
        ("main.tscn", "res://main.tscn"),
        ("res://levels/one.tscn", "res://levels/one.tscn"),
    ] {
        id += 1;
        ok(&session.call_tool(
            id,
            "editor.play.start",
            json!({ "scene": scene, "idempotencyId": format!("play-{id}") }),
        ));
        let asked = sent(&plugin, "editor.play.start");
        assert_eq!(asked.last().expect("asked")["scene"], expected);
    }
    let asked_before = sent(&plugin, "editor.play.start").len();
    for (scene, what) in [
        (json!("../outside.tscn"), "a path outside the project"),
        (json!("main.gd"), "a script"),
        (json!(""), "an empty scene"),
        (json!(3), "a number"),
    ] {
        id += 1;
        let response = session.call_tool(
            id,
            "editor.play.start",
            json!({ "scene": scene, "idempotencyId": format!("play-bad-{id}") }),
        );
        assert!(refused(&response), "{what} is refused: {response}");
    }
    assert_eq!(sent(&plugin, "editor.play.start").len(), asked_before);
    id += 1;
    let other = session.call_tool(id, "editor.status.get", json!({}));
    assert!(
        refused(&other),
        "the runtime profile serves no other editor tool: {other}"
    );
    id += 1;
    ok(&session.call_tool(
        id,
        "editor.play.stop",
        json!({ "idempotencyId": "play-stop" }),
    ));
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_start_declares_every_open_scene_and_a_lost_answer_blocks_writes() {
    let tree = ProjectTree::new("m8-play-lost");
    tree.write(
        "a.tscn",
        "[gd_scene format=3]\n\n[node name=\"A\" type=\"Node\"]\n",
    );
    tree.write(
        "b.tscn",
        "[gd_scene format=3]\n\n[node name=\"B\" type=\"Node\"]\n",
    );
    let mut session = open(&tree, "full");
    let plugin = playing_plugin(
        &rendezvous(&tree),
        FakeAnswer::Disconnect,
        json!([{ "path": "res://a.tscn" }, { "path": "res://b.tscn" }]),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let lost = session.call_tool(
        id,
        "editor.play.start",
        json!({ "idempotencyId": "play-lost" }),
    );
    assert_eq!(error_code(&lost), json!("uncertain"), "{lost}");
    plugin.join().expect("the plugin dropped its connection");
    id += 1;
    let blocked = session.call_tool(
        id,
        "file.create",
        json!({ "path": "notes/after.txt", "content": "x\n", "idempotencyId": "after-play" }),
    );
    assert!(refused(&blocked), "{blocked}");
    let _ = session.finish();
    let recorded = recorded_transaction(&tree, &lost);
    assert!(recorded.quarantined, "{recorded:?}");
    assert_eq!(
        recorded.touched_paths,
        vec!["a.tscn".to_owned(), "b.tscn".to_owned(), "*".to_owned()]
    );
}

#[test]
fn an_untrusted_project_never_runs_in_the_editor() {
    let tree = ProjectTree::new("m8-play-untrusted");
    let mut session = open_untrusted(&tree, "runtime");
    let plugin = playing_plugin(
        &rendezvous(&tree),
        FakeAnswer::Payload(playing()),
        json!([]),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let denied = session.call_tool(
        id,
        "editor.play.start",
        json!({ "idempotencyId": "play-untrusted" }),
    );
    assert_eq!(error_code(&denied), json!("permission_denied"), "{denied}");
    assert!(
        denied.to_string().contains("process.trust"),
        "the refusal names the setting: {denied}"
    );
    assert!(sent(&plugin, "editor.play.start").is_empty());
    assert!(
        sent(&plugin, "editor.state").is_empty(),
        "nothing is asked of the editor"
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_plugin_without_play_says_how_to_update_it() {
    let tree = ProjectTree::new("m8-play-old");
    let mut session = open(&tree, "full");
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state"]),
    );
    let started = Instant::now();
    let mut id = 1;
    loop {
        id += 1;
        let status = ok(&session.call_tool(id, "editor.status.get", json!({})));
        if status["connected"] == json!(true) {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(30), "{status}");
        std::thread::sleep(Duration::from_millis(50));
    }
    for tool in PLAY {
        id += 1;
        let refused = session.call_tool(id, tool, json!({ "idempotencyId": format!("old-{id}") }));
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
