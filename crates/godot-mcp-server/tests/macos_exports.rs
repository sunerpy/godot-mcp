//! M6 on a Mac: a macOS preset's export is smoke-run from its app bundle.
//!
//! The production server runs out of process with the non-shipping
//! `qa-fake-game` binary as its Godot executable. Given an output ending in
//! `.zip`, the stand-in packs the app bundle a macOS export would, `Fake.app`,
//! whose executable is a script that exits 0 — or, as scripted, a bundle that
//! also holds a symbolic link. The exact template is looked up where a Mac's
//! Godot looks for it, under the server's `HOME`. Every server-side path is the
//! shipped one: the macOS target, the supervised export, the artifact
//! registration, the archive check, the unpacking and the smoke run of the
//! bundle's executable. Real Godot builds are exercised by
//! `scripts/real_client/m6_smoke.py`.
#![cfg(target_os = "macos")]

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::{ProjectTree, StdioSession};

const PRESETS: &str = r#"[preset.0]

name="Mac"
platform="macOS"
runnable=true
export_path="build/Game.app"

[preset.0.options]

custom_template/debug=""
custom_template/release=""
binary_format/architecture="universal"
application/bundle_identifier="org.godotmcp.test"
"#;

/// A home holding the stand-in build's exact macOS template where a Mac's
/// Godot looks for it.
struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

fn home() -> Home {
    let directory = tempfile::tempdir().expect("home");
    let root = directory.path().canonicalize().expect("canonical home");
    let templates = root.join("Library/Application Support/Godot/export_templates/4.7.1.stable");
    std::fs::create_dir_all(&templates).expect("templates");
    std::fs::write(templates.join("version.txt"), "4.7.1.stable\n").expect("version");
    std::fs::write(templates.join("macos.zip"), b"template").expect("template");
    Home {
        _directory: directory,
        root,
    }
}

fn server(tree: &ProjectTree, home: &Home) -> StdioSession {
    let mut command = Command::new(env!("CARGO_BIN_EXE_qa-journal-foundation"));
    command
        .arg("--project")
        .arg(&tree.root)
        .arg("--profile")
        .arg("full")
        .arg("--godot-binary")
        .arg(Path::new(env!("CARGO_BIN_EXE_qa-fake-game")))
        .env("HOME", &home.root)
        .env_remove("GODOT_MCP_TEST_LAUNCH_CRASH_STAGE");
    StdioSession::handshake(StdioSession::start_command(command)).0
}

fn project(label: &str, behaviour: Option<&str>) -> ProjectTree {
    let tree = ProjectTree::new(label);
    tree.write("export_presets.cfg", PRESETS);
    tree.write(
        "main.tscn",
        "[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node2D\"]\n",
    );
    if let Some(behaviour) = behaviour {
        tree.write("fake-game.conf", &format!("{behaviour}\n"));
    }
    tree
}

fn structured(response: &Value) -> Value {
    assert!(
        response.get("error").is_none() && response["result"]["isError"] != json!(true),
        "the call must succeed: {response}"
    );
    response["result"]["structuredContent"].clone()
}

/// Export the Mac preset with a smoke run and answer its final status.
fn exported(session: &mut StdioSession) -> Value {
    let started = structured(&session.call_tool(
        2,
        "project.export",
        json!({ "preset": "Mac", "smoke": true, "idempotencyId": "mac-export" }),
    ));
    let export = started["export"]["session"]
        .as_str()
        .expect("export id")
        .to_owned();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut id = 10;
    loop {
        let status = structured(&session.call_tool(
            id,
            "project.export.status",
            json!({ "export": export }),
        ));
        if status["state"] != json!("running") && status["state"] != json!("starting") {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "the export never ended: {status}"
        );
        id += 1;
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_macos_export_is_smoke_run_from_its_app_bundle() {
    let tree = project("m6-mac-smoke", None);
    let home = home();
    let mut session = server(&tree, &home);
    let listed = structured(&session.call_tool(1, "export.presets.list", json!({})));
    assert_eq!(
        listed["presets"][0]["template"]["runnable_here"],
        json!(true),
        "{listed}"
    );

    let status = exported(&mut session);
    assert_eq!(status["export"]["outcome"], json!("succeeded"), "{status}");
    let outputs = status["export"]["outputs"].as_array().expect("outputs");
    assert_eq!(outputs.len(), 1, "{status}");
    assert_eq!(outputs[0]["name"], json!("Game.zip"), "{status}");
    let smoke = &status["export"]["smoke"];
    assert_eq!(smoke["ran"], json!(true), "{status}");
    assert_eq!(smoke["passed"], json!(true), "{status}");
    assert_eq!(smoke["termination"], json!("exited_clean"), "{status}");
    assert_eq!(
        smoke["executable"],
        json!("Fake.app/Contents/MacOS/Fake"),
        "the smoke run starts the executable the bundle's Info.plist names: {status}"
    );
    assert!(!tree.root.join("build").exists());
    assert!(
        !session
            .transcript
            .contains(&tree.root.display().to_string()),
        "no answer names a private path"
    );
    let _ = session.finish();
}

#[test]
fn a_bundle_holding_a_symbolic_link_is_exported_but_not_run() {
    let tree = project("m6-mac-link", Some("export:symlink"));
    let home = home();
    let mut session = server(&tree, &home);
    let status = exported(&mut session);
    assert_eq!(status["export"]["outcome"], json!("succeeded"), "{status}");
    let smoke = &status["export"]["smoke"];
    assert_eq!(smoke["ran"], json!(false), "{status}");
    assert!(
        smoke["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("symbolic link")),
        "{status}"
    );
    let _ = session.finish();
}
