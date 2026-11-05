//! Contract 0.4 game speed, image comparison and test scenarios over the real
//! stdio assembly.
//!
//! The production server runs out of process with the non-shipping
//! `qa-fake-game` binary as its Godot executable, so every server-side path is
//! the shipped one: argument checks, the refusals made before anything is
//! journaled, the probe's method list, the reference image read through the
//! held project, journaled commands, withdrawal at a scenario's deadline, a
//! lost probe, and the report and captures kept as artifacts. What the probe
//! does in a real game is checked by `live_runtime_scenario.rs` against real
//! Godot builds.

mod support;

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, error_code, sha256_hex};

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

/// The bytes of the artifact at `uri`.
fn artifact(session: &mut StdioSession, id: u64, uri: &str) -> Vec<u8> {
    let read = session.request(id, "resources/read", json!({ "uri": uri }));
    base64_decode(
        read["result"]["contents"][0]["blob"]
            .as_str()
            .unwrap_or_else(|| panic!("{uri} reads back: {read}")),
    )
}

/// Take a screenshot and keep its bytes in the project as `relative`: the
/// stand-in's one-pixel PNG, which it also describes as a comparison's
/// reference.
fn reference_from_screenshot(
    session: &mut StdioSession,
    tree: &ProjectTree,
    id: u64,
    game: &str,
    relative: &str,
) -> Vec<u8> {
    let shot = structured(&session.call_tool(
        id,
        "runtime.screenshot.capture",
        json!({ "session": game, "idempotencyId": format!("shot-{relative}") }),
    ));
    let bytes = artifact(
        session,
        id + 1,
        shot["artifact"]["uri"].as_str().expect("a screenshot uri"),
    );
    let path = tree.root.join(relative);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("directories");
    std::fs::write(path, &bytes).expect("write the reference");
    bytes
}

/// The time scale is written with the probe's own property write and read
/// back; one the answer does not prove is uncertain, and a scale out of range
/// is refused before anything is sent.
#[test]
fn the_time_scale_is_read_back_and_bounded() {
    let tree = ProjectTree::new("scenario-scale");
    let mut session = server(&tree);
    let game = start(&mut session, 2, "scale-start");

    let refused = session.call_tool(
        3,
        "runtime.time_scale.set",
        json!({ "session": game, "idempotencyId": "scale-far", "timeScale": 101 }),
    );
    assert_eq!(error_code(&refused), json!("invalid_input"), "{refused}");

    // The stand-in answers a write of the player's position: the time scale's
    // read-back is not in it, so the outcome is not proven.
    let unproven = session.call_tool(
        4,
        "runtime.time_scale.set",
        json!({ "session": game, "idempotencyId": "scale-unproven", "timeScale": 2 }),
    );
    assert_eq!(error_code(&unproven), json!("uncertain"), "{unproven}");
    let details = &unproven["result"]["structuredContent"]["details"];
    assert_eq!(
        details["requested"],
        json!({ "type": "float", "value": 2.0 }),
        "{unproven}"
    );
    assert_eq!(details["mutation_status"]["value"], json!("committed"));
    assert!(details["transaction_id"]["value"].is_string());
    let _ = session.finish();

    let tree = ProjectTree::new("scenario-scale-set");
    tree.write("fake-game.conf", "scenario\n");
    let mut session = server(&tree);
    let game = start(&mut session, 2, "scale-start");
    let set = structured(&session.call_tool(
        3,
        "runtime.time_scale.set",
        json!({ "session": game, "idempotencyId": "scale-set", "timeScale": 2 }),
    ));
    assert_eq!(set["before"], json!(1.0), "{set}");
    assert_eq!(set["requested"], json!(2.0));
    assert_eq!(set["after"], json!(2.0));
    assert_eq!(set["matched"], json!(true));
    assert_eq!(set["persisted"], json!(false), "a live change only");
    assert!(
        set["note"]
            .as_str()
            .is_some_and(|note| note.contains("nothing is written")),
        "{set}"
    );
    assert_eq!(set["mutation_status"], json!("committed"));
    assert_eq!(set["session"], json!(game));
    let _ = session.finish();
}

/// A comparison reads its reference through the held project, uploads it and
/// keeps the capture; what it cannot read or compare is refused before
/// anything is journaled.
#[test]
fn an_image_comparison_reads_the_project_reference() {
    let tree = ProjectTree::new("scenario-compare");
    tree.write("fake-game.conf", "scenario\n");
    tree.write("art/not-a-png.png", "plain text");
    let mut session = server(&tree);
    let game = start(&mut session, 2, "compare-start");
    let reference = reference_from_screenshot(&mut session, &tree, 3, &game, "art/menu.png");

    let compared = structured(&session.call_tool(
        5,
        "runtime.image.compare",
        json!({ "session": game, "idempotencyId": "compare-menu", "reference": { "path": "res://art/menu.png" } }),
    ));
    assert_eq!(compared["matched"], json!(true), "{compared}");
    assert_eq!(compared["differing_pixels"], json!(0));
    assert_eq!(compared["reference"]["path"], json!("res://art/menu.png"));
    assert_eq!(compared["difference"], Value::Null);
    let capture = artifact(
        &mut session,
        6,
        compared["capture"]["uri"].as_str().expect("a capture uri"),
    );
    assert_eq!(
        sha256_hex(&capture),
        compared["capture"]["sha256"].as_str().expect("a digest")
    );
    assert_eq!(compared["capture"]["role"], json!("capture"));
    assert_eq!(reference.len(), capture.len());

    for (id, path, code) in [
        (7, "res://art/missing.png", "not_found"),
        (8, "res://art/not-a-png.png", "invalid_input"),
        (9, "res://.godot/imported/menu.png", "invalid_input"),
        (10, "res://art/menu.jpg", "invalid_input"),
    ] {
        let refused = session.call_tool(
            id,
            "runtime.image.compare",
            json!({ "session": game, "idempotencyId": format!("compare-{id}"), "reference": { "path": path } }),
        );
        assert_eq!(error_code(&refused), json!(code), "{path}: {refused}");
    }
    let _ = session.finish();
}

/// A session that renders nothing is told so: a comparison is refused before
/// anything is journaled, and a scenario's image steps answer unavailable.
#[test]
fn a_headless_session_answers_unavailable_for_images() {
    let tree = ProjectTree::new("scenario-headless");
    let mut session = server(&tree);
    let game = start(&mut session, 2, "headless-start");
    std::fs::write(tree.root.join("ref.png"), b"\x89PNG\r\n\x1a\n").expect("write a PNG signature");
    let refused = session.call_tool(
        3,
        "runtime.image.compare",
        json!({ "session": game, "idempotencyId": "headless-compare", "reference": { "path": "ref.png" } }),
    );
    assert_eq!(error_code(&refused), json!("unavailable"), "{refused}");

    let report = structured(&session.call_tool(
        4,
        "test.scenario.run",
        json!({
            "session": game,
            "idempotencyId": "headless-scenario",
            "stopOnFailure": false,
            "steps": [
                { "kind": "screenshot", "id": "shot" },
                { "kind": "assert_property", "target": { "node": "Player", "property": "position" }, "op": "approx", "value": { "type": "vector2", "value": { "x": 1, "y": 2 } }, "tolerance": 0.1 }
            ]
        }),
    ));
    assert_eq!(report["steps"][0]["status"], json!("error"), "{report}");
    assert_eq!(report["steps"][0]["error"]["kind"], json!("unavailable"));
    assert_eq!(report["steps"][1]["status"], json!("passed"));
    assert_eq!(report["verdict"], json!("error"));
    let _ = session.finish();
}

/// Every step kind reaches the probe through the method of the tool it
/// mirrors; the report says what each did and is kept as a JSON artifact
/// whose bytes read back with the digest the answer names.
#[test]
fn a_scenario_reports_every_step_and_keeps_the_report() {
    let tree = ProjectTree::new("scenario-run");
    tree.write("fake-game.conf", "scenario\n");
    tree.write(
        "bullet.tscn",
        "[gd_scene format=3]\n\n[node name=\"Bullet\" type=\"Node2D\"]\n",
    );
    let mut session = server(&tree);
    let game = start(&mut session, 2, "run-start");
    reference_from_screenshot(&mut session, &tree, 3, &game, "ref/menu.png");

    let report = structured(&session.call_tool(
        5,
        "test.scenario.run",
        json!({
            "session": game,
            "idempotencyId": "run-all",
            "steps": [
                { "kind": "time_scale", "id": "fast", "timeScale": 2 },
                { "kind": "input", "id": "moves", "steps": [{ "kind": "action", "action": "ui_right" }, { "kind": "wait", "ms": 50 }] },
                { "kind": "set_property", "target": { "singleton": "Engine", "properties": { "time_scale": { "type": "float", "value": 2.0 } } } },
                { "kind": "press", "target": { "text": "Start" } },
                { "kind": "assert_property", "id": "where", "target": { "node": "Player", "property": "position" }, "op": "approx", "value": { "type": "vector2", "value": { "x": 1, "y": 2 } }, "tolerance": 0.5 },
                { "kind": "assert_ui_text", "node": "HUD/Score", "op": "contains", "text": "1" },
                { "kind": "screenshot", "id": "first" },
                { "kind": "screenshot", "id": "second" },
                { "kind": "compare_image", "id": "steady", "reference": { "step": "first" } },
                { "kind": "compare_image", "id": "menu", "reference": { "path": "res://ref/menu.png" }, "tolerance": 3 },
                { "kind": "stress", "id": "bullets", "scene": "res://bullet.tscn", "count": 2, "durationMs": 200 }
            ]
        }),
    ));
    let statuses: Vec<&str> = report["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .map(|step| step["status"].as_str().expect("status"))
        .collect();
    assert_eq!(statuses, ["passed"; 11], "{report}");
    assert_eq!(report["verdict"], json!("passed"));
    assert_eq!(report["stopped_by"], Value::Null);
    assert_eq!(report["counts"]["passed"], json!(11));
    assert_eq!(report["mutation_status"], json!("committed"));
    assert_eq!(report["session"], json!(game));
    assert_eq!(report["godot_version"], json!("4.7.1.stable.fake"));
    let steps = report["steps"].as_array().expect("steps");
    assert_eq!(steps[0]["detail"]["after"], json!(2.0), "{report}");
    assert_eq!(steps[1]["kind"], json!("input"));
    assert_eq!(steps[2]["id"], json!("step-3"), "the default id");
    assert_eq!(steps[4]["expected"]["op"], json!("approx"));
    assert_eq!(steps[4]["actual"]["value"]["type"], json!("vector2"));
    assert_eq!(steps[10]["detail"]["remaining"], json!(0));
    // Two screenshots of the same bytes are two artifacts.
    let first = steps[6]["artifacts"][0]["uri"].as_str().expect("first");
    let second = steps[7]["artifacts"][0]["uri"].as_str().expect("second");
    assert_ne!(first, second);
    assert_eq!(steps[6]["artifacts"][0]["step"], json!("first"));
    assert_eq!(steps[8]["expected"]["step"], json!("first"));
    assert_eq!(steps[9]["expected"]["path"], json!("res://ref/menu.png"));
    assert_eq!(steps[9]["expected"]["tolerance"], json!(3));

    let kept = &report["artifact"];
    assert_eq!(kept["media_type"], json!("application/json"), "{report}");
    let bytes = artifact(&mut session, 6, kept["uri"].as_str().expect("a report uri"));
    assert_eq!(
        sha256_hex(&bytes),
        kept["sha256"].as_str().expect("a digest")
    );
    let document: Value = serde_json::from_slice(&bytes).expect("the report is JSON");
    assert_eq!(document["verdict"], json!("passed"));
    assert_eq!(document["steps"], report["steps"]);
    assert_eq!(document["transaction_id"], report["transaction_id"]);
    let _ = session.finish();
}

/// A failed assertion skips the rest unless told otherwise; an uncertain step
/// stops the scenario either way.
#[test]
fn a_failure_skips_and_an_uncertain_step_stops() {
    let tree = ProjectTree::new("scenario-stop");
    let mut session = server(&tree);
    let game = start(&mut session, 2, "stop-start");
    // The stand-in's write answers the player's position, so a time-scale
    // step is uncertain.
    let report = structured(&session.call_tool(
        3,
        "test.scenario.run",
        json!({
            "session": game,
            "idempotencyId": "stop-uncertain",
            "stopOnFailure": false,
            "steps": [
                { "kind": "time_scale", "timeScale": 3 },
                { "kind": "screenshot" }
            ]
        }),
    ));
    assert_eq!(report["steps"][0]["status"], json!("uncertain"), "{report}");
    assert_eq!(report["steps"][1]["status"], json!("skipped"));
    assert_eq!(report["stopped_by"], json!("uncertain"));
    assert_eq!(report["verdict"], json!("uncertain"));

    // The stand-in adds two instances whatever it is asked for, so a stress
    // step that asks for five did not run as asked: an error, which stops a
    // scenario that stops on failure.
    let report = structured(&session.call_tool(
        4,
        "test.scenario.run",
        json!({
            "session": game,
            "idempotencyId": "stop-error",
            "steps": [
                { "kind": "stress", "scene": "res://bullet.tscn", "count": 5 },
                { "kind": "screenshot" }
            ]
        }),
    ));
    assert_eq!(report["steps"][0]["status"], json!("error"), "{report}");
    assert_eq!(report["steps"][1]["status"], json!("skipped"));
    assert_eq!(report["stopped_by"], json!("failure"));
    assert_eq!(report["verdict"], json!("error"));
    let _ = session.finish();
}

/// The deadline withdraws the step that runs when it passes, from the probe
/// too, and skips the rest.
#[test]
fn the_deadline_withdraws_the_running_step() {
    let tree = ProjectTree::new("scenario-deadline");
    let mut session = server(&tree);
    let game = start(&mut session, 2, "deadline-start");
    let report = structured(&session.call_tool(
        3,
        "test.scenario.run",
        json!({
            "session": game,
            "idempotencyId": "deadline",
            "maxDurationMs": 500,
            "steps": [
                { "kind": "wait", "condition": { "node_exists": { "node": "Spawned" } }, "timeoutMs": 30000 },
                { "kind": "screenshot" }
            ]
        }),
    ));
    // The scenario's own clock, not the call's: what the server does around
    // the run, such as keeping the report, is no part of its deadline, and a
    // loaded Windows runner once spent over 8 s on the call as a whole.
    assert!(
        report["elapsed_ms"]
            .as_u64()
            .is_some_and(|elapsed| elapsed < 8_000),
        "the stand-in's wait runs 10 s unless withdrawn: {report}"
    );
    marker(&tree, "fake-game.wait", "withdrawn runtime.wait");
    assert_eq!(report["steps"][0]["status"], json!("uncertain"), "{report}");
    assert_eq!(report["steps"][0]["detail"]["outcome"], json!("cancelled"));
    assert_eq!(report["steps"][1]["status"], json!("skipped"));
    assert_eq!(report["stopped_by"], json!("deadline"));
    assert_eq!(report["verdict"], json!("uncertain"));
    let _ = session.finish();
}

/// A probe that loses its connection mid-step leaves that step uncertain.
#[test]
fn a_lost_probe_leaves_its_step_uncertain() {
    let tree = ProjectTree::new("scenario-lossy");
    tree.write("fake-game.conf", "lossy\n");
    let mut session = server(&tree);
    let game = start(&mut session, 2, "lossy-start");
    let report = structured(&session.call_tool(
        3,
        "test.scenario.run",
        json!({
            "session": game,
            "idempotencyId": "lossy",
            "steps": [
                { "kind": "wait", "condition": { "node_exists": { "node": "Spawned" } } },
                { "kind": "screenshot" }
            ]
        }),
    ));
    assert_eq!(report["steps"][0]["status"], json!("uncertain"), "{report}");
    assert!(
        report["steps"][0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("lost")),
        "{report}"
    );
    assert_eq!(report["stopped_by"], json!("uncertain"));
    let _ = session.finish();
}

/// What a scenario cannot run is refused before anything is journaled or
/// sent: steps the schema cannot judge, a missing reference image and a probe
/// that predates the scenario methods.
#[test]
fn a_scenario_is_refused_before_it_runs() {
    let tree = ProjectTree::new("scenario-refused");
    let mut session = server(&tree);
    let game = start(&mut session, 2, "refused-start");
    let cases = [
        (
            json!([{ "kind": "screenshot", "id": "same" }, { "kind": "screenshot", "id": "same" }]),
            "invalid_input",
        ),
        (
            json!([{ "kind": "compare_image", "reference": { "step": "later" } }, { "kind": "screenshot", "id": "later" }]),
            "invalid_input",
        ),
        (
            json!([{ "kind": "compare_image", "reference": { "path": "res://missing.png" } }]),
            "not_found",
        ),
        (
            json!([{ "kind": "input", "steps": [{ "kind": "wait", "ms": 6000 }, { "kind": "wait", "ms": 6000 }] }]),
            "invalid_input",
        ),
    ];
    for (index, (steps, code)) in cases.into_iter().enumerate() {
        let refused = session.call_tool(
            10 + index as u64,
            "test.scenario.run",
            json!({ "session": game, "idempotencyId": format!("refused-{index}"), "steps": steps }),
        );
        assert_eq!(error_code(&refused), json!(code), "{steps}: {refused}");
    }
    let _ = session.finish();

    let tree = ProjectTree::new("scenario-old-probe");
    tree.write("fake-game.conf", "old-probe\n");
    let mut session = server(&tree);
    let game = start(&mut session, 2, "old-start");
    let refused = session.call_tool(
        3,
        "test.scenario.run",
        json!({
            "session": game,
            "idempotencyId": "old",
            "steps": [{ "kind": "assert_ui_text", "node": "HUD/Score", "text": "1" }]
        }),
    );
    assert_eq!(error_code(&refused), json!("unsupported"), "{refused}");
    assert!(
        refused
            .to_string()
            .contains("does not implement runtime.scenario.check"),
        "the refusal names the missing method: {refused}"
    );
    let refused = session.call_tool(
        4,
        "runtime.time_scale.set",
        json!({ "session": game, "idempotencyId": "old-scale", "timeScale": 1 }),
    );
    assert_eq!(error_code(&refused), json!("unsupported"), "{refused}");
    let _ = session.finish();
}

/// A retried scenario runs nothing again: the journal answers the recorded
/// status, as it does for every command to a game session, and the report the
/// first answer named stays readable.
#[test]
fn a_retried_scenario_answers_its_recorded_status() {
    let tree = ProjectTree::new("scenario-retry");
    let mut session = server(&tree);
    let game = start(&mut session, 2, "retry-start");
    let arguments = json!({
        "session": game,
        "idempotencyId": "retry",
        "steps": [{ "kind": "assert_ui_text", "node": "HUD/Score", "op": "contains", "text": "1" }]
    });
    let first = structured(&session.call_tool(3, "test.scenario.run", arguments.clone()));
    assert_eq!(first["replayed"], json!(false), "{first}");
    assert_eq!(first["verdict"], json!("passed"), "{first}");
    let again = structured(&session.call_tool(4, "test.scenario.run", arguments));
    assert_eq!(again["replayed"], json!(true), "{again}");
    assert_eq!(again["status"], json!("committed"), "{again}");
    assert_eq!(again["transaction_id"], first["transaction_id"], "{again}");
    assert_eq!(
        again.get("steps"),
        None,
        "the report is not run or answered again: {again}"
    );
    let bytes = artifact(
        &mut session,
        5,
        first["artifact"]["uri"].as_str().expect("a report uri"),
    );
    assert_eq!(
        sha256_hex(&bytes),
        first["artifact"]["sha256"].as_str().expect("a digest")
    );
    let _ = session.finish();
}
