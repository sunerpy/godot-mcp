//! Live acceptance of game speed, image comparison and test scenarios: the
//! shipped runtime probe in real Godot builds, driven through the production
//! MCP server against the fixture game in `fixtures/runtime_scenario`.
//!
//! `runtime_scenario.rs` pins what the server owns with the `qa-fake-game`
//! stand-in; this suite checks what the probe does in the exact Godot builds
//! the lock names: a time scale the game's own clock follows, assertions the
//! engine decides (vectors within a distance, values that are not finite), a
//! Control's text, presses, stress windows whose instances are freed, reports
//! kept as artifacts, and comparisons of the rendered game with reference
//! images.
//!
//! It needs real Godot builds, so every case is ignored on a plain
//! `cargo test`:
//!
//! * `GODOT_MCP_TEST_EDITORS` lists Godot binaries, comma-separated, or
//! * `GODOT_MCP_LOCKED_ARTIFACT` names one verified editor archive, which is
//!   unpacked with `unzip`.
//!
//! A run that names neither fails rather than passing without having run.
//! Image comparisons need a window, so that case runs only with
//! `GODOT_MCP_DISPLAY_REQUIRED=1`, under a display (`xvfb-run`); the headless
//! cases check that a session without one answers unavailable. Measurements
//! are printed to stderr: `--nocapture` shows them.

#![cfg(target_os = "linux")]

#[path = "support/live.rs"]
mod live;
mod support;

use std::path::Path;
use std::time::Instant;

use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, error_code, sha256_hex};

const FIXTURE: [(&str, &str); 4] = [
    (
        "project.godot",
        include_str!("fixtures/runtime_scenario/project.godot"),
    ),
    (
        "main.tscn",
        include_str!("fixtures/runtime_scenario/main.tscn"),
    ),
    ("main.gd", include_str!("fixtures/runtime_scenario/main.gd")),
    (
        "bullet.tscn",
        include_str!("fixtures/runtime_scenario/bullet.tscn"),
    ),
];

/// Where the fixture draws `Swatch`, in viewport pixels.
const SWATCH: (u64, u64, u64, u64) = (80, 60, 100, 50);

fn tagged(kind: &str, value: Value) -> Value {
    json!({ "type": kind, "value": value })
}

fn number(value: &Value) -> f64 {
    value
        .as_f64()
        .unwrap_or_else(|| panic!("{value} is not a number"))
}

fn color(red: f64, green: f64, blue: f64) -> Value {
    tagged(
        "color",
        json!({ "red": red, "green": green, "blue": blue, "alpha": 1.0 }),
    )
}

fn statuses(report: &Value) -> Vec<String> {
    report["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .map(|step| step["status"].as_str().expect("a status").to_owned())
        .collect()
}

fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for part in parts {
        for byte in *part {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
    }
    !crc
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&u32::try_from(data.len()).expect("a chunk").to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[kind, data]).to_be_bytes());
}

/// A `width` x `height` PNG of one opaque RGBA colour, stored uncompressed in
/// as many deflate blocks as it takes.
fn png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
    // Each row is its filter byte, none, and its pixels.
    let row: Vec<u8> = std::iter::once(0)
        .chain(rgba.iter().copied().cycle().take(width as usize * 4))
        .collect();
    let raw = row.repeat(height as usize);
    let (mut a, mut b) = (1_u32, 0_u32);
    for byte in &raw {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    let mut zlib = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (index, block) in blocks.iter().enumerate() {
        let length = u16::try_from(block.len()).expect("a stored block");
        zlib.push(u8::from(index + 1 == blocks.len()));
        zlib.extend_from_slice(&length.to_le_bytes());
        zlib.extend_from_slice(&(!length).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// The width and height a PNG's header declares.
fn png_size(bytes: &[u8]) -> (u64, u64) {
    assert!(
        bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.len() > 24,
        "a PNG"
    );
    let read = |at: usize| {
        u64::from(u32::from_be_bytes([
            bytes[at],
            bytes[at + 1],
            bytes[at + 2],
            bytes[at + 3],
        ]))
    };
    (read(16), read(20))
}

/// One server over a fresh copy of the fixture, with one running game.
struct Game {
    session: StdioSession,
    tree: ProjectTree,
    id: String,
    next: u64,
    binary: String,
    display: String,
}

impl Game {
    fn start(binary: &Path, label: &str, display: &str) -> Self {
        let tree = ProjectTree::new(label);
        for (name, text) in FIXTURE {
            tree.write(name, text);
        }
        let session = StdioSession::initialized(&Spawn {
            project: &tree.root,
            current_dir: None,
            profile: "full",
            godot: Some(binary),
        });
        let mut game = Self {
            session,
            tree,
            id: String::new(),
            next: 10,
            binary: binary.display().to_string(),
            display: display.to_owned(),
        };
        game.launch();
        game
    }

    /// Start the fixture game, and wait until its scene built the world.
    fn launch(&mut self) {
        self.next += 1;
        let started = self.session.call_tool(
            self.next,
            "project.run.start",
            json!({ "idempotencyId": format!("live-start-{}", self.next), "display": self.display }),
        );
        assert!(
            started.get("error").is_none() && started["result"]["isError"] != json!(true),
            "{}: the game did not start: {started}",
            self.binary
        );
        self.id = started["result"]["structuredContent"]["session"]["session"]
            .as_str()
            .expect("session id")
            .to_owned();
        self.read(
            "runtime.wait",
            json!({ "condition": { "node_exists": { "node": "HUD/Start" } }, "timeoutMs": 10000 }),
        );
    }

    fn arguments(&mut self, tool: &str, mut arguments: Value) -> Value {
        arguments["session"] = json!(self.id);
        let mutation = godot_mcp_domain::catalog::descriptor(tool)
            .is_some_and(godot_mcp_domain::catalog::ToolDescriptor::is_mutation);
        if mutation && arguments.get("idempotencyId").is_none() {
            arguments["idempotencyId"] = json!(format!("live-{}", self.next));
        }
        arguments
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.next += 1;
        let arguments = self.arguments(tool, arguments);
        self.session.call_tool(self.next, tool, arguments)
    }

    /// A call that must succeed, with its structured answer.
    fn read(&mut self, tool: &str, arguments: Value) -> Value {
        let response = self.call(tool, arguments.clone());
        if response.get("error").is_some() || response["result"]["isError"] == json!(true) {
            let stderr = self.stderr();
            panic!(
                "{}: {tool} {arguments} must succeed: {response}\n{stderr}",
                self.binary
            );
        }
        response["result"]["structuredContent"].clone()
    }

    /// A call that must be refused, with its error code and the response.
    fn refused(&mut self, tool: &str, arguments: Value) -> (Value, String) {
        let response = self.call(tool, arguments.clone());
        let refused =
            response.get("error").is_some() || response["result"]["isError"] == json!(true);
        assert!(
            refused,
            "{}: {tool} {arguments} must be refused: {response}",
            self.binary
        );
        (error_code(&response), response.to_string())
    }

    fn scenario(&mut self, arguments: Value) -> Value {
        self.read("test.scenario.run", arguments)
    }

    /// The bytes of the artifact at `uri`.
    fn artifact(&mut self, uri: &Value) -> Vec<u8> {
        let uri = uri
            .as_str()
            .unwrap_or_else(|| panic!("{}: no artifact uri: {uri}", self.binary));
        self.next += 1;
        let read = self
            .session
            .request(self.next, "resources/read", json!({ "uri": uri }));
        live::decode_base64(
            read["result"]["contents"][0]["blob"]
                .as_str()
                .unwrap_or_else(|| panic!("{}: {uri} does not read back: {read}", self.binary)),
        )
    }

    /// Check an artifact the answer names reads back with the digest and
    /// length it states, and answer its bytes.
    fn kept(&mut self, artifact: &Value) -> Vec<u8> {
        let bytes = self.artifact(&artifact["uri"]);
        assert_eq!(
            Some(sha256_hex(&bytes).as_str()),
            artifact["sha256"].as_str(),
            "{}: {artifact}",
            self.binary
        );
        assert_eq!(
            artifact["byte_length"],
            json!(bytes.len()),
            "{}: {artifact}",
            self.binary
        );
        bytes
    }

    fn property(&mut self, node: &str, name: &str) -> Value {
        let read = self.read(
            "runtime.properties.get",
            json!({ "node": node, "properties": [name] }),
        );
        read["properties"][name]["value"].clone()
    }

    fn engine_time_scale(&mut self) -> f64 {
        let batch = self.read(
            "runtime.properties.batch.get",
            json!({ "targets": [{ "singleton": "Engine", "properties": ["time_scale"] }] }),
        );
        number(&batch["targets"][0]["properties"]["time_scale"]["value"])
    }

    /// The fixture's game clock and the engine's tick clock, read in one
    /// frame.
    fn clocks(&mut self) -> (f64, u64) {
        let read = self.read(
            "runtime.properties.get",
            json!({ "node": ".", "properties": ["game_time", "real_ms"] }),
        );
        (
            number(&read["properties"]["game_time"]["value"]),
            read["properties"]["real_ms"]["value"]
                .as_u64()
                .expect("a tick count"),
        )
    }

    /// The pace of game time over `game_seconds` of it: game seconds per
    /// second of the engine's tick clock, with the two spans measured.
    fn pace(&mut self, game_seconds: f64) -> (f64, f64, u64) {
        let (game, real) = self.clocks();
        let waited = self.read(
            "runtime.wait",
            json!({ "condition": { "property": {
                "node": ".", "property": "game_time", "op": ">=",
                "value": tagged("float", json!(game + game_seconds))
            } }, "timeoutMs": 20000 }),
        );
        assert_eq!(
            waited["satisfied"],
            json!(true),
            "{}: game time did not advance: {waited}",
            self.binary
        );
        let (later_game, later_real) = self.clocks();
        let spent = later_real - real;
        (
            (later_game - game) * 1000.0 / spent as f64,
            later_game - game,
            spent,
        )
    }

    fn stderr(&mut self) -> String {
        self.next += 1;
        let logs = self.session.call_tool(
            self.next,
            "project.run.logs",
            json!({ "session": self.id, "stream": "stderr", "limit": 65536 }),
        );
        logs["result"]["structuredContent"]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    /// Stop the game, after checking it printed no error: no probe request
    /// may raise one.
    fn stop(&mut self) {
        let stderr = self.stderr();
        let errors: Vec<&str> = stderr
            .lines()
            .filter(|line| line.contains("ERROR"))
            .collect();
        assert!(
            errors.is_empty(),
            "{}: the game printed errors: {errors:?}\n{stderr}",
            self.binary
        );
        let stopped = self.read("project.run.stop", json!({}));
        assert_eq!(
            stopped["exit"]["termination"],
            json!("exited_clean"),
            "{}: {stopped}",
            self.binary
        );
    }

    fn finish(self) {
        let _ = self.session.finish();
    }
}

/// The time scale sets the pace of the game's own clock, holds until it is
/// changed, is bounded, and is not written anywhere: the project is
/// unchanged and the next session runs at real time.
#[test]
#[ignore = "needs real Godot builds: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn live_the_time_scale_sets_the_pace_of_game_time() {
    let (binaries, _unpacked) = live::editors();
    for binary in &binaries {
        let mut game = Game::start(binary, "live-scale", "headless");
        let settings = std::fs::read(game.tree.root.join("project.godot")).expect("settings");
        let (normal, _, _) = game.pace(0.3);
        let mut before = 1.0;
        for scale in [0.25, 3.0] {
            let set = game.read("runtime.time_scale.set", json!({ "timeScale": scale }));
            assert_eq!(set["before"], json!(before), "{}: {set}", game.binary);
            assert_eq!(set["requested"], json!(scale), "{}: {set}", game.binary);
            assert_eq!(set["after"], json!(scale), "{}: {set}", game.binary);
            assert_eq!(set["matched"], json!(true), "{}: {set}", game.binary);
            assert_eq!(set["persisted"], json!(false), "{}: {set}", game.binary);
            assert!((game.engine_time_scale() - scale).abs() < 1e-9);
            let (pace, spent_game, spent_real) = game.pace(0.6);
            eprintln!(
                "{}: at time scale {scale}, {spent_game:.3} s of game time took {spent_real} ms \
                 of the tick clock: {pace:.3} game seconds per second (at 1: {normal:.3})",
                game.binary
            );
            assert!(
                pace > scale * 0.8 && pace < scale * 1.25,
                "{}: at time scale {scale} game time ran at {pace}",
                game.binary
            );
            before = scale;
        }
        // At 0 game time stands still while the engine's frames go on.
        let stopped = game.read("runtime.time_scale.set", json!({ "timeScale": 0 }));
        assert_eq!(stopped["after"], json!(0.0), "{}: {stopped}", game.binary);
        let (frozen, real) = game.clocks();
        let waited = game.read(
            "runtime.wait",
            json!({ "condition": { "property": {
                "node": ".", "property": "real_ms", "op": ">=",
                "value": tagged("integer", json!(real + 300))
            } }, "timeoutMs": 5000 }),
        );
        assert_eq!(waited["satisfied"], json!(true), "{waited}");
        let (still, _) = game.clocks();
        assert!(
            (still - frozen).abs() < f64::EPSILON,
            "{}: game time moved from {frozen} to {still} at time scale 0",
            game.binary
        );
        let back = game.read("runtime.time_scale.set", json!({ "timeScale": 1 }));
        assert_eq!(back["before"], json!(0.0), "{}: {back}", game.binary);
        assert_eq!(back["after"], json!(1.0), "{}: {back}", game.binary);
        for far in [-0.5, 100.5] {
            let (code, response) =
                game.refused("runtime.time_scale.set", json!({ "timeScale": far }));
            assert_eq!(code, json!("invalid_input"), "{far}: {response}");
        }
        game.read("runtime.time_scale.set", json!({ "timeScale": 2 }));
        // Nothing was written: the project is unchanged, and the next
        // session runs at real time.
        game.stop();
        assert_eq!(
            std::fs::read(game.tree.root.join("project.godot")).expect("settings"),
            settings,
            "{}: the project settings changed",
            game.binary
        );
        game.launch();
        assert!(
            (game.engine_time_scale() - 1.0).abs() < 1e-9,
            "{}: a new session does not start at time scale 1",
            game.binary
        );
        game.stop();
        game.finish();
    }
}

/// A scenario whose every step passes: each step kind does in the game what
/// the tool it mirrors does, and the report kept as an artifact is the report
/// answered.
#[test]
#[ignore = "needs real Godot builds: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn live_a_scenario_reports_each_step_and_keeps_its_report() {
    let (binaries, _unpacked) = live::editors();
    for binary in &binaries {
        let mut game = Game::start(binary, "live-scenario", "headless");
        let started = Instant::now();
        let report = game.scenario(json!({ "steps": [
            { "kind": "assert_ui_text", "id": "score-zero", "node": "HUD/Score", "text": "Score: 0" },
            { "kind": "press", "id": "start", "target": { "node": "HUD/Start" } },
            { "kind": "wait", "id": "counted", "condition": { "property": {
                "node": ".", "property": "score", "op": ">=", "value": tagged("integer", json!(1))
            } }, "timeoutMs": 5000 },
            { "kind": "assert_ui_text", "id": "score-one", "node": "HUD/Score", "op": "contains", "text": "1" },
            { "kind": "assert_property", "id": "near", "target": { "node": "Player", "property": "position" },
              "op": "approx", "value": tagged("vector2", json!({ "x": 10.05, "y": 19.96 })), "tolerance": 0.1 },
            { "kind": "set_property", "id": "moved", "target": { "node": "Player", "properties": {
                "position": tagged("vector2", json!({ "x": 5, "y": 6 }))
            } } },
            { "kind": "assert_property", "id": "there", "target": { "node": "Player", "property": "position" },
              "op": "equals", "value": tagged("vector2", json!({ "x": 5, "y": 6 })) },
            { "kind": "time_scale", "id": "fast", "timeScale": 2 },
            { "kind": "assert_property", "id": "fast-engine", "target": { "singleton": "Engine", "property": "time_scale" },
              "op": "equals", "value": tagged("float", json!(2.0)) },
            { "kind": "input", "id": "tap", "steps": [{ "kind": "action", "action": "ui_right" }, { "kind": "wait", "ms": 50 }] },
            { "kind": "stress", "id": "bullets", "scene": "res://bullet.tscn", "count": 50, "durationMs": 300, "intervalMs": 50 },
            { "kind": "time_scale", "id": "normal", "timeScale": 1 },
            { "kind": "assert_property", "id": "ticking", "target": { "node": ".", "property": "game_time" },
              "op": "greater", "value": tagged("float", json!(0.0)) }
        ] }));
        let took = started.elapsed();
        assert_eq!(
            statuses(&report),
            vec!["passed"; 13],
            "{}: {report}",
            game.binary
        );
        assert_eq!(
            report["verdict"],
            json!("passed"),
            "{}: {report}",
            game.binary
        );
        assert_eq!(report["stopped_by"], Value::Null);
        assert_eq!(report["counts"]["passed"], json!(13));
        assert!(
            report["godot_version"]
                .as_str()
                .is_some_and(|version| version.starts_with('4')),
            "{report}"
        );
        let steps = report["steps"].as_array().expect("steps").clone();
        assert_eq!(
            steps[1]["detail"]["pressed_signal"],
            json!(true),
            "{report}"
        );
        assert_eq!(
            steps[2]["detail"]["outcome"],
            json!("satisfied"),
            "{report}"
        );
        assert_eq!(steps[3]["actual"]["text"], json!("Score: 1"), "{report}");
        assert_eq!(
            steps[4]["actual"]["value"],
            tagged("vector2", json!({ "x": 10.0, "y": 20.0 })),
            "{report}"
        );
        assert_eq!(steps[7]["detail"]["after"], json!(2.0), "{report}");
        assert_eq!(steps[9]["detail"]["steps_run"], json!(2), "{report}");
        let stress = &steps[10]["detail"];
        assert_eq!(stress["created"], json!(50), "{report}");
        assert_eq!(stress["present_during"], json!(50), "{report}");
        assert_eq!(stress["remaining"], json!(0), "{report}");
        assert_eq!(stress["freed"], json!(50), "{report}");
        let counts = &stress["counts"];
        let nodes = |when: &str| number(&counts[when]["OBJECT_NODE_COUNT"]);
        assert!(
            (nodes("during") - nodes("before") - 50.0).abs() < f64::EPSILON,
            "{}: {counts}",
            game.binary
        );
        assert!(
            (nodes("after") - nodes("before")).abs() < f64::EPSILON,
            "{}: the instances were not freed: {counts}",
            game.binary
        );
        assert!(
            stress["monitors"]["TIME_FPS"]["count"]
                .as_u64()
                .is_some_and(|count| count > 0),
            "{report}"
        );
        eprintln!(
            "{}: 13 steps passed in {} ms (answered after {} ms); the stress window held {} \
             nodes, then {} again; {} samples, TIME_FPS mean {}",
            game.binary,
            report["elapsed_ms"],
            took.as_millis(),
            counts["during"]["OBJECT_NODE_COUNT"],
            counts["after"]["OBJECT_NODE_COUNT"],
            stress["samples"],
            stress["monitors"]["TIME_FPS"]["mean"]
        );
        // What the steps changed holds in the game.
        assert_eq!(game.property(".", "score"), json!(1), "{}", game.binary);
        assert!((game.engine_time_scale() - 1.0).abs() < 1e-9);
        // The report kept as an artifact is the report answered.
        let kept = report["artifact"].clone();
        assert_eq!(kept["media_type"], json!("application/json"), "{report}");
        let bytes = game.kept(&kept);
        let document: Value = serde_json::from_slice(&bytes).expect("the report is JSON");
        assert_eq!(document["steps"], report["steps"], "{}", game.binary);
        assert_eq!(document["verdict"], json!("passed"));
        assert_eq!(document["transaction_id"], report["transaction_id"]);
        game.stop();
        game.finish();
    }
}

/// A failed step skips the rest unless told otherwise, and what was skipped
/// did not run; a step that cannot decide stops the run; and the deadline
/// withdraws the step it overtakes.
#[test]
#[ignore = "needs real Godot builds: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn live_a_failed_step_skips_the_rest() {
    let (binaries, _unpacked) = live::editors();
    for binary in &binaries {
        let mut game = Game::start(binary, "live-scenario-fail", "headless");
        let report = game.scenario(json!({ "steps": [
            { "kind": "assert_ui_text", "id": "wrong", "node": "HUD/Score", "text": "Score: 99" },
            { "kind": "time_scale", "id": "fast", "timeScale": 2 },
            { "kind": "press", "id": "start", "target": { "node": "HUD/Start" } }
        ] }));
        assert_eq!(
            statuses(&report),
            ["failed", "skipped", "skipped"],
            "{}: {report}",
            game.binary
        );
        assert_eq!(report["verdict"], json!("failed"));
        assert_eq!(report["stopped_by"], json!("failure"));
        let wrong = &report["steps"][0];
        assert_eq!(
            wrong["expected"],
            json!({ "op": "equals", "text": "Score: 99" })
        );
        assert_eq!(wrong["actual"]["text"], json!("Score: 0"), "{report}");
        // The skipped steps did not run.
        assert!((game.engine_time_scale() - 1.0).abs() < 1e-9);
        assert_eq!(game.property(".", "score"), json!(0), "{}", game.binary);

        // Told to go on, the run goes on after a failure.
        let report = game.scenario(json!({ "stopOnFailure": false, "steps": [
            { "kind": "assert_ui_text", "id": "wrong", "node": "HUD/Score", "op": "contains", "text": "99" },
            { "kind": "press", "id": "start", "target": { "node": "HUD/Start" } }
        ] }));
        assert_eq!(
            statuses(&report),
            ["failed", "passed"],
            "{}: {report}",
            game.binary
        );
        assert_eq!(report["verdict"], json!("failed"));
        assert_eq!(report["stopped_by"], Value::Null);
        assert_eq!(game.property(".", "score"), json!(1), "{}", game.binary);

        // A wait whose condition never holds cannot decide, which stops the
        // run whatever it was told.
        let report = game.scenario(json!({ "stopOnFailure": false, "steps": [
            { "kind": "wait", "id": "never", "condition": { "node_exists": { "node": "Nowhere" } }, "timeoutMs": 300 },
            { "kind": "press", "id": "start", "target": { "node": "HUD/Start" } }
        ] }));
        assert_eq!(
            statuses(&report),
            ["uncertain", "skipped"],
            "{}: {report}",
            game.binary
        );
        assert_eq!(report["steps"][0]["detail"]["outcome"], json!("timeout"));
        assert_eq!(report["stopped_by"], json!("uncertain"));
        assert_eq!(report["verdict"], json!("uncertain"));

        // The deadline withdraws the wait it overtakes from the probe.
        let started = Instant::now();
        let report = game.scenario(json!({ "maxDurationMs": 500, "steps": [
            { "kind": "wait", "id": "long", "condition": { "node_exists": { "node": "Nowhere" } }, "timeoutMs": 10000 },
            { "kind": "press", "id": "start", "target": { "node": "HUD/Start" } }
        ] }));
        let took = started.elapsed();
        assert_eq!(
            statuses(&report),
            ["uncertain", "skipped"],
            "{}: {report}",
            game.binary
        );
        assert_eq!(report["stopped_by"], json!("deadline"), "{report}");
        assert_eq!(report["steps"][0]["detail"]["outcome"], json!("cancelled"));
        assert!(
            took.as_millis() < 5000,
            "{}: a 500 ms scenario answered after {took:?}",
            game.binary
        );
        eprintln!(
            "{}: a 500 ms scenario withdrew its 10 s wait and answered after {} ms",
            game.binary,
            took.as_millis()
        );
        assert_eq!(game.property(".", "score"), json!(1), "{}", game.binary);
        game.stop();
        game.finish();
    }
}

/// Assertions follow the engine's own arithmetic: vectors compare within a
/// distance, numbers of either kind compare as numbers, a value that is not
/// finite never compares as near anything and is named, and a comparison that
/// does not apply is an error rather than a failure. A headless session
/// answers unavailable for every image step.
#[test]
#[ignore = "needs real Godot builds: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn live_assertions_follow_the_engines_arithmetic() {
    let (binaries, _unpacked) = live::editors();
    for binary in &binaries {
        let mut game = Game::start(binary, "live-scenario-checks", "headless");
        let near = |id: &str, tolerance: f64| {
            json!({ "kind": "assert_property", "id": id,
                "target": { "node": "Player", "property": "position" }, "op": "approx",
                "value": tagged("vector2", json!({ "x": 10.05, "y": 19.96 })), "tolerance": tolerance })
        };
        let check = |id: &str, property: &str, op: &str, value: Value, tolerance: Option<f64>| {
            let mut step = json!({ "kind": "assert_property", "id": id,
                "target": { "node": ".", "property": property }, "op": op, "value": value });
            if let Some(tolerance) = tolerance {
                step["tolerance"] = json!(tolerance);
            }
            step
        };
        let report = game.scenario(json!({ "stopOnFailure": false, "steps": [
            near("near", 0.1),
            near("too-far", 0.01),
            check("whole-as-real", "score", "equals", tagged("float", json!(0.0)), None),
            check("nan-equals", "drift", "equals", tagged("float", json!(0.0)), None),
            check("nan-differs", "drift", "not_equals", tagged("float", json!(0.0)), None),
            check("nan-near", "drift", "approx", tagged("float", json!(0.0)), Some(1.0e9)),
            check("nan-vector", "heading", "approx", tagged("vector2", json!({ "x": 0, "y": 1 })), Some(1.0e9)),
            check("nan-color", "tint", "approx", color(1.0, 0.0, 0.0), Some(0.5)),
            { "kind": "assert_property", "id": "unordered", "target": { "node": "Player", "property": "position" },
              "op": "less", "value": tagged("vector2", json!({ "x": 100, "y": 100 })) },
            { "kind": "assert_property", "id": "other-type", "target": { "node": "Player", "property": "position" },
              "op": "equals", "value": tagged("float", json!(1.0)) },
            check("ticking", "game_time", "greater", tagged("float", json!(0.0)), None)
        ] }));
        assert_eq!(
            statuses(&report),
            [
                "passed", "failed", "passed", "failed", "passed", "failed", "failed", "failed",
                "error", "error", "passed"
            ],
            "{}: {report}",
            game.binary
        );
        assert_eq!(report["verdict"], json!("failed"));
        let steps = report["steps"].as_array().expect("steps").clone();
        assert_eq!(steps[1]["expected"]["tolerance"], json!(0.01), "{report}");
        assert_eq!(steps[3]["actual"]["special"], json!("nan"), "{report}");
        assert_eq!(steps[3]["actual"]["value"], Value::Null, "{report}");
        assert_eq!(
            steps[6]["actual"]["special"],
            json!("non_finite"),
            "{report}"
        );
        assert_eq!(
            steps[7]["actual"]["special"],
            json!("non_finite"),
            "{report}"
        );
        for index in [8, 9] {
            assert_eq!(
                steps[index]["error"]["kind"],
                json!("invalid_input"),
                "{report}"
            );
        }
        assert!(
            steps[8]["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("does not order")),
            "{report}"
        );

        // Nothing is rendered without a window: image steps answer
        // unavailable, and the steps around them still run.
        std::fs::create_dir_all(game.tree.root.join("ref")).expect("ref");
        std::fs::write(
            game.tree.root.join("ref/small.png"),
            png(2, 2, [255, 0, 0, 255]),
        )
        .expect("a reference");
        let (code, response) = game.refused(
            "runtime.image.compare",
            json!({ "reference": { "path": "res://ref/small.png" } }),
        );
        assert_eq!(code, json!("unavailable"), "{response}");
        let report = game.scenario(json!({ "stopOnFailure": false, "steps": [
            { "kind": "screenshot", "id": "shot" },
            { "kind": "compare_image", "id": "same", "reference": { "step": "shot" } },
            { "kind": "compare_image", "id": "small", "reference": { "path": "ref/small.png" } },
            near("near", 0.1)
        ] }));
        assert_eq!(
            statuses(&report),
            ["error", "error", "error", "passed"],
            "{}: {report}",
            game.binary
        );
        for index in 0..3 {
            assert_eq!(
                report["steps"][index]["error"]["kind"],
                json!("unavailable"),
                "{report}"
            );
        }
        assert_eq!(report["verdict"], json!("error"));
        game.stop();
        game.finish();
    }
}

/// Image comparison of the rendered game: a capture matches the screenshot
/// it was taken as; a recoloured `Swatch` differs over exactly its rectangle,
/// within the tolerance of its largest channel difference and not one under;
/// a reference of another size fails without being resized; and a scenario
/// compares with its own screenshots.
#[test]
#[ignore = "needs real Godot builds and a display: GODOT_MCP_DISPLAY_REQUIRED=1 under xvfb-run"]
fn live_images_are_compared_in_a_window() {
    if std::env::var("GODOT_MCP_DISPLAY_REQUIRED").as_deref() != Ok("1") {
        eprintln!("GODOT_MCP_DISPLAY_REQUIRED is not 1; image comparison was not checked");
        return;
    }
    assert!(
        std::env::var_os("DISPLAY").is_some(),
        "GODOT_MCP_DISPLAY_REQUIRED=1 needs a display: run under xvfb-run"
    );
    let (binaries, _unpacked) = live::editors();
    for binary in &binaries {
        let mut game = Game::start(binary, "live-compare", "window");
        std::fs::create_dir_all(game.tree.root.join("ref")).expect("ref");
        let shot = game.read("runtime.screenshot.capture", json!({}));
        let screen = game.kept(&shot["artifact"]);
        std::fs::write(game.tree.root.join("ref/screen.png"), &screen).expect("a reference");

        let same = game.read(
            "runtime.image.compare",
            json!({ "reference": { "path": "res://ref/screen.png" } }),
        );
        assert_eq!(same["matched"], json!(true), "{}: {same}", game.binary);
        assert_eq!(same["differing_pixels"], json!(0), "{same}");
        assert_eq!(same["same_size"], json!(true), "{same}");
        assert_eq!(same["bounding_box"], Value::Null, "{same}");
        assert_eq!(same["difference"], Value::Null, "{same}");
        assert_eq!(same["reference"]["path"], json!("res://ref/screen.png"));
        assert_eq!(same["reference"]["sha256"], json!(sha256_hex(&screen)));
        let width = same["capture_size"]["width"].as_u64().expect("a width");
        let height = same["capture_size"]["height"].as_u64().expect("a height");
        assert_eq!(png_size(&screen), (width, height), "{same}");
        let capture = game.kept(&same["capture"]);
        assert_eq!(png_size(&capture), (width, height));

        game.read(
            "runtime.properties.set",
            json!({ "target": { "node": "Swatch", "properties": { "color": color(0.2, 0.4, 0.92) } } }),
        );
        let changed = game.read(
            "runtime.image.compare",
            json!({ "reference": { "path": "ref/screen.png" } }),
        );
        let (x, y, swatch_width, swatch_height) = SWATCH;
        let area = swatch_width * swatch_height;
        assert_eq!(
            changed["matched"],
            json!(false),
            "{}: {changed}",
            game.binary
        );
        assert_eq!(changed["differing_pixels"], json!(area), "{changed}");
        assert_eq!(changed["total_pixels"], json!(width * height), "{changed}");
        let ratio = number(&changed["ratio"]);
        assert!(
            (ratio - area as f64 / (width * height) as f64).abs() < 1e-9,
            "{changed}"
        );
        assert_eq!(
            changed["bounding_box"],
            json!({ "x": x, "y": y, "width": swatch_width, "height": swatch_height }),
            "{changed}"
        );
        let worst = changed["max_channel_difference"]
            .as_u64()
            .expect("a channel difference");
        assert!(worst > 1 && worst < 255, "{changed}");
        let difference = game.kept(&changed["difference"]);
        assert_eq!(png_size(&difference), (width, height));
        eprintln!(
            "{}: a {width}x{height} capture; the recoloured swatch differs in {} pixels \
             (ratio {ratio:.6}) by up to {worst} in a channel, inside {}",
            game.binary, changed["differing_pixels"], changed["bounding_box"]
        );

        // A pixel differs only by more than the tolerance: at the largest
        // channel difference nothing differs, and one under it the pixels
        // that differ by that much still do, all inside the swatch.
        let tolerated = game.read(
            "runtime.image.compare",
            json!({ "reference": { "path": "ref/screen.png" }, "tolerance": worst }),
        );
        assert_eq!(tolerated["matched"], json!(true), "{tolerated}");
        assert_eq!(tolerated["differing_pixels"], json!(0), "{tolerated}");
        assert_eq!(tolerated["max_channel_difference"], json!(worst));
        let strict = game.read(
            "runtime.image.compare",
            json!({ "reference": { "path": "ref/screen.png" }, "tolerance": worst - 1, "differenceImage": false }),
        );
        assert_eq!(strict["matched"], json!(false), "{strict}");
        let counted = strict["differing_pixels"].as_u64().expect("a count");
        assert!(counted >= 1 && counted <= area, "{strict}");
        let inside = &strict["bounding_box"];
        assert!(
            inside["x"].as_u64() >= Some(x)
                && inside["y"].as_u64() >= Some(y)
                && inside["x"]
                    .as_u64()
                    .zip(inside["width"].as_u64())
                    .map(|(at, wide)| at + wide)
                    <= Some(x + swatch_width)
                && inside["y"]
                    .as_u64()
                    .zip(inside["height"].as_u64())
                    .map(|(at, high)| at + high)
                    <= Some(y + swatch_height),
            "{strict}"
        );
        assert_eq!(strict["difference"], Value::Null, "{strict}");
        eprintln!(
            "{}: at tolerance {worst} nothing differs; at {} {counted} pixels do",
            game.binary,
            worst - 1
        );

        // A reference of another size fails; nothing is resized.
        std::fs::write(
            game.tree.root.join("ref/small.png"),
            png(2, 2, [255, 0, 0, 255]),
        )
        .expect("a reference");
        let small = game.read(
            "runtime.image.compare",
            json!({ "reference": { "path": "ref/small.png" } }),
        );
        assert_eq!(small["matched"], json!(false), "{small}");
        assert_eq!(small["same_size"], json!(false), "{small}");
        assert_eq!(small["differing_pixels"], Value::Null, "{small}");
        assert_eq!(
            small["reference"]["width"],
            json!(2),
            "{}: {small}",
            game.binary
        );
        assert!(
            small["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("two sizes")),
            "{small}"
        );
        game.kept(&small["capture"]);
        // A reference larger than one upload chunk reaches the probe whole.
        let large = png(640, 480, [0, 0, 255, 255]);
        assert!(large.len() > 1024 * 1024, "{} bytes", large.len());
        std::fs::write(game.tree.root.join("ref/large.png"), &large).expect("a reference");
        let larger = game.read(
            "runtime.image.compare",
            json!({ "reference": { "path": "ref/large.png" } }),
        );
        assert_eq!(larger["same_size"], json!(false), "{larger}");
        assert_eq!(
            larger["reference"],
            json!({ "path": "res://ref/large.png", "width": 640, "height": 480,
                "byte_length": large.len(), "sha256": sha256_hex(&large) }),
            "{}: {larger}",
            game.binary
        );

        // A scenario compares with its own screenshots and with the project's.
        let report = game.scenario(json!({ "steps": [
            { "kind": "screenshot", "id": "lighter" },
            { "kind": "compare_image", "id": "steady", "reference": { "step": "lighter" } },
            { "kind": "compare_image", "id": "project", "reference": { "path": "res://ref/screen.png" }, "tolerance": worst },
            { "kind": "set_property", "id": "recolour", "target": { "node": "Swatch", "properties": { "color": color(0.2, 0.4, 0.8) } } },
            { "kind": "compare_image", "id": "moved-on", "reference": { "step": "lighter" } },
            { "kind": "assert_ui_text", "id": "after", "node": "HUD/Score", "text": "Score: 0" }
        ] }));
        assert_eq!(
            statuses(&report),
            ["passed", "passed", "passed", "passed", "failed", "skipped"],
            "{}: {report}",
            game.binary
        );
        assert_eq!(report["verdict"], json!("failed"));
        let steps = report["steps"].as_array().expect("steps").clone();
        assert_eq!(steps[0]["artifacts"][0]["role"], json!("screenshot"));
        let moved = &steps[4];
        assert_eq!(moved["actual"]["differing_pixels"], json!(area), "{report}");
        assert_eq!(moved["expected"]["step"], json!("lighter"), "{report}");
        let artifacts = moved["artifacts"].as_array().expect("artifacts").clone();
        assert_eq!(artifacts.len(), 2, "a capture and a difference: {report}");
        for artifact in &artifacts {
            game.kept(artifact);
        }
        game.kept(&report["artifact"]);
        game.stop();
        game.finish();
    }
}
