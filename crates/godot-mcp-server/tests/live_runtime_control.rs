//! Live acceptance of typed runtime control and observation: the shipped
//! runtime probe in real Godot builds, driven through the production MCP
//! server against the fixture game in `fixtures/runtime_control`.
//!
//! `runtime_control.rs` pins what the server owns with the `qa-fake-game`
//! stand-in; this suite checks that the probe a session runs does what those
//! cases assume, in the exact Godot builds the lock names: conversions and
//! read-backs, waits, samples, searches, navigation, presses, mouse input and
//! watched signals, input recording and replay, withdrawals, the timing of
//! synthetic input, and stepping a game by physics frames, sampling it per
//! physics frame and replaying input by physics frame against the frame
//! fixture `frames.tscn`.
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

const FIXTURE: [(&str, &str); 8] = [
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
    (
        "frames.tscn",
        include_str!("fixtures/runtime_control/frames.tscn"),
    ),
    (
        "frames.gd",
        include_str!("fixtures/runtime_control/frames.gd"),
    ),
    (
        "early.gd",
        include_str!("fixtures/runtime_control/early.gd"),
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
        Self::launch(binary, label, display, None, "HUD/StartButton")
    }

    /// A game running the frame fixture, `frames.tscn`.
    fn start_frames(binary: &Path, label: &str, display: &str) -> Self {
        Self::launch(binary, label, display, Some("res://frames.tscn"), "Faller")
    }

    fn launch(binary: &Path, label: &str, display: &str, scene: Option<&str>, ready: &str) -> Self {
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
        let mut start = json!({ "idempotencyId": "live-start", "display": display });
        if let Some(scene) = scene {
            start["scene"] = json!(scene);
        }
        let started = session.call_tool(2, "project.run.start", start);
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
            json!({ "condition": { "node_exists": { "node": ready } }, "timeoutMs": 10000 }),
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
        // The game taps E itself 50 ms after it reads this write, which a slow
        // runner may do after the D tap has ended.
        game.set(
            ".",
            json!({ "external_key_in_ms": tagged("integer", json!(50)) }),
        );
        let tapped = game.measured_hold();
        // The recording stops only once the game has received its own E: the
        // root window reports an event to the recording before the game's
        // `_input` sees it, so the E is recorded whatever the runner's speed.
        let own_key = game.read(
            "runtime.wait",
            json!({ "condition": { "property": {
                "node": ".", "property": "seen", "op": "contains", "value": tagged("string", json!("E"))
            } }, "timeoutMs": 5000 }),
        );
        assert_eq!(
            own_key["satisfied"],
            json!(true),
            "{}: the game's own E never reached it: {own_key}",
            game.binary
        );
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

// --- stepping, sampling and replaying by physics frame -----------------------------

/// What the frame fixture counted, read in one request: the scene's own
/// counters, its `Always` child's and the `FrameEarly` autoload's.
#[derive(Debug, Clone)]
struct Counts {
    physics_calls: i64,
    process_calls: i64,
    last_physics_frame: i64,
    physics_frames: Vec<i64>,
    signal_calls: i64,
    timer_fires: i64,
    scene_timer_fires: i64,
    faller_steps: i64,
    always_calls: i64,
    early_calls: i64,
    early_frames: Vec<i64>,
}

impl Counts {
    /// The physics frames the scene processed since `before` was read.
    fn frames_since(&self, before: &Self) -> Vec<i64> {
        after_last(&self.physics_frames, &before.physics_frames)
    }

    /// The physics frames `FrameEarly` processed since `before` was read.
    fn early_frames_since(&self, before: &Self) -> Vec<i64> {
        after_last(&self.early_frames, &before.early_frames)
    }
}

/// The frames of `now` later than the last one of `then`.
fn after_last(now: &[i64], then: &[i64]) -> Vec<i64> {
    let seen = then.last().copied().unwrap_or(i64::MIN);
    now.iter().copied().filter(|frame| *frame > seen).collect()
}

impl Game {
    /// What the frame fixture counted, in one read.
    fn counts(&mut self) -> Counts {
        let read = self.read(
            "runtime.properties.batch.get",
            json!({ "targets": [
                { "node": ".", "properties": [
                    "physics_calls", "process_calls", "last_physics_frame", "physics_frames",
                    "signal_calls", "timer_fires", "scene_timer_fires", "faller_steps"
                ] },
                { "node": "Always", "properties": ["physics_calls"] },
                { "node": "/root/FrameEarly", "properties": ["physics_calls", "physics_frames"] }
            ] }),
        );
        let value = |target: usize, name: &str| -> Value {
            let value = read["targets"][target]["properties"][name]["value"].clone();
            assert!(
                !value.is_null(),
                "{name} of target {target} is unread: {read}"
            );
            value
        };
        let whole = |target: usize, name: &str| -> i64 {
            let found = value(target, name);
            found
                .as_i64()
                .unwrap_or_else(|| panic!("{name} is not a whole number: {found}"))
        };
        Counts {
            physics_calls: whole(0, "physics_calls"),
            process_calls: whole(0, "process_calls"),
            last_physics_frame: whole(0, "last_physics_frame"),
            physics_frames: whole_numbers(&value(0, "physics_frames")),
            signal_calls: whole(0, "signal_calls"),
            timer_fires: whole(0, "timer_fires"),
            scene_timer_fires: whole(0, "scene_timer_fires"),
            faller_steps: whole(0, "faller_steps"),
            always_calls: whole(1, "physics_calls"),
            early_calls: whole(2, "physics_calls"),
            early_frames: whole_numbers(&value(2, "physics_frames")),
        }
    }

    /// One of the frame fixture's recorded lists of physics frames.
    fn frames(&mut self, name: &str) -> Vec<i64> {
        whole_numbers(&self.property(".", name))
    }

    /// Step the game, and check the step completed and holds or releases it
    /// as `then` says.
    fn step(&mut self, arguments: Value) -> Value {
        let stepped = self.read("runtime.frames.step", arguments.clone());
        let frames = arguments["frames"].as_i64().expect("a frame count");
        let held = arguments["then"] != json!("resume");
        assert_step(&stepped, "completed", frames, held, &self.binary);
        assert_eq!(stepped["paused"], json!(held), "{}: {stepped}", self.binary);
        stepped
    }

    /// Wait, at most 10 s, until the fixture noted `line` after its first
    /// `from` lines.
    fn marker_since(&self, from: usize, line: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.markers().iter().skip(from).any(|noted| noted == line) {
            assert!(
                Instant::now() < deadline,
                "{}: the game never noted `{line}` after line {from}: {:?}",
                self.binary,
                self.markers()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait, at most 5 s, until the scene processed more than `calls`
    /// physics frames.
    fn runs_past(&mut self, calls: i64) {
        let ran = self.read(
            "runtime.wait",
            json!({ "condition": { "property": {
                "node": ".", "property": "physics_calls", "op": ">", "value": tagged("integer", json!(calls))
            } }, "timeoutMs": 5000 }),
        );
        assert_eq!(
            ran["satisfied"],
            json!(true),
            "{}: the game did not run: {ran}",
            self.binary
        );
    }
}

/// Check a step's answer: its outcome, the frames it advanced, whether it
/// holds the game, and that its first and last frames span what it advanced.
fn assert_step(answer: &Value, outcome: &str, advanced: i64, held: bool, binary: &str) {
    assert_eq!(answer["outcome"], json!(outcome), "{binary}: {answer}");
    assert_eq!(
        answer["frames_advanced"],
        json!(advanced),
        "{binary}: {answer}"
    );
    assert_eq!(answer["held"], json!(held), "{binary}: {answer}");
    if advanced > 0 {
        let (first, last) = span(answer);
        assert_eq!(last - first + 1, advanced, "{binary}: {answer}");
    } else {
        assert_eq!(answer["first_frame"], Value::Null, "{binary}: {answer}");
        assert_eq!(answer["last_frame"], Value::Null, "{binary}: {answer}");
    }
}

/// The first and last physics frame a step advanced.
fn span(answer: &Value) -> (i64, i64) {
    (
        answer["first_frame"].as_i64().expect("a first frame"),
        answer["last_frame"].as_i64().expect("a last frame"),
    )
}

/// The whole numbers of a list the probe encoded, each a tagged integer.
fn whole_numbers(list: &Value) -> Vec<i64> {
    list.as_array()
        .unwrap_or_else(|| panic!("{list} is not a list"))
        .iter()
        .map(|item| {
            item["value"]
                .as_i64()
                .unwrap_or_else(|| panic!("{item} is not a tagged whole number"))
        })
        .collect()
}

/// The physics frame of each sample a sample took.
fn sample_frames(sampled: &Value) -> Vec<i64> {
    sampled["samples"]
        .as_array()
        .expect("samples")
        .iter()
        .map(|sample| sample["physics_frame"].as_i64().expect("a physics frame"))
        .collect()
}

/// Whether each number is one more than the one before.
fn consecutive(numbers: &[i64]) -> bool {
    numbers.windows(2).all(|pair| pair[1] == pair[0] + 1)
}

/// The deliveries a replay answered: the caller's index of each event and
/// the physics frame the game sees it in, in delivery order.
fn deliveries(replayed: &Value) -> Vec<(i64, i64)> {
    replayed["deliveries"]
        .as_array()
        .expect("deliveries")
        .iter()
        .map(|delivery| {
            (
                delivery["index"].as_i64().expect("an index"),
                delivery["physics_frame"].as_i64().expect("a physics frame"),
            )
        })
        .collect()
}

/// The jump events `_input` received, from a tagged list of
/// `[physics frame counter, in a physics frame, pressed]`.
fn input_seen(list: &Value) -> Vec<(i64, bool, bool)> {
    list.as_array()
        .unwrap_or_else(|| panic!("{list} is not a list"))
        .iter()
        .map(|item| {
            let fields = item["value"].as_array().expect("an entry");
            (
                fields[0]["value"].as_i64().expect("a counter"),
                fields[1]["value"].as_bool().expect("in a physics frame"),
                fields[2]["value"].as_bool().expect("pressed"),
            )
        })
        .collect()
}

/// The frames of `list` from `from` on.
fn from_frame(list: &[i64], from: i64) -> Vec<i64> {
    list.iter()
        .copied()
        .filter(|frame| *frame >= from)
        .collect()
}

#[test]
#[ignore = "needs real Godot builds: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn live_a_step_advances_exactly_its_frames_and_holds() {
    let (binaries, _unpacked) = binaries();
    for binary in &binaries {
        let mut game = Game::start_frames(binary, "live-step", "headless");
        game.runs_past(5);

        // `frames` 0 holds the running game at the start of its next physics
        // frame.
        game.step(json!({ "frames": 0 }));
        game.marker("tree paused");

        // Held, the scene stands still while the engine's physics frames go
        // on, sampled at the start of each of 20 of them.
        let still = game.counts();
        let held = game.read(
            "runtime.sample",
            json!({ "series": [
                { "node": ".", "property": "physics_calls" },
                { "node": ".", "property": "process_calls" },
                { "node": ".", "property": "timer_fires" },
                { "node": ".", "property": "faller_steps" },
                { "node": "/root/FrameEarly", "property": "physics_calls" },
                { "node": "Always", "property": "physics_calls" },
                { "node": ".", "property": "signal_calls" },
                { "node": ".", "property": "scene_timer_fires" }
            ], "cadence": "physics_frame", "durationMs": 10000, "maxSamples": 20 }),
        );
        assert_eq!(held["stopped_by"], json!("max_samples"), "{held}");
        assert_eq!(held["paused_samples"], json!(20), "{held}");
        assert!(consecutive(&sample_frames(&held)), "{held}");
        let column = |index: usize| whole_numbers(&held["series"][index]["values"]);
        for (index, expected) in [
            (0, still.physics_calls),
            (1, still.process_calls),
            (2, still.timer_fires),
            (3, still.faller_steps),
            (4, still.early_calls),
        ] {
            assert_eq!(
                column(index),
                vec![expected; 20],
                "{}: series {index} moved while held: {held}",
                game.binary
            );
        }
        // What processes while the tree is paused goes on: a node set to
        // process always, the SceneTree's physics_frame signal, and a
        // SceneTreeTimer, which processes always unless told otherwise.
        assert!(consecutive(&column(5)), "{}: {held}", game.binary);
        assert!(consecutive(&column(6)), "{}: {held}", game.binary);
        let scene_timer = column(7);
        assert!(
            scene_timer.last() > scene_timer.first(),
            "{}: {held}",
            game.binary
        );
        eprintln!(
            "{}: held for 20 physics frames: the scene, its Timer node, its physics body and an autoload stood still; an always-processing node and the physics_frame signal ran once a frame; a SceneTreeTimer fired {} times",
            game.binary,
            scene_timer.last().unwrap_or(&0) - scene_timer.first().unwrap_or(&0)
        );

        // From a hold, exactly the frames asked for advance, each processed
        // by the scene and stepped by the physics server once.
        for frames in [1_i64, 3, 10, 60] {
            let before = game.counts();
            let began = Instant::now();
            let stepped = game.step(json!({ "frames": frames }));
            let took = began.elapsed();
            let after = game.counts();
            let (first, last) = span(&stepped);
            assert_eq!(
                after.physics_calls - before.physics_calls,
                frames,
                "{}: a {frames}-frame step: {stepped}",
                game.binary
            );
            assert_eq!(
                after.frames_since(&before),
                (first..=last).collect::<Vec<_>>(),
                "{}: the scene processed other frames than the step's: {stepped}",
                game.binary
            );
            assert_eq!(after.last_physics_frame, last, "{stepped}");
            assert_eq!(
                after.faller_steps - before.faller_steps,
                frames,
                "{}: the physics server stepped otherwise: {stepped}",
                game.binary
            );
            // A node at the probe's own priority that comes before it in the
            // tree, an autoload, sees the frames one later: the probe
            // releases the hold after it in the first frame, and holds again
            // after it in the frame after the last.
            assert_eq!(after.early_calls - before.early_calls, frames, "{stepped}");
            assert_eq!(
                after.early_frames_since(&before),
                (first + 1..=last + 1).collect::<Vec<_>>(),
                "{}: {stepped}",
                game.binary
            );
            eprintln!(
                "{}: a {frames}-frame step from a hold advanced frames {first}..={last} in {} ms ({took:?} round trip); between the reads around it the scene's _process ran {} times, an always-processing node's _physics_process {} times, its Timer fired {} times, a SceneTreeTimer {} times, the physics_frame signal {} times",
                game.binary,
                stepped["elapsed_ms"],
                after.process_calls - before.process_calls,
                after.always_calls - before.always_calls,
                after.timer_fires - before.timer_fires,
                after.scene_timer_fires - before.scene_timer_fires,
                after.signal_calls - before.signal_calls
            );
        }

        // Other requests keep working while the game is held.
        let timed = game.read(
            "runtime.sample",
            json!({ "series": [{ "node": ".", "property": "physics_calls" }], "durationMs": 300, "intervalMs": 50 }),
        );
        let count = timed["samples"].as_array().map_or(0, Vec::len);
        assert!(count >= 4, "{timed}");
        assert_eq!(timed["paused_samples"], json!(count), "{timed}");
        assert_eq!(
            timed["series"][0]["stats"]["min"], timed["series"][0]["stats"]["max"],
            "{timed}"
        );
        let tree = game.read("runtime.tree.get", json!({ "depth": 1 }));
        assert!(tree["nodes"].is_array(), "{tree}");

        // `then` resume releases the hold, and the game runs on.
        let held_calls = game.counts().physics_calls;
        game.step(json!({ "frames": 0, "then": "resume" }));
        game.runs_past(held_calls + 5);

        // From a running game a step counts from its next physics frame, and
        // the game holds from the frame after the last.
        let stepped = game.step(json!({ "frames": 10 }));
        let after = game.counts();
        let (first, last) = span(&stepped);
        assert_eq!(after.last_physics_frame, last, "{stepped}");
        let tail = &after.physics_frames[after.physics_frames.len() - 12..];
        assert_eq!(
            tail,
            (first - 2..=last).collect::<Vec<_>>().as_slice(),
            "{}: the scene ran up to the step and through it: {stepped}",
            game.binary
        );
        let early = &after.early_frames[after.early_frames.len() - 13..];
        assert_eq!(
            early,
            (first - 2..=last + 1).collect::<Vec<_>>().as_slice(),
            "{}: {stepped}",
            game.binary
        );
        assert_eq!(game.counts().physics_calls, after.physics_calls);

        // A step that runs out of time holds the game where it stopped.
        let before = game.counts();
        let timed_out = game.read(
            "runtime.frames.step",
            json!({ "frames": 600, "timeoutMs": 200 }),
        );
        let after = game.counts();
        let advanced = timed_out["frames_advanced"].as_i64().expect("advanced");
        assert!(
            advanced > 0 && advanced < 600,
            "{}: {timed_out}",
            game.binary
        );
        assert_step(&timed_out, "timed_out", advanced, true, &game.binary);
        assert_eq!(timed_out["paused"], json!(true), "{timed_out}");
        let (first, last) = span(&timed_out);
        assert_eq!(after.physics_calls - before.physics_calls, advanced);
        assert_eq!(
            after.frames_since(&before),
            (first..=last).collect::<Vec<_>>()
        );
        assert!(timed_out["elapsed_ms"].as_u64() >= Some(200), "{timed_out}");
        eprintln!(
            "{}: a 600-frame step with a 200 ms timeout advanced {advanced} frames in {} ms and held",
            game.binary, timed_out["elapsed_ms"]
        );

        // A withdrawn step holds the game where it stopped too.
        let before = game.counts();
        let from = game.markers().len();
        let stepping = game.send("runtime.frames.step", json!({ "frames": 600 }));
        // The step released the hold in its first frame: it is advancing.
        game.marker_since(from, "tree unpaused");
        game.session.notify(
            "notifications/cancelled",
            json!({ "requestId": stepping, "reason": "the client gave up" }),
        );
        let after = game.counts();
        let withdrawn = game.read(
            "runtime.sample",
            json!({ "series": [{ "node": ".", "property": "physics_calls" }], "cadence": "physics_frame", "maxSamples": 10 }),
        );
        assert_eq!(withdrawn["paused_samples"], json!(10), "{withdrawn}");
        assert_eq!(
            whole_numbers(&withdrawn["series"][0]["values"]),
            vec![after.physics_calls; 10],
            "{}: a withdrawn step left the game running: {withdrawn}",
            game.binary
        );
        let advanced = after.physics_calls - before.physics_calls;
        assert!(advanced > 0 && advanced < 600, "{advanced}");
        assert!(consecutive(&after.frames_since(&before)));
        game.marker_since(from, "tree paused");
        eprintln!(
            "{}: a withdrawn 600-frame step had advanced {advanced} frames and held",
            game.binary
        );

        // The game's own pause is never changed: a step is refused while the
        // game paused itself, and nothing moves.
        game.step(json!({ "frames": 0, "then": "resume" }));
        game.set(".", json!({ "paused": tagged("bool", json!(true)) }));
        let before = game.counts();
        for arguments in [
            json!({ "frames": 3 }),
            json!({ "frames": 0 }),
            json!({ "frames": 0, "then": "resume" }),
        ] {
            let (code, message) = game.refused("runtime.frames.step", arguments);
            assert_eq!(code, json!("conflict"), "{}: {message}", game.binary);
        }
        let paused = game.read(
            "runtime.sample",
            json!({ "series": [{ "node": ".", "property": "physics_calls" }], "cadence": "physics_frame", "maxSamples": 5 }),
        );
        assert_eq!(paused["paused_samples"], json!(5), "{paused}");
        assert_eq!(game.counts().physics_calls, before.physics_calls);
        assert_eq!(game.property(".", "paused"), json!(true));
        game.set(".", json!({ "paused": tagged("bool", json!(false)) }));

        // A pause the game makes during a step ends the step there: the
        // step says so and leaves the game's pause as it is.
        game.step(json!({ "frames": 0 }));
        game.set(
            ".",
            json!({ "pause_after_physics": tagged("integer", json!(5)) }),
        );
        let before = game.counts();
        let interfered = game.read("runtime.frames.step", json!({ "frames": 60 }));
        let after = game.counts();
        assert_step(&interfered, "interfered", 5, false, &game.binary);
        assert_eq!(interfered["paused"], json!(true), "{interfered}");
        assert_eq!(
            interfered["interference"]["paused"],
            json!(true),
            "{interfered}"
        );
        assert_eq!(
            interfered["interference"]["physics_frame"], interfered["last_frame"],
            "{interfered}"
        );
        assert_eq!(after.physics_calls - before.physics_calls, 5);
        // Pausing the tree stops the physics server at once, so the pause in
        // the fifth frame comes before that frame's step.
        assert_eq!(
            after.faller_steps - before.faller_steps,
            4,
            "{}: the physics server steps in the four frames before the game's pause",
            game.binary
        );
        assert_eq!(game.property(".", "paused"), json!(true));
        eprintln!(
            "{}: a game that paused itself in the 5th frame of a 60-frame step ended it there; the physics server took {} steps",
            game.binary,
            after.faller_steps - before.faller_steps
        );
        let (code, message) = game.refused("runtime.frames.step", json!({ "frames": 1 }));
        assert_eq!(code, json!("conflict"), "{message}");
        game.set(".", json!({ "paused": tagged("bool", json!(false)) }));

        // A game that unpauses the tree while the probe holds it takes the
        // pause over: it runs, and the probe holds nothing.
        game.step(json!({ "frames": 0 }));
        let held_calls = game.counts().physics_calls;
        game.set(".", json!({ "paused": tagged("bool", json!(false)) }));
        game.runs_past(held_calls + 3);
        game.step(json!({ "frames": 0, "then": "resume" }));

        // A session stopped while the probe holds its game releases the hold
        // before the game quits.
        game.step(json!({ "frames": 0 }));
        let from = game.markers().len();
        game.stop();
        assert!(
            game.markers()
                .iter()
                .skip(from)
                .any(|line| line == "tree unpaused"),
            "{}: the stop released no hold: {:?}",
            game.binary,
            game.markers()
        );
        game.finish();
    }
}

#[test]
#[ignore = "needs real Godot builds: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn live_a_held_game_runs_again_when_its_server_goes_away() {
    let (binaries, _unpacked) = binaries();
    for binary in &binaries {
        let mut game = Game::start_frames(binary, "live-orphan-hold", "headless");
        game.step(json!({ "frames": 0 }));
        game.marker("tree paused");
        let from = game.markers().len();
        game.assert_no_errors();
        let Game {
            session, tree, id, ..
        } = game;
        let base = tree.base.clone();
        let killed = Instant::now();
        session.kill();
        let deadline = killed + Duration::from_secs(20);
        while !markers_under(&base)
            .iter()
            .skip(from)
            .any(|line| line == "tree unpaused")
        {
            assert!(
                Instant::now() < deadline,
                "{}: the probe kept holding the game when its server went away: {:?}",
                binary.display(),
                markers_under(&base)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        eprintln!(
            "{}: a held game ran again within {} ms of its server being killed",
            binary.display(),
            killed.elapsed().as_millis()
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

#[test]
#[ignore = "needs real Godot builds: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn live_a_running_game_is_sampled_every_physics_frame() {
    let (binaries, _unpacked) = binaries();
    for binary in &binaries {
        let mut game = Game::start_frames(binary, "live-sample-frames", "headless");
        // Every idle frame stalls 35 ms, so physics frames run in bursts of
        // two or three to catch up.
        game.set(".", json!({ "stall_ms": tagged("integer", json!(35)) }));
        let sampled = game.read(
            "runtime.sample",
            json!({ "series": [
                { "node": ".", "property": "physics_calls" },
                { "node": ".", "property": "last_physics_frame" },
                { "node": ".", "property": "last_signal_frame" },
                { "node": ".", "property": "faller_steps" }
            ], "cadence": "physics_frame", "durationMs": 10000, "maxSamples": 60 }),
        );
        assert_eq!(sampled["stopped_by"], json!("max_samples"), "{sampled}");
        assert_eq!(
            sampled["time_base"]["cadence"],
            json!("physics_frame"),
            "{sampled}"
        );
        assert_eq!(
            sampled["time_base"]["interval_ms"],
            Value::Null,
            "{sampled}"
        );
        assert_eq!(sampled["paused_samples"], json!(0), "{sampled}");
        let frames = sample_frames(&sampled);
        assert_eq!(frames.len(), 60, "{sampled}");
        assert!(
            consecutive(&frames),
            "{}: a physics frame went unsampled: {frames:?}",
            game.binary
        );
        let column = |index: usize| whole_numbers(&sampled["series"][index]["values"]);
        assert!(consecutive(&column(0)), "{}: {sampled}", game.binary);
        // Each sample is taken at the start of its physics frame: the scene
        // processed the frame before it, and the physics_frame signal, which
        // the SceneTree emits before any node processes a frame, has reached
        // it for this one.
        assert_eq!(
            column(1),
            frames.iter().map(|frame| frame - 1).collect::<Vec<_>>(),
            "{}: {sampled}",
            game.binary
        );
        assert_eq!(column(2), frames, "{}: {sampled}", game.binary);
        assert!(consecutive(&column(3)), "{}: {sampled}", game.binary);
        let mut per_idle: std::collections::BTreeMap<i64, usize> =
            std::collections::BTreeMap::new();
        for sample in sampled["samples"].as_array().expect("samples") {
            *per_idle
                .entry(sample["frame"].as_i64().expect("a frame"))
                .or_default() += 1;
        }
        let burst = per_idle.values().max().copied().unwrap_or(0);
        assert!(burst >= 2, "{}: no burst: {per_idle:?}", game.binary);
        // Timed samples, one per idle frame at most, miss the physics frames
        // of a burst.
        let timed = game.read(
            "runtime.sample",
            json!({ "series": [{ "node": ".", "property": "physics_calls" }], "durationMs": 500, "intervalMs": 1 }),
        );
        let timed_frames = sample_frames(&timed);
        let skipped = timed_frames
            .windows(2)
            .filter(|pair| pair[1] - pair[0] > 1)
            .count();
        assert!(skipped > 0, "{}: {timed_frames:?}", game.binary);
        eprintln!(
            "{}: with 35 ms idle frames, 60 per-frame samples saw 60 consecutive physics frames, up to {burst} in one idle frame; 1 ms timed samples skipped physics frames {skipped} of {} times",
            game.binary,
            timed_frames.len().saturating_sub(1)
        );
        game.set(".", json!({ "stall_ms": tagged("integer", json!(0)) }));
        game.stop();
        game.finish();
    }
}

#[test]
#[ignore = "needs real Godot builds: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn live_input_is_replayed_by_physics_frame() {
    let (binaries, _unpacked) = binaries();
    for binary in &binaries {
        let mut game = Game::start_frames(binary, "live-replay-frames", "headless");

        // A recording names the physics frame in which the game first sees
        // each event: the one in which it is just pressed or just released.
        let started = game.read("input.recording.start", json!({}));
        let origin = started["first_physics_frame"]
            .as_i64()
            .expect("the recording's first physics frame");
        game.read(
            "input.sequence.send",
            json!({ "steps": [
                { "kind": "action", "action": "jump", "state": "press" },
                { "kind": "wait", "ms": 100 },
                { "kind": "action", "action": "jump", "state": "release" }
            ] }),
        );
        game.read(
            "runtime.sample",
            json!({ "series": [{ "monitor": "TIME_FPS" }], "cadence": "physics_frame", "maxSamples": 3 }),
        );
        let recorded = game.read("input.recording.stop", json!({}));
        assert_eq!(recorded["first_physics_frame"], json!(origin), "{recorded}");
        let jumps: Vec<Value> = recorded["events"]
            .as_array()
            .expect("events")
            .iter()
            .filter(|event| event["kind"] == json!("action") && event["action"] == json!("jump"))
            .cloned()
            .collect();
        assert_eq!(jumps.len(), 2, "{}: {recorded}", game.binary);
        let pressed = jumps[0]["frame"].as_i64().expect("a frame");
        let released = jumps[1]["frame"].as_i64().expect("a frame");
        assert_eq!(jumps[0]["pressed"], json!(true), "{recorded}");
        assert!(released > pressed, "{recorded}");
        assert!(
            game.frames("just_pressed").contains(&(origin + pressed)),
            "{}: {recorded}",
            game.binary
        );
        assert!(
            game.frames("just_released").contains(&(origin + released)),
            "{}: {recorded}",
            game.binary
        );
        assert!(game.frames("pressed_from").contains(&(origin + pressed)));
        assert!(game.frames("released_from").contains(&(origin + released)));

        // Replayed by physics frame, the recording lands in the frames it
        // names, counted from the replay's first.
        let rebased: Vec<Value> = jumps
            .iter()
            .map(|event| {
                let mut event = event.clone();
                event["frame"] = json!(event["frame"].as_i64().unwrap_or(0) - pressed);
                event
            })
            .collect();
        let replayed = game.read(
            "input.replay",
            json!({ "events": rebased, "schedule": "physics_frame" }),
        );
        let base = replayed["first_physics_frame"].as_i64().expect("a frame");
        assert_eq!(replayed["outcome"], json!("completed"), "{replayed}");
        assert_eq!(replayed["schedule"], json!("physics_frame"), "{replayed}");
        assert_eq!(replayed["max_lag_frames"], json!(0), "{replayed}");
        assert_eq!(
            deliveries(&replayed),
            vec![(0, base), (1, base + released - pressed)],
            "{replayed}"
        );
        assert!(game.frames("just_pressed").contains(&base), "{replayed}");
        assert!(
            game.frames("just_released")
                .contains(&(base + released - pressed)),
            "{replayed}"
        );

        // Events by frame, out of order and two of them in one frame, while
        // every idle frame stalls 35 ms and physics frames run in bursts.
        game.set(".", json!({ "stall_ms": tagged("integer", json!(35)) }));
        let events = json!([
            { "frame": 5, "kind": "key", "key": "Space", "pressed": true },
            { "frame": 0, "kind": "action", "action": "jump", "pressed": true },
            { "frame": 2, "kind": "action", "action": "jump", "pressed": false },
            { "frame": 9, "kind": "key", "key": "Space", "pressed": false },
            { "frame": 12, "kind": "joypad_button", "device": 0, "button": 0, "pressed": true },
            { "frame": 14, "kind": "joypad_button", "device": 0, "button": 0, "pressed": false },
            { "frame": 17, "kind": "action", "action": "jump", "pressed": true },
            { "frame": 17, "kind": "action", "action": "jump", "pressed": false }
        ]);
        let replayed = game.read(
            "input.replay",
            json!({ "events": events, "schedule": "physics_frame" }),
        );
        let base = replayed["first_physics_frame"].as_i64().expect("a frame");
        assert_eq!(replayed["outcome"], json!("completed"), "{replayed}");
        assert_eq!(replayed["events_sent"], json!(8), "{replayed}");
        assert_eq!(replayed["scheduled_frames"], json!(17), "{replayed}");
        assert_eq!(replayed["max_lag_frames"], json!(0), "{replayed}");
        assert_eq!(replayed["released"], json!([]), "{replayed}");
        assert_eq!(
            deliveries(&replayed),
            [
                (1, 0),
                (2, 2),
                (0, 5),
                (3, 9),
                (4, 12),
                (5, 14),
                (6, 17),
                (7, 17)
            ]
            .iter()
            .map(|(index, frame)| (*index, base + frame))
            .collect::<Vec<_>>(),
            "{}: {replayed}",
            game.binary
        );
        let at = |frames: &[i64]| frames.iter().map(|frame| base + frame).collect::<Vec<_>>();
        assert_eq!(
            from_frame(&game.frames("just_pressed"), base),
            at(&[0, 5, 12, 17]),
            "{}: {replayed}",
            game.binary
        );
        assert_eq!(
            from_frame(&game.frames("just_released"), base),
            at(&[2, 9, 14, 17]),
            "{}: {replayed}",
            game.binary
        );
        assert_eq!(
            from_frame(&game.frames("pressed_from"), base),
            at(&[0, 5, 12]),
            "{}: {replayed}",
            game.binary
        );
        assert_eq!(
            from_frame(&game.frames("released_from"), base),
            at(&[2, 9, 14]),
            "{}: {replayed}",
            game.binary
        );
        // `_input` received each event before the frame it lands in: those of
        // frame 0 between physics frames, the others at the end of the frame
        // before theirs, in list order within one frame.
        let seen: Vec<(i64, bool, bool)> = input_seen(&game.property(".", "input_seen"))
            .into_iter()
            .filter(|(frame, _, _)| *frame >= base - 1)
            .collect();
        assert_eq!(
            seen,
            vec![
                (base - 1, false, true),
                (base + 1, true, false),
                (base + 4, true, true),
                (base + 8, true, false),
                (base + 11, true, true),
                (base + 13, true, false),
                (base + 16, true, true),
                (base + 16, true, false),
            ],
            "{}: {replayed}",
            game.binary
        );
        let unhandled: Vec<(i64, bool, bool)> = input_seen(&game.property(".", "unhandled_seen"))
            .into_iter()
            .filter(|(frame, _, _)| *frame >= base - 1)
            .collect();
        eprintln!(
            "{}: 8 events replayed by physics frame under 35 ms idle frames landed in their frames in {} ms; _unhandled_input saw {unhandled:?} from frame {}",
            game.binary,
            replayed["elapsed_ms"],
            base - 1
        );

        // Replayed by time, each delivery names the frame the game saw it in.
        let replayed = game.read(
            "input.replay",
            json!({ "events": [
                { "t_ms": 0, "kind": "action", "action": "jump", "pressed": true },
                { "t_ms": 150, "kind": "action", "action": "jump", "pressed": false }
            ] }),
        );
        assert_eq!(replayed["schedule"], json!("time"), "{replayed}");
        assert_eq!(replayed["max_lag_frames"], Value::Null, "{replayed}");
        assert_eq!(replayed["scheduled_frames"], Value::Null, "{replayed}");
        assert!(replayed["max_lag_ms"].is_number(), "{replayed}");
        let timed = deliveries(&replayed);
        assert_eq!(timed.len(), 2, "{replayed}");
        assert!(
            game.frames("just_pressed").contains(&timed[0].1),
            "{}: {replayed}",
            game.binary
        );
        assert!(
            game.frames("just_released").contains(&timed[1].1),
            "{}: {replayed}",
            game.binary
        );
        game.set(".", json!({ "stall_ms": tagged("integer", json!(0)) }));

        // A held game runs no physics frame, so it takes no replay by frame.
        game.step(json!({ "frames": 0 }));
        let (code, message) = game.refused(
            "input.replay",
            json!({ "schedule": "physics_frame", "events": [
                { "frame": 0, "kind": "action", "action": "jump", "pressed": true }
            ] }),
        );
        assert_eq!(code, json!("conflict"), "{}: {message}", game.binary);
        game.step(json!({ "frames": 0, "then": "resume" }));
        game.stop();
        game.finish();
    }
}

#[test]
#[ignore = "needs real Godot builds and a display: GODOT_MCP_DISPLAY_REQUIRED=1 under xvfb-run"]
fn live_a_window_is_held_captured_and_stepped() {
    if std::env::var("GODOT_MCP_DISPLAY_REQUIRED").as_deref() != Ok("1") {
        eprintln!("GODOT_MCP_DISPLAY_REQUIRED is not 1; windowed frame stepping was not checked");
        return;
    }
    assert!(
        std::env::var_os("DISPLAY").is_some(),
        "GODOT_MCP_DISPLAY_REQUIRED=1 needs a display: run under xvfb-run"
    );
    let (binaries, _unpacked) = binaries();
    for binary in &binaries {
        let mut game = Game::start_frames(binary, "live-step-window", "window");
        game.runs_past(5);
        game.step(json!({ "frames": 0 }));
        // A held window still draws: it is captured, and nothing advances.
        let before = game.counts();
        let shot = game.read("runtime.screenshot.capture", json!({}));
        assert!(
            shot["artifact"]["byte_length"].as_u64() > Some(0),
            "{}: {shot}",
            game.binary
        );
        let after = game.counts();
        assert_eq!(after.physics_calls, before.physics_calls, "{shot}");
        assert_eq!(after.process_calls, before.process_calls, "{shot}");

        let stepped = game.step(json!({ "frames": 10 }));
        let (first, last) = span(&stepped);
        let stepped_counts = game.counts();
        assert_eq!(stepped_counts.physics_calls - after.physics_calls, 10);
        assert_eq!(
            stepped_counts.frames_since(&after),
            (first..=last).collect::<Vec<_>>()
        );
        assert_eq!(stepped_counts.faller_steps - after.faller_steps, 10);

        game.step(json!({ "frames": 0, "then": "resume" }));
        let sampled = game.read(
            "runtime.sample",
            json!({ "series": [{ "node": ".", "property": "physics_calls" }], "cadence": "physics_frame", "maxSamples": 20 }),
        );
        assert!(consecutive(&sample_frames(&sampled)), "{sampled}");
        assert!(
            consecutive(&whole_numbers(&sampled["series"][0]["values"])),
            "{sampled}"
        );
        let replayed = game.read(
            "input.replay",
            json!({ "schedule": "physics_frame", "events": [
                { "frame": 0, "kind": "key", "key": "Space", "pressed": true },
                { "frame": 3, "kind": "key", "key": "Space", "pressed": false }
            ] }),
        );
        let base = replayed["first_physics_frame"].as_i64().expect("a frame");
        assert_eq!(replayed["max_lag_frames"], json!(0), "{replayed}");
        assert_eq!(
            from_frame(&game.frames("just_pressed"), base),
            vec![base],
            "{}: {replayed}",
            game.binary
        );
        assert_eq!(
            from_frame(&game.frames("just_released"), base),
            vec![base + 3],
            "{}: {replayed}",
            game.binary
        );
        eprintln!(
            "{}: a held window was captured ({} bytes) without advancing; a 10-frame step advanced {first}..={last}",
            game.binary, shot["artifact"]["byte_length"]
        );
        game.stop();
        game.finish();
    }
}
