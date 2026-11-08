//! Contract 0.3 typed control and observation of a managed game session, over
//! the real stdio assembly.
//!
//! The production server runs out of process with the non-shipping
//! `qa-fake-game` binary as its Godot executable, so every server-side path is
//! the shipped one: argument checks, the probe's method list, journaled
//! commands, withdrawal on cancellation and on a session stop, a lost probe,
//! and the input recording kept as an artifact. What the probe does in a real
//! game is checked by `live_runtime_control.rs` against real Godot builds.

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

fn start(session: &mut StdioSession, id: u64, key: &str) -> String {
    let started = structured(&session.call_tool(
        id,
        "project.run.start",
        json!({ "idempotencyId": key, "display": "headless" }),
    ));
    started["session"]["session"]
        .as_str()
        .expect("session id")
        .to_owned()
}

/// Wait, at most 20 s, until `<project>/<file>` reads `expected`.
fn marker(tree: &ProjectTree, file: &str, expected: &str) {
    let path = tree.root.join(file);
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

fn base64_decode(text: &str) -> Vec<u8> {
    let value = |byte: u8| -> u32 {
        u32::from(match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            other => panic!("not base64: {other}"),
        })
    };
    let bytes: Vec<u8> = text.bytes().filter(|byte| *byte != b'=').collect();
    let mut out = Vec::new();
    for group in bytes.chunks(4) {
        let word = group.iter().enumerate().fold(0_u32, |word, (index, byte)| {
            word | (value(*byte) << (18 - 6 * index))
        });
        for index in 0..group.len().saturating_sub(1) {
            out.push(((word >> (16 - 8 * index)) & 0xff) as u8);
        }
    }
    out
}

/// Every typed control and observation tool reaches the probe, answers what
/// it said, and refuses an argument outside its bounds before sending.
#[test]
fn the_control_tools_answer_what_the_probe_did() {
    let tree = ProjectTree::new("runtime-control");
    let mut session = server(&tree);
    let id = start(&mut session, 2, "control-start");

    let batch = structured(&session.call_tool(
        3,
        "runtime.properties.batch.get",
        json!({ "session": id, "targets": [{ "node": "Player", "properties": ["position"] }, { "singleton": "Engine" }] }),
    ));
    assert_eq!(batch["session"], json!(id));
    assert_eq!(batch["targets"][1]["singleton"], json!("Engine"), "{batch}");

    let written = structured(&session.call_tool(
        4,
        "runtime.properties.set",
        json!({
            "session": id,
            "idempotencyId": "control-set",
            "target": { "node": "Player", "properties": { "position": { "type": "vector2", "value": { "x": 5.0, "y": 6.0 } } } }
        }),
    ));
    assert_eq!(written["mutation_status"], json!("committed"), "{written}");
    assert!(written["transaction_id"].is_string());
    assert_eq!(written["results"][0]["matched"], json!(true));

    let found = structured(&session.call_tool(
        5,
        "runtime.nodes.find",
        json!({ "session": id, "group": "players", "near": { "node": "Player", "radius": 100 } }),
    ));
    assert_eq!(found["nodes"][0]["name"], json!("Player"), "{found}");

    let sampled = structured(&session.call_tool(
        6,
        "runtime.sample",
        json!({ "session": id, "series": [{ "monitor": "TIME_FPS" }], "durationMs": 200 }),
    ));
    assert_eq!(
        sampled["series"][0]["stats"]["max"],
        json!(60.0),
        "{sampled}"
    );

    let path = session.call_tool(
        7,
        "runtime.navigation.path.get",
        json!({ "session": id, "node": "Player", "to": { "x": 9, "y": 9 } }),
    );
    assert_eq!(
        error_code(&path),
        json!("unavailable"),
        "the probe's refusal is answered: {path}"
    );

    let moved = structured(&session.call_tool(
        8,
        "runtime.node.move",
        json!({ "session": id, "idempotencyId": "control-move", "node": "Player", "to": { "x": 5, "y": 6 }, "mode": { "teleport": {} } }),
    ));
    assert_eq!(moved["outcome"], json!("teleported"), "{moved}");

    let pressed = structured(&session.call_tool(
        9,
        "runtime.ui.press",
        json!({ "session": id, "idempotencyId": "control-press", "target": { "text": "Start" } }),
    ));
    assert_eq!(pressed["pressed_signal"], json!(true), "{pressed}");

    let recording = structured(&session.call_tool(
        10,
        "input.recording.start",
        json!({ "session": id, "idempotencyId": "control-record" }),
    ));
    assert_eq!(recording["recording_id"], json!("input-recording-1"));
    let recorded = structured(&session.call_tool(
        11,
        "input.recording.stop",
        json!({ "session": id, "idempotencyId": "control-record-stop" }),
    ));
    assert_eq!(
        recorded["counts"],
        json!({ "total": 2, "probe": 1, "external": 1 })
    );
    let artifact = &recorded["artifact"];
    assert_eq!(
        artifact["media_type"],
        json!("application/json"),
        "{recorded}"
    );
    let uri = artifact["uri"]
        .as_str()
        .expect("an artifact uri")
        .to_owned();
    let read = session.request(12, "resources/read", json!({ "uri": uri }));
    let kept: Value = serde_json::from_slice(&base64_decode(
        read["result"]["contents"][0]["blob"]
            .as_str()
            .expect("the recording reads back"),
    ))
    .expect("the kept recording is JSON");
    assert_eq!(
        kept["events"], recorded["events"],
        "the artifact holds the events"
    );
    assert!(kept.get("frame").is_none() && kept.get("revision").is_none());

    // The recorded events are what a replay takes, by time or by physics
    // frame.
    let replayed = structured(&session.call_tool(
        13,
        "input.replay",
        json!({ "session": id, "idempotencyId": "control-replay", "events": recorded["events"] }),
    ));
    assert_eq!(replayed["outcome"], json!("completed"), "{replayed}");
    assert_eq!(recorded["events"][1]["frame"], json!(15), "{recorded}");
    let by_frame = structured(&session.call_tool(
        30,
        "input.replay",
        json!({ "session": id, "idempotencyId": "control-replay-frames", "events": recorded["events"], "schedule": "physics_frame" }),
    ));
    assert_eq!(by_frame["outcome"], json!("completed"), "{by_frame}");
    assert_eq!(
        by_frame["mutation_status"],
        json!("committed"),
        "{by_frame}"
    );

    let per_frame = structured(&session.call_tool(
        31,
        "runtime.sample",
        json!({ "session": id, "series": [{ "monitor": "TIME_FPS" }], "cadence": "physics_frame", "durationMs": 200 }),
    ));
    assert_eq!(per_frame["session"], json!(id), "{per_frame}");

    let stepped = structured(&session.call_tool(
        32,
        "runtime.frames.step",
        json!({ "session": id, "idempotencyId": "control-step", "frames": 3 }),
    ));
    assert_eq!(stepped["mutation_status"], json!("committed"), "{stepped}");
    assert!(stepped["transaction_id"].is_string(), "{stepped}");
    assert_eq!(stepped["frames_advanced"], json!(3), "{stepped}");
    assert_eq!(
        (
            stepped["first_frame"].clone(),
            stepped["last_frame"].clone()
        ),
        (json!(101), json!(103)),
        "{stepped}"
    );
    assert_eq!(stepped["held"], json!(true), "{stepped}");

    // Out of bounds is refused before anything reaches the probe.
    for (request, tool, arguments) in [
        (
            14,
            "runtime.wait",
            json!({ "session": id, "condition": { "node_exists": { "node": "A" } }, "timeoutMs": 30001 }),
        ),
        (
            15,
            "runtime.sample",
            json!({ "session": id, "series": [{ "monitor": "TIME_FPS" }], "durationMs": 10001 }),
        ),
        (
            16,
            "input.replay",
            json!({ "session": id, "idempotencyId": "control-late", "events": [{ "t_ms": 30001, "kind": "action", "action": "jump" }] }),
        ),
        (
            17,
            "runtime.properties.set",
            json!({ "session": id, "idempotencyId": "control-engine", "target": { "singleton": "Engine", "properties": { "physics_ticks_per_second": { "type": "integer", "value": 0 } } } }),
        ),
        (
            33,
            "runtime.frames.step",
            json!({ "session": id, "idempotencyId": "control-step-far", "frames": 601 }),
        ),
        (
            34,
            "runtime.frames.step",
            json!({ "session": id, "idempotencyId": "control-step-then", "frames": 1, "then": "pause" }),
        ),
        (
            35,
            "runtime.frames.step",
            json!({ "session": id, "idempotencyId": "control-step-long", "frames": 1, "timeoutMs": 30001 }),
        ),
        // Accepted by the schemas, which state no rule across fields, and
        // refused by the checks before anything is sent.
        (
            36,
            "runtime.sample",
            json!({ "session": id, "series": [{ "monitor": "TIME_FPS" }], "cadence": "physics_frame", "intervalMs": 50 }),
        ),
        (
            37,
            "input.replay",
            json!({ "session": id, "idempotencyId": "control-no-frame", "schedule": "physics_frame", "events": [{ "t_ms": 0, "kind": "action", "action": "jump" }] }),
        ),
        (
            38,
            "input.replay",
            json!({ "session": id, "idempotencyId": "control-no-time", "events": [{ "frame": 0, "kind": "action", "action": "jump" }] }),
        ),
    ] {
        let refused = session.call_tool(request, tool, arguments);
        assert_eq!(
            error_code(&refused),
            json!("invalid_input"),
            "{tool}: {refused}"
        );
    }

    let stopped = structured(&session.call_tool(
        18,
        "project.run.stop",
        json!({ "session": id, "idempotencyId": "control-stop" }),
    ));
    assert_eq!(
        stopped["exit"]["termination"],
        json!("exited_clean"),
        "{stopped}"
    );
    let _ = session.finish();
}

/// A probe that predates typed control says which methods it implements when
/// it connects; every new tool is refused as unsupported before anything is
/// sent or journaled, and what it does implement still answers.
#[test]
fn an_older_probe_is_told_unsupported_before_anything_is_sent() {
    let tree = ProjectTree::new("runtime-control-old");
    tree.write("fake-game.conf", "old-probe\n");
    let mut session = server(&tree);
    let id = start(&mut session, 2, "old-start");
    let calls = [
        (
            "runtime.properties.batch.get",
            json!({ "targets": [{ "node": "Player" }] }),
            "runtime.properties.batch",
        ),
        (
            "runtime.wait",
            json!({ "condition": { "node_exists": { "node": "A" } } }),
            "runtime.wait",
        ),
        (
            "runtime.sample",
            json!({ "series": [{ "monitor": "TIME_FPS" }] }),
            "runtime.sample",
        ),
        ("runtime.nodes.find", json!({}), "runtime.nodes.find"),
        (
            "runtime.navigation.path.get",
            json!({ "node": "Player", "to": { "x": 1, "y": 1 } }),
            "runtime.navigation.path",
        ),
        (
            "runtime.properties.set",
            json!({ "idempotencyId": "old-set", "target": { "node": "Player", "properties": { "visible": { "type": "bool", "value": false } } } }),
            "runtime.properties.set",
        ),
        (
            "runtime.node.move",
            json!({ "idempotencyId": "old-move", "node": "Player", "to": { "x": 1, "y": 1 }, "mode": { "teleport": {} } }),
            "runtime.node.move",
        ),
        (
            "runtime.ui.press",
            json!({ "idempotencyId": "old-press", "target": { "node": "HUD/Start" } }),
            "runtime.ui.press",
        ),
        (
            "input.recording.start",
            json!({ "idempotencyId": "old-record" }),
            "runtime.input.record.start",
        ),
        (
            "input.recording.stop",
            json!({ "idempotencyId": "old-record-stop" }),
            "runtime.input.record.stop",
        ),
        (
            "input.replay",
            json!({ "idempotencyId": "old-replay", "events": [{ "t_ms": 0, "kind": "action", "action": "jump" }] }),
            "runtime.input.replay",
        ),
        (
            "runtime.frames.step",
            json!({ "idempotencyId": "old-step", "frames": 1 }),
            "runtime.frames.step",
        ),
    ];
    for (index, (tool, mut arguments, method)) in calls.into_iter().enumerate() {
        arguments["session"] = json!(id);
        let refused = session.call_tool(3 + index as u64, tool, arguments);
        assert_eq!(
            error_code(&refused),
            json!("unsupported"),
            "{tool}: {refused}"
        );
        assert!(
            refused
                .to_string()
                .contains(&format!("does not implement {method}")),
            "{tool} names the missing method: {refused}"
        );
        assert!(
            !refused.to_string().contains("transaction_id"),
            "{tool} journaled nothing: {refused}"
        );
    }
    let tree_read =
        structured(&session.call_tool(20, "runtime.tree.get", json!({ "session": id })));
    assert_eq!(tree_read["session"], json!(id));
    let stopped = structured(&session.call_tool(
        21,
        "project.run.stop",
        json!({ "session": id, "idempotencyId": "old-stop" }),
    ));
    assert_eq!(stopped["state"], json!("exited"), "{stopped}");
    let _ = session.finish();
}

/// A probe that predates frame stepping still samples and replays by time,
/// and is told `unsupported` before anything is sent or journaled when it is
/// asked to step, or to sample or replay by physics frame, which it would do
/// by time instead.
#[test]
fn a_probe_without_frame_stepping_keeps_time_cadences_only() {
    let tree = ProjectTree::new("runtime-control-0-4");
    tree.write("fake-game.conf", "probe-0.4\n");
    let mut session = server(&tree);
    let id = start(&mut session, 2, "probe-0-4-start");
    let refusals = [
        (
            3,
            "runtime.frames.step",
            json!({ "session": id, "idempotencyId": "probe-0-4-step", "frames": 2 }),
            "does not implement runtime.frames.step",
        ),
        (
            4,
            "runtime.sample",
            json!({ "session": id, "series": [{ "monitor": "TIME_FPS" }], "cadence": "physics_frame" }),
            "predates physics-frame cadences",
        ),
        (
            5,
            "input.replay",
            json!({ "session": id, "idempotencyId": "probe-0-4-replay", "schedule": "physics_frame", "events": [{ "frame": 0, "kind": "action", "action": "jump" }] }),
            "predates physics-frame cadences",
        ),
    ];
    for (request, tool, arguments, reason) in refusals {
        let refused = session.call_tool(request, tool, arguments);
        assert_eq!(
            error_code(&refused),
            json!("unsupported"),
            "{tool}: {refused}"
        );
        assert!(refused.to_string().contains(reason), "{tool}: {refused}");
        assert!(
            !refused.to_string().contains("transaction_id"),
            "{tool} journaled nothing: {refused}"
        );
    }
    let sampled = structured(&session.call_tool(
        6,
        "runtime.sample",
        json!({ "session": id, "series": [{ "monitor": "TIME_FPS" }], "cadence": "interval", "durationMs": 200 }),
    ));
    assert_eq!(sampled["stopped_by"], json!("duration"), "{sampled}");
    let replayed = structured(&session.call_tool(
        7,
        "input.replay",
        json!({ "session": id, "idempotencyId": "probe-0-4-replay-time", "events": [{ "t_ms": 0, "frame": 3, "kind": "action", "action": "jump" }] }),
    ));
    assert_eq!(replayed["outcome"], json!("completed"), "{replayed}");
    let stopped = structured(&session.call_tool(
        8,
        "project.run.stop",
        json!({ "session": id, "idempotencyId": "probe-0-4-stop" }),
    ));
    assert_eq!(stopped["state"], json!("exited"), "{stopped}");
    let _ = session.finish();
}

fn send_wait(session: &mut StdioSession, request: u64, id: &str) {
    session.send(&json!({
        "jsonrpc": "2.0",
        "id": request,
        "method": "tools/call",
        "params": {
            "name": "runtime.wait",
            "arguments": { "session": id, "condition": { "node_exists": { "node": "Spawned" } }, "timeoutMs": 30000 }
        }
    }));
}

/// A wait the client cancels is withdrawn from the probe, which answers at
/// once; the session takes the next request.
#[test]
fn a_cancelled_wait_is_withdrawn_from_the_probe() {
    let tree = ProjectTree::new("runtime-control-cancel");
    let mut session = server(&tree);
    let id = start(&mut session, 2, "cancel-start");
    send_wait(&mut session, 3, &id);
    marker(&tree, "fake-game.wait", "received runtime.wait");
    session.notify(
        "notifications/cancelled",
        json!({ "requestId": 3, "reason": "the client gave up" }),
    );
    marker(&tree, "fake-game.wait", "withdrawn runtime.wait");
    let tree_read = structured(&session.call_tool(4, "runtime.tree.get", json!({ "session": id })));
    assert_eq!(tree_read["session"], json!(id));
    let stopped = structured(&session.call_tool(
        5,
        "project.run.stop",
        json!({ "session": id, "idempotencyId": "cancel-stop" }),
    ));
    assert_eq!(
        stopped["exit"]["termination"],
        json!("exited_clean"),
        "{stopped}"
    );
    let _ = session.finish();
}

/// Stopping a session withdraws the wait its probe is running, so the stop
/// is not queued behind it: the game ends cleanly at once, not when the wait
/// would have ended or when the stop gives up and terminates it.
#[test]
fn stopping_a_session_withdraws_the_wait_it_runs() {
    let tree = ProjectTree::new("runtime-control-stop");
    let mut session = server(&tree);
    let id = start(&mut session, 2, "stop-start");
    send_wait(&mut session, 3, &id);
    marker(&tree, "fake-game.wait", "received runtime.wait");
    let began = Instant::now();
    let stopped = structured(&session.call_tool(
        4,
        "project.run.stop",
        json!({ "session": id, "idempotencyId": "stop-stop" }),
    ));
    let took = began.elapsed();
    assert_eq!(
        stopped["exit"]["termination"],
        json!("exited_clean"),
        "{stopped}"
    );
    assert!(
        took < Duration::from_secs(5),
        "the stop waited {took:?}: it was queued behind the wait"
    );
    marker(&tree, "fake-game.wait", "withdrawn runtime.wait");
    // The withdrawn wait answered before the stop did, so the driver read its
    // answer on the way to the stop's.
    let answered = session
        .transcript
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|frame| frame["id"] == json!(3));
    let waited = structured(&answered.unwrap_or_else(|| session.await_response(3, "tools/call")));
    assert_eq!(waited["outcome"], json!("cancelled"), "{waited}");
    assert_eq!(waited["satisfied"], json!(false));
    let _ = session.finish();
}

/// A replay is a journaled command, so it holds the project's serialized
/// command lane while it runs. Stopping its session withdraws it before the
/// stop waits for that lane: the stop is not queued behind the replay, and
/// the replay answers what it released.
#[test]
fn stopping_a_session_withdraws_the_replay_it_runs() {
    let tree = ProjectTree::new("runtime-control-stop-replay");
    tree.write("fake-game.conf", "long-replay\n");
    let mut session = server(&tree);
    let id = start(&mut session, 2, "replay-start");
    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "input.replay",
            "arguments": {
                "session": id,
                "idempotencyId": "replay-long",
                "events": [
                    { "t_ms": 0, "kind": "key", "key": "D", "pressed": true },
                    { "t_ms": 20000, "kind": "key", "key": "D", "pressed": false }
                ]
            }
        }
    }));
    marker(&tree, "fake-game.replay", "received runtime.input.replay");
    let began = Instant::now();
    let stopped = structured(&session.call_tool(
        4,
        "project.run.stop",
        json!({ "session": id, "idempotencyId": "replay-stop" }),
    ));
    let took = began.elapsed();
    assert_eq!(
        stopped["exit"]["termination"],
        json!("exited_clean"),
        "{stopped}"
    );
    assert!(
        took < Duration::from_secs(5),
        "the stop waited {took:?}: it was queued behind the replay"
    );
    marker(&tree, "fake-game.replay", "withdrawn runtime.input.replay");
    let answered = session
        .transcript
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|frame| frame["id"] == json!(3));
    let replayed = structured(&answered.unwrap_or_else(|| session.await_response(3, "tools/call")));
    assert_eq!(replayed["outcome"], json!("cancelled"), "{replayed}");
    assert_eq!(replayed["released"], json!(["key:68"]), "{replayed}");
    let _ = session.finish();
}

/// A probe that loses its connection mid-request ends the request at once:
/// a wait is unavailable, and a replay whose outcome cannot be known is
/// answered as uncertain, never as done.
#[test]
fn a_lost_probe_ends_a_wait_and_leaves_a_replay_undecided() {
    let tree = ProjectTree::new("runtime-control-lost");
    tree.write("fake-game.conf", "lossy\n");
    let mut session = server(&tree);
    let id = start(&mut session, 2, "lost-start");
    let began = Instant::now();
    let lost = session.call_tool(
        3,
        "runtime.wait",
        json!({ "session": id, "condition": { "node_exists": { "node": "Spawned" } }, "timeoutMs": 30000 }),
    );
    assert_eq!(error_code(&lost), json!("unavailable"), "{lost}");
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "the lost wait ended when the connection did"
    );

    let again = start(&mut session, 4, "lost-start-again");
    let replay = session.call_tool(
        5,
        "input.replay",
        json!({ "session": again, "idempotencyId": "lost-replay", "events": [{ "t_ms": 0, "kind": "action", "action": "jump" }] }),
    );
    assert_eq!(error_code(&replay), json!("uncertain"), "{replay}");
    let _ = session.finish();
}

/// A step whose probe loses its connection before it answers is uncertain:
/// how many frames the game advanced, and whether it is held, is unknown.
#[test]
fn a_lost_probe_leaves_a_step_undecided() {
    let tree = ProjectTree::new("runtime-control-lost-step");
    tree.write("fake-game.conf", "lossy\n");
    let mut session = server(&tree);
    let id = start(&mut session, 2, "lost-step-start");
    let step = session.call_tool(
        3,
        "runtime.frames.step",
        json!({ "session": id, "idempotencyId": "lost-step", "frames": 30 }),
    );
    assert_eq!(error_code(&step), json!("uncertain"), "{step}");
    let _ = session.finish();
}
