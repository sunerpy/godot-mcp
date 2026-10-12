//! M5 acceptance: managed editor sessions and receipt-bound LSP/DAP commands
//! over the real stdio assembly.
//!
//! The production server runs out of process with the non-shipping
//! `qa-fake-game` binary as its Godot executable. Run with `--editor`, the
//! stand-in hosts a minimal GDScript language server and debug adapter on the
//! ports the server chose, ordered the way Godot's own servers are: the debug
//! adapter answers `launch` only after `configurationDone`, reports a
//! breakpoint stop, and answers the first `variables` read with `unknown` like
//! an editor still fetching the frame from its game. Every server-side path is
//! the shipped one: port allocation, attributing each listener to the editor
//! process, the receipt check before each connect, journaled commands and
//! stopping. Real Godot builds are exercised by
//! `scripts/real_client/m5_smoke.py`.
//!
//! Linux only: attributing a listener to the editor process reads `/proc`,
//! and these cases observe and end processes through it.
#![cfg(target_os = "linux")]

mod support;

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use support::{ProjectTree, Spawn, StdioSession, error_code};

const PLAYER: &str = "extends Node2D\n\nvar counter := 0\n\nfunc _ready() -> void:\n\tvar \
                      local_value := 41\n\tcounter = local_value + 1\n\tprint(\"ready \", \
                      counter)\n";
const BROKEN: &str = "extends Node\n\nfunc broken() -> void:\n\tvar x: int = \"not an int\"\n";

fn fake_godot() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_qa-fake-game"))
}

fn server(tree: &ProjectTree) -> StdioSession {
    StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
        godot: Some(fake_godot()),
    })
}

fn project(label: &str) -> ProjectTree {
    let tree = ProjectTree::new(label);
    tree.write("player.gd", PLAYER);
    tree.write("broken.gd", BROKEN);
    tree
}

fn structured(response: &Value) -> Value {
    assert!(
        response.get("error").is_none() && response["result"]["isError"] != json!(true),
        "the call must succeed: {response}"
    );
    response["result"]["structuredContent"].clone()
}

fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.split_whitespace()
            .nth(2)
            .is_none_or(|state| state != "Z")
    })
}

fn editor_pid(tree: &ProjectTree) -> u32 {
    let path = tree.root.join("fake-editor.pid");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(pid) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "the editor never recorded its pid"
        );
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

fn start_editor(session: &mut StdioSession, id: u64, idempotency: &str) -> String {
    let started = structured(&session.call_tool(
        id,
        "editor.session.start",
        json!({ "idempotencyId": idempotency }),
    ));
    assert_eq!(started["session"]["state"], json!("running"), "{started}");
    assert_eq!(started["session"]["kind"], json!("editor"), "{started}");
    let editor = started["session"]["session"]
        .as_str()
        .expect("session id")
        .to_owned();
    assert!(
        editor.starts_with("editor_") && editor.len() == 39,
        "{started}"
    );
    editor
}

fn command(session: &mut StdioSession, id: u64, editor: &str, name: &str, extra: Value) -> Value {
    let mut command = Map::new();
    command.insert(name.to_owned(), extra);
    session.call_tool(
        id,
        "debug.command",
        json!({
            "session": editor,
            "command": command,
            "idempotencyId": format!("debug-{id}-{name}"),
        }),
    )
}

#[test]
fn an_owned_editor_session_answers_language_and_debug_commands_through_its_receipt() {
    let tree = project("m5-debug");
    let mut session = server(&tree);

    // Nothing hosts either protocol before an editor session exists.
    let early = session.call_tool(2, "debug.session.get", json!({}));
    assert_eq!(error_code(&early), json!("unavailable"), "{early}");

    let editor = start_editor(&mut session, 3, "m5-editor");
    let pid = editor_pid(&tree);
    assert!(alive(pid), "the editor runs after start answered");

    // The same start, replayed, is the same session, not a second editor.
    let replayed = structured(&session.call_tool(
        4,
        "editor.session.start",
        json!({ "idempotencyId": "m5-editor" }),
    ));
    assert_eq!(replayed["session"]["session"], json!(editor));
    assert_eq!(replayed["replayed"], json!(true));

    let described =
        structured(&session.call_tool(5, "debug.session.get", json!({ "session": editor })));
    assert_eq!(
        described["language_server"]["receipt"]["owned"],
        json!(true),
        "{described}"
    );
    assert_eq!(
        described["debug_adapter"]["receipt"]["owned"],
        json!(true),
        "{described}"
    );
    assert_eq!(described["debug_adapter"]["connected"], json!(false));

    // Language server: engine diagnostics and document symbols.
    let diagnostics = structured(&command(
        &mut session,
        6,
        &editor,
        "diagnostics",
        json!({ "path": "broken.gd" }),
    ));
    assert_eq!(
        diagnostics["mutation_status"],
        json!("committed"),
        "{diagnostics}"
    );
    assert_eq!(diagnostics["path"], json!("res://broken.gd"));
    let found = diagnostics["diagnostics"].as_array().expect("diagnostics");
    assert_eq!(found.len(), 1, "{diagnostics}");
    assert_eq!(found[0]["line"], json!(4), "lines are one-based");
    assert!(
        found[0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("\"int\"")),
        "{diagnostics}"
    );
    let clean = structured(&command(
        &mut session,
        7,
        &editor,
        "diagnostics",
        json!({ "path": "res://player.gd" }),
    ));
    assert_eq!(clean["diagnostics"], json!([]), "a clean file reports none");
    let symbols = structured(&command(
        &mut session,
        8,
        &editor,
        "symbols",
        json!({ "path": "player.gd" }),
    ));
    let names: Vec<&str> = symbols["symbols"]
        .as_array()
        .expect("symbols")
        .iter()
        .filter_map(|symbol| symbol["name"].as_str())
        .collect();
    assert!(
        names.contains(&"_ready") && names.contains(&"counter"),
        "{symbols}"
    );
    let escaped = command(
        &mut session,
        9,
        &editor,
        "symbols",
        json!({ "path": "../outside.gd" }),
    );
    assert_eq!(error_code(&escaped), json!("invalid_input"), "{escaped}");

    // Debug adapter: breakpoint, launch, the stop, and what it shows.
    let breakpoints = structured(&command(
        &mut session,
        10,
        &editor,
        "breakpoints",
        json!({ "path": "player.gd", "lines": [7] }),
    ));
    assert_eq!(
        breakpoints["breakpoints"][0]["verified"],
        json!(true),
        "{breakpoints}"
    );
    assert_eq!(breakpoints["breakpoints"][0]["line"], json!(7));
    let launched = structured(&command(&mut session, 11, &editor, "launch", json!({})));
    assert_eq!(launched["launch_state"], json!("started"), "{launched}");
    let stopped = structured(&command(
        &mut session,
        12,
        &editor,
        "stopped",
        json!({ "timeoutMs": 10000 }),
    ));
    assert_eq!(
        stopped["stopped"]["reason"],
        json!("breakpoint"),
        "{stopped}"
    );
    let stack = structured(&command(&mut session, 13, &editor, "stack", json!({})));
    let top = &stack["frames"][0];
    assert_eq!(top["source"], json!("res://player.gd"), "{stack}");
    assert_eq!(top["line"], json!(7), "{stack}");
    let frame = top["id"].as_i64().expect("frame id");
    let scopes = structured(&command(
        &mut session,
        14,
        &editor,
        "scopes",
        json!({ "frame": frame }),
    ));
    let locals = scopes["scopes"]
        .as_array()
        .expect("scopes")
        .iter()
        .find(|scope| scope["name"] == json!("Locals"))
        .and_then(|scope| scope["reference"].as_i64())
        .expect("a Locals scope");
    let variables = structured(&command(
        &mut session,
        15,
        &editor,
        "variables",
        json!({ "reference": locals }),
    ));
    let local = variables["variables"]
        .as_array()
        .expect("variables")
        .iter()
        .find(|variable| variable["name"] == json!("local_value"))
        .cloned()
        .expect("local_value is a local");
    assert_eq!(local["value"], json!("41"), "{variables}");
    assert_eq!(local["type"], json!("int"));

    let resumed = structured(&command(&mut session, 16, &editor, "continue", json!({})));
    assert_eq!(resumed["continued"], json!(true), "{resumed}");
    let paused = structured(&command(&mut session, 17, &editor, "pause", json!({})));
    assert_eq!(paused["paused"], json!(true), "{paused}");
    let again = structured(&command(&mut session, 18, &editor, "stopped", json!({})));
    assert_eq!(again["stopped"]["reason"], json!("pause"), "{again}");

    let state =
        structured(&session.call_tool(19, "debug.session.get", json!({ "session": editor })));
    assert_eq!(state["debug_adapter"]["connected"], json!(true), "{state}");
    assert_eq!(state["debug_adapter"]["launch_state"], json!("started"));
    assert_eq!(
        state["debug_adapter"]["breakpoints"]["res://player.gd"],
        json!([7]),
        "{state}"
    );
    assert_eq!(state["language_server"]["connected"], json!(true));

    let ended = structured(&command(&mut session, 20, &editor, "stop", json!({})));
    assert_eq!(ended["stopped"], json!(true), "{ended}");
    let after =
        structured(&session.call_tool(21, "debug.session.get", json!({ "session": editor })));
    assert_eq!(after["debug_adapter"]["connected"], json!(false), "{after}");

    let transcript = session.transcript.clone();
    assert!(
        !transcript.contains(&tree.root.display().to_string()),
        "no answer names a private path"
    );

    let closed = structured(&session.call_tool(
        22,
        "editor.session.stop",
        json!({ "session": editor, "idempotencyId": "m5-editor-stop" }),
    ));
    assert_eq!(closed["state"], json!("exited"), "{closed}");
    assert_eq!(closed["exit"]["boundary_empty"], json!(true), "{closed}");
    wait_gone(pid);
    let late = command(
        &mut session,
        23,
        &editor,
        "symbols",
        json!({ "path": "player.gd" }),
    );
    assert_eq!(error_code(&late), json!("unavailable"), "{late}");
    let unknown = command(
        &mut session,
        24,
        &format!("editor_{}", "0".repeat(32)),
        "symbols",
        json!({ "path": "player.gd" }),
    );
    assert_eq!(error_code(&unknown), json!("not_found"), "{unknown}");
    let stopped_again = structured(&session.call_tool(
        25,
        "editor.session.stop",
        json!({ "session": editor, "idempotencyId": "m5-editor-stop-again" }),
    ));
    assert_eq!(stopped_again["already_ended"], json!(true));
    let _ = session.finish();
}

#[test]
fn a_listener_the_editor_does_not_hold_is_never_trusted() {
    // The stand-in leaves its language-server port to a helper process of its
    // own: something listens there, but not the editor this server launched.
    let tree = project("m5-squat");
    tree.write("fake-game.conf", "squat\n");
    let mut session = server(&tree);
    let refused = session.call_tool(
        2,
        "editor.session.start",
        json!({ "idempotencyId": "m5-squat" }),
    );
    assert_eq!(error_code(&refused), json!("unavailable"), "{refused}");
    assert!(
        refused.to_string().contains("not held by the editor"),
        "{refused}"
    );
    let listed = structured(&session.call_tool(3, "project.run.status", json!({})));
    let sessions = listed["sessions"].as_array().expect("sessions");
    assert_eq!(sessions.len(), 1, "{listed}");
    assert_eq!(sessions[0]["state"], json!("failed"), "{listed}");
    assert_eq!(
        sessions[0]["exit"]["boundary_empty"],
        json!(true),
        "{listed}"
    );
    let _ = session.finish();
}

/// The project's state directory is removed while an editor session runs, as
/// a test's temporary project is when the test fails part-way. The server can
/// no longer record anything, but its stop on shutdown still ends the editor
/// instead of leaving it running after the server is gone.
#[test]
fn an_editor_whose_state_directory_was_removed_still_ends_with_the_server() {
    let tree = project("m5-removed-state");
    let mut session = server(&tree);
    let _editor = start_editor(&mut session, 2, "m5-removed");
    let pid = editor_pid(&tree);
    std::fs::remove_dir_all(tree.root.join(".godot-mcp")).expect("remove the state directory");
    let stopping = Instant::now();
    let _ = session.finish();
    wait_gone(pid);
    assert!(
        stopping.elapsed() < Duration::from_secs(20),
        "the stop ended the editor; nothing waited out a timeout ({:?})",
        stopping.elapsed()
    );
}

#[test]
fn an_editor_that_died_is_never_dialled_again() {
    let tree = project("m5-dead-editor");
    let mut session = server(&tree);
    let editor = start_editor(&mut session, 2, "m5-dead");
    let pid = editor_pid(&tree);
    let symbols = structured(&command(
        &mut session,
        3,
        &editor,
        "symbols",
        json!({ "path": "player.gd" }),
    ));
    assert!(symbols["symbols"].is_array(), "{symbols}");
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(i32::try_from(pid).expect("pid")).expect("pid"),
        rustix::process::Signal::KILL,
    )
    .expect("kill the editor");
    wait_gone(pid);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let status =
            structured(&session.call_tool(4, "project.run.status", json!({ "session": editor })));
        if status["state"] == json!("exited") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the session never recorded the exit: {status}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let refused = command(
        &mut session,
        5,
        &editor,
        "symbols",
        json!({ "path": "player.gd" }),
    );
    assert_eq!(error_code(&refused), json!("unavailable"), "{refused}");
    let _ = session.finish();
}
