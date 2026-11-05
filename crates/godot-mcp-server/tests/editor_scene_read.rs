//! `editor.scene.read` at the MCP surface: engine reads of the scene the
//! connected editor is editing.
//!
//! What a real editor answers is checked by the live suite
//! (`live_authoring.rs`); this pins what the server owns against a scripted
//! plugin: the calls it checks before the editor is asked, what it sends, how
//! each answered call becomes a tagged value with the diagnostics the editor
//! logged while it ran, and that an answer that does not match the calls asked
//! is never passed off as values.

#![cfg(target_os = "linux")]

mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use godot_mcp_server::test_support::fake_editor_plugin::{
    FakeAnswer, FakeEditorPlugin, FakePluginScript,
};
use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, error_code};

const METHOD: &str = "editor.scene.read";

fn rendezvous(tree: &ProjectTree) -> PathBuf {
    tree.root
        .join(".godot-mcp")
        .join("editor-bridge.rendezvous")
}

fn open(tree: &ProjectTree) -> StdioSession {
    StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "editor",
        godot: None,
    })
}

fn ok(response: &Value) -> Value {
    assert!(
        response.get("error").is_none() && response["result"]["isError"] != json!(true),
        "the call must succeed: {response}"
    );
    response["result"]["structuredContent"].clone()
}

fn wait_connected(session: &mut StdioSession, next_id: &mut u64) {
    let started = Instant::now();
    loop {
        *next_id += 1;
        let status = ok(&session.call_tool(*next_id, "editor.status.get", json!({})));
        if status["connected"] == json!(true) {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the plugin never connected: {status}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn sent(plugin: &FakeEditorPlugin) -> Vec<Value> {
    plugin
        .received()
        .into_iter()
        .filter(|(received, _)| received == METHOD)
        .map(|(_, payload)| payload)
        .collect()
}

/// A current plugin in a Godot 4.7.1 editor answering the read with `answer`.
fn answering(answer: Value) -> FakePluginScript {
    FakePluginScript::implementing(&["editor.state", METHOD])
        .versions("0.4.0", "4.7.1.stable.fake")
        .answering(METHOD, FakeAnswer::Payload(answer))
}

fn integer(value: i64) -> Value {
    json!({ "type": "integer", "value": value })
}

fn vector2i(x: i64, y: i64) -> Value {
    json!({ "type": "vector2i", "value": { "x": x, "y": y } })
}

const DIGEST: &str = "4f2b0000000000000000000000000000000000000000000000000000000000aa";

/// The three calls the answers below answer.
fn three_calls() -> Value {
    json!([
        { "node": "Layer", "method": "get_used_cells" },
        { "node": "Player", "resource": { "animation": "walk" },
          "method": "track_get_key_count", "arguments": [integer(7)] },
        { "node": "Player", "resource": { "animation": "run" }, "method": "get_length" }
    ])
}

/// What an editor answers for [`three_calls`]: a painted cell, an
/// out-of-range track whose engine error it logged, and a missing animation.
fn three_answers(logged_complete: bool) -> Value {
    json!({
        "scene": "res://level.tscn",
        "digest": DIGEST,
        "unsaved": true,
        "log_capture": logged_complete,
        "calls": [
            { "index": 0, "method": "get_used_cells", "literal": "[Vector2i(1, 2), Vector2i(3, 4)]",
              "logged": [], "logged_complete": logged_complete },
            { "index": 1, "method": "track_get_key_count", "literal": "-1",
              "logged": if logged_complete { json!([{
                  "sequence": 41, "unix_ms": 1, "kind": "error", "stderr": true,
                  "text": "Index p_track = 7 is out of bounds (tracks.size() = 1).",
                  "function": "track_get_key_count",
                  "file": "scene/resources/animation.cpp", "line": 3150,
                  "code": "Index p_track = 7 is out of bounds (tracks.size() = 1).",
                  "rationale": "", "backtrace": []
              }]) } else { json!([]) },
              "logged_complete": logged_complete },
            { "index": 2, "method": "get_length", "error_kind": "not_found",
              "detail": "no animation run", "logged": [], "logged_complete": logged_complete }
        ]
    })
}

#[test]
fn each_call_answers_a_tagged_value_and_what_the_editor_logged_while_it_ran() {
    let tree = ProjectTree::new("an-editor-read");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(&rendezvous(&tree), answering(three_answers(true)));
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let answer = ok(&session.call_tool(
        id,
        METHOD,
        json!({ "scene": "level.tscn", "calls": three_calls() }),
    ));

    assert_eq!(
        sent(&plugin),
        vec![json!({ "scene": "res://level.tscn", "calls": three_calls() })],
        "the editor is asked for exactly the calls, with the scene as a res:// path"
    );
    assert_eq!(answer["backend"], json!("editor"), "{answer}");
    assert_eq!(answer["scene"], json!("res://level.tscn"), "{answer}");
    assert_eq!(answer["digest"], json!(DIGEST), "{answer}");
    assert_eq!(answer["unsaved"], json!(true), "{answer}");
    let calls = answer["calls"].as_array().expect("one answer per call");
    assert_eq!(calls.len(), 3, "{answer}");

    assert_eq!(
        calls[0]["value"],
        json!({ "type": "array", "value": [vector2i(1, 2), vector2i(3, 4)] }),
        "{answer}"
    );
    assert_eq!(calls[0]["diagnostics"], json!([]), "{answer}");
    assert_eq!(calls[0]["diagnostics_complete"], json!(true), "{answer}");

    assert_eq!(calls[1]["value"], integer(-1), "{answer}");
    let raised = calls[1]["diagnostics"].as_array().expect("diagnostics");
    assert_eq!(raised.len(), 1, "the call's own engine error: {answer}");
    assert_eq!(raised[0]["severity"], json!("error"));
    assert_eq!(raised[0]["category"], json!("engine_reported"));
    assert_eq!(
        raised[0]["message"],
        json!("Index p_track = 7 is out of bounds (tracks.size() = 1).")
    );
    assert_eq!(
        raised[0]["engine_location"],
        json!("scene/resources/animation.cpp:3150"),
        "an engine source position is never a project location: {answer}"
    );
    assert_eq!(raised[0]["location"], Value::Null);
    assert_eq!(raised[0]["function"], json!("track_get_key_count"));

    assert_eq!(calls[2]["value"], Value::Null, "{answer}");
    assert_eq!(
        calls[2]["error"],
        json!({ "kind": "not_found", "detail": "no animation run" }),
        "{answer}"
    );
    for (index, call) in calls.iter().enumerate() {
        assert_eq!(call["index"], json!(index), "{answer}");
    }
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn an_editor_that_cannot_hand_over_its_log_says_each_call_may_have_raised_more() {
    let tree = ProjectTree::new("an-editor-read-without-log");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        answering(three_answers(false)).versions("0.4.0", "4.4.1.stable.official"),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let answer = ok(&session.call_tool(id, METHOD, json!({ "calls": three_calls() })));
    assert_eq!(
        sent(&plugin),
        vec![json!({ "scene": "", "calls": three_calls() })],
        "no scene asks for the scene being edited"
    );
    for call in answer["calls"].as_array().expect("calls") {
        assert_eq!(call["diagnostics"], json!([]), "{answer}");
        assert_eq!(
            call["diagnostics_complete"],
            json!(false),
            "nothing says the call raised nothing: {answer}"
        );
    }
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn calls_the_reading_table_does_not_allow_are_refused_before_the_editor_is_asked() {
    let tree = ProjectTree::new("an-editor-read-refused");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(&rendezvous(&tree), answering(three_answers(true)));
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    let too_many: Vec<Value> = (0..65)
        .map(|_| json!({ "method": "get_used_cells" }))
        .collect();
    let resource = json!({ "type": "resource",
        "value": { "resource": "res://a.tres", "resource_type": "Resource" } });
    for (arguments, what) in [
        (json!({}), "no calls"),
        (json!({ "calls": [] }), "an empty list of calls"),
        (json!({ "calls": too_many }), "65 calls"),
        (
            json!({ "calls": [{ "method": "set_cell" }] }),
            "an editing method",
        ),
        (
            json!({ "calls": [{ "method": "queue_free" }] }),
            "a method of no table",
        ),
        (
            json!({ "calls": [{ "node": "/root", "method": "get_used_cells" }] }),
            "an absolute node path",
        ),
        (
            json!({ "calls": [{ "node": "a/../b", "method": "get_used_cells" }] }),
            "a node path that leaves the scene",
        ),
        (
            json!({ "calls": [{ "method": "get_length",
                                "resource": { "property": "a", "animation": "b" } }] }),
            "two selectors",
        ),
        (
            json!({ "calls": [{ "method": "get_length", "arguments": [resource] }] }),
            "a resource argument",
        ),
        (
            json!({ "calls": [{ "method": "get_length", "extra": 1 }] }),
            "an unknown call field",
        ),
        (
            json!({ "calls": [{ "method": "get_length" }], "path": "a.tscn" }),
            "an unknown field",
        ),
        (
            json!({ "scene": "levels/one.tres", "calls": [{ "method": "get_length" }] }),
            "a scene that is not a .tscn",
        ),
    ] {
        id += 1;
        let response = session.call_tool(id, METHOD, arguments.clone());
        assert_eq!(
            error_code(&response),
            json!("invalid_input"),
            "{what} is refused: {response}"
        );
    }
    assert!(
        sent(&plugin).is_empty(),
        "nothing refused reached the editor"
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn an_answer_that_does_not_match_the_calls_asked_gives_no_values() {
    let mut short = three_answers(true);
    short["calls"].as_array_mut().expect("calls").pop();
    let mut reordered = three_answers(true);
    reordered["calls"][1]["method"] = json!("get_used_cells");
    let mut unlabelled = three_answers(true);
    unlabelled["calls"][0]
        .as_object_mut()
        .expect("a call")
        .remove("literal");
    let mut no_log_flag = three_answers(true);
    no_log_flag["calls"][2]
        .as_object_mut()
        .expect("a call")
        .remove("logged_complete");
    let mut no_digest = three_answers(true);
    no_digest
        .as_object_mut()
        .expect("an answer")
        .remove("digest");
    for (answer, what) in [
        (short, "an answer missing a call"),
        (reordered, "an answer naming another method"),
        (unlabelled, "a call answering neither a value nor why"),
        (
            no_log_flag,
            "a call that does not say whether its log is complete",
        ),
        (no_digest, "an answer without the scene's digest"),
    ] {
        let tree = ProjectTree::new("an-editor-read-malformed");
        let mut session = open(&tree);
        let plugin = FakeEditorPlugin::spawn(&rendezvous(&tree), answering(answer));
        let mut id = 1;
        wait_connected(&mut session, &mut id);
        id += 1;
        let response = session.call_tool(id, METHOD, json!({ "calls": three_calls() }));
        assert_eq!(
            error_code(&response),
            json!("internal"),
            "{what} is refused: {response}"
        );
        let _ = session.finish();
        plugin.join().expect("the plugin ended");
    }
}

#[test]
fn a_plugin_that_predates_the_read_says_to_update_it() {
    let tree = ProjectTree::new("an-editor-read-old-plugin");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state"]).versions("0.3.0", "4.7.1.stable.fake"),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let response = session.call_tool(id, METHOD, json!({ "calls": three_calls() }));
    assert_eq!(error_code(&response), json!("unsupported"), "{response}");
    let text = response.to_string();
    assert!(
        text.contains("0.3.0") && text.contains("editor.addon.update"),
        "the refusal names the plugin's version and the update: {response}"
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}
