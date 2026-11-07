//! `editor.script.run` (ADR 0012) against real Godot editors (`#[ignore]`d;
//! `GODOT_MCP_TEST_EDITORS` or `GODOT_MCP_LOCKED_ARTIFACT` names them): a run
//! reaches the open project and the editor's own API; what it returns, prints,
//! fails with and changes comes back; and a run that outlasts its deadline
//! keeps running in the editor after the server stopped waiting, and its late
//! answer settles it, on the same session or reported to the next server. The
//! add-on's runner cases run headless in each build too.

#[path = "support/live.rs"]
mod live;
mod support;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use live::{Editor, Ids, editors, ok, wait_connected};
use serde_json::{Value, json};
use support::{ProjectTree, StdioSession, error_code};

/// A project with the add-on installed and enabled, and a server on it that
/// runs scripts.
fn scripting_project(label: &str) -> (ProjectTree, StdioSession, Ids) {
    let tree = ProjectTree::new(label);
    tree.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Scripted\"\n",
    );
    let mut session = scripting_server(&tree);
    let mut ids = Ids(1);
    let installed = session.call_tool(
        ids.next(),
        "editor.addon.install",
        json!({ "enable": true, "idempotencyId": "live-install" }),
    );
    assert!(installed.get("error").is_none(), "{installed}");
    (tree, session, ids)
}

/// A server on `tree` that runs scripts.
fn scripting_server(tree: &ProjectTree) -> StdioSession {
    let mut command = Command::new(env!("CARGO_BIN_EXE_qa-journal-foundation"));
    command
        .arg("--project")
        .arg(&tree.root)
        .arg("--profile")
        .arg("full")
        .arg("--allow-scripts")
        .env_remove("GODOT_MCP_TEST_LAUNCH_CRASH_STAGE");
    StdioSession::handshake(StdioSession::start_command(command)).0
}

fn run(session: &mut StdioSession, ids: &mut Ids, arguments: Value, editor: &Editor) -> Value {
    ok(
        &session.call_tool(ids.next(), "editor.script.run", arguments),
        &editor.context(),
    )
}

fn source_lines(answer: &Value) -> Vec<u64> {
    answer["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .filter_map(|diagnostic| diagnostic["source_line"].as_u64())
        .collect()
}

fn added(answer: &Value) -> Vec<String> {
    answer["changes"]["added"]
        .as_array()
        .expect("added")
        .iter()
        .filter_map(|file| file["path"].as_str().map(str::to_owned))
        .collect()
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_run_reaches_the_open_project_and_says_what_it_did() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        a_run_reaches_the_open_project_and_says_what_it_did_in(binary);
    }
}

fn a_run_reaches_the_open_project_and_says_what_it_did_in(binary: &Path) {
    let (tree, mut session, mut ids) = scripting_project("live-editor-script");
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    let status = wait_connected(&mut session, &mut ids, &editor);
    assert!(
        status["methods"]
            .as_array()
            .expect("methods")
            .iter()
            .any(|method| method == "editor.script.run"),
        "{}: the session carries the method: {status}",
        editor.binary
    );

    let made = run(
        &mut session,
        &mut ids,
        json!({
            "source": "extends RefCounted\n\nfunc run():\n\tprint(\"from the editor\")\n\tvar file := FileAccess.open(\"res://made_in_editor.txt\", FileAccess.WRITE)\n\tfile.store_string(\"made\")\n\tfile.close()\n\treturn [Engine.is_editor_hint(), ProjectSettings.get_setting(\"application/config/name\"), EditorInterface.get_edited_scene_root() == null]\n",
            "idempotencyId": "made",
        }),
        &editor,
    );
    let report = format!("{made}\n{}", editor.binary);
    let captured = made["log_capture"] == json!(true);
    assert_eq!(
        made["classification"],
        json!(if captured { "passed" } else { "inconclusive" }),
        "{report}"
    );
    assert_eq!(
        made["result_literal"],
        json!("[true, \"Scripted\", true]"),
        "{report}"
    );
    assert_eq!(added(&made), ["made_in_editor.txt"], "{report}");
    assert_eq!(
        std::fs::read_to_string(tree.root.join("made_in_editor.txt")).expect("made"),
        "made"
    );
    assert_eq!(made["applied"], json!(true), "{report}");
    assert_eq!(made["rolled_back"], json!(false), "{report}");
    if captured {
        assert!(
            made["output"]
                .as_array()
                .expect("output")
                .iter()
                .any(|line| line["text"] == "from the editor"),
            "{report}"
        );
    }

    let node = run(
        &mut session,
        &mut ids,
        json!({
            "source": "extends Node\n\nfunc run():\n\tawait get_tree().process_frame\n\treturn is_inside_tree()\n",
            "idempotencyId": "node",
        }),
        &editor,
    );
    assert_eq!(
        node["result"],
        json!({ "type": "bool", "value": true }),
        "{node}\n{}",
        editor.binary
    );

    let compile = run(
        &mut session,
        &mut ids,
        json!({
            "source": "extends RefCounted\n\nfunc run():\n\treturn undefined_name\n",
            "idempotencyId": "compile",
        }),
        &editor,
    );
    let report = format!("{compile}\n{}", editor.binary);
    assert_eq!(
        compile["classification"],
        json!("compile_error"),
        "{report}"
    );
    assert_eq!(compile["changes"]["total"], json!(0), "{report}");
    if captured {
        assert!(source_lines(&compile).contains(&4), "{report}");
    }

    let failed = run(
        &mut session,
        &mut ids,
        json!({
            "source": "extends RefCounted\n\nfunc run():\n\tvar items := []\n\treturn items[3]\n",
            "idempotencyId": "runtime",
        }),
        &editor,
    );
    let report = format!("{failed}\n{}", editor.binary);
    if captured {
        assert_eq!(failed["classification"], json!("failed"), "{report}");
        assert!(source_lines(&failed).contains(&5), "{report}");
    } else {
        assert_eq!(failed["classification"], json!("inconclusive"), "{report}");
    }

    let replay = run(
        &mut session,
        &mut ids,
        json!({
            "source": "extends RefCounted\n\nfunc run():\n\tprint(\"from the editor\")\n\tvar file := FileAccess.open(\"res://made_in_editor.txt\", FileAccess.WRITE)\n\tfile.store_string(\"made\")\n\tfile.close()\n\treturn [Engine.is_editor_hint(), ProjectSettings.get_setting(\"application/config/name\"), EditorInterface.get_edited_scene_root() == null]\n",
            "idempotencyId": "made",
        }),
        &editor,
    );
    assert_eq!(
        replay["replayed"],
        json!(true),
        "{replay}\n{}",
        editor.binary
    );
    let _ = session.finish();
    drop(editor);
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_run_that_outlasts_its_deadline_keeps_running_in_the_editor() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        a_run_that_outlasts_its_deadline_keeps_running_in_the_editor_in(binary);
    }
}

fn a_run_that_outlasts_its_deadline_keeps_running_in_the_editor_in(binary: &Path) {
    let (tree, mut session, mut ids) = scripting_project("live-editor-script-late");
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    let before = wait_connected(&mut session, &mut ids, &editor);
    let started = Instant::now();
    let late = session.call_tool(
        ids.next(),
        "editor.script.run",
        json!({
            "source": "extends Node\n\nfunc run():\n\tawait get_tree().create_timer(15.0).timeout\n\tvar file := FileAccess.open(\"res://late.txt\", FileAccess.WRITE)\n\tfile.store_string(\"late\")\n\tfile.close()\n\treturn 1\n",
            "deadline_ms": 1000,
            "idempotencyId": "late",
        }),
    );
    let report = format!("{late}\n{}", editor.context());
    assert_eq!(error_code(&late), json!("uncertain"), "{report}");
    assert!(
        late.to_string().contains("may still be running"),
        "{report}"
    );
    // The server waits for the deadline and a two-second grace, then stops:
    // well before the run's 15 s timer and the bridge's 30 s default, with
    // room for a loaded host's journal writes.
    assert!(
        started.elapsed() < Duration::from_secs(12),
        "the server stopped waiting at the deadline: {:?}\n{report}",
        started.elapsed()
    );
    assert!(
        !tree.root.join("late.txt").exists(),
        "the run had not reached its write when the server stopped waiting"
    );
    // While the run's outcome is unknown, no write is allowed.
    let blocked = session.call_tool(
        ids.next(),
        "file.create",
        json!({ "path": "after.txt", "content": "x\n", "idempotencyId": "after-late" }),
    );
    assert!(
        blocked.get("error").is_some() || blocked["result"]["isError"] == json!(true),
        "a write after an unanswered run is refused: {blocked}"
    );
    assert!(!tree.root.join("after.txt").exists());
    // The editor goes on running the code the server stopped waiting for.
    let waiting = Instant::now();
    while !tree.root.join("late.txt").exists() {
        assert!(
            waiting.elapsed() < Duration::from_secs(60),
            "the run never finished in the editor\n{}",
            editor.context()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Its answer, on the same session, settles the run and reopens writes.
    writes_reopen(&mut session, &mut ids, &editor);
    let after = ok(
        &session.call_tool(ids.next(), "editor.status.get", json!({})),
        &editor.context(),
    );
    assert_eq!(
        after["sessions_opened"],
        before["sessions_opened"],
        "{}: the answer came on the session the run was sent on\n{}",
        editor.binary,
        editor.context()
    );
    ok(
        &session.call_tool(
            ids.next(),
            "file.create",
            json!({ "path": "after.txt", "content": "x\n", "idempotencyId": "after-settled" }),
        ),
        &editor.context(),
    );
    let replay = run(
        &mut session,
        &mut ids,
        json!({
            "source": "extends Node\n\nfunc run():\n\tawait get_tree().create_timer(15.0).timeout\n\tvar file := FileAccess.open(\"res://late.txt\", FileAccess.WRITE)\n\tfile.store_string(\"late\")\n\tfile.close()\n\treturn 1\n",
            "deadline_ms": 1000,
            "idempotencyId": "late",
        }),
        &editor,
    );
    assert_eq!(
        replay["replayed"],
        json!(true),
        "{}: {replay}",
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
            "{}: the editor's late answer never settled the run: {}\n{}",
            editor.binary,
            health["journal"],
            editor.context()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_run_that_outlasts_its_server_is_settled_by_the_next_one() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        a_run_that_outlasts_its_server_is_settled_by_the_next_one_in(binary);
    }
}

/// The server stops while a run it gave up on goes on in the editor. The next
/// server for the project holds the quarantine its predecessor left; the
/// editor connects to it, and when the run ends, reports the answer the first
/// server never received.
fn a_run_that_outlasts_its_server_is_settled_by_the_next_one_in(binary: &Path) {
    let (tree, mut session, mut ids) = scripting_project("live-editor-script-restart");
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    let started = Instant::now();
    let source = "extends Node\n\nfunc run():\n\tawait get_tree().create_timer(20.0).timeout\n\tvar file := FileAccess.open(\"res://restarted.txt\", FileAccess.WRITE)\n\tfile.store_string(\"restarted\")\n\tfile.close()\n\treturn 3\n";
    let arguments = json!({ "source": source, "deadline_ms": 1000, "idempotencyId": "restarted" });
    let late = session.call_tool(ids.next(), "editor.script.run", arguments.clone());
    assert_eq!(
        error_code(&late),
        json!("uncertain"),
        "{late}\n{}",
        editor.context()
    );
    let _ = session.finish();
    assert!(
        !tree.root.join("restarted.txt").exists(),
        "{}: the run was still going when the first server stopped",
        editor.binary
    );

    let mut session = scripting_server(&tree);
    let mut ids = Ids(1);
    writes_reopen(&mut session, &mut ids, &editor);
    let settled_after = started.elapsed();
    assert!(
        tree.root.join("restarted.txt").exists(),
        "{}: the run finished before its answer settled it",
        editor.binary
    );
    ok(
        &session.call_tool(
            ids.next(),
            "file.create",
            json!({ "path": "after.txt", "content": "x\n", "idempotencyId": "after-restarted" }),
        ),
        &editor.context(),
    );
    let replay = run(&mut session, &mut ids, arguments, &editor);
    assert_eq!(
        replay["replayed"],
        json!(true),
        "{}: {replay}",
        editor.binary
    );
    eprintln!(
        "{}: next server settled {:.1} s after the call",
        editor.binary,
        settled_after.as_secs_f64()
    );
    let _ = session.finish();
    drop(editor);
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_run_that_holds_the_editor_past_its_deadline_is_settled_by_its_answer() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        a_run_that_holds_the_editor_past_its_deadline_is_settled_by_its_answer_in(binary);
    }
}

/// A run holds the editor's main thread past its deadline, which is past the
/// bridge's 30-second silence window, so the editor answers nothing meanwhile.
/// When the server stops waiting it awaits nothing else on the session unless
/// it has just sent a changed status, so it usually closes the silent session;
/// the editor dials again once the run ends and reports the answer on the new
/// session. Either way the answer settles the run.
fn a_run_that_holds_the_editor_past_its_deadline_is_settled_by_its_answer_in(binary: &Path) {
    let (tree, mut session, mut ids) = scripting_project("live-editor-script-silent");
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    let before = wait_connected(&mut session, &mut ids, &editor);
    let started = Instant::now();
    let source = "extends Node\n\nfunc run():\n\tOS.delay_msec(40000)\n\tvar file := FileAccess.open(\"res://silent.txt\", FileAccess.WRITE)\n\tfile.store_string(\"silent\")\n\tfile.close()\n\treturn 2\n";
    let arguments = json!({ "source": source, "deadline_ms": 29000, "idempotencyId": "silent" });
    let late = session.call_tool(ids.next(), "editor.script.run", arguments.clone());
    assert_eq!(
        error_code(&late),
        json!("uncertain"),
        "{late}\n{}",
        editor.context()
    );
    assert!(
        !tree.root.join("silent.txt").exists(),
        "{}: the run was still holding the editor when the server stopped waiting",
        editor.binary
    );
    writes_reopen(&mut session, &mut ids, &editor);
    let settled_after = started.elapsed();
    assert!(
        tree.root.join("silent.txt").exists(),
        "{}: the run finished before its answer settled it",
        editor.binary
    );
    let after = wait_connected(&mut session, &mut ids, &editor);
    let reconnected = after["sessions_opened"].as_u64() > before["sessions_opened"].as_u64();
    if reconnected {
        assert_eq!(
            after["last_close_reason"],
            json!("the plugin went silent"),
            "{}: {after}",
            editor.binary
        );
    }
    ok(
        &session.call_tool(
            ids.next(),
            "file.create",
            json!({ "path": "after.txt", "content": "x\n", "idempotencyId": "after-silent" }),
        ),
        &editor.context(),
    );
    let replay = run(&mut session, &mut ids, arguments, &editor);
    assert_eq!(
        replay["replayed"],
        json!(true),
        "{}: {replay}",
        editor.binary
    );
    eprintln!(
        "{}: settled {:.1} s after the call, on {} session",
        editor.binary,
        settled_after.as_secs_f64(),
        if reconnected { "the next" } else { "the same" }
    );
    let _ = session.finish();
    drop(editor);
}

/// The add-on runner's own cases, kept beside the add-on: they drive the
/// runner headless, with no editor and no server.
const RUNNER_CASES: &str =
    include_str!("../../godot-mcp-bridge/tests/fixtures/addon/script_runner_cases.gd");

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn the_runner_cases_pass_against_the_shipped_add_on() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        the_runner_cases_pass_against_the_shipped_add_on_in(binary);
    }
}

fn the_runner_cases_pass_against_the_shipped_add_on_in(binary: &Path) {
    let tree = ProjectTree::new("live-runner-cases");
    tree.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Runner cases\"\n",
    );
    for file in godot_mcp_bridge::addon::FILES {
        let path = tree
            .root
            .join(godot_mcp_bridge::addon::ADDON_DIRECTORY)
            .join(file.name);
        std::fs::create_dir_all(path.parent().expect("add-on directory")).expect("add-on dir");
        std::fs::write(&path, file.bytes).expect("add-on file");
    }
    tree.write("script_runner_cases.gd", RUNNER_CASES);
    let mut run = Editor::launch_with(
        binary,
        &tree.root,
        &tree.base.join("home"),
        &[
            "--headless",
            "--script",
            "res://script_runner_cases.gd",
            "--path",
        ],
    );
    let status = run.wait(Duration::from_secs(120));
    let printed = run.output();
    let context = format!("{}\n{printed}", run.binary);
    let declared = RUNNER_CASES
        .lines()
        .skip_while(|line| !line.starts_with("const CASES := ["))
        .skip(1)
        .take_while(|line| !line.starts_with(']'))
        .count();
    assert!(declared > 0, "the cases runner declares its cases");
    let failures: Vec<&str> = printed
        .lines()
        .filter(|line| line.starts_with("runner_case FAIL"))
        .collect();
    assert!(failures.is_empty(), "{failures:#?}\n{context}");
    let passed = printed
        .lines()
        .filter(|line| line.starts_with("runner_case ok "))
        .count();
    assert_eq!(passed, declared, "every declared case ran\n{context}");
    // The one source that does not compile is meant to; nothing else may
    // print an error, a warning or a leak.
    let noise: Vec<&str> = printed
        .lines()
        .filter(|line| {
            (line.contains("ERROR") || line.contains("WARNING") || line.contains("leaked"))
                && !line.contains("undefined_name")
        })
        .collect();
    assert!(noise.is_empty(), "{noise:#?}\n{context}");
    assert!(status.success(), "the cases exit 0: {status}\n{context}");
}
