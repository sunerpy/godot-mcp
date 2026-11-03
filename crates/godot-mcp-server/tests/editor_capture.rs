//! Contract 0.3 acceptance at the MCP surface: switching the editor's main
//! screen and capturing what it shows.
//!
//! A screenshot's effect is its artifact: the plugin captures, the server
//! reads the PNG back in bounded chunks, checks it against the length and
//! SHA-256 the plugin reported and registers it, owned by the caller, in the
//! artifact store. What a real editor draws is checked by the live-editor
//! suite, which needs a display.

#![cfg(target_os = "linux")]

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use godot_mcp_server::test_support::fake_editor_plugin::{
    FakeAnswer, FakeEditorPlugin, FakePluginScript,
};
use serde_json::{Value, json};
use support::{ProjectTree, Spawn, StdioSession, error_code, sha256_hex};

/// Stand-in PNG bytes: the server checks length and digest, not the format.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake editor screenshot";

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

fn sent(plugin: &FakeEditorPlugin, method: &str) -> Vec<Value> {
    plugin
        .received()
        .into_iter()
        .filter(|(received, _)| received == method)
        .map(|(_, payload)| payload)
        .collect()
}

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

fn described(bytes: &[u8]) -> Value {
    json!({
        "capture_id": "capture-1",
        "width": 4,
        "height": 3,
        "view": "2d",
        "frames": [{ "byte_length": PNG.len(), "sha256": sha256_hex(bytes) }]
    })
}

fn chunk(bytes: &[u8]) -> Value {
    json!({
        "capture_id": "capture-1",
        "frame": 0,
        "offset": 0,
        "length": bytes.len(),
        "total": bytes.len(),
        "data_base64": base64(bytes)
    })
}

/// Every artifact contract the server keeps on disk.
fn stored_contracts(tree: &ProjectTree) -> Vec<Value> {
    let directory = tree.root.join(".godot-mcp").join("artifacts");
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .map(|path: PathBuf| {
            serde_json::from_slice(&std::fs::read(Path::new(&path)).expect("contract"))
                .expect("contract JSON")
        })
        .collect()
}

#[test]
fn a_screenshot_is_read_back_checked_and_kept_as_the_callers_artifact() {
    let tree = ProjectTree::new("m8-capture");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&[
            "editor.state",
            "editor.capture.viewport",
            "editor.capture.chunk",
        ])
        .answering(
            "editor.capture.viewport",
            FakeAnswer::Payload(described(PNG)),
        )
        .answering("editor.capture.chunk", FakeAnswer::Payload(chunk(PNG))),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);

    id += 1;
    let shot = ok(&session.call_tool(
        id,
        "editor.screenshot.capture",
        json!({ "view": "2d", "maxWidth": 640, "idempotencyId": "shot-1" }),
    ));
    assert_eq!(shot["mutation_status"], "committed", "{shot}");
    assert_eq!(shot["view"], "2d", "{shot}");
    let artifact = &shot["artifact"];
    assert_eq!(artifact["media_type"], "image/png", "{shot}");
    assert_eq!(artifact["byte_length"], json!(PNG.len()), "{shot}");
    assert_eq!(artifact["sha256"], json!(sha256_hex(PNG)), "{shot}");
    assert_eq!(
        (artifact["width"].clone(), artifact["height"].clone()),
        (json!(4), json!(3))
    );

    let asked = &sent(&plugin, "editor.capture.viewport")[0];
    assert_eq!(
        (
            asked["view"].clone(),
            asked["viewport"].clone(),
            asked["max_width"].clone()
        ),
        (json!("2d"), json!(0), json!(640)),
        "{asked}"
    );
    assert!(
        asked.get("owner").is_none(),
        "the owner stays with the server: {asked}"
    );

    let uri = artifact["uri"].as_str().expect("artifact uri").to_owned();
    assert!(uri.starts_with("godot-mcp://artifacts/"), "{uri}");
    id += 1;
    let read = session.request(id, "resources/read", json!({ "uri": uri }));
    assert_eq!(
        read["result"]["contents"][0]["blob"],
        json!(base64(PNG)),
        "{read}"
    );
    assert_eq!(
        read["result"]["contents"][0]["mimeType"], "image/png",
        "{read}"
    );
    let contracts = stored_contracts(&tree);
    assert_eq!(contracts.len(), 1, "{contracts:?}");
    assert_eq!(
        contracts[0]["owner"]["principal"], "principal_0123456789abcdef",
        "the artifact belongs to the caller: {contracts:?}"
    );

    // An exact retry answers from the journal and captures nothing new.
    id += 1;
    let replay = ok(&session.call_tool(
        id,
        "editor.screenshot.capture",
        json!({ "view": "2d", "maxWidth": 640, "idempotencyId": "shot-1" }),
    ));
    assert_eq!(replay["replayed"], json!(true), "{replay}");
    assert_eq!(sent(&plugin, "editor.capture.viewport").len(), 1);

    for (arguments, what) in [
        (
            json!({ "view": "4d", "idempotencyId": "shot-bad-view" }),
            "an unknown view",
        ),
        (
            json!({ "viewport": 4, "idempotencyId": "shot-bad-port" }),
            "a fifth 3D viewport",
        ),
        (
            json!({ "maxWidth": 0, "idempotencyId": "shot-bad-width" }),
            "a zero width",
        ),
    ] {
        id += 1;
        let refused = session.call_tool(id, "editor.screenshot.capture", arguments);
        assert!(
            refused.get("error").is_some() || refused["result"]["isError"] == json!(true),
            "{what} is refused: {refused}"
        );
    }
    assert_eq!(sent(&plugin, "editor.capture.viewport").len(), 1);
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_capture_that_does_not_match_its_digest_keeps_nothing() {
    let tree = ProjectTree::new("m8-capture-bad");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&[
            "editor.state",
            "editor.capture.viewport",
            "editor.capture.chunk",
        ])
        .answering(
            "editor.capture.viewport",
            FakeAnswer::Payload(described(PNG)),
        )
        .answering(
            "editor.capture.chunk",
            FakeAnswer::Payload(chunk(b"\x89PNG\r\n\x1a\nother editor screenshot")),
        ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let refused = session.call_tool(
        id,
        "editor.screenshot.capture",
        json!({ "idempotencyId": "shot-bad" }),
    );
    assert!(
        refused.get("error").is_some() || refused["result"]["isError"] == json!(true),
        "{refused}"
    );
    assert_eq!(error_code(&refused), json!("internal"), "{refused}");
    assert!(
        refused
            .to_string()
            .contains("did not arrive as the peer described it"),
        "the capture is refused for its bytes: {refused}"
    );
    assert!(
        stored_contracts(&tree).is_empty(),
        "no artifact is registered"
    );
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_headless_editor_says_it_draws_nothing() {
    let tree = ProjectTree::new("m8-capture-headless");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&[
            "editor.state",
            "editor.capture.viewport",
            "editor.capture.chunk",
        ])
        .answering(
            "editor.capture.viewport",
            FakeAnswer::Error(
                godot_mcp_domain::ErrorKind::Unavailable,
                godot_mcp_bridge::protocol::mutation::MutationStatus::NotStarted,
            ),
        ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let refused = session.call_tool(
        id,
        "editor.screenshot.capture",
        json!({ "idempotencyId": "shot-headless" }),
    );
    assert_eq!(error_code(&refused), json!("unavailable"), "{refused}");
    assert!(sent(&plugin, "editor.capture.chunk").is_empty());
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn the_main_screen_is_switched_by_name() {
    let tree = ProjectTree::new("m8-main-screen");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state", "editor.main_screen.set"]).answering(
            "editor.main_screen.set",
            FakeAnswer::Payload(json!({ "main_screen": "3D", "action": "godot-mcp: main screen" })),
        ),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    id += 1;
    let switched = ok(&session.call_tool(
        id,
        "editor.main_screen.set",
        json!({ "screen": "3D", "idempotencyId": "screen-3d" }),
    ));
    assert_eq!(switched["main_screen"], "3D", "{switched}");
    assert_eq!(sent(&plugin, "editor.main_screen.set")[0]["screen"], "3D");
    id += 1;
    let refused = session.call_tool(
        id,
        "editor.main_screen.set",
        json!({ "screen": "AssetLib", "idempotencyId": "screen-assetlib" }),
    );
    assert!(
        refused.get("error").is_some() || refused["result"]["isError"] == json!(true),
        "only 2D, 3D and Script are named: {refused}"
    );
    // Switching writes no file, so nothing blocks the next write.
    id += 1;
    ok(&session.call_tool(
        id,
        "file.create",
        json!({ "path": "notes/after.txt", "content": "x\n", "idempotencyId": "after-screen" }),
    ));
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}

#[test]
fn a_plugin_without_screens_or_captures_says_how_to_update_it() {
    let tree = ProjectTree::new("m8-capture-old");
    let mut session = open(&tree);
    let plugin = FakeEditorPlugin::spawn(
        &rendezvous(&tree),
        FakePluginScript::implementing(&["editor.state"]),
    );
    let mut id = 1;
    wait_connected(&mut session, &mut id);
    for (tool, arguments) in [
        (
            "editor.screenshot.capture",
            json!({ "idempotencyId": "old-shot" }),
        ),
        (
            "editor.main_screen.set",
            json!({ "screen": "2D", "idempotencyId": "old-screen" }),
        ),
    ] {
        id += 1;
        let refused = session.call_tool(id, tool, arguments);
        assert_eq!(
            error_code(&refused),
            json!("unsupported"),
            "{tool}: {refused}"
        );
        assert!(
            refused.to_string().contains("editor.addon.update"),
            "{tool}: {refused}"
        );
    }
    let _ = session.finish();
    plugin.join().expect("the plugin ended");
}
