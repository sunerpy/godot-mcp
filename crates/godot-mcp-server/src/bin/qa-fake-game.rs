//! `qa-fake-game` — NON-SHIPPING, `test-hooks`-only stand-in for a Godot build.
//!
//! The managed-session acceptance suite runs the real server with this binary
//! configured as its Godot executable. It is not Godot and never pretends to
//! render anything; it reproduces the observable contract a managed game
//! session depends on:
//!
//! * `--version` and `--help` answer like a 4.7.1 editor build, so engine
//!   discovery and capability negotiation accept it;
//! * run as a game (`--path <project> ... --script <launcher>`), it prints a
//!   line on each stream, records its PID in `<project>/fake-game.pid`, reads
//!   the session's rendezvous file named by `GODOT_MCP_RUNTIME_RENDEZVOUS`,
//!   dials the session bridge and answers the runtime probe's methods with
//!   canned payloads, including a one-pixel PNG capture; an input sequence is
//!   answered as cancelled as soon as the server withdraws it, and
//!   `<project>/fake-game.sequence` records that it arrived and whether it was
//!   withdrawn; a wait runs until the server withdraws it, or 10 s, and
//!   `<project>/fake-game.wait` records the same;
//! * run as an editor (`--editor ... --lsp-port <p> --dap-port <q>`), it records
//!   its PID in `<project>/fake-editor.pid` and hosts a minimal GDScript
//!   language server and debug adapter on those loopback ports, ordered the
//!   way Godot's own are: `launch` is answered only after `configurationDone`,
//!   a breakpoint stop follows, and the first `variables` read is answered
//!   `unknown` like an editor still fetching the frame from its game;
//! * run as an export (`--export-release` or `--export-debug <preset> <out>`), it
//!   records `<project>/fake-export.started` and writes the file a template
//!   export would: a small executable script that prints one line and exits 0,
//!   so the exported build can be smoke-run — or, for an output ending in
//!   `.zip`, the zip a macOS export packs its app bundle into, `Fake.app` with
//!   that script as its executable — or, for an output ending in `.apk`, the
//!   APK an Android export writes, once the editor settings in its private
//!   home name an Android SDK view, a JDK and a debug keystore the way Godot
//!   4.7.1 needs them;
//! * run as a pack export (`--export-pack <preset> <out>`), it writes the
//!   project's files as one format 4 PCK — or a ZIP for an output ending in
//!   `.zip` — and as a patch export (`--export-patch ... --patches <a,b>`)
//!   only the files whose MD5 differs from the base packs' tables, failing
//!   with Godot's "No files or changes to export" and a header-only file when
//!   none does;
//! * the `barrier` and `supervise` roles run the bundled supervisor, so the same
//!   binary can stand in for `godot-mcp-supervisor` too.
//!
//! `<project>/fake-game.conf` selects the behaviour: `probe` (default) serves
//! until `runtime.session.stop`, then exits 0; `stubborn` answers the stop but
//! keeps running; `old-probe` describes only the methods of the 0.3.0 probe;
//! `lossy` closes the bridge connection, unanswered, on a wait or a replay;
//! `long-replay` runs a replay until the server withdraws it, or 10 s, and
//! records the same in `<project>/fake-game.replay`;
//! `scenario` describes the session as one that renders and answers a
//! property write with `Engine.time_scale` read back as 2, for the test
//! scenario tools;
//! `exit:<code>` exits at once with `<code>`; `escape` tries to
//! move itself into the cgroup `<project>/fake-game.escape` names and reports
//! what it could do in `<project>/fake-game.escape-report`; for an editor,
//! `squat` leaves the language-server port to a helper process instead of
//! listening there itself; for an export, `export:fail` reports Godot's failure
//! line and exits 1, `export:hang` writes part of the output and ends only when
//! stopped or after ten minutes, `export:huge` writes a 300 MiB (sparse)
//! output, and `export:symlink` puts a symbolic link into a bundle zip;
//! `patch:identical` also puts one unchanged file into a patch, `pack:corrupt`
//! damages a pack's first file, `apk:broken` writes an APK that is not a zip
//! and `apk:no-launcher` one whose manifest declares no launcher.

use std::io::Write as _;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::ExitCode;

use godot_mcp_engine::adapters::codec::{FrameReader, encode_frame};

use godot_mcp_bridge::protocol::mutation::MutationStatus;
use godot_mcp_domain::ErrorKind;
use godot_mcp_server::test_support::fake_editor_plugin::{
    FakeAnswer, FakeEditorPlugin, FakePluginScript,
};
use serde_json::json;
use sha2::{Digest, Sha256};

const HELP_FLAGS: &[&str] = &[
    "--help",
    "--version",
    "--verbose",
    "--headless",
    "--path",
    "--quit",
    "--quit-after",
    "--script",
    "--check-only",
    "--editor",
    "--import",
    "--export-release",
    "--export-debug",
    "--export-pack",
    "--export-patch",
    "--patches",
    "--doctool",
    "--debug-server",
    "--main-pack",
    "--remote-debug",
    "--resolution",
    "--position",
    "--lsp-port",
    "--dap-port",
];

/// What the stand-in's export writes: an executable that runs and exits 0.
const EXPORTED_BUILD: &str = "#!/bin/sh\necho \"fake exported game ran\"\nexit 0\n";

/// The diagnostic the stand-in language server publishes for a script that
/// assigns a string to an `int`.
const TYPE_MISMATCH: &str = "Cannot assign a value of type \"String\" as \"int\".";

fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for part in parts {
        for byte in *part {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
    }
    !crc
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(0).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[kind, data]).to_be_bytes());
}

/// A valid one-pixel opaque RGBA PNG: an IHDR, one stored-deflate IDAT, IEND.
fn one_pixel_png() -> Vec<u8> {
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut header = Vec::new();
    header.extend_from_slice(&1_u32.to_be_bytes());
    header.extend_from_slice(&1_u32.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &header);
    let scanline = [0_u8, 0x20, 0x40, 0xa0, 0xff];
    let (mut a, mut b) = (1_u32, 0_u32);
    for byte in scanline {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    let mut zlib = vec![0x78, 0x01, 0x01, 5, 0, 0xfa, 0xff];
    zlib.extend_from_slice(&scanline);
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);
    png
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for group in bytes.chunks(3) {
        let word = (u32::from(group[0]) << 16)
            | (u32::from(*group.get(1).unwrap_or(&0)) << 8)
            | u32::from(*group.get(2).unwrap_or(&0));
        for index in 0..4 {
            if index <= group.len() {
                out.push(char::from(
                    TABLE[((word >> (18 - 6 * index)) & 63) as usize],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The methods the runtime probe of contract 0.3.0's first release
/// implemented, before typed control and observation.
const PROBE_0_3_0_METHODS: &[&str] = &[
    "runtime.capture.chunk",
    "runtime.capture.recording",
    "runtime.capture.screenshot",
    "runtime.input.release",
    "runtime.input.send",
    "runtime.input.sequence",
    "runtime.input.state",
    "runtime.performance",
    "runtime.properties",
    "runtime.session.stop",
    "runtime.signals",
    "runtime.tree",
    "runtime.ui",
];

/// The canned answers of the typed control and observation methods.
fn control(
    script: FakePluginScript,
    project: &std::path::Path,
    behaviour: &str,
) -> FakePluginScript {
    let lossy = behaviour == "lossy";
    let observed = |mut payload: serde_json::Value, frame: u64| {
        payload["revision"] = json!(1);
        payload["frame"] = json!(frame);
        payload
    };
    let wait = if lossy {
        FakeAnswer::Disconnect
    } else {
        FakeAnswer::PayloadOnCancel {
            cancelled: observed(
                json!({ "condition": "node_exists", "satisfied": false, "outcome": "cancelled", "elapsed_ms": 40, "frames_waited": 2, "timeout_ms": 30000, "observed": { "present": false, "node": null } }),
                30,
            ),
            finished: observed(
                json!({ "condition": "node_exists", "satisfied": true, "outcome": "satisfied", "elapsed_ms": 250, "frames_waited": 15, "timeout_ms": 30000, "observed": { "present": true, "node": "/root/Main/Spawned" } }),
                31,
            ),
            within: std::time::Duration::from_secs(10),
            marker: Some(project.join("fake-game.wait")),
        }
    };
    let completed = observed(
        json!({ "outcome": "completed", "events_sent": 2, "events_skipped": 0, "events_planned": 2, "elapsed_ms": 120, "scheduled_ms": 100, "max_lag_ms": 4, "released": [], "held": [] }),
        40,
    );
    let replay = if lossy {
        FakeAnswer::Disconnect
    } else if behaviour == "long-replay" {
        FakeAnswer::PayloadOnCancel {
            cancelled: observed(
                json!({ "outcome": "cancelled", "events_sent": 1, "events_skipped": 0, "events_planned": 2, "elapsed_ms": 60, "scheduled_ms": 20000, "max_lag_ms": 3, "released": ["key:68"], "held": [] }),
                41,
            ),
            finished: completed,
            within: std::time::Duration::from_secs(10),
            marker: Some(project.join("fake-game.replay")),
        }
    } else {
        FakeAnswer::Payload(completed)
    };
    script
        .answering(
            "runtime.properties.batch",
            FakeAnswer::Payload(observed(
                json!({
                    "targets": [
                        { "node": "/root/Main/Player", "class": "Node2D", "properties": { "position": { "type": "vector2", "value": { "x": 1.0, "y": 2.0 } } }, "unrepresentable": {}, "missing": [], "truncated": false },
                        { "singleton": "Engine", "class": "Engine", "properties": { "time_scale": { "type": "float", "value": 1.0 } }, "unrepresentable": {}, "missing": [], "truncated": false }
                    ],
                    "truncated": false
                }),
                20,
            )),
        )
        .answering(
            "runtime.properties.set",
            FakeAnswer::Payload(observed(
                if behaviour == "scenario" {
                    json!({
                        "target": "Engine",
                        "class": "Engine",
                        "results": [{
                            "property": "time_scale",
                            "before": { "type": "float", "value": 1.0 },
                            "requested": { "type": "float", "value": 2.0 },
                            "after": { "type": "float", "value": 2.0 },
                            "matched": true
                        }],
                        "all_matched": true
                    })
                } else {
                    json!({
                        "target": "/root/Main/Player",
                        "node": "/root/Main/Player",
                        "class": "Node2D",
                        "results": [{
                            "property": "position",
                            "before": { "type": "vector2", "value": { "x": 1.0, "y": 2.0 } },
                            "requested": { "type": "vector2", "value": { "x": 5.0, "y": 6.0 } },
                            "after": { "type": "vector2", "value": { "x": 5.0, "y": 6.0 } },
                            "matched": true
                        }],
                        "all_matched": true
                    })
                },
                21,
            )),
        )
        .answering(
            "runtime.scenario.check",
            FakeAnswer::Payload(observed(
                json!({
                    "node": "/root/Main/Player",
                    "class": "Node2D",
                    "property": "position",
                    "op": "approx",
                    "holds": true,
                    "reason": null,
                    "value": { "type": "vector2", "value": { "x": 1.0, "y": 2.0 } },
                    "special": null,
                    "type": "Vector2",
                    "unrepresentable": null
                }),
                28,
            )),
        )
        .answering(
            "runtime.scenario.stress",
            FakeAnswer::Payload(observed(
                json!({
                    "scene": "res://bullet.tscn",
                    "parent": "/root/Main",
                    "requested": 2,
                    "created": 2,
                    "present_during": 2,
                    "remaining": 0,
                    "freed": 2,
                    "outcome": "completed",
                    "duration_ms": 200,
                    "interval_ms": 100,
                    "samples": 2,
                    "paused_samples": 0,
                    "monitors": { "TIME_FPS": { "count": 2, "min": 60.0, "max": 60.0, "mean": 60.0, "last": 60.0 } },
                    "counts": {
                        "before": { "OBJECT_COUNT": 40.0, "OBJECT_NODE_COUNT": 3.0, "OBJECT_ORPHAN_NODE_COUNT": 0.0 },
                        "during": { "OBJECT_COUNT": 44.0, "OBJECT_NODE_COUNT": 5.0, "OBJECT_ORPHAN_NODE_COUNT": 0.0 },
                        "after": { "OBJECT_COUNT": 40.0, "OBJECT_NODE_COUNT": 3.0, "OBJECT_ORPHAN_NODE_COUNT": 0.0 }
                    }
                }),
                29,
            )),
        )
        .answering("runtime.wait", wait)
        .answering(
            "runtime.sample",
            FakeAnswer::Payload(observed(
                json!({
                    "time_base": { "clock": "engine_ticks_ms", "started_ticks_ms": 1000, "ended_ticks_ms": 1200, "first_frame": 50, "last_frame": 62, "first_physics_frame": 30, "last_physics_frame": 42, "interval_ms": 100, "duration_ms": 200, "display": "headless", "focus_observable": false },
                    "samples": [
                        { "t_ms": 0, "frame": 50, "physics_frame": 30, "paused": false, "focused": true, "time_scale": 1.0 },
                        { "t_ms": 100, "frame": 56, "physics_frame": 36, "paused": false, "focused": true, "time_scale": 1.0 }
                    ],
                    "series": [{ "kind": "monitor", "monitor": "TIME_FPS", "custom": false, "values": [60.0, 60.0], "stats": { "count": 2, "min": 60.0, "max": 60.0, "mean": 60.0, "last": 60.0 } }],
                    "paused_samples": 0,
                    "unfocused_samples": 0,
                    "stopped_by": "duration",
                    "truncated": false
                }),
                22,
            )),
        )
        .answering(
            "runtime.nodes.find",
            FakeAnswer::Payload(observed(
                json!({ "nodes": [{ "path": "/root/Main/Player", "name": "Player", "class": "Node2D", "script": "res://player.gd", "children": 0, "groups": ["players"], "visible": true, "position": { "x": 1.0, "y": 2.0 } }], "matched": 1, "truncated": false, "scanned": 3, "scan_truncated": false }),
                23,
            )),
        )
        .answering(
            "runtime.navigation.path",
            FakeAnswer::Error(ErrorKind::Unavailable, MutationStatus::NotStarted),
        )
        .answering(
            "runtime.node.move",
            FakeAnswer::Payload(observed(
                json!({ "node": "/root/Main/Player", "mode": "teleport", "outcome": "teleported", "start": { "x": 1.0, "y": 2.0 }, "after": { "x": 5.0, "y": 6.0 }, "final": { "x": 5.0, "y": 6.0 }, "held": true }),
                24,
            )),
        )
        .answering(
            "runtime.ui.press",
            FakeAnswer::Payload(observed(
                json!({ "node": "/root/Main/HUD/Start", "text": "Start", "position": { "x": 20.0, "y": 10.0 }, "outcome": "clicked", "pressed_signal": true, "emissions": { "pressed": 1, "button_down": 1, "button_up": 1, "toggled": 0 }, "button_pressed": false, "toggle_mode": false, "disabled": false, "present": true }),
                25,
            )),
        )
        .answering(
            "runtime.input.record.start",
            FakeAnswer::Payload(observed(
                json!({ "recording_id": "input-recording-1", "started_ticks_ms": 2000, "max_events": 3000, "max_duration_ms": 30000 }),
                26,
            )),
        )
        .answering(
            "runtime.input.record.stop",
            FakeAnswer::Payload(observed(
                json!({
                    "recording_id": "input-recording-1",
                    "started_ticks_ms": 2000,
                    "duration_ms": 400,
                    "ended_by": "stopped",
                    "max_events": 3000,
                    "max_duration_ms": 30000,
                    "dropped": 0,
                    "events": [
                        { "t_ms": 10, "kind": "key", "key": "D", "physical": "D", "unicode": 0, "pressed": true, "echo": false, "modifiers": { "shift": false, "ctrl": false, "alt": false, "meta": false }, "source": "probe", "actions": ["move_right"] },
                        { "t_ms": 260, "kind": "key", "key": "D", "physical": "D", "unicode": 0, "pressed": false, "echo": false, "modifiers": { "shift": false, "ctrl": false, "alt": false, "meta": false }, "source": "external", "actions": ["move_right"] }
                    ],
                    "counts": { "total": 2, "probe": 1, "external": 1 }
                }),
                27,
            )),
        )
        .answering("runtime.input.replay", replay)
}

fn script(stop: FakeAnswer, project: &std::path::Path, behaviour: &str) -> FakePluginScript {
    let png = one_pixel_png();
    let digest: String = Sha256::digest(&png)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let frame = json!({ "byte_length": png.len(), "sha256": digest });
    // The probe implements the script method whether or not the server
    // offers it; the server sends it only when its operator allowed scripts.
    let methods = if behaviour == "old-probe" {
        PROBE_0_3_0_METHODS
    } else {
        godot_mcp_bridge::runtime::METHODS_WITH_SCRIPTS
    };
    // A stand-in that renders nothing says so, as a headless game does; the
    // `scenario` behaviour describes one that renders, for the image tools.
    let described = FakePluginScript::implementing(methods)
        .describing("renders", json!(behaviour == "scenario"))
        .describing("godot_version", json!("4.7.1.stable.fake"));
    control(described, project, behaviour)
        .answering(
            "runtime.image.reference",
            FakeAnswer::Payload(json!({
                "upload": "compare",
                "received": png.len(),
                "total": png.len(),
                "complete": true
            })),
        )
        .answering(
            "runtime.image.compare",
            FakeAnswer::Payload(json!({
                "capture_id": "capture-3",
                "width": 1,
                "height": 1,
                "frames": [frame],
                "fps": null,
                "capture_size": { "width": 1, "height": 1 },
                "reference": { "width": 1, "height": 1, "byte_length": png.len(), "sha256": digest },
                "tolerance": 0,
                "same_size": true,
                "matched": true,
                "reason": null,
                "total_pixels": 1,
                "differing_pixels": 0,
                "ratio": 0.0,
                "max_channel_difference": 0,
                "bounding_box": null,
                "difference_frame": null,
                "revision": 1,
                "frame": 18
            })),
        )
        .answering(
            "runtime.tree",
            FakeAnswer::Payload(json!({
                "current_scene": "/root/Main",
                "nodes": [
                    { "path": "/root", "name": "root", "class": "Window", "children": 1, "groups": [], "script": null },
                    { "path": "/root/Main", "name": "Main", "class": "Node2D", "children": 1, "groups": [], "script": null, "visible": true },
                    { "path": "/root/Main/Player", "name": "Player", "class": "Node2D", "children": 0, "groups": ["players"], "script": "res://player.gd", "visible": true }
                ],
                "revision": 1,
                "frame": 10,
                "truncated": false
            })),
        )
        .answering(
            "runtime.properties",
            FakeAnswer::Payload(json!({
                "node": "/root/Main/Player",
                "class": "Node2D",
                "properties": { "position": { "type": "vector2", "value": { "x": 1.0, "y": 2.0 } } },
                "unrepresentable": {},
                "revision": 1,
                "frame": 11,
                "truncated": false
            })),
        )
        .answering(
            "runtime.input.send",
            FakeAnswer::Payload(json!({
                "held": [{ "input": "action:ui_right", "expires_in_ms": 500 }],
                "revision": 1,
                "frame": 12
            })),
        )
        .answering(
            "runtime.input.state",
            FakeAnswer::Payload(json!({ "held": [], "revision": 1, "frame": 13 })),
        )
        .answering(
            "runtime.performance",
            FakeAnswer::Payload(json!({
                "monitors": { "TIME_FPS": 60.0, "OBJECT_NODE_COUNT": 3.0 },
                "custom": {},
                "revision": 1,
                "frame": 14
            })),
        )
        .answering(
            "runtime.input.release",
            FakeAnswer::Error(ErrorKind::Unavailable, MutationStatus::NotStarted),
        )
        .answering(
            "runtime.input.sequence",
            FakeAnswer::PayloadOnCancel {
                cancelled: json!({ "steps_run": 1, "cancelled": true, "held": [], "revision": 1, "frame": 16 }),
                finished: json!({ "steps_run": 2, "cancelled": false, "held": [], "revision": 1, "frame": 17 }),
                within: std::time::Duration::from_millis(1500),
                marker: Some(project.join("fake-game.sequence")),
            },
        )
        .answering(
            "runtime.capture.screenshot",
            FakeAnswer::Payload(json!({
                "capture_id": "capture-1",
                "width": 1,
                "height": 1,
                "frames": [frame],
                "fps": null,
                "revision": 1,
                "frame": 14
            })),
        )
        .answering(
            "runtime.capture.recording",
            FakeAnswer::ErrorOnCancel {
                kind: ErrorKind::Cancelled,
                finished: json!({
                    "capture_id": "capture-2",
                    "width": 1,
                    "height": 1,
                    "frames": [frame, frame],
                    "fps": 10,
                    "revision": 1,
                    "frame": 15
                }),
                within: std::time::Duration::from_millis(1500),
                marker: Some(project.join("fake-game.recording")),
            },
        )
        .answering(
            "runtime.capture.chunk",
            FakeAnswer::Payload(json!({
                "capture_id": "capture-1",
                "frame": 0,
                "offset": 0,
                "length": png.len(),
                "total": png.len(),
                "data_base64": base64(&png)
            })),
        )
        .answering(
            "runtime.script.run",
            FakeAnswer::SaveThenAnswer {
                path: project.join("made_by_script.txt"),
                contents: b"made".to_vec(),
                payload: json!({
                    "stage": "done",
                    "ran": true,
                    "literal": "5",
                    "source_file": "gdscript://1.gd",
                    "log_capture": true,
                    "log": [{ "kind": "message", "text": "from the fake game", "stderr": false }],
                    "log_skipped": 0,
                    "log_truncated": false,
                    "action": "godot-mcp: runtime.script.run"
                }),
            },
        )
        .answering("runtime.session.stop", stop)
}

fn send_frame(stream: &mut TcpStream, message: &serde_json::Value) -> bool {
    encode_frame(message.to_string().as_bytes())
        .ok()
        .is_some_and(|frame| stream.write_all(&frame).is_ok())
}

/// Serve every connection to `listener` with `serve`, one thread each.
fn accept_forever(listener: TcpListener, serve: fn(TcpStream)) {
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || serve(stream));
        }
    });
}

/// One language-server connection: `initialize`, diagnostics pushed on
/// `didOpen`, and `documentSymbol`.
fn serve_language_server(stream: TcpStream) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = FrameReader::new(stream);
    while let Ok(frame) = reader.read_frame() {
        let Ok(message) = serde_json::from_slice::<serde_json::Value>(&frame) else {
            return;
        };
        let method = message["method"].as_str().unwrap_or_default();
        let id = message.get("id").cloned();
        let reply = match method {
            "initialize" => Some(json!({
                "capabilities": {
                    "textDocumentSync": { "openClose": true, "change": 1 },
                    "documentSymbolProvider": true,
                    "workspaceSymbolProvider": false
                }
            })),
            "textDocument/didOpen" => {
                let uri = message["params"]["textDocument"]["uri"].clone();
                let text = message["params"]["textDocument"]["text"]
                    .as_str()
                    .unwrap_or_default();
                let diagnostics = if text.contains("\"not an int\"") {
                    json!([{
                        "range": {
                            "start": { "line": 3, "character": 14 },
                            "end": { "line": 3, "character": 26 }
                        },
                        "severity": 1,
                        "source": "gdscript",
                        "message": TYPE_MISMATCH
                    }])
                } else {
                    json!([])
                };
                send_frame(
                    &mut writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": { "uri": uri, "diagnostics": diagnostics }
                    }),
                );
                None
            }
            "textDocument/documentSymbol" => Some(json!([{
                "name": "player",
                "kind": 5,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 8, "character": 0 } },
                "children": [
                    {
                        "name": "counter",
                        "kind": 13,
                        "detail": "var counter: int",
                        "range": { "start": { "line": 2, "character": 0 }, "end": { "line": 2, "character": 16 } }
                    },
                    {
                        "name": "_ready",
                        "kind": 12,
                        "detail": "func _ready() -> void",
                        "range": { "start": { "line": 4, "character": 0 }, "end": { "line": 7, "character": 0 } }
                    }
                ]
            }])),
            _ => None,
        };
        if let (Some(id), Some(result)) = (id, reply)
            && !send_frame(
                &mut writer,
                &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            )
        {
            return;
        }
    }
}

/// One debug-adapter connection, answering in Godot's own order.
fn serve_debug_adapter(stream: TcpStream) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = FrameReader::new(stream);
    let mut seq = 0_i64;
    let mut next = move || {
        seq += 1;
        seq
    };
    let mut pending_launch: Option<(i64, String)> = None;
    let mut breakpoint: Option<(String, u64)> = None;
    let mut variables_asked = false;
    while let Ok(frame) = reader.read_frame() {
        let Ok(message) = serde_json::from_slice::<serde_json::Value>(&frame) else {
            return;
        };
        let command = message["command"].as_str().unwrap_or_default().to_owned();
        let request = message["seq"].as_i64().unwrap_or_default();
        let arguments = &message["arguments"];
        let mut respond = |writer: &mut TcpStream, success: bool, body: serde_json::Value| {
            let mut response = json!({
                "seq": next(),
                "type": "response",
                "request_seq": request,
                "success": success,
                "command": command,
                "body": body,
            });
            if !success {
                response["message"] = json!("unknown");
            }
            send_frame(writer, &response)
        };
        let event = |writer: &mut TcpStream, name: &str, body: serde_json::Value| {
            send_frame(
                writer,
                &json!({ "seq": 0, "type": "event", "event": name, "body": body }),
            )
        };
        match command.as_str() {
            "initialize" => {
                respond(
                    &mut writer,
                    true,
                    json!({ "supportsConfigurationDoneRequest": true, "supportsSetVariable": true }),
                );
                event(&mut writer, "initialized", json!({}));
            }
            "setBreakpoints" => {
                let path = arguments["source"]["path"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let lines: Vec<u64> = arguments["lines"]
                    .as_array()
                    .map(|lines| lines.iter().filter_map(serde_json::Value::as_u64).collect())
                    .unwrap_or_default();
                breakpoint = lines.first().map(|line| (path.clone(), *line));
                let acknowledged: Vec<serde_json::Value> = lines
                    .iter()
                    .map(|line| json!({ "verified": true, "line": line, "source": { "path": path } }))
                    .collect();
                respond(&mut writer, true, json!({ "breakpoints": acknowledged }));
            }
            // Godot stores the launch and answers it only once configuration
            // is done.
            "launch" => pending_launch = Some((request, command.clone())),
            "configurationDone" => {
                respond(&mut writer, true, json!({}));
                event(
                    &mut writer,
                    "process",
                    json!({ "name": "game", "startMethod": "launch" }),
                );
                if let Some((launch, name)) = pending_launch.take() {
                    send_frame(
                        &mut writer,
                        &json!({
                            "seq": next(),
                            "type": "response",
                            "request_seq": launch,
                            "success": true,
                            "command": name,
                            "body": {},
                        }),
                    );
                }
                if breakpoint.is_some() {
                    event(
                        &mut writer,
                        "stopped",
                        json!({ "reason": "breakpoint", "threadId": 1, "allThreadsStopped": true }),
                    );
                }
            }
            "threads" => {
                respond(
                    &mut writer,
                    true,
                    json!({ "threads": [{ "id": 1, "name": "Main" }] }),
                );
            }
            "stackTrace" => {
                let (path, line) = breakpoint.clone().unwrap_or_default();
                respond(
                    &mut writer,
                    true,
                    json!({
                        "stackFrames": [{
                            "id": 0,
                            "name": "_ready",
                            "line": line,
                            "column": 1,
                            "source": { "name": "player.gd", "path": path }
                        }],
                        "totalFrames": 1
                    }),
                );
            }
            "scopes" => {
                respond(
                    &mut writer,
                    true,
                    json!({ "scopes": [
                        { "name": "Locals", "variablesReference": 1, "expensive": false },
                        { "name": "Members", "variablesReference": 2, "expensive": false },
                        { "name": "Globals", "variablesReference": 3, "expensive": false }
                    ] }),
                );
            }
            "variables" if !variables_asked => {
                // The editor has not received the frame's variables yet.
                variables_asked = true;
                respond(
                    &mut writer,
                    false,
                    json!({ "error": { "id": 0, "format": "unknown" } }),
                );
            }
            "variables" => {
                let variables = match arguments["variablesReference"].as_i64() {
                    Some(1) => {
                        json!([{ "name": "local_value", "value": "41", "type": "int", "variablesReference": 0 }])
                    }
                    Some(2) => {
                        json!([{ "name": "counter", "value": "0", "type": "int", "variablesReference": 0 }])
                    }
                    _ => json!([]),
                };
                respond(&mut writer, true, json!({ "variables": variables }));
            }
            "continue" => {
                respond(&mut writer, true, json!({ "allThreadsContinued": true }));
                event(&mut writer, "continued", json!({ "threadId": 1 }));
            }
            "pause" => {
                respond(&mut writer, true, json!({}));
                event(
                    &mut writer,
                    "stopped",
                    json!({ "reason": "pause", "threadId": 1, "allThreadsStopped": true }),
                );
            }
            "disconnect" => {
                respond(&mut writer, true, json!({}));
                event(&mut writer, "terminated", json!({}));
            }
            // Godot writes no response for a command it does not implement.
            _ => {}
        }
    }
}

/// The export flags this stand-in answers.
const EXPORT_MODES: [&str; 4] = [
    "--export-release",
    "--export-debug",
    "--export-pack",
    "--export-patch",
];

/// Every file of the project an export packs, by its path in the pack:
/// nothing hidden, none of this stand-in's own files, not the presets, and
/// `project.godot` as `project.binary`, as Godot packs it.
fn project_files(project: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    fn walk(directory: &std::path::Path, prefix: &str, out: &mut Vec<(String, Vec<u8>)>) {
        let Ok(listing) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in listing.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name.starts_with("fake-") {
                continue;
            }
            let path = format!("{prefix}{name}");
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                walk(&entry.path(), &format!("{path}/"), out);
            } else if kind.is_file() && path != "export_presets.cfg" {
                let packed = if path == "project.godot" {
                    "project.binary".to_owned()
                } else {
                    path
                };
                if let Ok(bytes) = std::fs::read(entry.path()) {
                    out.push((packed, bytes));
                }
            }
        }
    }
    let mut files = Vec::new();
    walk(project, "", &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

/// The value after `flag`.
fn after<'a>(arguments: &'a [String], flag: &str) -> Option<&'a String> {
    arguments
        .iter()
        .position(|argument| argument == flag)
        .and_then(|at| arguments.get(at + 1))
}

/// Run as a pack or patch export of `preset` into `output`.
fn pack_export(
    arguments: &[String],
    preset: &str,
    output: &str,
    behaviour: &str,
    patch: bool,
    project: &std::path::Path,
) -> ExitCode {
    use godot_mcp_engine::export::md5::md5;
    use godot_mcp_engine::export::pack::{fixture, read_pack};

    let files = project_files(project);
    let selected: Vec<&(String, Vec<u8>)> = if patch {
        let mut known: std::collections::BTreeMap<String, [u8; 16]> =
            std::collections::BTreeMap::new();
        for base in after(arguments, "--patches")
            .map(|list| list.split(',').collect::<Vec<_>>())
            .unwrap_or_default()
        {
            let Some(pack) = std::fs::read(base)
                .ok()
                .and_then(|bytes| read_pack(&bytes).ok())
            else {
                eprintln!("ERROR: Couldn't load patch pack: {base}");
                return ExitCode::from(1);
            };
            for entry in pack.entries {
                if entry.removal() {
                    known.remove(&entry.path);
                } else {
                    known.insert(entry.path, entry.md5);
                }
            }
        }
        let mut changed: Vec<&(String, Vec<u8>)> = files
            .iter()
            .filter(|(path, bytes)| known.get(path) != Some(&md5(bytes)))
            .collect();
        if behaviour == "patch:identical"
            && !changed.is_empty()
            && let Some(unchanged) = files.iter().find(|file| !changed.contains(file))
        {
            changed.push(unchanged);
            changed.sort_by(|left, right| left.0.cmp(&right.0));
        }
        changed
    } else {
        files.iter().collect()
    };
    if selected.is_empty() {
        eprintln!("ERROR: Save PCK: No files or changes to export.");
        let _ = std::fs::write(output, fixture::header_only(4, (4, 7, 1)));
        eprintln!("ERROR: Project export for preset \"{preset}\" failed.");
        return ExitCode::from(1);
    }
    let bytes = if output.ends_with(".zip") {
        use godot_mcp_engine::export::bundle::fixture::{Entry, stored_zip};
        let entries: Vec<Entry<'_>> = selected
            .iter()
            .map(|(path, bytes)| Entry {
                name: path,
                data: bytes,
                mode: 0o100_644,
            })
            .collect();
        stored_zip(&entries)
    } else {
        let entries: Vec<fixture::Entry<'_>> = selected
            .iter()
            .map(|(path, bytes)| fixture::Entry {
                path,
                data: bytes,
                flags: 0,
            })
            .collect();
        let mut bytes = fixture::pck(4, (4, 7, 1), &entries);
        if behaviour == "pack:corrupt" && bytes.len() > 0x70 {
            bytes[0x70] ^= 0xff;
        }
        bytes
    };
    if std::fs::write(output, bytes).is_err() {
        return ExitCode::from(70);
    }
    println!("[ DONE ] savepack");
    ExitCode::SUCCESS
}

/// Run as an Android export of `preset` into `output`: the private home's
/// editor settings must name an SDK view with `adb` and `build-tools`, a JDK
/// with `java`, and, for a debug export, a debug keystore, as Godot 4.7.1
/// checks them; the APK names the preset's package.
fn android_export(
    project: &std::path::Path,
    preset: &str,
    output: &str,
    behaviour: &str,
    debug: bool,
) -> ExitCode {
    use godot_mcp_engine::export::apk::fixture::apk;
    use godot_mcp_engine::export::axml::fixture::{Launcher, Spec};

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let settings = std::fs::read_to_string(home.join(".config/godot/editor_settings-4.7.tres"))
        .unwrap_or_default();
    let setting = |key: &str| {
        settings.lines().find_map(|line| {
            line.strip_prefix(key)
                .and_then(|rest| rest.trim_start().strip_prefix('='))
                .map(|value| value.trim().trim_matches('"').to_owned())
        })
    };
    let mut errors = Vec::new();
    let sdk = setting("export/android/android_sdk_path").map(PathBuf::from);
    if !sdk
        .as_ref()
        .is_some_and(|sdk| sdk.join("platform-tools/adb").is_file())
    {
        errors.push("Unable to find Android SDK platform-tools' adb command.");
    }
    if !sdk.as_ref().is_some_and(|sdk| {
        std::fs::read_dir(sdk.join("build-tools")).is_ok_and(|mut versions| {
            versions.any(|version| {
                version.is_ok_and(|version| version.path().join("apksigner").is_file())
            })
        })
    }) {
        errors.push("Unable to find Android SDK build-tools' apksigner command.");
    }
    if !setting("export/android/java_sdk_path")
        .is_some_and(|jdk| std::path::Path::new(&jdk).join("bin/java").is_file())
    {
        errors.push("A valid Java SDK path is required in Editor Settings.");
    }
    if debug
        && !setting("export/android/debug_keystore")
            .is_some_and(|keystore| std::path::Path::new(&keystore).is_file())
    {
        errors.push("Debug keystore not configured in the Editor Settings nor in the preset.");
    }
    if !errors.is_empty() {
        eprintln!(
            "ERROR: Cannot export project with preset \"{preset}\" due to configuration errors:"
        );
        for error in errors {
            eprintln!("{error}");
        }
        eprintln!("ERROR: Project export for preset \"{preset}\" failed.");
        return ExitCode::from(1);
    }
    let package = std::fs::read_to_string(project.join("export_presets.cfg"))
        .ok()
        .and_then(|text| godot_mcp_engine::export::presets::parse_presets(&text).ok())
        .and_then(|presets| {
            presets
                .into_iter()
                .find(|candidate| candidate.name() == preset)
                .and_then(|found| found.option_str("package/unique_name").map(str::to_owned))
        })
        .unwrap_or_else(|| "org.godotengine.$genname".to_owned())
        .replace("$genname", "fakegame");
    let mut spec = Spec::godot(&package);
    spec.debuggable = debug;
    if behaviour == "apk:no-launcher" {
        spec.launcher = Launcher::None;
    }
    let bytes = if behaviour == "apk:broken" {
        b"not a zip".to_vec()
    } else {
        apk(&spec)
    };
    if std::fs::write(output, bytes).is_err() {
        return ExitCode::from(70);
    }
    println!("[ DONE ] export");
    ExitCode::SUCCESS
}

/// Run as an export of `preset` into `output`, as scripted by `behaviour`.
fn export(project: &std::path::Path, arguments: &[String], behaviour: &str) -> ExitCode {
    let at = arguments
        .iter()
        .position(|argument| EXPORT_MODES.contains(&argument.as_str()));
    let (Some(at), Some(preset), Some(output)) = (
        at,
        at.and_then(|at| arguments.get(at + 1)),
        at.and_then(|at| arguments.get(at + 2)),
    ) else {
        eprintln!("qa-fake-game: an export needs <preset> <output>");
        return ExitCode::from(64);
    };
    println!("[  10% ] savepack | Started Packing");
    let _ = std::fs::write(project.join("fake-export.started"), preset);
    let mode = arguments[at].as_str();
    if behaviour != "export:fail" && matches!(mode, "--export-pack" | "--export-patch") {
        return pack_export(
            arguments,
            preset,
            output,
            behaviour,
            mode == "--export-patch",
            project,
        );
    }
    if behaviour != "export:fail" && output.ends_with(".apk") {
        return android_export(project, preset, output, behaviour, mode == "--export-debug");
    }
    match behaviour {
        "export:fail" => {
            eprintln!("ERROR: Project export for preset \"{preset}\" failed.");
            ExitCode::from(1)
        }
        "export:hang" => {
            let _ = std::fs::write(output, b"partial export output");
            // Hang until cancelled, but at most ten minutes, so a test that
            // fails before it cancels leaves no process running for long.
            std::thread::sleep(std::time::Duration::from_secs(600));
            ExitCode::from(1)
        }
        "export:huge" => {
            let written =
                std::fs::File::create(output).and_then(|file| file.set_len(300 * 1024 * 1024));
            if written.is_err() {
                return ExitCode::from(70);
            }
            println!("[ DONE ] savepack");
            ExitCode::SUCCESS
        }
        _ if output.ends_with(".zip") => {
            use godot_mcp_engine::export::bundle::fixture::{Entry, app_bundle};

            let link = Entry {
                name: "Fake.app/Contents/Frameworks/Current",
                data: b"/etc",
                mode: 0o120_644,
            };
            let extra: &[Entry<'_>] = if behaviour == "export:symlink" {
                &[link]
            } else {
                &[]
            };
            if std::fs::write(output, app_bundle("Fake", EXPORTED_BUILD.as_bytes(), extra)).is_err()
            {
                return ExitCode::from(70);
            }
            println!("[ DONE ] savepack");
            ExitCode::SUCCESS
        }
        _ => {
            if std::fs::write(output, EXPORTED_BUILD).is_err() {
                return ExitCode::from(70);
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let _ = std::fs::set_permissions(output, std::fs::Permissions::from_mode(0o755));
            }
            println!("[ DONE ] savepack");
            ExitCode::SUCCESS
        }
    }
}

fn port_after(arguments: &[String], flag: &str) -> Option<u16> {
    arguments
        .iter()
        .position(|argument| argument == flag)
        .and_then(|index| arguments.get(index + 1))
        .and_then(|port| port.parse().ok())
}

/// Run as an editor hosting the two protocol servers until terminated.
fn editor(arguments: &[String], project: &std::path::Path, behaviour: &str) -> ExitCode {
    println!("fake editor started");
    eprintln!("fake editor warning");
    let _ = std::fs::write(
        project.join("fake-editor.pid"),
        std::process::id().to_string(),
    );
    let (Some(lsp), Some(dap)) = (
        port_after(arguments, "--lsp-port"),
        port_after(arguments, "--dap-port"),
    ) else {
        eprintln!("qa-fake-game: an editor needs --lsp-port and --dap-port");
        return ExitCode::from(64);
    };
    if behaviour == "squat" {
        // A process that is not the editor listens on the language-server
        // port; the editor itself never does.
        let helper = std::env::current_exe().map(|binary| {
            std::process::Command::new(binary)
                .arg("squat-port")
                .arg(lsp.to_string())
                .spawn()
        });
        if !matches!(helper, Ok(Ok(_))) {
            eprintln!("qa-fake-game: the squatting helper did not start");
            return ExitCode::from(70);
        }
    } else {
        match TcpListener::bind(("127.0.0.1", lsp)) {
            Ok(listener) => accept_forever(listener, serve_language_server),
            Err(error) => {
                eprintln!("qa-fake-game: the language server cannot listen: {error}");
                return ExitCode::from(70);
            }
        }
    }
    match TcpListener::bind(("127.0.0.1", dap)) {
        Ok(listener) => accept_forever(listener, serve_debug_adapter),
        Err(error) => {
            eprintln!("qa-fake-game: the debug adapter cannot listen: {error}");
            return ExitCode::from(70);
        }
    }
    // An editor runs until its boundary is terminated.
    loop {
        std::thread::park();
    }
}

fn main() -> ExitCode {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments
        .first()
        .is_some_and(|argument| argument == "squat-port")
    {
        let port = arguments.get(1).and_then(|port| port.parse::<u16>().ok());
        let Some(Ok(_listener)) = port.map(|port| TcpListener::bind(("127.0.0.1", port))) else {
            return ExitCode::from(70);
        };
        loop {
            std::thread::park();
        }
    }
    if arguments
        .first()
        .is_some_and(|argument| matches!(argument.as_str(), "barrier" | "supervise"))
    {
        return match godot_mcp_engine::supervision::supervisor::parse_role(&arguments) {
            Ok(role) => match godot_mcp_engine::supervision::supervisor::run(role) {
                Ok(status) => ExitCode::from(status),
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::from(godot_mcp_engine::supervision::supervisor::USAGE_EXIT)
                }
            },
            Err(error) => {
                eprintln!("{error}");
                ExitCode::from(godot_mcp_engine::supervision::supervisor::USAGE_EXIT)
            }
        };
    }
    if arguments.iter().any(|argument| argument == "--version") {
        println!("4.7.1.stable.official.fakegame");
        return ExitCode::SUCCESS;
    }
    if arguments.iter().any(|argument| argument == "--help") {
        println!("Godot Engine v4.7.1.stable.official.fakegame\n\nGeneral options:");
        for flag in HELP_FLAGS {
            println!("  {flag:<32}A flag this stand-in accepts.");
        }
        return ExitCode::SUCCESS;
    }
    let project = arguments
        .iter()
        .position(|argument| argument == "--path")
        .and_then(|index| arguments.get(index + 1))
        .map(PathBuf::from);
    let Some(project) = project else {
        eprintln!("qa-fake-game: no --path");
        return ExitCode::from(64);
    };
    let behaviour = std::fs::read_to_string(project.join("fake-game.conf"))
        .unwrap_or_default()
        .trim()
        .to_owned();
    if arguments
        .iter()
        .any(|argument| EXPORT_MODES.contains(&argument.as_str()))
    {
        return export(&project, &arguments, &behaviour);
    }
    if arguments.iter().any(|argument| argument == "--editor") {
        return editor(&arguments, &project, &behaviour);
    }
    println!("fake game started");
    eprintln!("fake game warning");
    let _ = std::fs::write(
        project.join("fake-game.pid"),
        std::process::id().to_string(),
    );
    if let Some(code) = behaviour.strip_prefix("exit:") {
        return ExitCode::from(code.parse::<u8>().unwrap_or(1));
    }
    if behaviour == "escape" {
        // Try to leave the boundary: move this process into the cgroup whose
        // membership file the project names, and count what the cgroup
        // filesystem shows. The report says what worked.
        let target = std::fs::read_to_string(project.join("fake-game.escape")).unwrap_or_default();
        let moved = std::fs::write(target.trim(), std::process::id().to_string()).is_ok();
        let visible = std::fs::read_dir("/sys/fs/cgroup").map_or(0, Iterator::count);
        let _ = std::fs::write(
            project.join("fake-game.escape-report"),
            format!("moved={moved} cgroupfs_entries={visible}\n"),
        );
        return ExitCode::SUCCESS;
    }
    let Some(rendezvous) = std::env::var_os(godot_mcp_bridge::runtime::RENDEZVOUS_ENV) else {
        eprintln!("qa-fake-game: no rendezvous");
        return ExitCode::from(64);
    };
    let stop = if behaviour == "stubborn" {
        FakeAnswer::Payload(json!({ "stopping": true, "released": [] }))
    } else {
        FakeAnswer::PayloadThenExit {
            payload: json!({ "stopping": true, "released": [] }),
            code: 0,
        }
    };
    let probe = FakeEditorPlugin::spawn(
        &PathBuf::from(rendezvous),
        script(stop, &project, &behaviour),
    );
    match probe.join() {
        Ok(()) if behaviour == "stubborn" => {
            // The bridge ended but this game will not: only a terminated
            // boundary ends it.
            loop {
                std::thread::park();
            }
        }
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("qa-fake-game: {error}");
            ExitCode::from(70)
        }
    }
}
