//! Contract 0.4 acceptance at the MCP surface: the editor's cameras, script
//! tabs, dialogs, performance monitors, resource previews and restart.
//!
//! What a real editor does is checked by the live-editor suite
//! (`live_editor_more.rs`); this pins what the server owns against a scripted
//! plugin: the arguments it checks before the editor is asked, what it sends,
//! how it says a Godot build or a plugin lacks what a tool needs, and how a
//! restart is proven by a different editor process connecting.

#![cfg(target_os = "linux")]

mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use godot_mcp_bridge::protocol::mutation::MutationStatus;
use godot_mcp_domain::ErrorKind;
use godot_mcp_server::test_support::fake_editor_plugin::{
    FakeAnswer, FakeEditorPlugin, FakePluginScript,
};
use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, error_code, sha256_hex};

/// Stand-in PNG bytes: the server checks length and digest, not the format.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake resource preview";

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

fn refused(response: &Value) -> bool {
    response.get("error").is_some() || response["result"]["isError"] == json!(true)
}

fn wait_connected(session: &mut StdioSession, next_id: &mut u64) -> Value {
    let started = Instant::now();
    loop {
        *next_id += 1;
        let status = ok(&session.call_tool(*next_id, "editor.status.get", json!({})));
        if status["connected"] == json!(true) {
            return status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the plugin never connected: {status}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn sent(plugin: &FakeEditorPlugin, method: &str) -> Vec<Value> {
    plugin
        .received()
        .into_iter()
        .filter(|(received, _)| received == method)
        .map(|(_, payload)| payload)
        .collect()
}

/// `payload` without the transaction id the server adds to every mutation.
fn without_transaction(payload: &Value) -> Value {
    let mut payload = payload.clone();
    if let Some(object) = payload.as_object_mut() {
        object.remove("transaction_id");
    }
    payload
}

fn plugin_of(tree: &ProjectTree, script: FakePluginScript) -> FakeEditorPlugin {
    FakeEditorPlugin::spawn(&rendezvous(tree), script)
}

/// A current plugin in a Godot 4.7.1 editor that implements `methods`.
fn current(methods: &[&str]) -> FakePluginScript {
    FakePluginScript::implementing(methods)
        .versions("0.4.0", "4.7.1.stable.fake")
        .describing("pid", json!(4242))
        .describing("camera_set", json!(true))
        .describing("script_close", json!(true))
}

/// Call `tool` with each of `cases` and require each to be refused as
/// invalid input without `method` being sent.
fn refuses_each(
    session: &mut StdioSession,
    id: &mut u64,
    plugin: &FakeEditorPlugin,
    tool: &str,
    method: &str,
    cases: &[(Value, &str)],
) {
    let before = sent(plugin, method).len();
    for (arguments, what) in cases {
        *id += 1;
        let response = session.call_tool(*id, tool, arguments.clone());
        assert_eq!(
            error_code(&response),
            json!("invalid_input"),
            "{tool}: {what} is refused: {response}"
        );
    }
    assert_eq!(
        sent(plugin, method).len(),
        before,
        "{tool}: nothing refused reached the editor"
    );
}

// --- cameras --------------------------------------------------------------------

#[test]
fn a_camera_read_names_the_views_it_reads() {
    let tree = ProjectTree::new("a-camera-read");
    let mut session = open(&tree);
    let plugin = plugin_of(
        &tree,
        current(&["editor.state", "editor.camera.read"]).answering(
            "editor.camera.read",
            FakeAnswer::Payload(json!({ "main_screen": "3D", "3d": [] })),
        ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    for (arguments, expected) in [
        (json!({}), json!({ "view": "", "viewport": -1 })),
        (
            json!({ "view": "2d" }),
            json!({ "view": "2d", "viewport": -1 }),
        ),
        (
            json!({ "view": "3d" }),
            json!({ "view": "3d", "viewport": -1 }),
        ),
        (
            json!({ "viewport": 2 }),
            json!({ "view": "3d", "viewport": 2 }),
        ),
        (
            json!({ "view": "3d", "viewport": 0 }),
            json!({ "view": "3d", "viewport": 0 }),
        ),
    ] {
        id += 1;
        let answer = ok(&session.call_tool(id, "editor.camera.get", arguments.clone()));
        assert_eq!(answer["main_screen"], "3D", "{answer}");
        let asked = sent(&plugin, "editor.camera.read");
        assert_eq!(
            asked.last().expect("asked"),
            &expected,
            "what {arguments} reads"
        );
    }
    refuses_each(
        &mut session,
        &mut id,
        &plugin,
        "editor.camera.get",
        "editor.camera.read",
        &[
            (
                json!({ "view": "2d", "viewport": 1 }),
                "a 3D viewport of the 2D view",
            ),
            (json!({ "viewport": 4 }), "a fifth 3D viewport"),
            (json!({ "viewport": -1 }), "a negative viewport"),
            (json!({ "view": "4d" }), "an unknown view"),
        ],
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_camera_is_moved_to_look_from_one_point_at_another() {
    let tree = ProjectTree::new("a-camera-set");
    let mut session = open(&tree);
    let plugin = plugin_of(
        &tree,
        current(&["editor.state", "editor.camera.set"]).answering(
            "editor.camera.set",
            FakeAnswer::Payload(json!({ "viewport": 0, "camera": { "shown": true } })),
        ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let moved = ok(&session.call_tool(
        id,
        "editor.camera.set",
        json!({
            "position": { "x": 6, "y": 4, "z": 8 },
            "target": { "x": 0, "y": 0, "z": 0 },
            "idempotencyId": "camera-1"
        }),
    ));
    assert_eq!(moved["mutation_status"], "committed", "{moved}");
    assert_eq!(
        without_transaction(&sent(&plugin, "editor.camera.set")[0]),
        json!({
            "viewport": 0,
            "position": { "x": 6.0, "y": 4.0, "z": 8.0 },
            "target": { "x": 0.0, "y": 0.0, "z": 0.0 }
        })
    );
    id += 1;
    ok(&session.call_tool(
        id,
        "editor.camera.set",
        json!({
            "viewport": 3,
            "position": { "x": 0, "y": 10, "z": 0 },
            "target": { "x": 0, "y": 0, "z": 0.5 },
            "idempotencyId": "camera-2"
        }),
    ));
    assert_eq!(sent(&plugin, "editor.camera.set")[1]["viewport"], json!(3));
    let point = json!({ "x": 1, "y": 2, "z": 3 });
    refuses_each(
        &mut session,
        &mut id,
        &plugin,
        "editor.camera.set",
        "editor.camera.set",
        &[
            (
                json!({ "position": point, "target": point, "idempotencyId": "same" }),
                "a camera looking at itself",
            ),
            (
                json!({ "position": point, "idempotencyId": "no-target" }),
                "no target",
            ),
            (
                json!({ "position": { "x": 1, "y": 2 }, "target": point, "idempotencyId": "2d" }),
                "a point without z",
            ),
            (
                json!({ "position": { "x": 1.0e10, "y": 2, "z": 3 }, "target": point, "idempotencyId": "far" }),
                "a point too far out",
            ),
            (
                json!({ "viewport": 4, "position": { "x": 0, "y": 0, "z": 0 }, "target": point, "idempotencyId": "fifth" }),
                "a fifth 3D viewport",
            ),
        ],
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_camera_the_editor_does_not_keep_is_uncertain() {
    let tree = ProjectTree::new("a-camera-drift");
    let mut session = open(&tree);
    let plugin = plugin_of(
        &tree,
        current(&["editor.state", "editor.camera.set"]).answering(
            "editor.camera.set",
            FakeAnswer::Error(ErrorKind::Uncertain, MutationStatus::Committed),
        ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let response = session.call_tool(
        id,
        "editor.camera.set",
        json!({
            "position": { "x": 6, "y": 4, "z": 8 },
            "target": { "x": 0, "y": 0, "z": 0 },
            "idempotencyId": "camera-drift"
        }),
    );
    assert_eq!(error_code(&response), json!("uncertain"), "{response}");
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn godot_4_4_1_moves_no_camera_and_says_why() {
    let tree = ProjectTree::new("a-camera-44");
    let mut session = open(&tree);
    let plugin = plugin_of(
        &tree,
        FakePluginScript::implementing(&["editor.state", "editor.camera.read"])
            .versions("0.4.0", "4.4.1.stable.official")
            .describing("pid", json!(4242))
            .describing("camera_set", json!(false)),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let response = session.call_tool(
        id,
        "editor.camera.set",
        json!({
            "position": { "x": 6, "y": 4, "z": 8 },
            "target": { "x": 0, "y": 0, "z": 0 },
            "idempotencyId": "camera-44"
        }),
    );
    assert_eq!(error_code(&response), json!("unsupported"), "{response}");
    let text = response.to_string();
    assert!(
        text.contains("4.4.1.stable.official") && text.contains("4.5.1"),
        "the refusal names the editor's Godot and the release whose cameras hold: {response}"
    );
    assert!(
        !text.contains("editor.addon.update"),
        "an update of the plugin would not help: {response}"
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

// --- script tabs ----------------------------------------------------------------

#[test]
fn a_script_tab_is_closed_by_its_res_path() {
    let tree = ProjectTree::new("a-script-close");
    let mut session = open(&tree);
    let plugin = plugin_of(
        &tree,
        current(&["editor.state", "editor.script.close"]).answering(
            "editor.script.close",
            FakeAnswer::Payload(json!({ "closed": "res://player.gd", "open_scripts": [] })),
        ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    for path in ["player.gd", "res://player.gd"] {
        id += 1;
        let closed = ok(&session.call_tool(
            id,
            "editor.script.close",
            json!({ "path": path, "idempotencyId": format!("close-{id}") }),
        ));
        assert_eq!(closed["closed"], "res://player.gd", "{closed}");
        assert_eq!(
            without_transaction(sent(&plugin, "editor.script.close").last().expect("sent")),
            json!({ "path": "res://player.gd" })
        );
    }
    refuses_each(
        &mut session,
        &mut id,
        &plugin,
        "editor.script.close",
        "editor.script.close",
        &[
            (
                json!({ "path": "main.tscn", "idempotencyId": "scene" }),
                "a scene",
            ),
            (
                json!({ "path": "../outside.gd", "idempotencyId": "outside" }),
                "a path outside the project",
            ),
            (
                json!({ "path": "", "idempotencyId": "empty" }),
                "an empty path",
            ),
            (json!({ "idempotencyId": "none" }), "no path"),
        ],
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn an_unsaved_script_tab_stays_open() {
    let tree = ProjectTree::new("a-script-unsaved");
    let mut session = open(&tree);
    let plugin = plugin_of(
        &tree,
        current(&["editor.state", "editor.script.close"]).answering(
            "editor.script.close",
            FakeAnswer::Error(ErrorKind::Conflict, MutationStatus::NotStarted),
        ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let response = session.call_tool(
        id,
        "editor.script.close",
        json!({ "path": "player.gd", "idempotencyId": "close-unsaved" }),
    );
    assert_eq!(error_code(&response), json!("conflict"), "{response}");
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn godot_before_4_7_1_closes_no_script_tab() {
    let tree = ProjectTree::new("a-script-46");
    let mut session = open(&tree);
    let plugin = plugin_of(
        &tree,
        FakePluginScript::implementing(&["editor.state"])
            .versions("0.4.0", "4.6.1.stable.official")
            .describing("pid", json!(4242))
            .describing("script_close", json!(false)),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let response = session.call_tool(
        id,
        "editor.script.close",
        json!({ "path": "player.gd", "idempotencyId": "close-46" }),
    );
    assert_eq!(error_code(&response), json!("unsupported"), "{response}");
    let text = response.to_string();
    assert!(
        text.contains("4.6.1.stable.official") && text.contains("close_file"),
        "the refusal names the editor's Godot and the API it lacks: {response}"
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

// --- dialogs ----------------------------------------------------------------------

fn dialog_plugin(respond: FakeAnswer) -> FakePluginScript {
    current(&[
        "editor.state",
        "editor.dialogs.read",
        "editor.dialog.respond",
    ])
    .answering(
        "editor.dialogs.read",
        FakeAnswer::Payload(json!({
            "dialogs": [{
                "id": "77",
                "class": "AcceptDialog",
                "title": "Alert!",
                "text": "Error while parsing file 'broken.tscn'.",
                "exclusive": true,
                "buttons": [{ "name": "ok", "role": "ok", "text": "OK", "disabled": false }]
            }],
            "truncated": false
        })),
    )
    .answering("editor.dialog.respond", respond)
}

#[test]
fn dialogs_are_listed_and_answered_one_button_at_a_time() {
    let tree = ProjectTree::new("a-dialogs");
    let mut session = open(&tree);
    let plugin = plugin_of(
        &tree,
        dialog_plugin(FakeAnswer::Payload(json!({
            "dialog": "77",
            "button": "ok",
            "closed": true,
            "dialogs": []
        }))),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    for (arguments, limit) in [(json!({}), 32), (json!({ "limit": 2 }), 2)] {
        id += 1;
        let listed = ok(&session.call_tool(id, "editor.dialogs.get", arguments));
        assert_eq!(listed["dialogs"][0]["title"], "Alert!", "{listed}");
        assert_eq!(
            sent(&plugin, "editor.dialogs.read").last().expect("asked"),
            &json!({ "limit": limit })
        );
    }
    refuses_each(
        &mut session,
        &mut id,
        &plugin,
        "editor.dialogs.get",
        "editor.dialogs.read",
        &[
            (json!({ "limit": 0 }), "no dialog at all"),
            (json!({ "limit": 65 }), "more dialogs than one read lists"),
        ],
    );
    id += 1;
    let pressed = ok(&session.call_tool(
        id,
        "editor.dialog.respond",
        json!({ "dialog": "77", "button": "ok", "idempotencyId": "press-1" }),
    ));
    assert_eq!(pressed["mutation_status"], "committed", "{pressed}");
    assert_eq!(pressed["closed"], json!(true), "{pressed}");
    assert_eq!(
        without_transaction(&sent(&plugin, "editor.dialog.respond")[0]),
        json!({ "dialog": "77", "button": "ok" })
    );
    refuses_each(
        &mut session,
        &mut id,
        &plugin,
        "editor.dialog.respond",
        "editor.dialog.respond",
        &[
            (
                json!({ "dialog": "abc", "button": "ok", "idempotencyId": "bad-id" }),
                "an id that is not a number",
            ),
            (
                json!({ "dialog": "0", "button": "ok", "idempotencyId": "zero" }),
                "the null object id",
            ),
            (
                json!({ "dialog": "77", "button": "", "idempotencyId": "no-button" }),
                "an empty button",
            ),
            (
                json!({ "dialog": "77", "button": "a\nb", "idempotencyId": "control" }),
                "a button with a control character",
            ),
            (
                json!({ "dialog": "77", "idempotencyId": "missing" }),
                "no button",
            ),
        ],
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_lost_dialog_answer_is_uncertain_and_blocks_writes() {
    let tree = ProjectTree::new("a-dialog-lost");
    let mut session = open(&tree);
    let plugin = plugin_of(&tree, dialog_plugin(FakeAnswer::Disconnect));
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let lost = session.call_tool(
        id,
        "editor.dialog.respond",
        json!({ "dialog": "77", "button": "ok", "idempotencyId": "press-lost" }),
    );
    assert_eq!(error_code(&lost), json!("uncertain"), "{lost}");
    plugin.join().expect("the plugin dropped its connection");
    id += 1;
    let blocked = session.call_tool(
        id,
        "file.create",
        json!({ "path": "notes/after.txt", "content": "x\n", "idempotencyId": "after-press" }),
    );
    assert!(
        refused(&blocked),
        "a button may have written any project file, so writes wait: {blocked}"
    );
    let _ = session.finish();
}

// --- performance ------------------------------------------------------------------

#[test]
fn the_editors_performance_monitors_are_read_by_name() {
    let tree = ProjectTree::new("a-performance");
    let mut session = open(&tree);
    let plugin = plugin_of(
        &tree,
        current(&["editor.state", "editor.performance.read"]).answering(
            "editor.performance.read",
            FakeAnswer::Payload(json!({ "monitors": { "TIME_FPS": 60.0 }, "custom": {} })),
        ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    for (arguments, expected) in [
        (json!({}), json!({ "monitors": null })),
        (
            json!({ "monitors": ["TIME_FPS", "OBJECT_NODE_COUNT"] }),
            json!({ "monitors": ["TIME_FPS", "OBJECT_NODE_COUNT"] }),
        ),
    ] {
        id += 1;
        let answer = ok(&session.call_tool(id, "editor.performance.get", arguments));
        assert_eq!(answer["monitors"]["TIME_FPS"], json!(60.0), "{answer}");
        assert_eq!(
            sent(&plugin, "editor.performance.read")
                .last()
                .expect("asked"),
            &expected
        );
    }
    let many: Vec<String> = (0..257).map(|index| format!("M{index}")).collect();
    refuses_each(
        &mut session,
        &mut id,
        &plugin,
        "editor.performance.get",
        "editor.performance.read",
        &[
            (json!({ "monitors": [""] }), "an empty name"),
            (
                json!({ "monitors": many }),
                "more names than monitors exist",
            ),
            (
                json!({ "monitors": "TIME_FPS" }),
                "a name that is not a list",
            ),
        ],
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

// --- resource previews --------------------------------------------------------------

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let word = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for position in 0..4 {
            if position <= chunk.len() {
                out.push(char::from(
                    ALPHABET[((word >> (18 - 6 * position)) & 63) as usize],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn preview_described(thumbnail: bool) -> Value {
    let frame = json!({ "byte_length": PNG.len(), "sha256": sha256_hex(PNG) });
    let mut frames = vec![frame.clone()];
    let mut images = vec![json!({ "role": "preview", "width": 64, "height": 64 })];
    if thumbnail {
        frames.push(frame);
        images.push(json!({ "role": "thumbnail", "width": 16, "height": 16 }));
    }
    json!({
        "capture_id": "capture-1",
        "path": "res://icon.svg",
        "frames": frames,
        "images": images
    })
}

fn preview_chunk() -> Value {
    json!({
        "capture_id": "capture-1",
        "frame": 0,
        "offset": 0,
        "length": PNG.len(),
        "total": PNG.len(),
        "data_base64": base64(PNG)
    })
}

fn previewing(described: FakeAnswer) -> FakePluginScript {
    current(&[
        "editor.state",
        "editor.preview.resource",
        "editor.capture.chunk",
    ])
    .answering("editor.preview.resource", described)
    .answering("editor.capture.chunk", FakeAnswer::Payload(preview_chunk()))
}

#[test]
fn a_preview_and_its_thumbnail_are_read_back_and_kept_as_artifacts() {
    let tree = ProjectTree::new("a-preview");
    let mut session = open(&tree);
    let plugin = plugin_of(
        &tree,
        previewing(FakeAnswer::Payload(preview_described(true))),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let answer = ok(&session.call_tool(
        id,
        "editor.resource.preview",
        json!({ "path": "icon.svg", "idempotencyId": "preview-1" }),
    ));
    assert_eq!(answer["mutation_status"], "committed", "{answer}");
    assert_eq!(answer["path"], "res://icon.svg", "{answer}");
    for (key, width) in [("preview", 64), ("thumbnail", 16)] {
        let artifact = &answer[key];
        assert_eq!(artifact["media_type"], "image/png", "{answer}");
        assert_eq!(artifact["sha256"], json!(sha256_hex(PNG)), "{answer}");
        assert_eq!(artifact["width"], json!(width), "{answer}");
        let uri = artifact["uri"].as_str().expect("artifact uri").to_owned();
        id += 1;
        let read = session.request(id, "resources/read", json!({ "uri": uri }));
        assert_eq!(
            read["result"]["contents"][0]["blob"],
            json!(base64(PNG)),
            "{read}"
        );
    }
    assert_eq!(
        answer["preview"]["uri"], answer["thumbnail"]["uri"],
        "two identical images are kept once: {answer}"
    );
    let asked = &sent(&plugin, "editor.preview.resource")[0];
    assert_eq!(
        (asked["path"].clone(), asked["timeout_ms"].clone()),
        (json!("res://icon.svg"), json!(10_000)),
        "{asked}"
    );
    assert!(
        asked.get("owner").is_none(),
        "the owner stays with the server: {asked}"
    );
    let chunks = sent(&plugin, "editor.capture.chunk");
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk["frame"].clone())
            .collect::<Vec<_>>(),
        vec![json!(0), json!(1)],
        "both images are read back: {chunks:?}"
    );
    refuses_each(
        &mut session,
        &mut id,
        &plugin,
        "editor.resource.preview",
        "editor.preview.resource",
        &[
            (
                json!({ "path": "", "idempotencyId": "empty" }),
                "an empty path",
            ),
            (
                json!({ "path": "../x.png", "idempotencyId": "outside" }),
                "a path outside the project",
            ),
            (
                json!({ "path": "a.png", "timeoutMs": 99, "idempotencyId": "short" }),
                "a deadline too short to wait for",
            ),
        ],
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_preview_without_a_thumbnail_keeps_one_artifact() {
    let tree = ProjectTree::new("a-preview-single");
    let mut session = open(&tree);
    let plugin = plugin_of(
        &tree,
        previewing(FakeAnswer::Payload(preview_described(false))),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let answer = ok(&session.call_tool(
        id,
        "editor.resource.preview",
        json!({ "path": "box.tres", "timeoutMs": 2000, "idempotencyId": "preview-2" }),
    ));
    assert_eq!(answer["preview"]["width"], json!(64), "{answer}");
    assert_eq!(answer["thumbnail"], Value::Null, "{answer}");
    assert_eq!(
        sent(&plugin, "editor.preview.resource")[0]["timeout_ms"],
        json!(2000)
    );
    assert_eq!(sent(&plugin, "editor.capture.chunk").len(), 1);
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn an_editor_that_makes_no_preview_says_so() {
    let tree = ProjectTree::new("a-preview-none");
    let mut session = open(&tree);
    let plugin = plugin_of(
        &tree,
        previewing(FakeAnswer::Error(
            ErrorKind::Unavailable,
            MutationStatus::NotStarted,
        )),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let response = session.call_tool(
        id,
        "editor.resource.preview",
        json!({ "path": "icon.svg", "idempotencyId": "preview-none" }),
    );
    assert_eq!(error_code(&response), json!("unavailable"), "{response}");
    assert!(sent(&plugin, "editor.capture.chunk").is_empty());
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

// --- restart ----------------------------------------------------------------------

fn restarting(pid: u64, restart: FakeAnswer, state: FakeAnswer) -> FakePluginScript {
    FakePluginScript::implementing(&["editor.state", "editor.restart"])
        .versions("0.4.0", "4.7.1.stable.fake")
        .describing("pid", json!(pid))
        .answering("editor.restart", restart)
        .answering("editor.state", state)
}

fn quiet_state() -> FakeAnswer {
    FakeAnswer::Payload(json!({
        "open_scenes": [],
        "open_scripts": [],
        "restart": { "declined": 0, "question": null }
    }))
}

#[test]
fn a_restart_is_proven_by_a_new_editor_process_connecting() {
    let tree = ProjectTree::new("a-restart");
    let mut session = open(&tree);
    let restarted = restarting(222, FakeAnswer::Payload(json!({})), quiet_state());
    let plugin = plugin_of(
        &tree,
        restarting(
            111,
            FakeAnswer::PayloadThenRedial {
                payload: json!({ "requested": true, "pid": 111 }),
                then: Some(Box::new(restarted)),
            },
            quiet_state(),
        ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let answer = ok(&session.call_tool(
        id,
        "editor.restart",
        json!({ "timeoutMs": 30_000, "idempotencyId": "restart-1" }),
    ));
    assert_eq!(answer["restarted"], json!(true), "{answer}");
    assert_eq!(
        (answer["previous_pid"].clone(), answer["pid"].clone()),
        (json!(111), json!(222)),
        "{answer}"
    );
    assert_eq!(answer["mutation_status"], "committed", "{answer}");
    let asked = &sent(&plugin, "editor.restart")[0];
    assert_eq!(
        without_transaction(asked),
        json!({ "save": false, "scenes": [] }),
        "{asked}"
    );
    let status = wait_connected(&mut session, &mut id);
    assert_eq!(status["connected"], json!(true), "{status}");
    // An exact retry answers from the journal and restarts nothing.
    id += 1;
    let replay = ok(&session.call_tool(
        id,
        "editor.restart",
        json!({ "timeoutMs": 30_000, "idempotencyId": "restart-1" }),
    ));
    assert_eq!(replay["replayed"], json!(true), "{replay}");
    assert_eq!(sent(&plugin, "editor.restart").len(), 1);
    let _ = session.finish();
    plugin.join().expect("the plugins ended");
}

#[test]
fn a_restart_no_new_editor_answers_is_uncertain() {
    let tree = ProjectTree::new("a-restart-silent");
    let mut session = open(&tree);
    let plugin = plugin_of(
        &tree,
        restarting(
            111,
            FakeAnswer::Payload(json!({ "requested": true, "pid": 111 })),
            quiet_state(),
        ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let response = session.call_tool(
        id,
        "editor.restart",
        json!({ "timeoutMs": 1000, "idempotencyId": "restart-silent" }),
    );
    assert_eq!(error_code(&response), json!("uncertain"), "{response}");
    let text = response.to_string();
    assert!(
        text.contains("1000 ms") && text.contains("111"),
        "the answer says how long it waited and which editor it asked: {response}"
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_restart_the_editor_declined_restarted_nothing() {
    let tree = ProjectTree::new("a-restart-declined");
    let mut session = open(&tree);
    let plugin = plugin_of(
        &tree,
        restarting(
            111,
            FakeAnswer::Payload(json!({ "requested": true, "pid": 111 })),
            FakeAnswer::After {
                method: "editor.restart".to_owned(),
                before: Box::new(quiet_state()),
                after: Box::new(FakeAnswer::Payload(json!({
                    "open_scenes": [],
                    "open_scripts": [],
                    "restart": {
                        "declined": 1,
                        "question": {
                            "title": "Please Confirm...",
                            "text": "Save modified resources before reloading?"
                        }
                    }
                }))),
            },
        ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let response = session.call_tool(
        id,
        "editor.restart",
        json!({ "timeoutMs": 30_000, "idempotencyId": "restart-declined" }),
    );
    assert_eq!(error_code(&response), json!("conflict"), "{response}");
    assert!(
        response
            .to_string()
            .contains("Save modified resources before reloading?"),
        "the refusal quotes what the editor asked: {response}"
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_declined_saving_restart_names_what_it_saved_first() {
    let tree = ProjectTree::new("a-restart-declined-save");
    tree.write(
        "a.tscn",
        "[gd_scene format=3]\n\n[node name=\"A\" type=\"Node\"]\n",
    );
    let mut session = open(&tree);
    let plugin = plugin_of(
        &tree,
        restarting(
            111,
            FakeAnswer::Payload(json!({
                "requested": true,
                "pid": 111,
                "saved": [{ "path": "res://a.tscn", "sha256": "0".repeat(64) }],
                "also_saved": [],
                "scripts_saved": ["res://player.gd"]
            })),
            FakeAnswer::After {
                method: "editor.restart".to_owned(),
                before: Box::new(FakeAnswer::Payload(json!({
                    "open_scenes": [{ "path": "res://a.tscn", "unsaved": true }],
                    "open_scripts": [],
                    "restart": { "declined": 0, "question": null }
                }))),
                after: Box::new(FakeAnswer::Payload(json!({
                    "open_scenes": [{ "path": "res://a.tscn", "unsaved": false }],
                    "open_scripts": [],
                    "restart": {
                        "declined": 1,
                        "question": {
                            "title": "Please Confirm...",
                            "text": "Save modified resources before reloading?"
                        }
                    }
                }))),
            },
        ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let response = session.call_tool(
        id,
        "editor.restart",
        json!({ "save": true, "timeoutMs": 30_000, "idempotencyId": "restart-declined-save" }),
    );
    assert_eq!(error_code(&response), json!("conflict"), "{response}");
    let text = response.to_string();
    assert!(
        text.contains("Save modified resources before reloading?"),
        "the refusal quotes what the editor asked: {response}"
    );
    assert!(
        text.contains("res://a.tscn") && text.contains("res://player.gd"),
        "the refusal names what the restart saved before the editor was asked: {response}"
    );
    assert!(
        !text.contains("nothing was saved"),
        "a restart that saved first does not say it saved nothing: {response}"
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_saving_restart_declares_every_open_scene_and_script() {
    let tree = ProjectTree::new("a-restart-save");
    let scene = "[gd_scene format=3]\n\n[node name=\"A\" type=\"Node\"]\n";
    tree.write("a.tscn", scene);
    tree.write("player.gd", "extends Node\n");
    let mut session = open(&tree);
    let state = FakeAnswer::Payload(json!({
        "open_scenes": [{ "path": "res://a.tscn" }],
        "open_scripts": [{ "path": "res://player.gd", "unsaved": true }],
        "restart": { "declined": 0, "question": null }
    }));
    let restarted = restarting(222, FakeAnswer::Payload(json!({})), quiet_state());
    let plugin = plugin_of(
        &tree,
        restarting(
            111,
            FakeAnswer::PayloadThenRedial {
                payload: json!({ "requested": true, "pid": 111, "saved": [] }),
                then: Some(Box::new(restarted)),
            },
            state,
        ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let answer = ok(&session.call_tool(
        id,
        "editor.restart",
        json!({ "save": true, "timeoutMs": 30_000, "idempotencyId": "restart-save" }),
    ));
    assert_eq!(answer["restarted"], json!(true), "{answer}");
    let asked = &sent(&plugin, "editor.restart")[0];
    assert_eq!(asked["save"], json!(true), "{asked}");
    assert_eq!(
        asked["scenes"],
        json!([{ "path": "res://a.tscn", "sha256": sha256_hex(scene.as_bytes()) }]),
        "{asked}"
    );
    let _ = session.finish();
    plugin.join().expect("the plugins ended");
}

#[test]
fn a_plugin_that_does_not_say_its_process_restarts_nothing() {
    let tree = ProjectTree::new("a-restart-nopid");
    let mut session = open(&tree);
    let plugin = plugin_of(
        &tree,
        FakePluginScript::implementing(&["editor.state", "editor.restart"])
            .versions("0.4.0", "4.7.1.stable.fake")
            .answering("editor.state", quiet_state()),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let response = session.call_tool(
        id,
        "editor.restart",
        json!({ "idempotencyId": "restart-nopid" }),
    );
    assert!(refused(&response), "{response}");
    assert_eq!(error_code(&response), json!("unavailable"), "{response}");
    assert!(sent(&plugin, "editor.restart").is_empty());
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_plugin_without_the_new_tools_says_how_to_update_it() {
    let tree = ProjectTree::new("a-old-plugin");
    let mut session = open(&tree);
    let plugin = plugin_of(&tree, FakePluginScript::implementing(&["editor.state"]));
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    for (tool, arguments) in [
        ("editor.camera.get", json!({})),
        ("editor.dialogs.get", json!({})),
        ("editor.performance.get", json!({})),
        (
            "editor.dialog.respond",
            json!({ "dialog": "7", "button": "ok", "idempotencyId": "old-1" }),
        ),
        ("editor.restart", json!({ "idempotencyId": "old-2" })),
        (
            "editor.resource.preview",
            json!({ "path": "icon.svg", "idempotencyId": "old-3" }),
        ),
    ] {
        id += 1;
        let response = session.call_tool(id, tool, arguments);
        assert_eq!(
            error_code(&response),
            json!("unsupported"),
            "{tool}: {response}"
        );
        assert!(
            response.to_string().contains("editor.addon.update"),
            "{tool}: the refusal says how to update the plugin: {response}"
        );
    }
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}
