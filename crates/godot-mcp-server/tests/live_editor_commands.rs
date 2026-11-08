//! `editor.commands.list` and `editor.commands.call` (ADR 0016) against real
//! Godot editors (`#[ignore]`d; `GODOT_MCP_TEST_EDITORS` or
//! `GODOT_MCP_LOCKED_ARTIFACT` names them): a declared command runs in the
//! editor and its value, output, error and changes come back; a call the
//! declaration does not allow never reaches the editor; a provider that does
//! not compile is reported with its line; a command declared not to change
//! files that did is flagged; a retry replays; and a call that outlasts its
//! deadline is uncertain, blocks writes and still finishes in the editor,
//! whose answer then settles it and reopens writes, and whose session stays
//! usable. The add-on's own cases run headless in each build too.

#[path = "support/live.rs"]
mod live;
mod support;

use std::path::Path;
use std::time::{Duration, Instant};

use live::{Editor, Ids, editors, ok, project_with_addon, wait_connected};
use serde_json::{Value, json};
use support::{ProjectTree, StdioSession, error_code};

const DECLARATION: &str = r#"{
  "version": 1,
  "providers": [
    {
      "id": "levels",
      "script": "res://tools/level_commands.gd",
      "commands": [
        {
          "name": "rebuild",
          "title": "Rebuild a level",
          "description": "Writes levels/level_<seed>.txt and counts each run in levels/runs_<seed>.log.",
          "input_schema": {
            "type": "object",
            "additionalProperties": false,
            "required": ["seed"],
            "properties": { "seed": { "type": "integer", "minimum": 0 } }
          },
          "mutates": true
        },
        {
          "name": "where",
          "title": "Where am I",
          "description": "Answers whether it runs in the editor.",
          "input_schema": { "type": "object", "additionalProperties": false, "properties": {} },
          "mutates": false
        },
        {
          "name": "sneaky",
          "title": "Sneaky",
          "description": "Declared not to change files, but writes one.",
          "input_schema": { "type": "object" },
          "mutates": false
        },
        {
          "name": "fail",
          "title": "Fail",
          "description": "Reports an error.",
          "input_schema": { "type": "object" },
          "mutates": false
        },
        {
          "name": "slow",
          "title": "Slow",
          "description": "Waits six seconds, then writes slow.txt.",
          "input_schema": { "type": "object" },
          "mutates": true
        }
      ]
    },
    {
      "id": "broken",
      "script": "res://tools/broken_commands.gd",
      "commands": [{
        "name": "oops",
        "title": "Oops",
        "description": "Its script does not compile.",
        "input_schema": { "type": "object" },
        "mutates": false
      }]
    },
    {
      "id": "ghost",
      "script": "res://tools/missing.gd",
      "commands": [{
        "name": "boo",
        "title": "Boo",
        "description": "Its script does not exist.",
        "input_schema": { "type": "object" },
        "mutates": false
      }]
    }
  ]
}
"#;

const LEVEL_COMMANDS: &str = r#"@tool
extends RefCounted


func mcp_call(command: String, args: Dictionary) -> Dictionary:
	match command:
		"rebuild":
			var seed_value: int = args["seed"]
			print("rebuilding with seed ", seed_value)
			DirAccess.make_dir_recursive_absolute("res://levels")
			var level := FileAccess.open("res://levels/level_%d.txt" % seed_value, FileAccess.WRITE)
			level.store_string("rooms=3\n")
			level.close()
			var runs_path := "res://levels/runs_%d.log" % seed_value
			var runs := FileAccess.get_file_as_string(runs_path) if FileAccess.file_exists(runs_path) else ""
			var log_file := FileAccess.open(runs_path, FileAccess.WRITE)
			log_file.store_string(runs + "ran\n")
			log_file.close()
			return {"ok": true, "value": {"rooms": 3, "seed": seed_value}}
		"where":
			return {"ok": true, "value": Engine.is_editor_hint()}
		"sneaky":
			var file := FileAccess.open("res://sneaky.txt", FileAccess.WRITE)
			file.store_string("sneaky\n")
			file.close()
			return {"ok": true, "value": "done"}
		"fail":
			return {"ok": false, "error": "no level is open"}
		"slow":
			await (Engine.get_main_loop() as SceneTree).create_timer(6.0).timeout
			var late := FileAccess.open("res://slow.txt", FileAccess.WRITE)
			late.store_string("slow\n")
			late.close()
			return {"ok": true, "value": 1}
	return {"ok": false, "error": "unknown command " + command}
"#;

/// Line 5 names an identifier nothing declares.
const BROKEN_COMMANDS: &str = "@tool\nextends RefCounted\n\nfunc mcp_call(_command: String, _args: Dictionary):\n\treturn undefined_command_name\n";

fn commands_project(label: &str) -> (ProjectTree, StdioSession, Ids) {
    project_with_addon(
        label,
        &[
            ("godot_mcp_commands.json", DECLARATION),
            ("tools/level_commands.gd", LEVEL_COMMANDS),
            ("tools/broken_commands.gd", BROKEN_COMMANDS),
        ],
    )
}

fn call(
    session: &mut StdioSession,
    ids: &mut Ids,
    provider: &str,
    command: &str,
    arguments: Value,
    idempotency_id: &str,
) -> Value {
    session.call_tool(
        ids.next(),
        "editor.commands.call",
        json!({
            "provider": provider,
            "command": command,
            "arguments": arguments,
            "idempotencyId": idempotency_id,
        }),
    )
}

fn listed<'a>(list: &'a Value, provider: &str, name: &str) -> &'a Value {
    list["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .find(|found| found["id"] == provider)
        .and_then(|found| {
            found["commands"]
                .as_array()
                .expect("commands")
                .iter()
                .find(|found| found["name"] == name)
        })
        .unwrap_or_else(|| panic!("{provider}/{name} is listed: {list}"))
}

fn added(answer: &Value) -> Vec<String> {
    answer["changes"]["added"]
        .as_array()
        .expect("added")
        .iter()
        .filter_map(|file| file["path"].as_str().map(str::to_owned))
        .collect()
}

/// Whether the session's methods carry `editor.commands.call`.
fn serves_commands(status: &Value) -> bool {
    status["methods"]
        .as_array()
        .expect("methods")
        .iter()
        .any(|method| method == "editor.commands.call")
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_declared_command_runs_in_the_editor_and_says_what_it_did() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        a_declared_command_runs_in_the_editor_and_says_what_it_did_in(binary);
    }
}

fn a_declared_command_runs_in_the_editor_and_says_what_it_did_in(binary: &Path) {
    let (tree, mut session, mut ids) = commands_project("live-editor-commands");
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    let status = wait_connected(&mut session, &mut ids, &editor);
    assert!(
        serves_commands(&status),
        "{}: the session carries the method: {status}",
        editor.binary
    );
    let list = ok(
        &session.call_tool(ids.next(), "editor.commands.list", json!({})),
        &editor.context(),
    );
    let report = format!("{list}\n{}", editor.binary);
    assert_eq!(list["status"], json!("valid"), "{report}");
    assert_eq!(
        list["editor"],
        json!({ "connected": true, "serves_commands": true }),
        "{report}"
    );
    assert_eq!(
        listed(&list, "levels", "rebuild")["available"],
        json!(true),
        "{report}"
    );
    assert_eq!(
        listed(&list, "broken", "oops")["available"],
        json!(true),
        "{report}"
    );
    assert_eq!(
        listed(&list, "ghost", "boo")["available"],
        json!(false),
        "{report}"
    );

    // Refused before the editor is asked: nothing runs, nothing is journaled.
    let wrong = call(
        &mut session,
        &mut ids,
        "levels",
        "rebuild",
        json!({ "seed": "x" }),
        "wrong",
    );
    assert_eq!(
        error_code(&wrong),
        json!("invalid_input"),
        "{wrong}\n{}",
        editor.binary
    );
    let ghost = call(&mut session, &mut ids, "ghost", "boo", json!({}), "ghost");
    assert_eq!(
        error_code(&ghost),
        json!("invalid_input"),
        "{ghost}\n{}",
        editor.binary
    );
    assert!(
        ghost.to_string().contains("does not exist"),
        "{ghost}\n{}",
        editor.binary
    );
    assert!(
        !tree.root.join("levels").exists(),
        "a refused call ran nothing"
    );

    let mark = editor.mark();
    let rebuilt = ok(
        &call(
            &mut session,
            &mut ids,
            "levels",
            "rebuild",
            json!({ "seed": 7 }),
            "rebuild-7",
        ),
        &editor.context(),
    );
    let report = format!("{rebuilt}\n{}", editor.binary);
    let captured = rebuilt["log_capture"] == json!(true);
    assert_eq!(
        rebuilt["classification"],
        json!(if captured { "passed" } else { "inconclusive" }),
        "{report}"
    );
    assert_eq!(
        rebuilt["result"],
        json!({
            "type": "dictionary",
            "value": {
                "rooms": { "type": "integer", "value": 3 },
                "seed": { "type": "integer", "value": 7 }
            }
        }),
        "{report}"
    );
    assert_eq!(
        rebuilt["command"],
        json!({ "provider": "levels", "name": "rebuild", "mutates": true }),
        "{report}"
    );
    let mut made = added(&rebuilt);
    made.sort();
    assert_eq!(
        made,
        ["levels/level_7.txt", "levels/runs_7.log"],
        "{report}"
    );
    assert_eq!(rebuilt["declaration_mismatch"], Value::Null, "{report}");
    assert_eq!(rebuilt["applied"], json!(true), "{report}");
    assert_eq!(rebuilt["rolled_back"], json!(false), "{report}");
    assert_eq!(rebuilt["mutation_status"], json!("committed"), "{report}");
    if captured {
        assert!(
            rebuilt["output"]
                .as_array()
                .expect("output")
                .iter()
                .any(|line| line["text"] == "rebuilding with seed 7"),
            "{report}"
        );
    }

    let place = ok(
        &call(
            &mut session,
            &mut ids,
            "levels",
            "where",
            json!({}),
            "where",
        ),
        &editor.context(),
    );
    assert_eq!(
        place["result"],
        json!({ "type": "bool", "value": true }),
        "{place}\n{}",
        editor.binary
    );
    assert_eq!(
        place["changes"]["total"],
        json!(0),
        "{place}\n{}",
        editor.binary
    );
    assert_eq!(
        place["declaration_mismatch"],
        Value::Null,
        "{place}\n{}",
        editor.binary
    );

    let sneaky = ok(
        &call(
            &mut session,
            &mut ids,
            "levels",
            "sneaky",
            json!({}),
            "sneaky",
        ),
        &editor.context(),
    );
    let report = format!("{sneaky}\n{}", editor.binary);
    assert_eq!(added(&sneaky), ["sneaky.txt"], "{report}");
    assert!(
        sneaky["declaration_mismatch"]
            .as_str()
            .is_some_and(|text| text.contains("mutates false")),
        "{report}"
    );

    let failed = ok(
        &call(&mut session, &mut ids, "levels", "fail", json!({}), "fail"),
        &editor.context(),
    );
    let report = format!("{failed}\n{}", editor.binary);
    assert_eq!(failed["classification"], json!("failed"), "{report}");
    assert_eq!(failed["error"], json!("no level is open"), "{report}");
    assert_eq!(failed["result"], Value::Null, "{report}");
    editor.assert_no_errors_since(mark, "the declared commands that work");

    let replay = ok(
        &call(
            &mut session,
            &mut ids,
            "levels",
            "rebuild",
            json!({ "seed": 7 }),
            "rebuild-7",
        ),
        &editor.context(),
    );
    assert_eq!(
        replay["replayed"],
        json!(true),
        "{replay}\n{}",
        editor.binary
    );
    assert_eq!(
        std::fs::read_to_string(tree.root.join("levels/runs_7.log")).expect("runs"),
        "ran\n",
        "a replay never calls the command again\n{}",
        editor.binary
    );

    let broken = ok(
        &call(&mut session, &mut ids, "broken", "oops", json!({}), "oops"),
        &editor.context(),
    );
    let report = format!("{broken}\n{}", editor.binary);
    assert_eq!(broken["classification"], json!("compile_error"), "{report}");
    assert_eq!(broken["changes"]["total"], json!(0), "{report}");
    if captured {
        assert!(
            broken["diagnostics"]
                .as_array()
                .expect("diagnostics")
                .iter()
                .any(|diagnostic| diagnostic["source_line"] == json!(5)),
            "{report}"
        );
    }
    let _ = session.finish();
    drop(editor);
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_call_that_outlasts_its_deadline_blocks_writes_and_the_editor_finishes_it() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        a_call_that_outlasts_its_deadline_blocks_writes_and_the_editor_finishes_it_in(binary);
    }
}

fn a_call_that_outlasts_its_deadline_blocks_writes_and_the_editor_finishes_it_in(binary: &Path) {
    let (tree, mut session, mut ids) = commands_project("live-editor-commands-late");
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    let started = Instant::now();
    let late = session.call_tool(
        ids.next(),
        "editor.commands.call",
        json!({
            "provider": "levels",
            "command": "slow",
            "deadline_ms": 1000,
            "idempotencyId": "slow",
        }),
    );
    let report = format!("{late}\n{}", editor.context());
    assert_eq!(error_code(&late), json!("uncertain"), "{report}");
    assert!(
        late.to_string().contains("may still be running"),
        "{report}"
    );
    // The deadline and a two-second grace, well before the command's six
    // seconds, with room for a loaded host's journal writes.
    assert!(
        started.elapsed() < Duration::from_secs(12),
        "the server stopped waiting at the deadline: {:?}\n{report}",
        started.elapsed()
    );
    // While the call's outcome is unknown, no write is allowed, and no other
    // command call either.
    let blocked = session.call_tool(
        ids.next(),
        "file.create",
        json!({ "path": "after.txt", "content": "x\n", "idempotencyId": "after-late" }),
    );
    assert!(
        blocked.get("error").is_some() || blocked["result"]["isError"] == json!(true),
        "a write after an unanswered call is refused: {blocked}"
    );
    assert!(!tree.root.join("after.txt").exists());
    let blocked_call = call(
        &mut session,
        &mut ids,
        "levels",
        "where",
        json!({}),
        "after-slow",
    );
    assert!(
        blocked_call.get("error").is_some() || blocked_call["result"]["isError"] == json!(true),
        "a command call after an unanswered one is refused too: {blocked_call}"
    );
    // The editor goes on running the command the server stopped waiting for.
    let waiting = Instant::now();
    while !tree.root.join("slow.txt").exists() {
        assert!(
            waiting.elapsed() < Duration::from_secs(60),
            "the command never finished in the editor\n{}",
            editor.context()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Its answer settles the call and reopens writes (ADR 0015).
    writes_reopen(&mut session, &mut ids, &editor);
    ok(
        &session.call_tool(
            ids.next(),
            "file.create",
            json!({ "path": "after.txt", "content": "x\n", "idempotencyId": "after-settled" }),
        ),
        &editor.context(),
    );
    // A retry answers the settled outcome from the journal; the editor is not
    // asked to run the command again.
    let replay = ok(
        &session.call_tool(
            ids.next(),
            "editor.commands.call",
            json!({
                "provider": "levels",
                "command": "slow",
                "deadline_ms": 1000,
                "idempotencyId": "slow",
            }),
        ),
        &editor.context(),
    );
    assert_eq!(
        replay["replayed"],
        json!(true),
        "{}: {replay}",
        editor.binary
    );
    // Reads still work, and the editor's session outlived the late answer.
    let status = ok(
        &session.call_tool(ids.next(), "editor.status.get", json!({})),
        &editor.context(),
    );
    assert_eq!(
        status["connected"],
        json!(true),
        "{status}\n{}",
        editor.binary
    );
    let list = ok(
        &session.call_tool(ids.next(), "editor.commands.list", json!({})),
        &editor.context(),
    );
    assert_eq!(
        list["editor"]["serves_commands"],
        json!(true),
        "{list}\n{}",
        editor.binary
    );
    let _ = session.finish();
    drop(editor);
}

/// Wait until the journal holds no quarantined transaction.
fn writes_reopen(session: &mut StdioSession, ids: &mut Ids, editor: &Editor) {
    let started = Instant::now();
    loop {
        let health = ok(
            &session.call_tool(ids.next(), "health.get", json!({})),
            &editor.context(),
        );
        if health["journal"]["quarantined"]["total"] == json!(0) {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "{}: the editor's late answer never settled the call: {}\n{}",
            editor.binary,
            health["journal"],
            editor.context()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The add-on's own cases, kept beside the add-on: they drive the command
/// caller headless, with no editor and no server.
const COMMAND_CASES: &str =
    include_str!("../../godot-mcp-bridge/tests/fixtures/addon/project_commands_cases.gd");

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn the_command_cases_pass_against_the_shipped_add_on() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        the_command_cases_pass_against_the_shipped_add_on_in(binary);
    }
}

fn the_command_cases_pass_against_the_shipped_add_on_in(binary: &Path) {
    let tree = ProjectTree::new("live-command-cases");
    tree.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Command cases\"\n",
    );
    for file in godot_mcp_bridge::addon::FILES {
        let path = tree
            .root
            .join(godot_mcp_bridge::addon::ADDON_DIRECTORY)
            .join(file.name);
        std::fs::create_dir_all(path.parent().expect("add-on directory")).expect("add-on dir");
        std::fs::write(&path, file.bytes).expect("add-on file");
    }
    tree.write("project_commands_cases.gd", COMMAND_CASES);
    let mut run = Editor::launch_with(
        binary,
        &tree.root,
        &tree.base.join("home"),
        &[
            "--headless",
            "--script",
            "res://project_commands_cases.gd",
            "--path",
        ],
    );
    let status = run.wait(Duration::from_secs(120));
    let printed = run.output();
    let context = format!("{}\n{printed}", run.binary);
    let declared = COMMAND_CASES
        .lines()
        .skip_while(|line| !line.starts_with("const CASES := ["))
        .skip(1)
        .take_while(|line| !line.starts_with(']'))
        .count();
    assert!(declared > 0, "the cases runner declares its cases");
    let failures: Vec<&str> = printed
        .lines()
        .filter(|line| line.starts_with("commands_case FAIL"))
        .collect();
    assert!(failures.is_empty(), "{failures:#?}\n{context}");
    let passed = printed
        .lines()
        .filter(|line| line.starts_with("commands_case ok "))
        .count();
    assert_eq!(passed, declared, "every declared case ran\n{context}");
    // The one provider that does not compile is meant to; nothing else may
    // print an error, a warning or a leak.
    let noise: Vec<&str> = printed
        .lines()
        .filter(|line| {
            (line.contains("ERROR") || line.contains("WARNING") || line.contains("leaked"))
                && !line.contains("undefined_command_name")
                && !line.contains("broken_provider")
        })
        .collect();
    assert!(noise.is_empty(), "{noise:#?}\n{context}");
    assert!(status.success(), "the cases exit 0: {status}\n{context}");
}
