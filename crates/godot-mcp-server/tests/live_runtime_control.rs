//! Live acceptance of typed runtime control and observation: the shipped
//! runtime probe in real Godot builds, driven through the production MCP
//! server against the fixture game in `fixtures/runtime_control`.
//!
//! `runtime_control.rs` pins what the server owns with the `qa-fake-game`
//! stand-in; this suite checks that the probe a session runs does what those
//! cases assume, in the exact Godot builds the lock names: conversions and
//! read-backs, waits, samples, searches, navigation, presses, mouse input and
//! watched signals, input recording and replay, withdrawals, and the timing of
//! synthetic input.
//!
//! It needs real Godot builds, so every case is ignored on a plain
//! `cargo test`:
//!
//! * `GODOT_MCP_TEST_EDITORS` lists Godot binaries, comma-separated, or
//! * `GODOT_MCP_LOCKED_ARTIFACT` names one verified editor archive, which is
//!   unpacked with `unzip`.
//!
//! A run that names neither fails rather than passing without having run. The
//! windowed case runs only with `GODOT_MCP_DISPLAY_REQUIRED=1`, under a display
//! (`xvfb-run`). Measured timings are printed to stderr: `--nocapture` shows
//! them.

#![cfg(target_os = "linux")]

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, error_code};

const EDITORS: &str = "GODOT_MCP_TEST_EDITORS";
const ARTIFACT: &str = "GODOT_MCP_LOCKED_ARTIFACT";

const FIXTURE: [(&str, &str); 5] = [
    (
        "project.godot",
        include_str!("fixtures/runtime_control/project.godot"),
    ),
    (
        "main.tscn",
        include_str!("fixtures/runtime_control/main.tscn"),
    ),
    ("main.gd", include_str!("fixtures/runtime_control/main.gd")),
    ("hero.gd", include_str!("fixtures/runtime_control/hero.gd")),
    (
        "anchor.gd",
        include_str!("fixtures/runtime_control/anchor.gd"),
    ),
];

/// The Godot binaries this run checks, with the scratch directory an unpacked
/// archive lives in.
fn binaries() -> (Vec<PathBuf>, Option<tempfile::TempDir>) {
    if let Ok(listed) = std::env::var(EDITORS) {
        let binaries: Vec<PathBuf> = listed
            .split(',')
            .filter(|binary| !binary.is_empty())
            .map(PathBuf::from)
            .collect();
        assert!(!binaries.is_empty(), "{EDITORS} lists no Godot binary");
        return (binaries, None);
    }
    let archive = PathBuf::from(std::env::var_os(ARTIFACT).unwrap_or_else(|| {
        panic!("set {EDITORS} or {ARTIFACT} to run the live runtime-control suite")
    }));
    let name = archive
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".zip"))
        .unwrap_or_else(|| panic!("{} is not a zip archive", archive.display()))
        .to_owned();
    let work = tempfile::tempdir().expect("scratch directory for the engine");
    let status = Command::new("unzip")
        .arg("-q")
        .arg("-o")
        .arg(&archive)
        .arg("-d")
        .arg(work.path())
        .status()
        .expect("run unzip");
    assert!(
        status.success(),
        "unzip {} failed: {status}",
        archive.display()
    );
    let binary = work.path().join(&name);
    assert!(
        binary.is_file(),
        "{} does not unpack to {name}",
        archive.display()
    );
    (vec![binary], Some(work))
}

/// One server over a fresh copy of the fixture, with one running game.
struct Game {
    session: StdioSession,
    tree: ProjectTree,
    id: String,
    next: u64,
    binary: String,
}

impl Game {
    fn start(binary: &Path, label: &str, display: &str) -> Self {
        let tree = ProjectTree::new(label);
        for (name, text) in FIXTURE {
            tree.write(name, text);
        }
        let mut session = StdioSession::initialized(&Spawn {
            project: &tree.root,
            current_dir: None,
            profile: "full",
            godot: Some(binary),
        });
        let started = session.call_tool(
            2,
            "project.run.start",
            json!({ "idempotencyId": "live-start", "display": display }),
        );
        assert!(
            started.get("error").is_none() && started["result"]["isError"] != json!(true),
            "{}: the game did not start: {started}",
            binary.display()
        );
        let id = started["result"]["structuredContent"]["session"]["session"]
            .as_str()
            .expect("session id")
            .to_owned();
        let mut game = Self {
            session,
            tree,
            id,
            next: 10,
            binary: binary.display().to_string(),
        };
        // The fixture is ready once its scene built the world.
        game.read(
            "runtime.wait",
            json!({ "condition": { "node_exists": { "node": "HUD/StartButton" } }, "timeoutMs": 10000 }),
        );
        game
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

    /// Send a call without waiting for its answer; returns its request id.
    fn send(&mut self, tool: &str, arguments: Value) -> u64 {
        self.next += 1;
        let arguments = self.arguments(tool, arguments);
        self.session.send(&json!({
            "jsonrpc": "2.0",
            "id": self.next,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments }
        }));
        self.next
    }

    /// The answer to request `id`, whether it already arrived or not.
    fn answer(&mut self, id: u64) -> Value {
        let arrived = self
            .session
            .transcript
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|frame| frame["id"] == json!(id));
        arrived.unwrap_or_else(|| self.session.await_response(id, "tools/call"))
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

    /// One property of a running node, untagged.
    fn property(&mut self, node: &str, name: &str) -> Value {
        let read = self.read(
            "runtime.properties.get",
            json!({ "node": node, "properties": [name] }),
        );
        read["properties"][name]["value"].clone()
    }

    fn set(&mut self, node: &str, properties: Value) -> Value {
        self.read(
            "runtime.properties.set",
            json!({ "target": { "node": node, "properties": properties } }),
        )
    }

    /// Wait until the fixture measured a release of `move_right`, and answer
    /// how long the game saw it held.
    fn measured_hold(&mut self) -> u64 {
        let waited = self.read(
            "runtime.wait",
            json!({ "condition": { "property": {
                "node": ".", "property": "last_hold_ms", "op": ">=", "value": tagged("integer", json!(0))
            } }, "timeoutMs": 5000 }),
        );
        assert_eq!(
            waited["satisfied"],
            json!(true),
            "{}: no release reached the game: {waited}",
            self.binary
        );
        self.property(".", "last_hold_ms")
            .as_u64()
            .expect("a measured hold")
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

    /// Fail when the game printed an engine or script error: no probe request
    /// may raise one.
    fn assert_no_errors(&mut self) {
        let stderr = self.stderr();
        let errors = support::game_errors(&stderr);
        assert!(
            errors.is_empty(),
            "{}: the game printed errors: {errors:?}\n{stderr}",
            self.binary
        );
    }

    /// The lines of the fixture's `user://markers.log`, wherever the
    /// session's private home put it.
    fn markers(&self) -> Vec<String> {
        markers_under(&self.tree.base)
    }

    /// Wait, at most 10 s, until the fixture noted `line`.
    fn marker(&self, line: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.markers().iter().any(|noted| noted == line) {
            assert!(
                Instant::now() < deadline,
                "{}: the game never noted `{line}`: {:?}",
                self.binary,
                self.markers()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Stop the game, after checking it printed no error.
    fn stop(&mut self) -> Value {
        self.assert_no_errors();
        self.stop_now()
    }

    /// Stop the game, and check that it ended by itself.
    fn stop_now(&mut self) -> Value {
        let stopped = self.read("project.run.stop", json!({}));
        assert_eq!(
            stopped["exit"]["termination"],
            json!("exited_clean"),
            "{}: {stopped}",
            self.binary
        );
        stopped
    }

    fn finish(self) {
        let _ = self.session.finish();
    }
}

fn markers_under(base: &Path) -> Vec<String> {
    let mut pending = vec![base.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push(path);
            } else if path.file_name().is_some_and(|name| name == "markers.log") {
                return std::fs::read_to_string(&path)
                    .unwrap_or_default()
                    .lines()
                    .map(str::to_owned)
                    .collect();
            }
        }
    }
    Vec::new()
}

/// The hold a `released move_right after N ms` marker line reports.
fn released_after(line: &str) -> Option<u64> {
    line.strip_prefix("released move_right after ")?
        .strip_suffix(" ms")?
        .parse()
        .ok()
}

fn tagged(kind: &str, value: Value) -> Value {
    json!({ "type": kind, "value": value })
}

fn number(value: &Value) -> f64 {
    value
        .as_f64()
        .unwrap_or_else(|| panic!("{value} is not a number"))
}

fn results_by_property(answer: &Value) -> std::collections::BTreeMap<String, Value> {
    answer["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|result| {
            (
                result["property"].as_str().expect("a property").to_owned(),
                result.clone(),
            )
        })
        .collect()
}

fn names(found: &Value) -> Vec<String> {
    found["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .map(|node| node["name"].as_str().expect("a name").to_owned())
        .collect()
}

/// What a windowed game prints on a hosted CI runner, which has no sound
/// device: the ALSA driver's failure is not counted, any other error is.
#[test]
fn a_missing_sound_device_is_no_game_error() {
    let stderr = "ERROR: Condition \"status < 0\" is true. Returning: ERR_CANT_OPEN\n   \
                  at: init_output_device (drivers/alsa/audio_driver_alsa.cpp:97)\n\
                  WARNING: All audio drivers failed, falling back to the dummy driver.\n\
                  SCRIPT ERROR: Invalid call. Nonexistent function 'jump'.\n   \
                  at: _ready (res://main.gd:3)\n";
    assert_eq!(
        support::game_errors(stderr),
        ["SCRIPT ERROR: Invalid call. Nonexistent function 'jump'."]
    );
}

#[test]
#[ignore = "needs real Godot builds: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn live_properties_are_converted_written_and_read_back() {
    let (binaries, _unpacked) = binaries();
    for binary in &binaries {
        let mut game = Game::start(binary, "live-properties", "headless");
        let written = game.set(
            "Player",
            json!({
                "speed": tagged("integer", json!(200)),
                "health": tagged("float", json!(12.0)),
                "title": tagged("string_name", json!("boss")),
                "tags": tagged("array", json!([tagged("string", json!("a")), tagged("string", json!("b"))])),
                "stock": tagged("dictionary", json!({ "gold": tagged("integer", json!(3)) })),
                "cell": tagged("vector2", json!({ "x": 2.0, "y": 3.0 })),
                "clamped": tagged("integer", json!(500)),
                "ignored": tagged("integer", json!(1))
            }),
        );
        assert_eq!(written["mutation_status"], json!("committed"), "{written}");
        let results = results_by_property(&written);
        for converted in ["speed", "health", "title", "tags", "stock", "cell"] {
            assert_eq!(
                results[converted]["matched"],
                json!(true),
                "{}: {converted}: {written}",
                game.binary
            );
        }
        assert_eq!(
            results["speed"]["after"]["type"],
            json!("float"),
            "{written}"
        );
        assert_eq!(
            results["health"]["after"]["type"],
            json!("integer"),
            "{written}"
        );
        assert_eq!(
            results["title"]["after"]["type"],
            json!("string"),
            "{written}"
        );
        assert_eq!(
            results["cell"]["after"]["type"],
            json!("vector2i"),
            "{written}"
        );
        assert_eq!(
            results["clamped"]["matched"],
            json!(false),
            "a clamping setter is reported: {written}"
        );
        assert_eq!(
            results["clamped"]["after"]["value"],
            json!(100),
            "{written}"
        );
        assert_eq!(
            results["ignored"]["matched"],
            json!(false),
            "an ignoring setter is reported: {written}"
        );
        assert_eq!(results["ignored"]["after"]["value"], json!(7), "{written}");
        assert_eq!(written["all_matched"], json!(false), "{written}");
        assert_eq!(game.property("Player", "health"), json!(12));

        for (properties, code, says) in [
            (
                json!({ "health": tagged("float", json!(1.5)) }),
                "invalid_input",
                "truncating",
            ),
            (
                json!({ "nonexistent": tagged("integer", json!(1)) }),
                "not_found",
                "no property",
            ),
            (
                json!({ "multiplayer": tagged("null", Value::Null) }),
                "invalid_input",
                "read-only",
            ),
            (
                json!({ "tags": tagged("array", json!([tagged("integer", json!(1))])) }),
                "invalid_input",
                "item 0",
            ),
            (
                json!({ "health": tagged("integer", json!(1)), "nonexistent": tagged("integer", json!(1)) }),
                "not_found",
                "no property",
            ),
        ] {
            let (refused, message) = game.refused(
                "runtime.properties.set",
                json!({ "target": { "node": "Player", "properties": properties } }),
            );
            assert_eq!(refused, json!(code), "{}: {message}", game.binary);
            assert!(message.contains(says), "{}: {message}", game.binary);
        }
        // A refused write changes nothing, not even a property it named first.
        assert_eq!(game.property("Player", "health"), json!(12));

        let engine = game.read(
            "runtime.properties.set",
            json!({ "target": { "singleton": "Engine", "properties": {
                "time_scale": tagged("float", json!(0.5)),
                "max_fps": tagged("integer", json!(120))
            } } }),
        );
        assert_eq!(engine["all_matched"], json!(true), "{engine}");
        let batch = game.read(
            "runtime.properties.batch.get",
            json!({ "targets": [
                { "node": "Player", "properties": ["speed", "position", "nope"] },
                { "singleton": "Engine" },
                { "node": "Nowhere" },
                { "node": ".", "properties": ["beats"] }
            ] }),
        );
        let targets = batch["targets"].as_array().expect("targets");
        assert_eq!(targets.len(), 4, "{batch}");
        assert!(
            (number(&targets[0]["properties"]["speed"]["value"]) - 200.0).abs() < 1e-9,
            "{batch}"
        );
        assert_eq!(targets[0]["missing"], json!(["nope"]), "{batch}");
        assert!(
            (number(&targets[1]["properties"]["time_scale"]["value"]) - 0.5).abs() < 1e-9,
            "{batch}"
        );
        assert_eq!(
            targets[1]["properties"]["max_fps"]["value"],
            json!(120),
            "{batch}"
        );
        assert_eq!(
            targets[1]["properties"]["physics_ticks_per_second"]["value"],
            json!(60),
            "{batch}"
        );
        assert_eq!(targets[2]["error"]["code"], json!("not_found"), "{batch}");
        assert!(
            targets[3]["properties"]["beats"]["value"].is_number(),
            "{batch}"
        );
        game.read(
            "runtime.properties.set",
            json!({ "target": { "singleton": "Engine", "properties": {
                "time_scale": tagged("float", json!(1.0)),
                "max_fps": tagged("integer", json!(0))
            } } }),
        );
        game.stop();
        game.finish();
    }
}

#[test]
#[ignore = "needs real Godot builds: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn live_waits_hold_time_out_and_count_signals() {
    let (binaries, _unpacked) = binaries();
    for binary in &binaries {
        let mut game = Game::start(binary, "live-waits", "headless");
        let signal = game.read(
            "runtime.wait",
            json!({ "condition": { "signal": { "node": ".", "signal": "beat", "count": 2 } }, "timeoutMs": 3000 }),
        );
        assert_eq!(signal["satisfied"], json!(true), "{signal}");
        assert!(
            signal["observed"]["emissions"].as_u64() >= Some(2),
            "{signal}"
        );
        eprintln!(
            "{}: two 0.2 s beats were waited for in {} ms",
            game.binary, signal["elapsed_ms"]
        );

        game.set(".", json!({ "spawn_in_ms": tagged("integer", json!(300)) }));
        let spawned = game.read(
            "runtime.wait",
            json!({ "condition": { "node_exists": { "node": "Spawned" } }, "timeoutMs": 5000 }),
        );
        assert_eq!(spawned["satisfied"], json!(true), "{spawned}");
        assert!(spawned["elapsed_ms"].as_u64() >= Some(200), "{spawned}");
        assert_eq!(
            spawned["observed"]["node"],
            json!("/root/Main/Spawned"),
            "{spawned}"
        );

        game.set(
            ".",
            json!({ "despawn_in_ms": tagged("integer", json!(200)) }),
        );
        let gone = game.read(
            "runtime.wait",
            json!({ "condition": { "node_gone": { "node": "Doomed" } }, "timeoutMs": 5000 }),
        );
        assert_eq!(gone["satisfied"], json!(true), "{gone}");

        let began = Instant::now();
        let timed_out = game.read(
            "runtime.wait",
            json!({ "condition": { "node_exists": { "node": "Never" } }, "timeoutMs": 300 }),
        );
        let at_client = began.elapsed().as_millis();
        assert_eq!(timed_out["satisfied"], json!(false), "{timed_out}");
        assert_eq!(timed_out["outcome"], json!("timeout"), "{timed_out}");
        assert!(timed_out["elapsed_ms"].as_u64() >= Some(300), "{timed_out}");
        assert_eq!(
            timed_out["observed"]["present"],
            json!(false),
            "the last observation is answered: {timed_out}"
        );
        eprintln!(
            "{}: a 300 ms wait timed out after {} ms in the game, {at_client} ms at the client",
            game.binary, timed_out["elapsed_ms"]
        );

        let approx = game.read(
            "runtime.wait",
            json!({ "condition": { "property": {
                "node": "Player", "property": "position", "op": "approx",
                "value": tagged("vector2", json!({ "x": 351.0, "y": 50.0 })), "tolerance": 2.0
            } }, "timeoutMs": 1000 }),
        );
        assert_eq!(approx["satisfied"], json!(true), "{approx}");
        assert_eq!(
            approx["observed"]["value"]["type"],
            json!("vector2"),
            "{approx}"
        );
        let contained = game.read(
            "runtime.wait",
            json!({ "condition": { "property": {
                "node": "Player", "property": "title", "op": "contains", "value": tagged("string", json!("er"))
            } }, "timeoutMs": 1000 }),
        );
        assert_eq!(contained["satisfied"], json!(true), "{contained}");
        let counted = game.read(
            "runtime.wait",
            json!({ "condition": { "property": {
                "node": ".", "property": "beats", "op": ">=", "value": tagged("integer", json!(6))
            } }, "timeoutMs": 3000 }),
        );
        assert_eq!(counted["satisfied"], json!(true), "{counted}");
        let (refused, message) = game.refused(
            "runtime.wait",
            json!({ "condition": { "property": {
                "node": "Player", "property": "health", "op": "<", "value": tagged("string", json!("x"))
            } } }),
        );
        assert_eq!(
            refused,
            json!("invalid_input"),
            "an incomparable typed property: {message}"
        );

        game.set(
            ".",
            json!({ "announce_in_ms": tagged("integer", json!(200)) }),
        );
        let shown = game.read(
            "runtime.wait",
            json!({ "condition": { "ui_text": { "text": "Go!" } }, "timeoutMs": 3000 }),
        );
        assert_eq!(shown["satisfied"], json!(true), "{shown}");
        assert_eq!(
            shown["observed"]["control"],
            json!("/root/Main/HUD/Status"),
            "{shown}"
        );
        game.stop();
        game.finish();
    }
}

#[test]
#[ignore = "needs real Godot builds: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn live_a_withdrawn_or_stopped_request_ends_at_once() {
    let (binaries, _unpacked) = binaries();
    for binary in &binaries {
        let mut game = Game::start(binary, "live-withdraw", "headless");
        // A wait the client cancels is withdrawn from the probe: the next
        // request is not queued behind it.
        let waiting = game.send(
            "runtime.wait",
            json!({ "condition": { "node_exists": { "node": "Never" } }, "timeoutMs": 30000 }),
        );
        game.session.notify(
            "notifications/cancelled",
            json!({ "requestId": waiting, "reason": "the client gave up" }),
        );
        let began = Instant::now();
        let tree = game.read("runtime.tree.get", json!({ "depth": 1 }));
        let took = began.elapsed();
        assert!(tree["nodes"].is_array(), "{tree}");
        assert!(
            took < Duration::from_secs(5),
            "{}: the read after a cancelled wait took {took:?}",
            game.binary
        );
        eprintln!(
            "{}: the read after a cancelled 30 s wait answered in {took:?}",
            game.binary
        );

        // A replay the session's stop withdraws releases what it pressed,
        // and the stop is not queued behind it.
        let replaying = game.send(
            "input.replay",
            json!({ "events": [
                { "t_ms": 0, "kind": "key", "key": "D", "pressed": true },
                { "t_ms": 20000, "kind": "key", "key": "D", "pressed": false }
            ] }),
        );
        // The game sees the replayed press before the stop is sent. The probe
        // runs one request at a time and is busy replaying, so the game's own
        // marker file says so.
        game.marker("pressed move_right");
        game.assert_no_errors();
        let began = Instant::now();
        let stopped = game.stop_now();
        let took = began.elapsed();
        assert!(
            took < Duration::from_secs(5),
            "{}: the stop took {took:?}",
            game.binary
        );
        let replayed = game.answer(replaying);
        assert!(
            replayed.get("error").is_none() && replayed["result"]["isError"] != json!(true),
            "{}: {replayed}",
            game.binary
        );
        let replayed = &replayed["result"]["structuredContent"];
        assert_eq!(replayed["outcome"], json!("cancelled"), "{replayed}");
        assert_eq!(
            replayed["released"],
            json!(["key:68"]),
            "the withdrawn replay let go: {replayed}"
        );
        assert_eq!(
            stopped["released_input"],
            json!([]),
            "nothing was left for the stop: {stopped}"
        );
        let released = game
            .markers()
            .iter()
            .rev()
            .find_map(|line| released_after(line))
            .expect("the game saw the replayed key released");
        eprintln!(
            "{}: a stop {took:?} long withdrew a 20 s replay, whose key the game saw held {released} ms",
            game.binary
        );
        assert!(released < 20_000, "{released}");
        game.finish();
    }
}

#[test]
#[ignore = "needs real Godot builds: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn live_samples_report_series_statistics_and_pauses() {
    let (binaries, _unpacked) = binaries();
    for binary in &binaries {
        let mut game = Game::start(binary, "live-samples", "headless");
        let sampled = game.read(
            "runtime.sample",
            json!({ "series": [
                { "node": ".", "property": "beats" },
                { "node": "Player", "property": "position" },
                { "singleton": "Engine", "property": "time_scale" },
                { "monitor": "TIME_FPS" },
                { "monitor": "OBJECT_NODE_COUNT" }
            ], "durationMs": 600, "intervalMs": 100 }),
        );
        let samples = sampled["samples"].as_array().expect("samples");
        assert!(
            (5..=8).contains(&samples.len()),
            "{}: {sampled}",
            game.binary
        );
        let series = sampled["series"].as_array().expect("series");
        assert_eq!(series.len(), 5, "{sampled}");
        let beats = &series[0]["stats"];
        assert!(
            number(&beats["max"]) > number(&beats["min"]),
            "beats advance: {sampled}"
        );
        assert_eq!(
            series[1]["stats"],
            Value::Null,
            "a vector has no statistics: {sampled}"
        );
        assert_eq!(
            series[1]["values"][0]["type"],
            json!("vector2"),
            "{sampled}"
        );
        assert!(
            (number(&series[2]["stats"]["mean"]) - 1.0).abs() < 1e-9,
            "{sampled}"
        );
        assert_eq!(series[3]["kind"], json!("monitor"), "{sampled}");
        assert!(series[3]["stats"]["count"].as_u64() >= Some(5), "{sampled}");
        assert!(
            number(&series[4]["stats"]["min"]) > 10.0,
            "the fixture holds many nodes: {sampled}"
        );
        assert_eq!(
            sampled["time_base"]["focus_observable"],
            json!(false),
            "a headless display server reports focus it does not have: {sampled}"
        );
        let steps: Vec<u64> = samples
            .windows(2)
            .map(|pair| {
                pair[1]["t_ms"].as_u64().unwrap_or(0) - pair[0]["t_ms"].as_u64().unwrap_or(0)
            })
            .collect();
        eprintln!(
            "{}: 100 ms sample intervals were {steps:?} ms; {} frames over the sample",
            game.binary,
            number(&sampled["time_base"]["last_frame"])
                - number(&sampled["time_base"]["first_frame"])
        );

        game.set(".", json!({ "paused": tagged("bool", json!(true)) }));
        let paused = game.read(
            "runtime.sample",
            json!({ "series": [{ "node": ".", "property": "beats" }], "durationMs": 400, "intervalMs": 50 }),
        );
        let count = paused["samples"].as_array().map_or(0, Vec::len);
        assert!(count >= 5, "{paused}");
        assert_eq!(
            paused["paused_samples"].as_u64(),
            Some(count as u64),
            "{paused}"
        );
        assert_eq!(
            paused["series"][0]["stats"]["min"], paused["series"][0]["stats"]["max"],
            "a paused game's beats stand still: {paused}"
        );
        game.set(".", json!({ "paused": tagged("bool", json!(false)) }));
        game.stop();
        game.finish();
    }
}

#[test]
#[ignore = "needs real Godot builds: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn live_nodes_are_found_by_class_script_group_name_and_distance() {
    let (binaries, _unpacked) = binaries();
    for binary in &binaries {
        let mut game = Game::start(binary, "live-find", "headless");
        let near = game.read(
            "runtime.nodes.find",
            json!({ "group": "coins", "near": { "node": "Player", "radius": 200 } }),
        );
        assert_eq!(
            names(&near),
            ["Coin1", "Coin2"],
            "{}: nearest first: {near}",
            game.binary
        );
        assert!(
            number(&near["nodes"][0]["distance"]) < number(&near["nodes"][1]["distance"]),
            "{near}"
        );
        for (filter, expected) in [
            (json!({ "class": "CharacterBody2D" }), vec!["Player"]),
            (json!({ "class": "PhysicsBody2D" }), vec!["Player"]),
            (json!({ "class": "RuntimeHero" }), vec!["Player"]),
            (json!({ "script": "hero.gd" }), vec!["Player"]),
            (json!({ "name": "Coin*" }), vec!["Coin1", "Coin2", "Coin3"]),
            (json!({ "group": "players" }), vec!["Player"]),
            (json!({ "class": "Area2D", "root": "Elsewhere" }), vec![]),
            (
                json!({ "class": "CharacterBody3D", "near": { "point": { "x": 1, "y": 0, "z": 1 }, "radius": 1 } }),
                vec!["Walker"],
            ),
        ] {
            let found = game.read("runtime.nodes.find", filter.clone());
            assert_eq!(
                names(&found),
                expected,
                "{}: {filter}: {found}",
                game.binary
            );
        }
        let limited = game.read("runtime.nodes.find", json!({ "name": "Coin*", "limit": 1 }));
        assert_eq!(limited["matched"], json!(3), "{limited}");
        assert_eq!(limited["truncated"], json!(true), "{limited}");
        game.stop();
        game.finish();
    }
}

#[test]
#[ignore = "needs real Godot builds: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn live_paths_are_queried_and_nodes_moved() {
    let (binaries, _unpacked) = binaries();
    for binary in &binaries {
        let mut game = Game::start(binary, "live-navigation", "headless");
        let around = game.read(
            "runtime.navigation.path.get",
            json!({ "node": "Player", "to": { "x": 50, "y": 350 } }),
        );
        assert_eq!(
            around["reaches_target"],
            json!(true),
            "{}: {around}",
            game.binary
        );
        assert_eq!(
            around["map"],
            json!("agent /root/Main/Player/Agent"),
            "{around}"
        );
        // The L turns at (100, 100): the path is longer than the straight
        // line's 424.3.
        let length = number(&around["length"]);
        assert!(length > 430.0 && length < 600.0, "{around}");
        eprintln!(
            "{}: the path around the corner is {length:.1} long over {} points, at map iteration {}",
            game.binary,
            around["path"].as_array().map_or(0, Vec::len),
            around["map_iteration"]
        );
        let short = game.read(
            "runtime.navigation.path.get",
            json!({ "node": "Player", "to": { "x": 900, "y": 50 } }),
        );
        assert_eq!(short["reaches_target"], json!(false), "{short}");
        assert!(number(&short["end_distance"]) > 400.0, "{short}");
        let flat = game.read(
            "runtime.navigation.path.get",
            json!({ "node": "Arena/Walker", "to": { "x": 9, "y": 0, "z": 9 } }),
        );
        assert_eq!(flat["reaches_target"], json!(true), "{flat}");
        assert_eq!(flat["dimension"], json!(3), "{flat}");
        let (code, message) = game.refused(
            "runtime.navigation.path.get",
            json!({ "node": "Elsewhere/Stray", "to": { "x": 1, "y": 1 }, "syncTimeoutMs": 300 }),
        );
        assert_eq!(
            code,
            json!("unavailable"),
            "a world without a region: {message}"
        );
        assert!(message.contains("no navigation region"), "{message}");

        let walked = game.read(
            "runtime.node.move",
            json!({ "node": "Player", "to": { "x": 50, "y": 350 }, "mode": { "along_path": { "speed": 600, "timeoutMs": 5000 } } }),
        );
        assert_eq!(walked["outcome"], json!("arrived"), "{walked}");
        assert!(number(&walked["remaining"]) < 1.0, "{walked}");
        eprintln!(
            "{}: a {:.1}-unit walk at 600 units/s took {} physics frames and {} ms",
            game.binary,
            number(&walked["moved"]),
            walked["frames"],
            walked["elapsed_ms"]
        );
        let teleported = game.read(
            "runtime.node.move",
            json!({ "node": "Player", "to": { "x": 350, "y": 50 }, "mode": { "teleport": {} } }),
        );
        assert_eq!(teleported["outcome"], json!("teleported"), "{teleported}");
        assert_eq!(teleported["held"], json!(true), "{teleported}");
        let blocked = game.read(
            "runtime.node.move",
            json!({ "node": "Anchor", "to": { "x": 50, "y": 50 }, "mode": { "along_path": { "speed": 100 } } }),
        );
        assert_eq!(
            blocked["outcome"],
            json!("blocked"),
            "game code that holds a node: {blocked}"
        );
        let slow = game.read(
            "runtime.node.move",
            json!({ "node": "Player", "to": { "x": 50, "y": 350 }, "mode": { "along_path": { "speed": 10, "timeoutMs": 300 } } }),
        );
        assert_eq!(slow["outcome"], json!("timeout"), "{slow}");
        game.stop();
        game.finish();
    }
}

#[test]
#[ignore = "needs real Godot builds: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn live_buttons_are_pressed_through_synthetic_mouse_input() {
    let (binaries, _unpacked) = binaries();
    for binary in &binaries {
        let mut game = Game::start(binary, "live-press", "headless");
        let by_node = game.read(
            "runtime.ui.press",
            json!({ "target": { "node": "HUD/StartButton" } }),
        );
        assert_eq!(
            by_node["pressed_signal"],
            json!(true),
            "{}: {by_node}",
            game.binary
        );
        assert_eq!(by_node["emissions"]["button_down"], json!(1), "{by_node}");
        let by_text = game.read("runtime.ui.press", json!({ "target": { "text": "Start" } }));
        assert_eq!(by_text["pressed_signal"], json!(true), "{by_text}");
        let toggled = game.read(
            "runtime.ui.press",
            json!({ "target": { "node": "HUD/ToggleButton" } }),
        );
        assert_eq!(toggled["button_pressed"], json!(true), "{toggled}");
        assert_eq!(toggled["emissions"]["toggled"], json!(1), "{toggled}");
        for (target, code, says) in [
            (
                json!({ "node": "HUD/LockedButton" }),
                "invalid_input",
                "disabled",
            ),
            (
                json!({ "node": "HUD/HiddenButton" }),
                "invalid_input",
                "not visible",
            ),
            (json!({ "node": "HUD/FarButton" }), "unavailable", "64x64"),
            (
                json!({ "node": "HUD/Status" }),
                "invalid_input",
                "not a button",
            ),
            (
                json!({ "text": "Nothing like it" }),
                "not_found",
                "no visible button",
            ),
        ] {
            let (refused, message) = game.refused("runtime.ui.press", json!({ "target": target }));
            assert_eq!(refused, json!(code), "{}: {message}", game.binary);
            assert!(message.contains(says), "{message}");
        }
        assert_eq!(game.property(".", "presses"), json!(2));
        assert_eq!(game.property(".", "toggles"), json!(1));
        let shown = game.read("runtime.ui.get", json!({}));
        assert!(
            shown["controls"]
                .as_array()
                .expect("controls")
                .iter()
                .any(|control| control["text"] == json!("Started 2")),
            "{shown}"
        );
        let status = game.read("project.run.status", json!({}));
        assert_eq!(
            status["held_input"],
            json!([]),
            "a press holds nothing afterwards: {status}"
        );
        game.stop();
        game.finish();
    }
}

/// `input.mouse.send` moves the pointer and clicks with no press helper in
/// between, and `runtime.signals.get` with `watch` counts what the button
/// emitted afterwards: its own signals prove the click landed on it.
#[test]
#[ignore = "needs real Godot builds: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn live_the_mouse_is_moved_and_clicked_and_watched_signals_are_counted() {
    let (binaries, _unpacked) = binaries();
    for binary in &binaries {
        let mut game = Game::start(binary, "live-mouse", "headless");
        let unwatched = game.read("runtime.signals.get", json!({ "node": "." }));
        let beat = unwatched["signals"]
            .as_array()
            .expect("signals")
            .iter()
            .find(|signal| signal["name"] == json!("beat"))
            .cloned()
            .unwrap_or_else(|| panic!("{}: no beat signal: {unwatched}", game.binary));
        assert_eq!(beat["arguments"], json!(["count"]), "{beat}");
        assert_eq!(beat["emissions"], Value::Null, "unwatched: {beat}");

        let watched = game.read(
            "runtime.signals.get",
            json!({ "node": "HUD/StartButton", "watch": true }),
        );
        let signal = |answer: &Value, name: &str| -> Value {
            answer["signals"]
                .as_array()
                .expect("signals")
                .iter()
                .find(|signal| signal["name"] == json!(name))
                .cloned()
                .unwrap_or_else(|| panic!("no {name} signal: {answer}"))
        };
        let pressed = signal(&watched, "pressed");
        assert_eq!(pressed["emissions"], json!(0), "{}: {watched}", game.binary);
        assert!(
            pressed["connections"]
                .as_array()
                .expect("connections")
                .iter()
                .any(|connection| connection["method"] == json!("_on_start")
                    && connection["target"] == json!("/root/Main")),
            "the game's own connection is listed, the probe's is not: {pressed}"
        );

        let shown = game.read("runtime.ui.get", json!({}));
        let rect = shown["controls"]
            .as_array()
            .expect("controls")
            .iter()
            .find(|control| control["path"] == json!("/root/Main/HUD/StartButton"))
            .map(|control| control["rect"].clone())
            .unwrap_or_else(|| panic!("{}: no StartButton: {shown}", game.binary));
        let centre = json!({
            "x": number(&rect["x"]) + number(&rect["width"]) / 2.0,
            "y": number(&rect["y"]) + number(&rect["height"]) / 2.0,
        });
        game.read(
            "input.mouse.send",
            json!({ "event": { "move": { "position": centre } } }),
        );
        game.read(
            "input.mouse.send",
            json!({ "event": { "button": { "position": centre, "state": "tap", "holdMs": 100 } } }),
        );
        let landed = game.read(
            "runtime.wait",
            json!({ "condition": { "property": {
                "node": ".", "property": "presses", "op": ">=", "value": tagged("integer", json!(1))
            } }, "timeoutMs": 5000 }),
        );
        assert_eq!(
            landed["satisfied"],
            json!(true),
            "{}: the click did not press the button: {landed}",
            game.binary
        );

        let counted = game.read("runtime.signals.get", json!({ "node": "HUD/StartButton" }));
        // The move alone puts the pointer over the button; the tap presses it.
        for (name, count) in [
            ("mouse_entered", 1),
            ("pressed", 1),
            ("button_down", 1),
            ("button_up", 1),
        ] {
            assert_eq!(
                signal(&counted, name)["emissions"],
                json!(count),
                "{}: {name}: {counted}",
                game.binary
            );
        }
        eprintln!(
            "{}: a mouse tap at {centre} pressed StartButton",
            game.binary
        );
        let status = game.read("project.run.status", json!({}));
        assert_eq!(
            status["held_input"],
            json!([]),
            "a tap holds nothing once it ended: {status}"
        );
        game.stop();
        game.finish();
    }
}

#[test]
#[ignore = "needs real Godot builds and a display: GODOT_MCP_DISPLAY_REQUIRED=1 under xvfb-run"]
fn live_a_button_beyond_a_headless_viewport_is_pressed_in_a_window() {
    if std::env::var("GODOT_MCP_DISPLAY_REQUIRED").as_deref() != Ok("1") {
        eprintln!("GODOT_MCP_DISPLAY_REQUIRED is not 1; the windowed press was not checked");
        return;
    }
    assert!(
        std::env::var_os("DISPLAY").is_some(),
        "GODOT_MCP_DISPLAY_REQUIRED=1 needs a display: run under xvfb-run"
    );
    let (binaries, _unpacked) = binaries();
    for binary in &binaries {
        let mut game = Game::start(binary, "live-press-window", "window");
        let far = game.read(
            "runtime.ui.press",
            json!({ "target": { "node": "HUD/FarButton" } }),
        );
        assert_eq!(far["pressed_signal"], json!(true), "{}: {far}", game.binary);
        let sampled = game.read(
            "runtime.sample",
            json!({ "series": [{ "monitor": "TIME_FPS" }], "durationMs": 300, "intervalMs": 100 }),
        );
        assert_eq!(
            sampled["time_base"]["focus_observable"],
            json!(true),
            "{sampled}"
        );
        eprintln!(
            "{}: a windowed press at {} fired; {} of {} samples saw the window unfocused",
            game.binary,
            far["position"],
            sampled["unfocused_samples"],
            sampled["samples"].as_array().map_or(0, Vec::len)
        );
        game.stop();
        game.finish();
    }
}

#[test]
#[ignore = "needs real Godot builds: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn live_input_is_recorded_and_replayed() {
    let (binaries, _unpacked) = binaries();
    for binary in &binaries {
        let mut game = Game::start(binary, "live-record", "headless");
        let started = game.read("input.recording.start", json!({ "maxEvents": 500 }));
        assert_eq!(started["max_events"], json!(500), "{started}");
        let (twice, message) = game.refused("input.recording.start", json!({}));
        assert_eq!(twice, json!("conflict"), "{message}");
        game.read(
            "input.key.send",
            json!({ "key": "D", "state": "tap", "holdMs": 250 }),
        );
        // The game taps E itself 50 ms from now, before the D tap ends.
        game.set(
            ".",
            json!({ "external_key_in_ms": tagged("integer", json!(50)) }),
        );
        let tapped = game.measured_hold();
        let recorded = game.read("input.recording.stop", json!({}));
        let events = recorded["events"].as_array().expect("events").clone();
        let probe: Vec<&Value> = events
            .iter()
            .filter(|event| event["source"] == json!("probe"))
            .collect();
        let external: Vec<&Value> = events
            .iter()
            .filter(|event| event["source"] == json!("external"))
            .collect();
        assert!(
            probe.iter().any(|event| {
                event["kind"] == json!("key")
                    && event["key"] == json!("D")
                    && event["pressed"] == json!(true)
            }),
            "{}: {recorded}",
            game.binary
        );
        assert!(
            external
                .iter()
                .any(|event| event["kind"] == json!("key") && event["key"] == json!("E")),
            "the game's own key is external: {recorded}"
        );
        assert!(
            probe
                .iter()
                .any(|event| event["actions"] == json!(["move_right"])),
            "matched actions are named: {recorded}"
        );
        assert_eq!(
            recorded["counts"]["total"],
            json!(events.len()),
            "{recorded}"
        );
        assert_eq!(
            recorded["artifact"]["media_type"],
            json!("application/json"),
            "{recorded}"
        );
        let (none, message) = game.refused("input.recording.stop", json!({}));
        assert_eq!(none, json!("not_found"), "{message}");

        // Replay the probe's own key events, then a joypad A and an action.
        let keys: Vec<&Value> = probe
            .iter()
            .copied()
            .filter(|event| event["kind"] == json!("key"))
            .collect();
        let base = keys
            .first()
            .and_then(|event| event["t_ms"].as_u64())
            .expect("a recorded key");
        let mut replay: Vec<Value> = keys
            .iter()
            .map(|event| {
                let mut event = (*event).clone();
                event["t_ms"] = json!(event["t_ms"].as_u64().unwrap_or(0) - base);
                event
            })
            .collect();
        replay.extend([
            json!({ "t_ms": 300, "kind": "joypad_button", "device": 0, "button": 0, "pressed": true }),
            json!({ "t_ms": 350, "kind": "joypad_button", "device": 0, "button": 0, "pressed": false }),
            json!({ "t_ms": 360, "kind": "action", "action": "jump", "pressed": true }),
            json!({ "t_ms": 400, "kind": "other", "class": "InputEventMagnifyGesture" }),
        ]);
        game.set(
            ".",
            json!({ "jumps": tagged("integer", json!(0)), "last_hold_ms": tagged("integer", json!(-1)) }),
        );
        let replayed = game.read("input.replay", json!({ "events": replay }));
        assert_eq!(replayed["outcome"], json!("completed"), "{replayed}");
        assert_eq!(replayed["events_skipped"], json!(1), "{replayed}");
        assert_eq!(
            replayed["released"],
            json!(["action:jump"]),
            "what the replay left held is released: {replayed}"
        );
        assert_eq!(
            game.property(".", "jumps"),
            json!(2),
            "a joypad A and an action both jump"
        );
        let hold = game
            .property(".", "last_hold_ms")
            .as_u64()
            .expect("a replayed hold");
        eprintln!(
            "{}: a 250 ms key tap was held {tapped} ms; its replay was held {hold} ms; the latest event was sent {} ms late",
            game.binary, replayed["max_lag_ms"]
        );
        assert!((230..=400).contains(&hold), "{hold}");
        let status = game.read("project.run.status", json!({}));
        assert_eq!(status["held_input"], json!([]), "{status}");
        game.stop();
        game.finish();
    }
}

#[test]
#[ignore = "needs real Godot builds: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn live_synthetic_input_is_held_for_its_time_and_released_on_stop() {
    let (binaries, _unpacked) = binaries();
    for binary in &binaries {
        let mut game = Game::start(binary, "live-timing", "headless");
        let mut measured = Vec::new();
        for hold in [100_u64, 400] {
            game.set(".", json!({ "last_hold_ms": tagged("integer", json!(-1)) }));
            game.read(
                "input.key.send",
                json!({ "key": "D", "state": "press", "holdMs": hold }),
            );
            let lasted = game.measured_hold();
            assert!(
                lasted + 20 >= hold && lasted <= hold + 150,
                "{}: a {hold} ms hold lasted {lasted} ms",
                game.binary
            );
            measured.push((hold, lasted));
        }
        game.set(".", json!({ "last_hold_ms": tagged("integer", json!(-1)) }));
        let sequence = game.read(
            "input.sequence.send",
            json!({ "steps": [
                { "kind": "key", "key": "D", "state": "press", "holdMs": 30000 },
                { "kind": "wait", "ms": 300 },
                { "kind": "key", "key": "D", "state": "release" }
            ] }),
        );
        assert_eq!(sequence["steps_run"], json!(3), "{sequence}");
        let waited = game.measured_hold();
        assert!(
            (280..=450).contains(&waited),
            "{}: a 300 ms sequence wait held the key {waited} ms",
            game.binary
        );
        eprintln!(
            "{}: (asked, held) {measured:?} ms; a 300 ms sequence wait held {waited} ms",
            game.binary
        );

        // A press still held when the session stops is released by the stop,
        // and the game sees the release before it quits.
        game.read(
            "input.key.send",
            json!({ "key": "D", "state": "press", "holdMs": 30000 }),
        );
        let before = game.markers().len();
        let stopped = game.stop();
        assert_eq!(
            stopped["released_input"][0]["input"],
            json!("key:68"),
            "{stopped}"
        );
        let markers = game.markers();
        let released = markers[before..]
            .iter()
            .find_map(|line| released_after(line))
            .unwrap_or_else(|| {
                panic!(
                    "{}: the game never saw the stop's release: {markers:?}",
                    game.binary
                )
            });
        eprintln!(
            "{}: a 30 s press was released by the stop after {released} ms",
            game.binary
        );
        assert!(released < 30_000, "{released}");
        game.finish();
    }
}

#[test]
#[ignore = "needs real Godot builds: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn live_held_input_is_released_when_the_server_goes_away() {
    let (binaries, _unpacked) = binaries();
    for binary in &binaries {
        let mut game = Game::start(binary, "live-orphan", "headless");
        game.read(
            "input.key.send",
            json!({ "key": "D", "state": "press", "holdMs": 30000 }),
        );
        let pressed = game.read(
            "runtime.wait",
            json!({ "condition": { "property": {
                "node": ".", "property": "seen", "op": "contains", "value": tagged("string", json!("D"))
            } }, "timeoutMs": 5000 }),
        );
        assert_eq!(pressed["satisfied"], json!(true), "{pressed}");
        game.assert_no_errors();
        let Game {
            session, tree, id, ..
        } = game;
        let base = tree.base.clone();
        session.kill();
        let killed = Instant::now();
        let deadline = killed + Duration::from_secs(20);
        let released = loop {
            if let Some(hold) = markers_under(&base)
                .iter()
                .rev()
                .find_map(|line| released_after(line))
            {
                break hold;
            }
            assert!(
                Instant::now() < deadline,
                "{}: the probe released nothing when its server went away: {:?}",
                binary.display(),
                markers_under(&base)
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        eprintln!(
            "{}: a 30 s press was released after {released} ms, {} ms after its server was killed",
            binary.display(),
            killed.elapsed().as_millis()
        );
        assert!(
            released < 30_000,
            "released by the lost connection, not by its expiry"
        );
        // The next server ends the game the killed one left.
        let mut next = StdioSession::initialized(&Spawn {
            project: &tree.root,
            current_dir: None,
            profile: "full",
            godot: Some(binary),
        });
        let listed = next.call_tool(2, "project.run.status", json!({ "session": id }));
        assert!(listed.get("error").is_none(), "{listed}");
        assert_eq!(
            listed["result"]["structuredContent"]["state"],
            json!("reconciled"),
            "{}: {listed}",
            binary.display()
        );
        let _ = next.finish();
    }
}
