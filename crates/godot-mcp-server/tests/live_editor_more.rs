//! The editor tools contract 0.4.0 adds for cameras, script tabs, dialogs,
//! performance monitors, resource previews and restarts (ADR 0013), against
//! real Godot editors through the production MCP server. Every case is
//! `#[ignore]`d and runs in each editor `GODOT_MCP_TEST_EDITORS` or
//! `GODOT_MCP_LOCKED_ARTIFACT` names; the windowed preview case runs only with
//! `GODOT_MCP_DISPLAY_REQUIRED=1`, under a display such as `xvfb-run`.
//!
//! A restarted editor is not this test's child: each case that restarts one
//! finds it by the process id the restart answers, checks that it runs this
//! case's project, and kills it, so no editor is left behind.

#![cfg(target_os = "linux")]

#[path = "support/live.rs"]
mod live;
mod support;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use live::{Editor, Ids, editors, ok, project_with_addon, wait_connected};
use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, error_code, sha256_hex};

const BOX: &str =
    "[gd_resource type=\"BoxMesh\" format=3]\n\n[resource]\nsize = Vector3(1, 2, 1)\n";
const SCENE_3D: &str = "[gd_scene load_steps=2 format=3]\n\n\
    [ext_resource type=\"BoxMesh\" path=\"res://box.tres\" id=\"1\"]\n\n\
    [node name=\"Main3D\" type=\"Node3D\"]\n\n\
    [node name=\"Mesh\" type=\"MeshInstance3D\" parent=\".\"]\nmesh = ExtResource(\"1\")\n";
const SCENE_2D: &str = "[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node2D\"]\n\n\
    [node name=\"Block\" type=\"Polygon2D\" parent=\".\"]\n\
    color = Color(1, 0, 0, 1)\n\
    polygon = PackedVector2Array(0, 0, 400, 0, 400, 300, 0, 300)\n";
const SCRIPT: &str = "extends Node\n\nfunc hello() -> int:\n\treturn 1\n";
const OTHER_SCRIPT: &str = "extends Node\n";
const BROKEN: &str = "this is not a scene file\n[[[ broken\n";
const MISSING_DEPENDENCY: &str = "[gd_scene load_steps=2 format=3]\n\n\
    [ext_resource type=\"Texture2D\" path=\"res://does_not_exist.png\" id=\"1\"]\n\n\
    [node name=\"Missing\" type=\"Sprite2D\"]\ntexture = ExtResource(\"1\")\n";
const ICON: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"128\" height=\"128\" \
    viewBox=\"0 0 128 128\"><rect width=\"128\" height=\"128\" fill=\"#203040\"/><circle \
    cx=\"64\" cy=\"64\" r=\"40\" fill=\"#e04020\"/><rect x=\"10\" y=\"90\" width=\"50\" \
    height=\"20\" fill=\"#20e040\"/></svg>\n";
/// How an editor that can restart is started: headless through the display
/// driver option, which Godot passes on to the editor it restarts.
const RESTARTABLE: [&str; 6] = [
    "--display-driver",
    "headless",
    "--audio-driver",
    "Dummy",
    "--editor",
    "--path",
];
/// How long a case waits for a restarted editor's add-on to connect.
const RESTART_WAIT_MS: u64 = 180_000;

/// A project with the add-on installed and enabled, and a server on it that
/// runs scripts (`editor.script.run`), which a case uses to change what the
/// editor holds and to show dialogs of its own.
fn scripting_project(label: &str, files: &[(&str, &str)]) -> (ProjectTree, StdioSession, Ids) {
    let tree = ProjectTree::new(label);
    tree.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Live\"\n",
    );
    for (path, contents) in files {
        tree.write(path, contents);
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_qa-journal-foundation"));
    command
        .arg("--project")
        .arg(&tree.root)
        .arg("--profile")
        .arg("full")
        .arg("--allow-scripts")
        .env_remove("GODOT_MCP_TEST_LAUNCH_CRASH_STAGE");
    let mut session = StdioSession::handshake(StdioSession::start_command(command)).0;
    let mut ids = Ids(1);
    let installed = session.call_tool(
        ids.next(),
        "editor.addon.install",
        json!({ "enable": true, "idempotencyId": "live-install" }),
    );
    assert!(installed.get("error").is_none(), "{installed}");
    (tree, session, ids)
}

/// Run `body` as the `run()` function of an editor script, and answer its
/// result's literal.
fn run(session: &mut StdioSession, ids: &mut Ids, body: &str, editor: &Editor) -> String {
    let id = ids.next();
    let answer = ok(
        &session.call_tool(
            id,
            "editor.script.run",
            json!({
                "source": format!("extends RefCounted\n\nfunc run():\n{body}"),
                "idempotencyId": format!("live-run-{id}"),
            }),
        ),
        &editor.context(),
    );
    // Godot 4.4.1 captures no log, so a run that returned is `inconclusive`
    // there, and only its result tells it returned.
    assert!(
        ["passed", "inconclusive"].contains(&answer["classification"].as_str().unwrap_or_default())
            && answer["result_literal"].is_string(),
        "{}: the script ran and returned: {answer}",
        editor.binary
    );
    answer["result_literal"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn call(session: &mut StdioSession, ids: &mut Ids, tool: &str, arguments: Value) -> Value {
    session.call_tool(ids.next(), tool, arguments)
}

/// The text of a refusal with `code`, failing loudly on any other answer.
fn refused(response: &Value, code: &str, editor: &Editor) -> String {
    assert_eq!(
        error_code(response),
        json!(code),
        "{}: {response}",
        editor.binary
    );
    response.to_string()
}

fn status(session: &mut StdioSession, ids: &mut Ids, editor: &Editor) -> Value {
    ok(
        &session.call_tool(ids.next(), "editor.status.get", json!({})),
        &editor.context(),
    )
}

fn lists(status: &Value, method: &str) -> bool {
    status["methods"]
        .as_array()
        .expect("methods")
        .iter()
        .any(|listed| listed == method)
}

fn version_starts(status: &Value, prefixes: &[&str]) -> bool {
    let version = status["godot_version"]
        .as_str()
        .expect("the connected editor's Godot version");
    prefixes.iter().any(|prefix| version.starts_with(prefix))
}

fn near(point: &Value, expected: [f64; 3]) -> bool {
    ["x", "y", "z"].iter().zip(expected).all(|(axis, value)| {
        point[*axis]
            .as_f64()
            .is_some_and(|got| (got - value).abs() < 1.0e-3)
    })
}

// --- cameras ------------------------------------------------------------------------

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn the_editor_cameras_are_read_and_a_shown_3d_camera_is_moved() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        cameras_in(binary);
    }
}

fn cameras_in(binary: &Path) {
    let (tree, mut session, mut ids) = project_with_addon(
        "live-cameras",
        &[("main3d.tscn", SCENE_3D), ("box.tres", BOX)],
    );
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    let connected = wait_connected(&mut session, &mut ids, &editor);
    let mark = editor.mark();
    ok(
        &call(
            &mut session,
            &mut ids,
            "editor.scene.open",
            json!({ "path": "main3d.tscn", "idempotencyId": "cameras-open" }),
        ),
        &editor.context(),
    );
    ok(
        &call(
            &mut session,
            &mut ids,
            "editor.main_screen.set",
            json!({ "screen": "3D", "idempotencyId": "cameras-3d" }),
        ),
        &editor.context(),
    );
    let read = ok(
        &call(&mut session, &mut ids, "editor.camera.get", json!({})),
        &editor.context(),
    );
    let report = format!("{}: {read}", editor.binary);
    assert_eq!(read["main_screen"], "3D", "{report}");
    assert_eq!(read["2d"]["shown"], false, "{report}");
    assert!(read["2d"]["zoom"].is_number(), "{report}");
    let views = read["3d"].as_array().expect("four 3D viewports");
    assert_eq!(views.len(), 4, "{report}");
    for (index, view) in views.iter().enumerate() {
        assert_eq!(view["viewport"], index, "{report}");
        assert_eq!(view["camera"]["projection"], "perspective", "{report}");
        assert!(view["camera"]["fov"].is_number(), "{report}");
    }
    // The editor's default layout shows one 3D viewport.
    assert_eq!(views[0]["shown"], true, "{report}");
    assert_eq!(views[1]["shown"], false, "{report}");
    let one = ok(
        &call(
            &mut session,
            &mut ids,
            "editor.camera.get",
            json!({ "viewport": 0 }),
        ),
        &editor.context(),
    );
    assert_eq!(one["3d"].as_array().map(Vec::len), Some(1), "{one}");
    assert!(
        one.get("2d").is_none(),
        "one 3D viewport reads no 2D view: {one}"
    );
    let flat = ok(
        &call(
            &mut session,
            &mut ids,
            "editor.camera.get",
            json!({ "view": "2d" }),
        ),
        &editor.context(),
    );
    assert!(flat["2d"]["center"]["x"].is_number(), "{flat}");
    assert!(
        flat.get("3d").is_none(),
        "the 2D view reads no 3D viewport: {flat}"
    );

    let can_move = lists(&connected, "editor.camera.set");
    assert_eq!(
        can_move,
        !version_starts(&connected, &["4.4."]),
        "{}: cameras a plugin moves hold from Godot 4.5.1 on",
        editor.binary
    );
    let target = json!({ "x": 0, "y": 0, "z": 0 });
    let moved = call(
        &mut session,
        &mut ids,
        "editor.camera.set",
        json!({ "position": { "x": 6, "y": 4, "z": 8 }, "target": target, "idempotencyId": "cameras-move" }),
    );
    if !can_move {
        let text = refused(&moved, "unsupported", &editor);
        assert!(text.contains("4.5.1"), "{text}");
        editor.assert_no_errors_since(mark, "reading the cameras");
        let _ = session.finish();
        return;
    }
    let moved = ok(&moved, &editor.context());
    let length = (36.0_f64 + 16.0 + 64.0).sqrt();
    assert!(
        near(&moved["camera"]["position"], [6.0, 4.0, 8.0]),
        "{moved}"
    );
    assert!(
        near(
            &moved["camera"]["forward"],
            [-6.0 / length, -4.0 / length, -8.0 / length]
        ),
        "{moved}"
    );
    let again = ok(
        &call(
            &mut session,
            &mut ids,
            "editor.camera.get",
            json!({ "viewport": 0 }),
        ),
        &editor.context(),
    );
    assert!(
        near(&again["3d"][0]["camera"]["position"], [6.0, 4.0, 8.0]),
        "{}: the editor kept the camera where it was moved: {again}",
        editor.binary
    );
    // A viewport that is not shown keeps no camera moved in it.
    let hidden = call(
        &mut session,
        &mut ids,
        "editor.camera.set",
        json!({ "viewport": 1, "position": { "x": 1, "y": 1, "z": 1 }, "target": target, "idempotencyId": "cameras-hidden" }),
    );
    assert!(refused(&hidden, "conflict", &editor).contains("not shown"));
    ok(
        &call(
            &mut session,
            &mut ids,
            "editor.main_screen.set",
            json!({ "screen": "2D", "idempotencyId": "cameras-2d" }),
        ),
        &editor.context(),
    );
    let flat_screen = call(
        &mut session,
        &mut ids,
        "editor.camera.set",
        json!({ "position": { "x": 2, "y": 2, "z": 2 }, "target": target, "idempotencyId": "cameras-on-2d" }),
    );
    assert!(refused(&flat_screen, "conflict", &editor).contains("editor.main_screen.set"));
    let same = call(
        &mut session,
        &mut ids,
        "editor.camera.set",
        json!({ "position": target, "target": target, "idempotencyId": "cameras-same" }),
    );
    refused(&same, "invalid_input", &editor);
    editor.assert_no_errors_since(mark, "reading and moving the cameras");
    let _ = session.finish();
    drop(editor);
}

// --- script tabs --------------------------------------------------------------------

/// Type into the script editor's current tab, as a user would.
const TYPE_INTO_TAB: &str = "\tvar code = EditorInterface.get_script_editor().get_current_editor().get_base_editor()\n\
    \tcode.set_caret_line(0)\n\tcode.insert_text_at_caret(\"# typed\\n\")\n\treturn code.get_version() != code.get_saved_version()\n";
/// Take the typing back.
const UNDO_IN_TAB: &str = "\tvar code = EditorInterface.get_script_editor().get_current_editor().get_base_editor()\n\
    \tcode.undo()\n\treturn code.get_version() == code.get_saved_version()\n";

fn open_script_entry(status: &Value, path: &str) -> Option<Value> {
    status["editor"]["open_scripts"]
        .as_array()
        .expect("open scripts")
        .iter()
        .find(|entry| entry["path"] == path)
        .cloned()
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_script_tab_closes_unless_it_holds_edits() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        script_tabs_in(binary);
    }
}

fn script_tabs_in(binary: &Path) {
    let (tree, mut session, mut ids) = scripting_project(
        "live-script-tabs",
        &[("player.gd", SCRIPT), ("other.gd", OTHER_SCRIPT)],
    );
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    let connected = wait_connected(&mut session, &mut ids, &editor);
    let can_close = lists(&connected, "editor.script.close");
    assert_eq!(
        can_close,
        version_starts(&connected, &["4.7."]),
        "{}: closing a script tab needs ScriptEditor.close_file of Godot 4.7",
        editor.binary
    );
    ok(
        &call(
            &mut session,
            &mut ids,
            "editor.script.open",
            json!({ "path": "player.gd", "idempotencyId": "tabs-open" }),
        ),
        &editor.context(),
    );
    let mark = editor.mark();
    if !can_close {
        let text = refused(
            &call(
                &mut session,
                &mut ids,
                "editor.script.close",
                json!({ "path": "player.gd", "idempotencyId": "tabs-close-old" }),
            ),
            "unsupported",
            &editor,
        );
        assert!(text.contains("close_file"), "{text}");
        let after = status(&mut session, &mut ids, &editor);
        assert!(
            open_script_entry(&after, "res://player.gd").is_some(),
            "{after}"
        );
        editor.assert_no_errors_since(mark, "refusing to close a script tab");
        let _ = session.finish();
        return;
    }
    let not_open = call(
        &mut session,
        &mut ids,
        "editor.script.close",
        json!({ "path": "other.gd", "idempotencyId": "tabs-not-open" }),
    );
    refused(&not_open, "not_found", &editor);
    assert_eq!(run(&mut session, &mut ids, TYPE_INTO_TAB, &editor), "true");
    let typed = status(&mut session, &mut ids, &editor);
    assert_eq!(
        open_script_entry(&typed, "res://player.gd").expect("player.gd")["unsaved"],
        true,
        "{typed}"
    );
    let unsaved = call(
        &mut session,
        &mut ids,
        "editor.script.close",
        json!({ "path": "player.gd", "idempotencyId": "tabs-unsaved" }),
    );
    assert!(refused(&unsaved, "conflict", &editor).contains("discard"));
    let kept = status(&mut session, &mut ids, &editor);
    assert!(
        open_script_entry(&kept, "res://player.gd").is_some(),
        "{kept}"
    );
    assert_eq!(run(&mut session, &mut ids, UNDO_IN_TAB, &editor), "true");
    let closed = ok(
        &call(
            &mut session,
            &mut ids,
            "editor.script.close",
            json!({ "path": "player.gd", "idempotencyId": "tabs-close" }),
        ),
        &editor.context(),
    );
    assert_eq!(closed["closed"], "res://player.gd", "{closed}");
    assert!(
        !closed["open_scripts"]
            .as_array()
            .expect("open scripts")
            .iter()
            .any(|path| path == "res://player.gd"),
        "{closed}"
    );
    let after = status(&mut session, &mut ids, &editor);
    assert!(
        open_script_entry(&after, "res://player.gd").is_none(),
        "{after}"
    );
    assert_eq!(
        std::fs::read_to_string(tree.root.join("player.gd")).expect("player.gd"),
        SCRIPT,
        "closing writes nothing"
    );
    editor.assert_no_errors_since(mark, "closing a script tab");
    let _ = session.finish();
    drop(editor);
}

// --- dialogs ------------------------------------------------------------------------

fn dialogs(session: &mut StdioSession, ids: &mut Ids, editor: &Editor) -> Vec<Value> {
    ok(
        &call(session, ids, "editor.dialogs.get", json!({})),
        &editor.context(),
    )["dialogs"]
        .as_array()
        .expect("dialogs")
        .clone()
}

fn button_names(dialog: &Value) -> Vec<String> {
    dialog["buttons"]
        .as_array()
        .expect("buttons")
        .iter()
        .map(|button| button["name"].as_str().expect("name").to_owned())
        .collect()
}

/// Show two dialogs of the test's own: one with a custom button, and one whose
/// OK button is disabled.
const SHOW_DIALOGS: &str = "\tvar asking := ConfirmationDialog.new()\n\
    \tasking.title = \"Live question\"\n\tasking.dialog_text = \"Pick one\"\n\
    \tasking.add_button(\"Third\", false, \"third_action\")\n\tasking.exclusive = false\n\
    \tEditorInterface.get_base_control().add_child(asking)\n\tasking.popup_centered()\n\
    \tvar blocked := AcceptDialog.new()\n\tblocked.title = \"Live blocked\"\n\
    \tblocked.dialog_text = \"Cannot\"\n\tblocked.get_ok_button().disabled = true\n\tblocked.exclusive = false\n\
    \tEditorInterface.get_base_control().add_child(blocked)\n\tblocked.popup_centered()\n\
    \treturn [asking.visible, blocked.visible]\n";
const FREE_BLOCKED: &str = "\tfor node in EditorInterface.get_base_control().find_children(\"*\", \"AcceptDialog\", false, false):\n\
    \t\tif node.title == \"Live blocked\" or node.title == \"Live question\":\n\t\t\tnode.queue_free()\n\treturn true\n";

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn the_editors_dialogs_are_listed_and_answered_one_button_at_a_time() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        dialogs_in(binary);
    }
}

fn dialogs_in(binary: &Path) {
    let (tree, mut session, mut ids) = scripting_project(
        "live-dialogs",
        &[
            ("broken.tscn", BROKEN),
            ("missing_dep.tscn", MISSING_DEPENDENCY),
            ("main.tscn", SCENE_2D),
        ],
    );
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    let connected = wait_connected(&mut session, &mut ids, &editor);
    assert!(
        dialogs(&mut session, &mut ids, &editor).is_empty(),
        "{}: a fresh editor shows no dialog",
        editor.binary
    );

    // A dialog the editor opens itself: it cannot parse a scene it was asked
    // to open.
    let opened = call(
        &mut session,
        &mut ids,
        "editor.scene.open",
        json!({ "path": "broken.tscn", "idempotencyId": "dialogs-broken" }),
    );
    assert!(
        opened.get("error").is_some() || opened["result"]["isError"] == json!(true),
        "{}: a scene the editor cannot parse does not open: {opened}",
        editor.binary
    );
    let shown = dialogs(&mut session, &mut ids, &editor);
    let alert = shown
        .iter()
        .find(|dialog| {
            dialog["text"]
                .as_str()
                .is_some_and(|text| text.contains("broken.tscn"))
        })
        .unwrap_or_else(|| panic!("{}: the editor's alert is listed: {shown:?}", editor.binary))
        .clone();
    assert_eq!(alert["class"], "AcceptDialog", "{alert}");
    assert_eq!(button_names(&alert), ["ok"], "{alert}");
    let mark = editor.mark();
    let id = alert["id"].as_str().expect("id").to_owned();
    let pressed = ok(
        &call(
            &mut session,
            &mut ids,
            "editor.dialog.respond",
            json!({ "dialog": id, "button": "ok", "idempotencyId": "dialogs-alert-ok" }),
        ),
        &editor.context(),
    );
    assert_eq!(pressed["closed"], true, "{pressed}");
    assert!(
        !dialogs(&mut session, &mut ids, &editor)
            .iter()
            .any(|dialog| dialog["id"] == id),
        "{}: the alert closed",
        editor.binary
    );
    let stale = call(
        &mut session,
        &mut ids,
        "editor.dialog.respond",
        json!({ "dialog": id, "button": "ok", "idempotencyId": "dialogs-alert-again" }),
    );
    refused(&stale, "not_found", &editor);
    editor.assert_no_errors_since(mark, "answering the editor's alert");

    // A dialog that asks: the scene names a file that is gone.
    let missing = call(
        &mut session,
        &mut ids,
        "editor.scene.open",
        json!({ "path": "missing_dep.tscn", "idempotencyId": "dialogs-missing" }),
    );
    let shown = dialogs(&mut session, &mut ids, &editor);
    let asking = shown
        .iter()
        .find(|dialog| dialog["class"] == "DependencyErrorDialog")
        .unwrap_or_else(|| {
            panic!(
                "{}: the editor asks about the missing file: {missing}\n{shown:?}",
                editor.binary
            )
        })
        .clone();
    // Close and Open Anyway; Godot 4.4.1 and 4.5.1 also offer Fix
    // Dependencies, a custom button.
    let mut names = button_names(&asking);
    names.sort();
    let expected: &[&str] = if version_starts(&connected, &["4.4.", "4.5."]) {
        &["cancel", "fixdeps", "ok"]
    } else {
        &["cancel", "ok"]
    };
    assert_eq!(names, expected, "{}: {asking}", editor.binary);
    let id = asking["id"].as_str().expect("id").to_owned();
    let mark = editor.mark();
    let unknown = call(
        &mut session,
        &mut ids,
        "editor.dialog.respond",
        json!({ "dialog": id, "button": "nope", "idempotencyId": "dialogs-nope" }),
    );
    assert!(refused(&unknown, "not_found", &editor).contains("cancel"));
    let cancelled = ok(
        &call(
            &mut session,
            &mut ids,
            "editor.dialog.respond",
            json!({ "dialog": id, "button": "cancel", "idempotencyId": "dialogs-missing-cancel" }),
        ),
        &editor.context(),
    );
    assert_eq!(cancelled["closed"], true, "{cancelled}");
    let after = status(&mut session, &mut ids, &editor);
    assert_ne!(
        after["editor"]["edited_scene"], "res://missing_dep.tscn",
        "{}: cancelling did not open the scene: {after}",
        editor.binary
    );
    editor.assert_no_errors_since(mark, "cancelling the editor's question");

    // Dialogs of the test's own: a custom button, and a disabled one.
    assert_eq!(
        run(&mut session, &mut ids, SHOW_DIALOGS, &editor),
        "[true, true]"
    );
    let shown = dialogs(&mut session, &mut ids, &editor);
    let question = shown
        .iter()
        .find(|dialog| dialog["title"] == "Live question")
        .expect("the question is listed")
        .clone();
    let blocked = shown
        .iter()
        .find(|dialog| dialog["title"] == "Live blocked")
        .expect("the blocked dialog is listed")
        .clone();
    assert!(
        question["buttons"]
            .as_array()
            .expect("buttons")
            .iter()
            .any(|button| button["name"] == "third_action" && button["role"] == "custom"),
        "{question}"
    );
    assert_eq!(blocked["buttons"][0]["disabled"], true, "{blocked}");
    let limited = ok(
        &call(
            &mut session,
            &mut ids,
            "editor.dialogs.get",
            json!({ "limit": 1 }),
        ),
        &editor.context(),
    );
    assert_eq!(
        limited["dialogs"].as_array().map(Vec::len),
        Some(1),
        "{limited}"
    );
    assert_eq!(limited["truncated"], true, "{limited}");
    let mark = editor.mark();
    let third = ok(
        &call(
            &mut session,
            &mut ids,
            "editor.dialog.respond",
            json!({ "dialog": question["id"], "button": "third_action", "idempotencyId": "dialogs-third" }),
        ),
        &editor.context(),
    );
    assert_eq!(
        third["closed"], false,
        "a custom action leaves its dialog shown: {third}"
    );
    let disabled = call(
        &mut session,
        &mut ids,
        "editor.dialog.respond",
        json!({ "dialog": blocked["id"], "button": "ok", "idempotencyId": "dialogs-disabled" }),
    );
    assert!(refused(&disabled, "conflict", &editor).contains("disabled"));
    let closed = ok(
        &call(
            &mut session,
            &mut ids,
            "editor.dialog.respond",
            json!({ "dialog": question["id"], "button": "cancel", "idempotencyId": "dialogs-question-cancel" }),
        ),
        &editor.context(),
    );
    assert_eq!(closed["closed"], true, "{closed}");
    editor.assert_no_errors_since(mark, "answering dialogs of the test's own");
    run(&mut session, &mut ids, FREE_BLOCKED, &editor);
    let _ = session.finish();
    drop(editor);
}

// --- performance monitors -----------------------------------------------------------

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn the_editor_reports_its_own_performance_monitors() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        monitors_in(binary);
    }
}

fn monitors_in(binary: &Path) {
    let (tree, mut session, mut ids) = project_with_addon("live-monitors", &[]);
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    let mark = editor.mark();
    let all = ok(
        &call(&mut session, &mut ids, "editor.performance.get", json!({})),
        &editor.context(),
    );
    let monitors = all["monitors"].as_object().expect("monitors");
    assert!(
        monitors["OBJECT_NODE_COUNT"]
            .as_f64()
            .is_some_and(|nodes| nodes > 0.0),
        "{}: the editor holds nodes: {all}",
        editor.binary
    );
    assert!(monitors.contains_key("TIME_FPS"), "{all}");
    eprintln!("{}: {} performance monitors", editor.binary, monitors.len());
    let some = ok(
        &call(
            &mut session,
            &mut ids,
            "editor.performance.get",
            json!({ "monitors": ["OBJECT_NODE_COUNT", "TIME_PROCESS"] }),
        ),
        &editor.context(),
    );
    let mut named: Vec<&String> = some["monitors"]
        .as_object()
        .expect("monitors")
        .keys()
        .collect();
    named.sort();
    assert_eq!(named, ["OBJECT_NODE_COUNT", "TIME_PROCESS"], "{some}");
    let unknown = call(
        &mut session,
        &mut ids,
        "editor.performance.get",
        json!({ "monitors": ["NOT_A_MONITOR"] }),
    );
    assert!(refused(&unknown, "invalid_input", &editor).contains("NOT_A_MONITOR"));
    editor.assert_no_errors_since(mark, "reading the editor's monitors");
    let _ = session.finish();
    drop(editor);
}

// --- resource previews --------------------------------------------------------------

/// One preview artifact, read back through its resource and checked.
fn read_artifact(session: &mut StdioSession, ids: &mut Ids, artifact: &Value) -> Vec<u8> {
    let read = session.request(
        ids.next(),
        "resources/read",
        json!({ "uri": artifact["uri"] }),
    );
    let bytes = live::decode_base64(
        read["result"]["contents"][0]["blob"]
            .as_str()
            .unwrap_or_else(|| panic!("the artifact reads back: {read}")),
    );
    assert_eq!(
        bytes.len() as u64,
        artifact["byte_length"].as_u64().expect("length")
    );
    assert_eq!(
        sha256_hex(&bytes),
        artifact["sha256"].as_str().expect("digest")
    );
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "a PNG");
    let word = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().expect("four bytes"));
    assert_eq!(
        (json!(word(16)), json!(word(20))),
        (artifact["width"].clone(), artifact["height"].clone()),
        "the PNG is as large as the answer says"
    );
    bytes
}

fn scanned(session: &mut StdioSession, ids: &mut Ids, editor: &Editor) {
    ok(
        &call(
            session,
            ids,
            "editor.filesystem.scan",
            json!({ "idempotencyId": format!("previews-scan-{}", ids.0) }),
        ),
        &editor.context(),
    );
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_headless_editor_makes_no_previews_and_says_so() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        headless_previews_in(binary);
    }
}

fn headless_previews_in(binary: &Path) {
    let (tree, mut session, mut ids) =
        project_with_addon("live-previews-headless", &[("icon.svg", ICON)]);
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    scanned(&mut session, &mut ids, &editor);
    let mark = editor.mark();
    let none = call(
        &mut session,
        &mut ids,
        "editor.resource.preview",
        json!({ "path": "icon.svg", "idempotencyId": "previews-headless" }),
    );
    assert!(refused(&none, "unavailable", &editor).contains("headless"));
    let gone = call(
        &mut session,
        &mut ids,
        "editor.resource.preview",
        json!({ "path": "gone.png", "idempotencyId": "previews-gone" }),
    );
    refused(&gone, "not_found", &editor);
    editor.assert_no_errors_since(mark, "asking a headless editor for previews");
    let _ = session.finish();
    drop(editor);
}

#[test]
#[ignore = "needs real Godot editors and a display: GODOT_MCP_DISPLAY_REQUIRED=1 under xvfb-run"]
fn a_windowed_editor_previews_resources_as_artifacts() {
    if std::env::var("GODOT_MCP_DISPLAY_REQUIRED").as_deref() != Ok("1") {
        eprintln!("GODOT_MCP_DISPLAY_REQUIRED is not 1; windowed previews were not checked");
        return;
    }
    assert!(
        std::env::var_os("DISPLAY").is_some(),
        "GODOT_MCP_DISPLAY_REQUIRED=1 needs a display: run under xvfb-run"
    );
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        windowed_previews_in(binary);
    }
}

fn windowed_previews_in(binary: &Path) {
    let (tree, mut session, mut ids) = project_with_addon(
        "live-previews",
        &[
            ("icon.svg", ICON),
            ("box.tres", BOX),
            ("main.tscn", SCENE_2D),
        ],
    );
    let editor = Editor::launch_windowed(binary, &tree.root, &tree.base.join("home"));
    wait_connected(&mut session, &mut ids, &editor);
    scanned(&mut session, &mut ids, &editor);
    let mark = editor.mark();
    let icon = ok(
        &call(
            &mut session,
            &mut ids,
            "editor.resource.preview",
            json!({ "path": "icon.svg", "idempotencyId": "previews-icon" }),
        ),
        &editor.context(),
    );
    assert_eq!(icon["path"], "res://icon.svg", "{icon}");
    let preview = read_artifact(&mut session, &mut ids, &icon["preview"]);
    let thumbnail = read_artifact(&mut session, &mut ids, &icon["thumbnail"]);
    assert_ne!(preview, thumbnail, "{}: {icon}", editor.binary);
    let mesh = ok(
        &call(
            &mut session,
            &mut ids,
            "editor.resource.preview",
            json!({ "path": "box.tres", "idempotencyId": "previews-mesh" }),
        ),
        &editor.context(),
    );
    read_artifact(&mut session, &mut ids, &mesh["preview"]);
    let scene = call(
        &mut session,
        &mut ids,
        "editor.resource.preview",
        json!({ "path": "main.tscn", "idempotencyId": "previews-scene" }),
    );
    assert!(refused(&scene, "unavailable", &editor).contains("main.tscn"));
    let gone = call(
        &mut session,
        &mut ids,
        "editor.resource.preview",
        json!({ "path": "gone.png", "idempotencyId": "previews-gone" }),
    );
    refused(&gone, "not_found", &editor);
    editor.assert_no_errors_since(mark, "previewing resources");
    let _ = session.finish();
    drop(editor);
}

// --- restarts -----------------------------------------------------------------------

/// Whether process `pid` runs and is not a zombie.
fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
    })
}

/// The arguments process `pid` runs with, or none.
fn command_line(pid: u32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|raw| {
            raw.split(|byte| *byte == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// A restarted editor, killed and waited for when dropped.
struct Restarted {
    pid: u32,
}

impl Restarted {
    /// The editor the restart answered, once it is proven to be an editor of
    /// `project`: the restarted editor is no child of this test.
    fn found(pid: u32, project: &Path) -> Self {
        let arguments = command_line(pid);
        let project = project.display().to_string();
        assert!(
            arguments
                .iter()
                .any(|argument| argument.trim_end_matches('/') == project),
            "process {pid} is no editor of {project}: {arguments:?}"
        );
        Self { pid }
    }
}

impl Drop for Restarted {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg(self.pid.to_string())
            .status();
        let started = Instant::now();
        while alive(self.pid) && started.elapsed() < Duration::from_secs(30) {
            std::thread::sleep(Duration::from_millis(100));
        }
        if !std::thread::panicking() {
            assert!(
                !alive(self.pid),
                "the restarted editor {} is gone",
                self.pid
            );
        }
    }
}

fn restart(session: &mut StdioSession, ids: &mut Ids, arguments: Value) -> Value {
    let mut arguments = arguments;
    arguments["idempotencyId"] = json!(format!("live-restart-{}", ids.0));
    if arguments.get("timeoutMs").is_none() {
        arguments["timeoutMs"] = json!(RESTART_WAIT_MS);
    }
    call(session, ids, "editor.restart", arguments)
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn an_editor_started_with_headless_is_not_restarted() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        headless_restart_in(binary);
    }
}

fn headless_restart_in(binary: &Path) {
    let (tree, mut session, mut ids) = project_with_addon("live-restart-headless", &[]);
    let editor = Editor::launch(binary, &tree.root, &tree.base.join("home"));
    let before = wait_connected(&mut session, &mut ids, &editor);
    let mark = editor.mark();
    let response = restart(&mut session, &mut ids, json!({}));
    assert!(refused(&response, "unsupported", &editor).contains("--headless"));
    let after = status(&mut session, &mut ids, &editor);
    assert_eq!(after["connected"], true, "{after}");
    assert_eq!(
        after["sessions_opened"], before["sessions_opened"],
        "{after}"
    );
    editor.assert_no_errors_since(mark, "refusing to restart a --headless editor");
    let _ = session.finish();
    drop(editor);
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn an_editor_restarts_once_nothing_it_holds_can_be_lost() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        restart_in(binary);
    }
}

fn restart_in(binary: &Path) {
    let (tree, mut session, mut ids) =
        scripting_project("live-restart", &[("main.tscn", SCENE_2D)]);
    let mut editor = Editor::launch_with(binary, &tree.root, &tree.base.join("home"), &RESTARTABLE);
    let before = wait_connected(&mut session, &mut ids, &editor);
    ok(
        &call(
            &mut session,
            &mut ids,
            "editor.scene.open",
            json!({ "path": "main.tscn", "idempotencyId": "restart-open" }),
        ),
        &editor.context(),
    );
    let digest = live::digest_of(&mut session, &mut ids, &editor.context());
    ok(
        &call(
            &mut session,
            &mut ids,
            "editor.scene.edit",
            json!({
                "expectedDigest": digest,
                "operations": [{
                    "op": "set_property", "node": "Block", "property": "color",
                    "value": { "type": "color", "value": { "red": 0.0, "green": 0.0, "blue": 1.0, "alpha": 1.0 } }
                }],
                "idempotencyId": "restart-edit"
            }),
        ),
        &editor.context(),
    );
    let mark = editor.mark();
    let unsaved = restart(&mut session, &mut ids, json!({}));
    assert!(refused(&unsaved, "conflict", &editor).contains("main.tscn"));
    let still = status(&mut session, &mut ids, &editor);
    assert_eq!(
        still["sessions_opened"], before["sessions_opened"],
        "{still}"
    );
    editor.assert_no_errors_since(mark, "refusing to restart over an unsaved scene");

    let restarted = ok(
        &restart(&mut session, &mut ids, json!({ "save": true })),
        &editor.context(),
    );
    let pid = u32::try_from(restarted["pid"].as_u64().expect("pid")).expect("a process id");
    let _restarted = Restarted::found(pid, &tree.root);
    assert_eq!(restarted["restarted"], true, "{restarted}");
    assert_eq!(restarted["previous_pid"], editor.pid(), "{restarted}");
    assert_ne!(pid, editor.pid(), "{restarted}");
    assert!(
        restarted["saved"]
            .as_array()
            .expect("saved")
            .iter()
            .any(|scene| scene["path"] == "res://main.tscn"),
        "{restarted}"
    );
    let saved = std::fs::read_to_string(tree.root.join("main.tscn")).expect("main.tscn");
    assert!(
        saved.contains("Color(0, 0, 1, 1)"),
        "{}: the restart saved the scene first:\n{saved}",
        editor.binary
    );
    let exited = editor.wait(Duration::from_secs(60));
    assert!(
        exited.success(),
        "{}: the editor that restarted exited: {exited}",
        editor.binary
    );
    let after = status(&mut session, &mut ids, &editor);
    assert_eq!(after["connected"], true, "{after}");
    assert_eq!(
        after["editor"]["edited_scene"], "res://main.tscn",
        "{}: the restarted editor reopened the edited scene: {after}",
        editor.binary
    );
    let _ = session.finish();
}

/// A resource file of its own, which no scene holds.
const DATA: &str = "[gd_resource type=\"Resource\" format=3]\n\n[resource]\n";
/// Change a resource loaded from its own file, which the editor's global
/// history holds rather than a scene's or a script tab's: the editor then asks
/// whether to save "modified resources" before it reloads.
const GLOBAL_CHANGE: &str = "\tvar resource: Resource = load(\"res://data.tres\")\n\
    \tvar history := EditorInterface.get_editor_undo_redo()\n\
    \thistory.create_action(\"live: global change\", UndoRedo.MERGE_DISABLE, resource)\n\
    \thistory.add_do_property(resource, \"resource_name\", \"changed\")\n\
    \thistory.add_undo_property(resource, \"resource_name\", \"\")\n\
    \thistory.commit_action()\n\
    \treturn history.get_object_history_id(resource)\n";

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_question_the_editor_asks_before_restarting_is_cancelled() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        declined_restart_in(binary);
    }
}

fn declined_restart_in(binary: &Path) {
    let (tree, mut session, mut ids) =
        scripting_project("live-restart-declined", &[("data.tres", DATA)]);
    let editor = Editor::launch_with(binary, &tree.root, &tree.base.join("home"), &RESTARTABLE);
    let before = wait_connected(&mut session, &mut ids, &editor);
    // EditorUndoRedoManager.GLOBAL_HISTORY is 0.
    assert_eq!(run(&mut session, &mut ids, GLOBAL_CHANGE, &editor), "0");
    let mark = editor.mark();
    let response = restart(&mut session, &mut ids, json!({ "timeoutMs": 60_000 }));
    let text = refused(&response, "conflict", &editor);
    assert!(
        text.contains("modified resources"),
        "{}: the refusal quotes the editor's question: {text}",
        editor.binary
    );
    let after = status(&mut session, &mut ids, &editor);
    assert_eq!(after["connected"], true, "{after}");
    assert_eq!(
        after["sessions_opened"], before["sessions_opened"],
        "{after}"
    );
    assert_eq!(after["editor"]["restart"]["declined"], 1, "{after}");
    assert_eq!(after["editor"]["restart"]["pending"], false, "{after}");
    assert!(
        dialogs(&mut session, &mut ids, &editor).is_empty(),
        "{}: the question was cancelled",
        editor.binary
    );
    editor.assert_no_errors_since(mark, "cancelling the editor's question");
    let _ = session.finish();
    drop(editor);
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn a_script_tab_with_edits_restarts_only_once_saved() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        script_restart_in(binary);
    }
}

fn script_restart_in(binary: &Path) {
    let (tree, mut session, mut ids) =
        scripting_project("live-restart-script", &[("player.gd", SCRIPT)]);
    let mut editor = Editor::launch_with(binary, &tree.root, &tree.base.join("home"), &RESTARTABLE);
    let before = wait_connected(&mut session, &mut ids, &editor);
    let saves_scripts = version_starts(&before, &["4.7."]);
    ok(
        &call(
            &mut session,
            &mut ids,
            "editor.script.open",
            json!({ "path": "player.gd", "idempotencyId": "restart-script-open" }),
        ),
        &editor.context(),
    );
    assert_eq!(run(&mut session, &mut ids, TYPE_INTO_TAB, &editor), "true");
    let typed = status(&mut session, &mut ids, &editor);
    let reported =
        open_script_entry(&typed, "res://player.gd").expect("player.gd")["unsaved"].clone();
    eprintln!(
        "{}: a typed-into script tab reports unsaved: {reported}",
        editor.binary
    );
    assert_eq!(reported, true, "{}: {typed}", editor.binary);
    let unsaved = restart(&mut session, &mut ids, json!({ "timeoutMs": 60_000 }));
    assert!(refused(&unsaved, "conflict", &editor).contains("player.gd"));
    let saving = restart(&mut session, &mut ids, json!({ "save": true }));
    if !saves_scripts {
        assert!(refused(&saving, "unsupported", &editor).contains("save_all_scripts"));
        let after = status(&mut session, &mut ids, &editor);
        assert_eq!(
            after["sessions_opened"], before["sessions_opened"],
            "{after}"
        );
        let _ = session.finish();
        drop(editor);
        return;
    }
    let restarted = ok(&saving, &editor.context());
    let pid = u32::try_from(restarted["pid"].as_u64().expect("pid")).expect("a process id");
    let _restarted = Restarted::found(pid, &tree.root);
    assert_eq!(
        restarted["scripts_saved"],
        json!(["res://player.gd"]),
        "{restarted}"
    );
    assert!(
        std::fs::read_to_string(tree.root.join("player.gd"))
            .expect("player.gd")
            .contains("# typed"),
        "the restart saved the script first"
    );
    let exited = editor.wait(Duration::from_secs(60));
    assert!(exited.success(), "{}: {exited}", editor.binary);
    let _ = session.finish();
}

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn an_editor_this_server_runs_as_a_session_is_not_restarted() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        managed_restart_in(binary);
    }
}

fn managed_restart_in(binary: &Path) {
    let tree = ProjectTree::new("live-restart-managed");
    tree.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Live\"\n",
    );
    let mut session = StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
        godot: Some(binary),
    });
    let mut ids = Ids(1);
    let context = binary.display().to_string();
    ok(
        &call(
            &mut session,
            &mut ids,
            "editor.addon.install",
            json!({ "enable": true, "idempotencyId": "managed-install" }),
        ),
        &context,
    );
    let started = ok(
        &call(
            &mut session,
            &mut ids,
            "editor.session.start",
            json!({ "lifetimeSeconds": 600, "idempotencyId": "managed-start" }),
        ),
        &context,
    );
    let id = started["session"]["session"]
        .as_str()
        .expect("session id")
        .to_owned();
    let waited = Instant::now();
    loop {
        let now = ok(
            &call(&mut session, &mut ids, "editor.status.get", json!({})),
            &context,
        );
        if now["connected"] == json!(true) {
            break;
        }
        assert!(
            waited.elapsed() < Duration::from_secs(90),
            "{context}: the managed editor's add-on never connected: {now}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    let response = call(
        &mut session,
        &mut ids,
        "editor.restart",
        json!({ "timeoutMs": 5_000, "idempotencyId": "managed-restart" }),
    );
    assert_eq!(
        error_code(&response),
        json!("conflict"),
        "{context}: {response}"
    );
    let text = response.to_string();
    assert!(
        text.contains("managed session") && text.contains(&id),
        "{context}: {text}"
    );
    ok(
        &call(
            &mut session,
            &mut ids,
            "editor.session.stop",
            json!({ "session": id, "idempotencyId": "managed-stop" }),
        ),
        &context,
    );
    let _ = session.finish();
}
