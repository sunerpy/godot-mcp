//! M5 acceptance: managed game sessions over the real stdio assembly.
//!
//! The production server runs out of process with the non-shipping
//! `qa-fake-game` binary as its Godot executable. The stand-in negotiates like
//! an editor build and, run as a game, serves the runtime probe's methods over
//! the session's own authenticated bridge, so every server-side path is the
//! shipped one: discovery, the detached supervised launch, readiness, journaled
//! runtime commands, capture verification into the artifact store, output
//! archiving, stopping, terminating, and the next server reconciling what a
//! killed one left running. Real Godot builds are exercised by
//! `scripts/real_client/m5_smoke.py`.

mod support;

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, error_code};

fn fake_game() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_qa-fake-game"))
}

fn server(tree: &ProjectTree) -> StdioSession {
    StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
        godot: Some(fake_game()),
    })
}

fn structured(response: &Value) -> Value {
    assert!(
        response.get("error").is_none() && response["result"]["isError"] != json!(true),
        "the call must succeed: {response}"
    );
    response["result"]["structuredContent"].clone()
}

/// Whether `pid` is a live, non-zombie process, observed the way the server
/// itself observes one where it can, and through `tasklist` on Windows.
fn alive(pid: u32) -> bool {
    if godot_mcp_engine::supervision::procfs::observations_available() {
        return godot_mcp_engine::supervision::procfs::process_identity(pid)
            .expect("this host observes processes")
            .is_some();
    }
    let listed = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .expect("tasklist runs");
    String::from_utf8_lossy(&listed.stdout).contains(&format!("\"{pid}\""))
}

fn game_pid(tree: &ProjectTree) -> u32 {
    let path = tree.root.join("fake-game.pid");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(pid) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "the game never recorded its pid");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while alive(pid) {
        assert!(Instant::now() < deadline, "process {pid} is still running");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_started_game_answers_reads_commands_and_captures_until_it_is_stopped() {
    let tree = ProjectTree::new("m5-session");
    let mut session = server(&tree);

    let started = structured(&session.call_tool(
        2,
        "project.run.start",
        json!({ "idempotencyId": "m5-start", "display": "headless" }),
    ));
    let id = started["session"]["session"]
        .as_str()
        .expect("session id")
        .to_owned();
    assert!(id.starts_with("run_") && id.len() == 36, "{started}");
    assert_eq!(
        started["session"]["state"],
        json!("running"),
        "start answers a running session"
    );
    assert_eq!(started["session"]["display"], json!("headless"));
    assert_eq!(started["session"]["probe"]["connected"], json!(true));
    // A trusted project's default boundary: the weak locator on Linux and
    // macOS, a Job Object on Windows.
    assert_eq!(
        started["execution"]["boundary_strength"],
        json!(if cfg!(windows) { "strong" } else { "weak" }),
        "{started}"
    );
    let pid = game_pid(&tree);
    assert!(alive(pid), "the game runs after start answered");
    let transcript = session.transcript.clone();
    assert!(
        !transcript.contains(&tree.root.display().to_string()),
        "no answer names a private path"
    );

    // The same start, replayed, is the same session, not a second game.
    let replayed = structured(&session.call_tool(
        3,
        "project.run.start",
        json!({ "idempotencyId": "m5-start", "display": "headless" }),
    ));
    assert_eq!(replayed["session"]["session"], json!(id));
    assert_eq!(replayed["replayed"], json!(true));

    let tree_read = structured(&session.call_tool(4, "runtime.tree.get", json!({ "session": id })));
    assert_eq!(tree_read["session"], json!(id));
    assert_eq!(tree_read["revision"], json!(1));
    assert!(
        tree_read["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .any(|node| node["path"] == json!("/root/Main/Player"))
    );
    let properties = structured(&session.call_tool(
        5,
        "runtime.properties.get",
        json!({ "session": id, "node": "Player", "properties": ["position"] }),
    ));
    assert_eq!(
        properties["properties"]["position"]["type"],
        json!("vector2")
    );

    let input = structured(&session.call_tool(
        6,
        "input.action.send",
        json!({ "session": id, "idempotencyId": "m5-right", "action": "ui_right", "state": "press", "holdMs": 500 }),
    ));
    assert_eq!(input["mutation_status"], json!("committed"), "{input}");
    assert!(input["transaction_id"].is_string());
    assert_eq!(input["held"][0]["input"], json!("action:ui_right"));
    let refused = session.call_tool(
        7,
        "input.action.send",
        json!({ "session": id, "action": "ui_right" }),
    );
    assert_eq!(
        error_code(&refused),
        json!("invalid_input"),
        "a command needs an idempotencyId"
    );

    let shot = structured(&session.call_tool(
        8,
        "runtime.screenshot.capture",
        json!({ "session": id, "idempotencyId": "m5-shot" }),
    ));
    let artifact = &shot["artifact"];
    assert_eq!(artifact["media_type"], json!("image/png"));
    assert_eq!(
        (artifact["width"].clone(), artifact["height"].clone()),
        (json!(1), json!(1))
    );
    let uri = artifact["uri"].as_str().expect("artifact uri").to_owned();
    assert!(uri.starts_with("godot-mcp://artifacts/"), "{uri}");
    let read = session.request(9, "resources/read", json!({ "uri": uri }));
    let blob = read["result"]["contents"][0]["blob"]
        .as_str()
        .expect("blob")
        .to_owned();
    assert!(!blob.is_empty(), "the artifact reads back: {read}");
    assert_eq!(
        read["result"]["contents"][0]["mimeType"],
        json!("image/png"),
        "{read}"
    );

    let recording = structured(&session.call_tool(
        10,
        "runtime.recording.capture",
        json!({ "session": id, "idempotencyId": "m5-rec", "durationMs": 200, "fps": 10 }),
    ));
    assert_eq!(recording["artifact"]["media_type"], json!("image/apng"));
    assert_eq!(recording["artifact"]["frames"], json!(2));

    let logs = structured(&session.call_tool(11, "project.run.logs", json!({ "session": id })));
    assert!(
        logs["text"]
            .as_str()
            .is_some_and(|text| text.contains("fake game started")),
        "{logs}"
    );
    assert_eq!(logs["running"], json!(true));
    let errors = structured(&session.call_tool(
        12,
        "project.run.logs",
        json!({ "session": id, "stream": "stderr" }),
    ));
    assert!(
        errors["text"]
            .as_str()
            .is_some_and(|text| text.contains("fake game warning"))
    );

    let stopped = structured(&session.call_tool(
        13,
        "project.run.stop",
        json!({ "session": id, "idempotencyId": "m5-stop" }),
    ));
    assert_eq!(stopped["state"], json!("exited"), "{stopped}");
    assert_eq!(stopped["exit"]["termination"], json!("exited_clean"));
    assert_eq!(stopped["exit"]["boundary_empty"], json!(true));
    wait_gone(pid);

    // An ended session answers its record and its archived output, and takes
    // no more commands.
    let status = structured(&session.call_tool(14, "project.run.status", json!({ "session": id })));
    assert_eq!(status["state"], json!("exited"));
    let archived = structured(&session.call_tool(15, "project.run.logs", json!({ "session": id })));
    assert_eq!(archived["running"], json!(false));
    assert_eq!(archived["eof"], json!(true));
    assert!(
        archived["text"]
            .as_str()
            .is_some_and(|text| text.contains("fake game started"))
    );
    let late = session.call_tool(
        16,
        "input.key.send",
        json!({ "session": id, "idempotencyId": "m5-late", "key": "A" }),
    );
    assert_eq!(error_code(&late), json!("unavailable"), "{late}");
    let again = structured(&session.call_tool(
        17,
        "project.run.stop",
        json!({ "session": id, "idempotencyId": "m5-stop-again" }),
    ));
    assert_eq!(again["already_ended"], json!(true));
    let _ = session.finish();
}

#[test]
fn a_game_that_ignores_the_stop_is_terminated_inside_its_boundary() {
    let tree = ProjectTree::new("m5-stubborn");
    tree.write("fake-game.conf", "stubborn\n");
    let mut session = server(&tree);
    let started = structured(&session.call_tool(
        2,
        "project.run.start",
        json!({ "idempotencyId": "m5-stubborn", "display": "headless" }),
    ));
    let id = started["session"]["session"]
        .as_str()
        .expect("session id")
        .to_owned();
    let pid = game_pid(&tree);
    let stopped = structured(&session.call_tool(
        3,
        "project.run.stop",
        json!({ "session": id, "idempotencyId": "m5-stubborn-stop" }),
    ));
    assert_eq!(stopped["state"], json!("exited"), "{stopped}");
    assert_ne!(
        stopped["exit"]["termination"],
        json!("exited_clean"),
        "{stopped}"
    );
    assert_eq!(stopped["exit"]["boundary_empty"], json!(true));
    wait_gone(pid);
    let _ = session.finish();
}

#[test]
fn a_game_that_exits_before_it_is_ready_fails_its_start() {
    let tree = ProjectTree::new("m5-early-exit");
    tree.write("fake-game.conf", "exit:3\n");
    let mut session = server(&tree);
    let refused = session.call_tool(
        2,
        "project.run.start",
        json!({ "idempotencyId": "m5-early", "display": "headless" }),
    );
    assert_eq!(error_code(&refused), json!("unavailable"), "{refused}");
    assert!(
        refused
            .to_string()
            .contains("exited before it became ready"),
        "{refused}"
    );
    let listed = structured(&session.call_tool(3, "project.run.status", json!({})));
    let sessions = listed["sessions"].as_array().expect("sessions");
    assert_eq!(sessions.len(), 1, "{listed}");
    assert_eq!(sessions[0]["state"], json!("failed"), "{listed}");
    assert_eq!(sessions[0]["exit"]["exit_code"], json!(3), "{listed}");
    let _ = session.finish();
}

#[test]
fn a_session_a_killed_server_left_running_is_terminated_by_the_next_one() {
    let tree = ProjectTree::new("m5-orphan");
    // A game that keeps running after its bridge closed: the stand-in for a
    // game whose probe has not yet given up on its lost server.
    tree.write("fake-game.conf", "stubborn\n");
    let mut first = server(&tree);
    let started = structured(&first.call_tool(
        2,
        "project.run.start",
        json!({ "idempotencyId": "m5-orphan", "display": "headless" }),
    ));
    let id = started["session"]["session"]
        .as_str()
        .expect("session id")
        .to_owned();
    let pid = game_pid(&tree);
    first.kill();
    if cfg!(windows) {
        // A Job Object is kill-on-close: the killed server held its last
        // handle, so the game ended with the server.
        wait_gone(pid);
    } else {
        assert!(alive(pid), "a killed server leaves its game running");
    }

    let mut second = server(&tree);
    wait_gone(pid);
    let status = structured(&second.call_tool(2, "project.run.status", json!({ "session": id })));
    assert_eq!(status["state"], json!("reconciled"), "{status}");
    assert!(
        status["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("proven empty"))
    );
    let logs = structured(&second.call_tool(3, "project.run.logs", json!({ "session": id })));
    assert!(
        logs["text"]
            .as_str()
            .is_some_and(|text| text.contains("fake game started")),
        "{logs}"
    );
    let _ = second.finish();
}

fn marker(tree: &ProjectTree, expected: &str) {
    let path = tree.root.join("fake-game.sequence");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if std::fs::read_to_string(&path).is_ok_and(|text| text == expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the probe never recorded `{expected}`: {:?}",
            std::fs::read_to_string(&path)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A recording the client cancels is withdrawn from the probe, which keeps no
/// frame of it, and the server registers no artifact (plan section 10.3:
/// partial media is removed on clean cancellation).
#[test]
fn a_cancelled_recording_keeps_no_media() {
    let tree = ProjectTree::new("m5-cancel-recording");
    let mut session = server(&tree);
    let started = structured(&session.call_tool(
        2,
        "project.run.start",
        json!({ "idempotencyId": "m5-recording-start", "display": "headless" }),
    ));
    let id = started["session"]["session"]
        .as_str()
        .expect("session id")
        .to_owned();
    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "runtime.recording.capture",
            "arguments": { "session": id, "idempotencyId": "m5-cancel-recording", "durationMs": 5000 }
        }
    }));
    let path = tree.root.join("fake-game.recording");
    let wait_for = |expected: &str| {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if std::fs::read_to_string(&path).is_ok_and(|text| text == expected) {
                return;
            }
            assert!(Instant::now() < deadline, "the probe never saw: {expected}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    wait_for("received runtime.capture.recording");
    session.notify(
        "notifications/cancelled",
        json!({ "requestId": 3, "reason": "the client gave up" }),
    );
    wait_for("withdrawn runtime.capture.recording");
    let resources = session.request(4, "resources/list", json!({}));
    assert!(
        !resources.to_string().contains("image/apng"),
        "a cancelled recording registers no artifact: {resources}"
    );
    let stopped = structured(&session.call_tool(
        5,
        "project.run.stop",
        json!({ "session": id, "idempotencyId": "m5-recording-stop" }),
    ));
    assert_eq!(stopped["state"], json!("exited"), "{stopped}");
    let _ = session.finish();
}

#[test]
fn a_cancelled_input_sequence_is_withdrawn_from_the_probe() {
    let tree = ProjectTree::new("m5-cancel");
    let mut session = server(&tree);
    let started = structured(&session.call_tool(
        2,
        "project.run.start",
        json!({ "idempotencyId": "m5-cancel-start", "display": "headless" }),
    ));
    let id = started["session"]["session"]
        .as_str()
        .expect("session id")
        .to_owned();
    // A sequence that holds an action through a long wait, cancelled by the
    // client while the probe runs it.
    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "input.sequence.send",
            "arguments": {
                "session": id,
                "idempotencyId": "m5-cancel-sequence",
                "steps": [
                    { "kind": "action", "action": "ui_right", "state": "press" },
                    { "kind": "wait", "ms": 5000 }
                ]
            }
        }
    }));
    marker(&tree, "received runtime.input.sequence");
    session.notify(
        "notifications/cancelled",
        json!({ "requestId": 3, "reason": "the client gave up" }),
    );
    marker(&tree, "withdrawn runtime.input.sequence");
    // The session takes the next command once the withdrawn one answered.
    let tree_read = structured(&session.call_tool(4, "runtime.tree.get", json!({ "session": id })));
    assert_eq!(tree_read["session"], json!(id));
    let stopped = structured(&session.call_tool(
        5,
        "project.run.stop",
        json!({ "session": id, "idempotencyId": "m5-cancel-stop" }),
    ));
    assert_eq!(stopped["state"], json!("exited"), "{stopped}");
    let _ = session.finish();
}
