//! `runtime.script.run` (ADR 0012) against real Godot builds (`#[ignore]`d;
//! `GODOT_MCP_TEST_EDITORS` or `GODOT_MCP_LOCKED_ARTIFACT` names them): a run
//! reaches the running game's scene and autoloads; what it returns, prints,
//! fails with and changes comes back; a run its deadline outlasts goes on in
//! the game; and stopping the session ends a run that never returns. Without
//! an engine: the operator's switch decides whether a game is ever asked.

#[path = "support/live.rs"]
mod live;
mod support;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::{ProjectTree, StdioSession, error_code};

const MAIN_SCENE: &str = "[gd_scene load_steps=2 format=3]\n\n\
    [ext_resource type=\"Script\" path=\"res://main.gd\" id=\"1\"]\n\n\
    [node name=\"Main\" type=\"Node2D\"]\nscript = ExtResource(\"1\")\n";

/// A small game: a main scene with a score, and an autoload with lives.
fn game_tree(label: &str) -> ProjectTree {
    let tree = ProjectTree::new(label);
    tree.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Scripted game\"\n\
         run/main_scene=\"res://main.tscn\"\n\n[autoload]\n\nGame=\"*res://game.gd\"\n",
    );
    tree.write("main.tscn", MAIN_SCENE);
    tree.write("main.gd", "extends Node2D\n\nvar score := 7\n");
    tree.write("game.gd", "extends Node\n\nvar lives := 3\n");
    tree
}

/// A server on `tree` running games through `binary`, with scripts when
/// `scripts`.
fn server(tree: &ProjectTree, binary: Option<&Path>, scripts: bool) -> StdioSession {
    let mut command = Command::new(env!("CARGO_BIN_EXE_qa-journal-foundation"));
    command
        .arg("--project")
        .arg(&tree.root)
        .arg("--profile")
        .arg("full")
        .env_remove("GODOT_MCP_TEST_LAUNCH_CRASH_STAGE");
    if let Some(binary) = binary {
        command.arg("--godot-binary").arg(binary);
    }
    if scripts {
        command.arg("--allow-scripts");
    }
    StdioSession::handshake(StdioSession::start_command(command)).0
}

/// One running game session on a scripting server.
struct Game {
    session: StdioSession,
    tree: ProjectTree,
    id: String,
    next: u64,
    binary: String,
}

impl Game {
    fn start(binary: &Path, label: &str) -> Self {
        let tree = game_tree(label);
        let mut session = server(&tree, Some(binary), true);
        let started = session.call_tool(
            2,
            "project.run.start",
            json!({ "idempotencyId": "script-start", "display": "headless" }),
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
        Self {
            session,
            tree,
            id,
            next: 10,
            binary: binary.display().to_string(),
        }
    }

    fn call(&mut self, tool: &str, mut arguments: Value) -> Value {
        self.next += 1;
        arguments["session"] = json!(self.id);
        let mutation = godot_mcp_domain::catalog::descriptor(tool)
            .is_some_and(godot_mcp_domain::catalog::ToolDescriptor::is_mutation);
        if mutation && arguments.get("idempotencyId").is_none() {
            arguments["idempotencyId"] = json!(format!("script-{}", self.next));
        }
        self.session.call_tool(self.next, tool, arguments)
    }

    /// Send a call without waiting for its answer; returns its request id.
    fn send(&mut self, tool: &str, mut arguments: Value) -> u64 {
        self.next += 1;
        arguments["session"] = json!(self.id);
        arguments["idempotencyId"] = json!(format!("script-{}", self.next));
        self.session.send(&json!({
            "jsonrpc": "2.0",
            "id": self.next,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments }
        }));
        self.next
    }

    fn answer(&mut self, id: u64) -> Value {
        let arrived = self
            .session
            .transcript
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|frame| frame["id"] == json!(id));
        arrived.unwrap_or_else(|| self.session.await_response(id, "tools/call"))
    }

    fn ok(&mut self, tool: &str, arguments: Value) -> Value {
        let response = self.call(tool, arguments.clone());
        assert!(
            response.get("error").is_none() && response["result"]["isError"] != json!(true),
            "{}: {tool} {arguments} must succeed: {response}",
            self.binary
        );
        response["result"]["structuredContent"].clone()
    }

    fn run(&mut self, source: &str) -> Value {
        self.ok("runtime.script.run", json!({ "source": source }))
    }

    fn stop(mut self) {
        let _ = self.call("project.run.stop", json!({}));
        let _ = self.session.finish();
    }
}

fn source_lines(answer: &Value) -> Vec<u64> {
    answer["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .filter_map(|diagnostic| diagnostic["source_line"].as_u64())
        .collect()
}

/// A game on the `qa-fake-game` stand-in, on a server running scripts when
/// `scripts`.
#[cfg(target_os = "linux")]
fn fake_game(label: &str, scripts: bool) -> (ProjectTree, StdioSession, String) {
    let tree = game_tree(label);
    let mut session = server(
        &tree,
        Some(Path::new(env!("CARGO_BIN_EXE_qa-fake-game"))),
        scripts,
    );
    let started = session.call_tool(
        2,
        "project.run.start",
        json!({ "idempotencyId": "fake-start", "display": "headless" }),
    );
    let id = started["result"]["structuredContent"]["session"]["session"]
        .as_str()
        .unwrap_or_else(|| panic!("the fake game starts: {started}"))
        .to_owned();
    (tree, session, id)
}

#[cfg(target_os = "linux")]
#[test]
fn a_game_is_never_asked_to_run_a_script_unless_the_server_allows_scripts() {
    let (tree, mut session, id) = fake_game("runtime-script-off", false);
    let refused = session.call_tool(
        3,
        "runtime.script.run",
        json!({
            "session": id,
            "source": "extends RefCounted\n\nfunc run():\n\treturn 1\n",
            "idempotencyId": "off",
        }),
    );
    assert_eq!(
        error_code(&refused),
        json!("permission_denied"),
        "{refused}"
    );
    assert!(
        refused.to_string().contains("--allow-scripts"),
        "the refusal names the switch: {refused}"
    );
    assert!(
        !tree.root.join("made_by_script.txt").exists(),
        "nothing reached the game"
    );
    let _ = session.call_tool(
        4,
        "project.run.stop",
        json!({ "session": id, "idempotencyId": "off-stop" }),
    );
    let _ = session.finish();
}

#[cfg(target_os = "linux")]
#[test]
fn an_allowed_run_is_answered_with_its_value_its_output_and_what_it_changed() {
    let (_tree, mut session, id) = fake_game("runtime-script-on", true);
    let response = session.call_tool(
        3,
        "runtime.script.run",
        json!({
            "session": id,
            "source": "extends RefCounted\n\nfunc run():\n\treturn 5\n",
            "idempotencyId": "on",
        }),
    );
    let answer = live::ok(&response, "the fake game");
    assert_eq!(answer["classification"], "passed", "{answer}");
    assert_eq!(answer["result"], json!({ "type": "integer", "value": 5 }));
    assert_eq!(answer["session"], json!(id), "{answer}");
    assert_eq!(
        answer["output"],
        json!([{ "text": "from the fake game", "stderr": false }])
    );
    assert_eq!(
        answer["changes"]["added"][0]["path"], "made_by_script.txt",
        "{answer}"
    );
    assert_eq!(answer["applied"], true);
    assert_eq!(answer["rolled_back"], false);
    let _ = session.call_tool(
        4,
        "project.run.stop",
        json!({ "session": id, "idempotencyId": "on-stop" }),
    );
    let _ = session.finish();
}

#[test]
#[ignore = "needs a Godot build: set GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_run_reaches_the_running_game_and_says_what_it_did() {
    let (binaries, _unpacked) = live::editors();
    for binary in &binaries {
        a_run_reaches_the_running_game_and_says_what_it_did_in(binary);
    }
}

fn a_run_reaches_the_running_game_and_says_what_it_did_in(binary: &Path) {
    let mut game = Game::start(binary, "live-runtime-script");
    let made = game.run(
        "extends RefCounted\n\nfunc run():\n\tprint(\"from the game\")\n\tvar main = Engine.get_main_loop().current_scene\n\tmain.score += 5\n\tvar file := FileAccess.open(\"res://made_in_game.txt\", FileAccess.WRITE)\n\tfile.store_string(\"made\")\n\tfile.close()\n\treturn [main.name, main.score, Game.lives, Engine.is_editor_hint()]\n",
    );
    let report = format!("{made}\n{}", game.binary);
    let captured = made["log_capture"] == json!(true);
    assert_eq!(
        made["classification"],
        json!(if captured { "passed" } else { "inconclusive" }),
        "{report}"
    );
    assert_eq!(
        made["result_literal"],
        json!("[&\"Main\", 12, 3, false]"),
        "{report}"
    );
    let added: Vec<&str> = made["changes"]["added"]
        .as_array()
        .expect("added")
        .iter()
        .filter_map(|file| file["path"].as_str())
        .collect();
    assert_eq!(added, ["made_in_game.txt"], "{report}");
    assert_eq!(made["session"], json!(game.id), "{report}");
    if captured {
        assert!(
            made["output"]
                .as_array()
                .expect("output")
                .iter()
                .any(|line| line["text"] == "from the game"),
            "{report}"
        );
    }
    // The typed tools see what the script changed.
    let score = game.ok(
        "runtime.properties.get",
        json!({ "node": ".", "properties": ["score"] }),
    );
    assert!(
        score.to_string().contains("12"),
        "{}: the scene holds the new score: {score}",
        game.binary
    );

    let node = game.run(
        "extends Node\n\nfunc run():\n\tawait get_tree().process_frame\n\treturn is_inside_tree()\n",
    );
    assert_eq!(
        node["result"],
        json!({ "type": "bool", "value": true }),
        "{node}\n{}",
        game.binary
    );

    let compile = game.run("extends RefCounted\n\nfunc run():\n\treturn undefined_name\n");
    let report = format!("{compile}\n{}", game.binary);
    assert_eq!(
        compile["classification"],
        json!("compile_error"),
        "{report}"
    );
    if captured {
        assert!(source_lines(&compile).contains(&4), "{report}");
    }
    let failed =
        game.run("extends RefCounted\n\nfunc run():\n\tvar items := []\n\treturn items[3]\n");
    let report = format!("{failed}\n{}", game.binary);
    if captured {
        assert_eq!(failed["classification"], json!("failed"), "{report}");
        assert!(source_lines(&failed).contains(&5), "{report}");
    } else {
        assert_eq!(failed["classification"], json!("inconclusive"), "{report}");
    }
    game.stop();
}

#[test]
#[ignore = "needs a Godot build: set GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_run_its_deadline_outlasts_is_uncertain_and_goes_on_in_the_game() {
    let (binaries, _unpacked) = live::editors();
    for binary in &binaries {
        a_run_its_deadline_outlasts_is_uncertain_and_goes_on_in_the_game_in(binary);
    }
}

fn a_run_its_deadline_outlasts_is_uncertain_and_goes_on_in_the_game_in(binary: &Path) {
    let mut game = Game::start(binary, "live-runtime-script-late");
    let started = Instant::now();
    let late = game.call(
        "runtime.script.run",
        json!({
            "source": "extends Node\n\nfunc run():\n\tawait get_tree().create_timer(15.0).timeout\n\tvar file := FileAccess.open(\"res://late.txt\", FileAccess.WRITE)\n\tfile.store_string(\"late\")\n\tfile.close()\n\treturn 1\n",
            "deadline_ms": 1000,
        }),
    );
    let report = format!("{late}\n{}", game.binary);
    assert_eq!(error_code(&late), json!("uncertain"), "{report}");
    assert!(
        late.to_string().contains("may still be running"),
        "{report}"
    );
    // Well before the run's 15 s timer, with room for a loaded host.
    assert!(
        started.elapsed() < Duration::from_secs(12),
        "the server stopped waiting at the deadline: {:?}\n{report}",
        started.elapsed()
    );
    let late_file = game.tree.root.join("late.txt");
    assert!(!late_file.exists(), "the run had not written yet");
    let waiting = Instant::now();
    while !late_file.exists() {
        assert!(
            waiting.elapsed() < Duration::from_secs(60),
            "{}: the run never finished in the game",
            game.binary
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // A game's writes are outside the server's journal, as the game's own
    // code's are: the unresolved run blocks no later call.
    let after = game.ok(
        "runtime.properties.get",
        json!({ "node": ".", "properties": ["score"] }),
    );
    assert!(after.to_string().contains('7'), "{after}");
    game.stop();
}

#[test]
#[ignore = "needs a Godot build: set GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn stopping_the_session_ends_a_run_that_never_returns() {
    let (binaries, _unpacked) = live::editors();
    for binary in &binaries {
        stopping_the_session_ends_a_run_that_never_returns_in(binary);
    }
}

fn stopping_the_session_ends_a_run_that_never_returns_in(binary: &Path) {
    let mut game = Game::start(binary, "live-runtime-script-endless");
    let endless = game.send(
        "runtime.script.run",
        json!({
            "source": "extends Node\n\nfunc run():\n\tvar file := FileAccess.open(\"res://endless_started.txt\", FileAccess.WRITE)\n\tfile.store_string(\"started\")\n\tfile.close()\n\twhile true:\n\t\tawait get_tree().process_frame\n",
            "deadline_ms": 600_000,
        }),
    );
    let marker = game.tree.root.join("endless_started.txt");
    let waiting = Instant::now();
    while !marker.exists() {
        assert!(
            waiting.elapsed() < Duration::from_secs(30),
            "{}: the run never started in the game",
            game.binary
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let stopping = Instant::now();
    let stopped = game.call("project.run.stop", json!({}));
    let waited = stopping.elapsed();
    assert!(
        stopped.get("error").is_none() && stopped["result"]["isError"] != json!(true),
        "{}: the stop succeeds: {stopped}",
        game.binary
    );
    assert!(
        waited < Duration::from_secs(20),
        "{}: the stop does not wait for the run's deadline: {waited:?}",
        game.binary
    );
    let ended = game.answer(endless);
    let report = format!("{ended}\n{}", game.binary);
    assert_eq!(error_code(&ended), json!("uncertain"), "{report}");
    assert!(
        ended.to_string().contains("until its session stops"),
        "{report}"
    );
    let _ = game.session.finish();
}
