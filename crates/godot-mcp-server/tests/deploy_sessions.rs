//! Pack, patch and Android exports and device deploys over the real stdio
//! assembly.
//!
//! The production server runs out of process with the non-shipping
//! `qa-fake-game` binary as its Godot executable and a fake Android SDK and
//! JDK whose `adb`, `apksigner`, `keytool` and `java` are links to the
//! non-shipping `qa-fake-adb` binary, which plays the scenario a
//! `fake-android.conf` beside them names. Every server-side path is the
//! shipped one: the request checks, the base packs read and copied, the
//! export's private Godot home and keystore, the supervised export, the pack
//! and APK read back, the device phases and their journal, and the next
//! server's reconciliation. Real Godot builds are exercised by
//! `live_deploy.rs`; no real device is.
#![cfg(target_os = "linux")]

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use support::{ProjectTree, StdioSession, error_code, sha256_hex};

const PRESETS: &str = r#"[preset.0]

name="Linux"
platform="Linux"
runnable=true
export_path="build/game.x86_64"

[preset.0.options]

custom_template/debug=""
custom_template/release=""
binary_format/architecture="x86_64"

[preset.1]

name="Android"
platform="Android"
runnable=true
export_path="build/game.apk"

[preset.1.options]

custom_template/debug=""
custom_template/release=""
gradle_build/use_gradle_build=false
gradle_build/export_format=0
architectures/arm64-v8a=true
package/unique_name="org.godotmcp.deploy"

[preset.2]

name="Gradle"
platform="Android"
runnable=false
export_path="build/gradle.apk"

[preset.2.options]

gradle_build/use_gradle_build=true
package/unique_name="org.godotmcp.gradle"
"#;

/// The release signing credentials of the Android preset, kept where Godot
/// keeps them.
const CREDENTIALS: &str = r#"[preset.1]

[preset.1.options]

keystore/release="/keystores/release.keystore"
keystore/release_user="release"
keystore/release_password="never-echoed-release-password"
"#;

fn fake_godot() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_qa-fake-game"))
}

/// A data home holding the stand-in build's Linux and Android templates.
struct DataHome {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

fn data_home() -> DataHome {
    let directory = tempfile::tempdir().expect("data home");
    let root = directory.path().to_path_buf();
    let templates = root.join("godot/export_templates/4.7.1.stable");
    std::fs::create_dir_all(&templates).expect("templates");
    std::fs::write(templates.join("version.txt"), "4.7.1.stable\n").expect("version");
    for template in [
        "linux_release.x86_64",
        "linux_debug.x86_64",
        "android_debug.apk",
        "android_release.apk",
    ] {
        std::fs::write(templates.join(template), b"template").expect("template");
    }
    DataHome {
        _directory: directory,
        root,
    }
}

/// A fake Android SDK and JDK whose tools are the `qa-fake-adb` stand-in.
struct FakeAndroid {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl FakeAndroid {
    fn new(scenario: &str) -> Self {
        let directory = tempfile::tempdir().expect("fake android");
        let root = directory.path().canonicalize().expect("canonical");
        let binary = Path::new(env!("CARGO_BIN_EXE_qa-fake-adb"));
        for tool in [
            "sdk/platform-tools/adb",
            "sdk/build-tools/36.0.0/apksigner",
            "jdk/bin/java",
            "jdk/bin/keytool",
        ] {
            let path = root.join(tool);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("tool directory");
            std::os::unix::fs::symlink(binary, &path).expect("tool link");
        }
        let fake = Self {
            _directory: directory,
            root,
        };
        fake.set(scenario);
        fake
    }

    fn sdk(&self) -> PathBuf {
        self.root.join("sdk")
    }

    fn jdk(&self) -> PathBuf {
        self.root.join("jdk")
    }

    /// Choose the scenario every later tool run plays.
    fn set(&self, configuration: &str) {
        std::fs::write(
            self.root.join("fake-android.conf"),
            format!("{}\n", configuration.replace(';', "\n")),
        )
        .expect("configuration");
    }

    /// Every tool run so far, one line each: `<tool> HOME=<home> <arguments>`.
    fn log(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("fake-android.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn runs_of(&self, tool: &str) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|line| line.split(' ').next() == Some(tool))
            .collect()
    }

    fn file(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(name)).ok()
    }
}

fn server_command(tree: &ProjectTree, data: &DataHome, android: Option<&FakeAndroid>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_qa-journal-foundation"));
    command
        .arg("--project")
        .arg(&tree.root)
        .arg("--profile")
        .arg("full")
        .arg("--godot-binary")
        .arg(fake_godot())
        .env("XDG_DATA_HOME", &data.root)
        .env_remove("GODOT_MCP_TEST_LAUNCH_CRASH_STAGE")
        .env_remove("QA_FAKE_ADB_SCENARIO");
    if let Some(android) = android {
        command
            .arg("--android-sdk")
            .arg(android.sdk())
            .arg("--android-java-home")
            .arg(android.jdk());
    }
    command
}

fn server(tree: &ProjectTree, data: &DataHome, android: Option<&FakeAndroid>) -> StdioSession {
    StdioSession::handshake(StdioSession::start_command(server_command(
        tree, data, android,
    )))
    .0
}

fn project(label: &str) -> ProjectTree {
    let tree = ProjectTree::new(label);
    tree.write("export_presets.cfg", PRESETS);
    tree.write(
        "main.tscn",
        "[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node2D\"]\n",
    );
    tree.write("data/levels.json", "{\"levels\": 3}\n");
    tree
}

fn behave(tree: &ProjectTree, behaviour: &str) {
    tree.write("fake-game.conf", &format!("{behaviour}\n"));
}

fn structured(response: &Value) -> Value {
    assert!(
        response.get("error").is_none() && response["result"]["isError"] != json!(true),
        "the call must succeed: {response}"
    );
    response["result"]["structuredContent"].clone()
}

/// A tagged Godot value of an error's details, as plain JSON.
fn untag(value: &Value) -> Value {
    match (
        value.get("type").and_then(Value::as_str),
        value.get("value"),
    ) {
        (Some("array"), Some(Value::Array(items))) => {
            Value::Array(items.iter().map(untag).collect())
        }
        (Some("dictionary"), Some(Value::Object(entries))) => Value::Object(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), untag(value)))
                .collect::<Map<String, Value>>(),
        ),
        (Some("null"), _) => Value::Null,
        (Some(_), Some(inner)) => inner.clone(),
        _ => value.clone(),
    }
}

/// The details of a refused call, untagged.
fn details(response: &Value) -> Value {
    let error = if response.get("error").is_some() {
        &response["error"]["data"]
    } else {
        &response["result"]["structuredContent"]
    };
    match &error["details"] {
        Value::Object(entries) => Value::Object(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), untag(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn message(response: &Value) -> String {
    if response.get("error").is_some() {
        response["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    } else {
        response["result"]["structuredContent"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }
}

fn start(session: &mut StdioSession, id: u64, preset: &str, extra: Value) -> Value {
    let mut arguments = json!({ "preset": preset, "idempotencyId": format!("export-{id}") });
    if let (Value::Object(arguments), Value::Object(extra)) = (&mut arguments, extra) {
        arguments.extend(extra);
    }
    session.call_tool(id, "project.export", arguments)
}

/// Poll the export's status until it leaves `running`.
fn ended(session: &mut StdioSession, first_id: u64, export: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut id = first_id;
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

/// Export `preset` and wait for its end; answers the export ID and status.
fn export(session: &mut StdioSession, id: u64, preset: &str, extra: Value) -> (String, Value) {
    let started = structured(&start(session, id, preset, extra));
    let export = started["export"]["session"]
        .as_str()
        .expect("export id")
        .to_owned();
    let status = ended(session, id * 100, &export);
    (export, status)
}

fn read_artifact(session: &mut StdioSession, id: u64, uri: &str) -> Vec<u8> {
    let response = session.request(id, "resources/read", json!({ "uri": uri }));
    let blob = response["result"]["contents"][0]["blob"]
        .as_str()
        .unwrap_or_else(|| panic!("the artifact reads: {response}"));
    decode_base64(blob)
}

fn decode_base64(text: &str) -> Vec<u8> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buffer = 0_u32;
    let mut bits = 0;
    for byte in text.bytes().filter(|byte| *byte != b'=') {
        let value = TABLE
            .iter()
            .position(|candidate| *candidate == byte)
            .expect("base64 digit") as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    out
}

fn entry_paths(pack: &Value) -> Vec<String> {
    pack["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|entry| entry["path"].as_str().expect("path").to_owned())
        .collect()
}

/// The session directories the server keeps.
fn session_dirs(tree: &ProjectTree) -> Vec<PathBuf> {
    tree.state_dirs()
        .iter()
        .flat_map(|state| {
            std::fs::read_dir(state.join("sessions"))
                .into_iter()
                .flatten()
        })
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect()
}

#[test]
fn a_pack_export_answers_the_file_table_it_read_back() {
    let tree = project("pack");
    let data = data_home();
    let mut session = server(&tree, &data, None);

    let (_, status) = export(&mut session, 2, "Linux", json!({ "kind": "pack" }));
    assert_eq!(status["export"]["outcome"], json!("succeeded"), "{status}");
    assert_eq!(status["export"]["kind"], json!("pack"));
    assert_eq!(status["export"]["template"], Value::Null, "{status}");
    let outputs = status["export"]["outputs"].as_array().expect("outputs");
    assert_eq!(outputs.len(), 1, "{status}");
    assert_eq!(outputs[0]["name"], json!("game.pck"));
    let pack = &status["export"]["pack"];
    assert_eq!(pack["format"], json!("pck"), "{status}");
    assert_eq!(pack["readable"], json!(true));
    assert_eq!(pack["format_version"], json!(4));
    assert_eq!(pack["engine_version"], json!("4.7.1"));
    assert_eq!(
        entry_paths(pack),
        ["data/levels.json", "main.tscn", "project.binary"],
        "{status}"
    );
    assert!(
        pack["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .all(|entry| entry["digest"] == json!("verified"))
    );
    assert_eq!(pack["truncated"], json!(false));
    // The artifact holds exactly the pack the facts describe.
    let uri = outputs[0]["artifact"].as_str().expect("uri").to_owned();
    let bytes = read_artifact(&mut session, 20, &uri);
    assert_eq!(&bytes[..4], b"GDPC");
    assert_eq!(
        sha256_hex(&bytes),
        outputs[0]["sha256"].as_str().expect("sha")
    );

    let (_, zipped) = export(
        &mut session,
        3,
        "Linux",
        json!({ "kind": "pack", "packFormat": "zip" }),
    );
    assert_eq!(zipped["export"]["outcome"], json!("succeeded"), "{zipped}");
    assert_eq!(zipped["export"]["outputs"][0]["name"], json!("game.zip"));
    assert_eq!(zipped["export"]["pack"]["format"], json!("zip"));
    assert_eq!(
        entry_paths(&zipped["export"]["pack"]),
        ["data/levels.json", "main.tscn", "project.binary"]
    );

    // A damaged pack is never a result.
    behave(&tree, "pack:corrupt");
    let (_, damaged) = export(&mut session, 4, "Linux", json!({ "kind": "pack" }));
    assert_eq!(damaged["export"]["outcome"], json!("failed"), "{damaged}");
    assert!(
        damaged["export"]["failure"]
            .as_str()
            .is_some_and(|failure| failure.contains("damaged")),
        "{damaged}"
    );
    assert_eq!(damaged["export"]["outputs"], json!([]));
    assert_eq!(damaged["export"]["pack"], Value::Null);
    let _ = session.finish();
}

#[test]
fn what_a_pack_or_patch_cannot_be_is_refused_before_anything_runs() {
    let tree = project("pack-refusals");
    tree.write("not-a-pack.pck", "plain text, not a pack\n");
    let data = data_home();
    let mut session = server(&tree, &data, None);
    let refusals = [
        (
            json!({ "kind": "pack", "mode": "debug" }),
            "invalid_input",
            "debug",
        ),
        (
            json!({ "kind": "pack", "smoke": true }),
            "invalid_input",
            "smoke",
        ),
        (json!({ "kind": "patch" }), "invalid_input", "patches"),
        (
            json!({ "packFormat": "zip" }),
            "invalid_input",
            "packFormat",
        ),
        (
            json!({ "kind": "patch", "patches": ["base.pck"], "packFormat": "zip" }),
            "invalid_input",
            "packFormat",
        ),
        (
            json!({ "kind": "pack", "patches": ["base.pck"] }),
            "invalid_input",
            "patches",
        ),
        (json!({ "kind": "bundle" }), "invalid_input", "kind"),
        (
            json!({ "kind": "patch", "patches": ["missing.pck"] }),
            "not_found",
            "missing.pck",
        ),
        (
            json!({ "kind": "patch", "patches": ["res://not-a-pack.pck"] }),
            "invalid_input",
            "not a PCK",
        ),
        (
            json!({ "kind": "patch", "patches": ["/etc/passwd"] }),
            "invalid_input",
            "project-relative",
        ),
        (
            json!({ "kind": "patch", "patches": ["../outside.pck"] }),
            "invalid_input",
            "project-relative",
        ),
        (
            json!({ "kind": "patch", "patches": [".godot-mcp/journal.db"] }),
            "invalid_input",
            "private state",
        ),
        (
            json!({ "kind": "patch", "patches": [format!("godot-mcp://artifacts/artifact_{}", "0".repeat(64))] }),
            "not_found",
            "export of yours",
        ),
    ];
    for (index, (extra, code, says)) in refusals.into_iter().enumerate() {
        let id = 2 + index as u64;
        let refused = start(&mut session, id, "Linux", extra.clone());
        assert_eq!(error_code(&refused), json!(code), "{extra}: {refused}");
        assert!(message(&refused).contains(says), "{extra}: {refused}");
    }
    // Nothing was launched, and nothing was left behind.
    assert!(!tree.root.join("fake-export.started").exists());
    assert!(session_dirs(&tree).is_empty(), "{:?}", session_dirs(&tree));
    let listed = structured(&session.call_tool(90, "project.export.status", json!({})));
    assert_eq!(listed["exports"], json!([]), "{listed}");
    let _ = session.finish();
}

/// Wait, at most 60 s, until `path` exists.
fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Every Godot build writes temporaries of an export under fixed names in
/// the host's temporary directory (measured: `/tmp/tmpproject.binary` on
/// 4.4.1 to 4.7.1, and `/tmp/packtmp`, every packed file, on 4.4.1), so two
/// exports at once could read each other's: the server runs one at a time.
#[test]
fn a_second_export_is_refused_while_one_runs() {
    let tree = project("one-export");
    let data = data_home();
    behave(&tree, "export:hang");
    let mut session = server(&tree, &data, None);
    let first = structured(&start(&mut session, 2, "Linux", json!({})));
    let first = first["export"]["session"]
        .as_str()
        .expect("export id")
        .to_owned();
    wait_for(&tree.root.join("fake-export.started"));
    std::fs::remove_file(tree.root.join("fake-export.started")).expect("marker");

    for (id, extra) in [(3, json!({ "kind": "pack" })), (4, json!({}))] {
        let refused = start(&mut session, id, "Linux", extra.clone());
        assert_eq!(
            error_code(&refused),
            json!("unavailable"),
            "{extra}: {refused}"
        );
        assert!(message(&refused).contains(&first), "{extra}: {refused}");
        let retryable = if refused.get("error").is_some() {
            refused["error"]["data"]["retryable"].clone()
        } else {
            refused["result"]["structuredContent"]["retryable"].clone()
        };
        assert_eq!(retryable, json!(true), "{refused}");
    }
    // The refused exports launched nothing and kept nothing.
    assert!(!tree.root.join("fake-export.started").exists());
    assert_eq!(session_dirs(&tree).len(), 1, "{:?}", session_dirs(&tree));

    let cancelled = structured(&session.call_tool(
        5,
        "project.export.cancel",
        json!({ "export": first, "idempotencyId": "cancel-5" }),
    ));
    assert_eq!(cancelled["state"], json!("cancelled"), "{cancelled}");
    behave(&tree, "probe");
    let (_, status) = export(&mut session, 6, "Linux", json!({ "kind": "pack" }));
    assert_eq!(status["export"]["outcome"], json!("succeeded"), "{status}");
    let _ = session.finish();
}

#[test]
fn a_patch_against_an_earlier_export_counts_what_it_changed() {
    let tree = project("patch");
    let data = data_home();
    let mut session = server(&tree, &data, None);
    let (_, base) = export(&mut session, 2, "Linux", json!({ "kind": "pack" }));
    let base_output = &base["export"]["outputs"][0];
    let base_uri = base_output["artifact"].as_str().expect("uri").to_owned();
    let base_sha = base_output["sha256"].as_str().expect("sha").to_owned();

    // Nothing changed since the base: Godot exports nothing, and says so.
    let (_, same) = export(
        &mut session,
        3,
        "Linux",
        json!({ "kind": "patch", "patches": [base_uri] }),
    );
    assert_eq!(same["export"]["outcome"], json!("failed"), "{same}");
    assert_eq!(same["export"]["nothing_changed"], json!(true), "{same}");
    assert!(
        same["export"]["failure"]
            .as_str()
            .is_some_and(|failure| failure.contains("No files or changes to export")),
        "{same}"
    );
    assert_eq!(same["export"]["outputs"], json!([]), "{same}");
    assert_eq!(same["export"]["bases"][0]["sha256"], json!(base_sha));

    // One changed file and one new one.
    tree.write(
        "main.tscn",
        "[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node3D\"]\n",
    );
    tree.write("data/extra.json", "[]\n");
    let (_, patched) = export(
        &mut session,
        4,
        "Linux",
        json!({ "kind": "patch", "patches": [base_uri] }),
    );
    assert_eq!(
        patched["export"]["outcome"],
        json!("succeeded"),
        "{patched}"
    );
    assert_eq!(patched["export"]["kind"], json!("patch"));
    assert_eq!(patched["export"]["nothing_changed"], json!(false));
    let bases = patched["export"]["bases"].as_array().expect("bases");
    assert_eq!(bases.len(), 1, "{patched}");
    assert_eq!(bases[0]["source"], json!(base_uri));
    assert_eq!(bases[0]["sha256"], json!(base_sha));
    let pack = &patched["export"]["pack"];
    assert_eq!(
        entry_paths(pack),
        ["data/extra.json", "main.tscn"],
        "{patched}"
    );
    assert_eq!(pack["patch"]["new"], json!(1), "{patched}");
    assert_eq!(pack["patch"]["changed"], json!(1), "{patched}");
    assert_eq!(pack["patch"]["identical"], json!(0));
    assert_eq!(pack["patch"]["changed_files_only"], json!(true));
    assert_eq!(pack["entries"][0]["change"], json!("new"));
    assert_eq!(pack["entries"][1]["change"], json!("changed"));

    // A base pack in the project serves as well as an artifact.
    let bytes = read_artifact(&mut session, 50, &base_uri);
    std::fs::create_dir_all(tree.root.join("packs")).expect("packs");
    std::fs::write(tree.root.join("packs/base.pck"), &bytes).expect("base in the project");
    behave(&tree, "patch:identical");
    let (_, flagged) = export(
        &mut session,
        5,
        "Linux",
        json!({ "kind": "patch", "patches": ["res://packs/base.pck"] }),
    );
    assert_eq!(
        flagged["export"]["outcome"],
        json!("succeeded"),
        "{flagged}"
    );
    assert_eq!(
        flagged["export"]["bases"][0]["source"],
        json!("res://packs/base.pck")
    );
    let summary = &flagged["export"]["pack"]["patch"];
    assert_eq!(summary["identical"], json!(1), "{flagged}");
    assert_eq!(
        summary["identical_paths"],
        json!(["data/levels.json"]),
        "{flagged}"
    );
    assert_eq!(summary["changed_files_only"], json!(false));
    // The private copies of the base packs went with the exports.
    for directory in session_dirs(&tree) {
        assert!(!directory.join("bases").exists(), "{}", directory.display());
    }
    let _ = session.finish();
}

#[test]
fn an_android_debug_export_is_signed_by_the_servers_one_debug_key_and_read_back() {
    let tree = project("android-debug");
    let data = data_home();
    let android = FakeAndroid::new("scenario=success");
    let mut session = server(&tree, &data, Some(&android));

    let (export_id, status) = export(&mut session, 2, "Android", json!({ "mode": "debug" }));
    assert_eq!(status["export"]["outcome"], json!("succeeded"), "{status}");
    assert_eq!(status["export"]["template"], json!("android_debug.apk"));
    let outputs = status["export"]["outputs"].as_array().expect("outputs");
    assert_eq!(outputs.len(), 1, "{status}");
    assert_eq!(outputs[0]["name"], json!("game.apk"));
    let apk = &status["export"]["apk"];
    assert_eq!(apk["package"], json!("org.godotmcp.deploy"), "{status}");
    assert_eq!(apk["launcher"], json!("com.godot.game.GodotAppLauncher"));
    assert_eq!(apk["launcher_kind"], json!("activity-alias"));
    assert_eq!(apk["target_activity"], json!("com.godot.game.GodotApp"));
    assert_eq!(apk["debuggable"], json!(true));
    assert_eq!(apk["abis"], json!(["arm64-v8a"]));
    assert_eq!(apk["signature"]["verified"], json!(true));
    assert_eq!(apk["signature"]["verifier"], json!("apksigner 36.0.0"));
    assert_eq!(
        apk["signature"]["keystore"],
        json!("server-debug"),
        "{status}"
    );
    assert_eq!(
        apk["signature"]["signer_sha256"],
        json!("6f8b7d5b2f0e3c1a9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f4a3b2c1d0e9f8a7b6c")
    );
    // apksigner ran with the JDK on PATH, never with the server's home.
    let signer = android.runs_of("apksigner");
    assert_eq!(signer.len(), 1, "{:?}", android.log());
    assert!(signer[0].contains("verify --print-certs"), "{signer:?}");
    // The export's private home is gone with it.
    for directory in session_dirs(&tree) {
        for staged in ["android-home", "android-sdk", "scratch"] {
            assert!(
                !directory.join(staged).exists(),
                "{}/{staged}",
                directory.display()
            );
        }
    }
    assert!(
        !status
            .to_string()
            .contains(&tree.base.display().to_string()),
        "{status}"
    );

    // A second debug export signs with the same key: keytool ran once.
    let (_, again) = export(&mut session, 3, "Android", json!({ "mode": "debug" }));
    assert_eq!(again["export"]["outcome"], json!("succeeded"), "{again}");
    assert_eq!(android.runs_of("keytool").len(), 1, "{:?}", android.log());
    assert!(!export_id.is_empty());

    // An APK apksigner does not accept is not a result: Godot 4.7.1 exits 0
    // with an unsigned APK when it cannot sign.
    android.set("scenario=success;apksigner=fail");
    let (_, unsigned) = export(&mut session, 4, "Android", json!({ "mode": "debug" }));
    assert_eq!(unsigned["export"]["outcome"], json!("failed"), "{unsigned}");
    assert!(
        unsigned["export"]["failure"]
            .as_str()
            .is_some_and(|failure| failure.contains("does not verify")),
        "{unsigned}"
    );
    assert_eq!(unsigned["export"]["outputs"], json!([]));
    android.set("scenario=success");
    behave(&tree, "apk:broken");
    let (_, broken) = export(&mut session, 5, "Android", json!({ "mode": "debug" }));
    assert_eq!(broken["export"]["outcome"], json!("failed"), "{broken}");
    assert!(
        broken["export"]["failure"]
            .as_str()
            .is_some_and(|failure| failure.contains("not accepted")),
        "{broken}"
    );
    let _ = session.finish();
}

#[test]
fn an_android_export_without_what_it_needs_is_refused_before_launch() {
    let tree = project("android-refusals");
    let data = data_home();
    // No SDK or JDK configured.
    let mut session = server(&tree, &data, None);
    let refused = start(&mut session, 2, "Android", json!({ "mode": "debug" }));
    assert_eq!(error_code(&refused), json!("unavailable"), "{refused}");
    assert!(message(&refused).contains("[android]"), "{refused}");
    let gradle = start(&mut session, 3, "Gradle", json!({ "mode": "debug" }));
    assert_eq!(error_code(&gradle), json!("unsupported"), "{gradle}");
    assert!(message(&gradle).contains("Gradle"), "{gradle}");
    let _ = session.finish();

    let android = FakeAndroid::new("scenario=success;keytool=fail");
    let mut session = server(&tree, &data, Some(&android));
    let release = start(&mut session, 2, "Android", json!({}));
    assert_eq!(error_code(&release), json!("unavailable"), "{release}");
    assert!(message(&release).contains("keystore/release"), "{release}");
    let keytool = start(&mut session, 3, "Android", json!({ "mode": "debug" }));
    assert_eq!(error_code(&keytool), json!("unavailable"), "{keytool}");
    assert!(message(&keytool).contains("keytool"), "{keytool}");
    assert!(!tree.root.join("fake-export.started").exists());
    assert!(session_dirs(&tree).is_empty(), "{:?}", session_dirs(&tree));

    // With the release credentials in Godot's store, a release export runs;
    // the password is never shown.
    tree.write(".godot/export_credentials.cfg", CREDENTIALS);
    let (_, signed) = export(&mut session, 4, "Android", json!({}));
    assert_eq!(signed["export"]["outcome"], json!("succeeded"), "{signed}");
    assert_eq!(
        signed["export"]["apk"]["debuggable"],
        json!(false),
        "{signed}"
    );
    assert_eq!(
        signed["export"]["apk"]["signature"]["keystore"],
        json!("project-release"),
        "{signed}"
    );
    assert_eq!(
        android.runs_of("keytool").len(),
        1,
        "only the refused debug export ran it"
    );
    assert!(!session.transcript.contains("never-echoed-release-password"));
    let finished = session.finish();
    assert!(!finished.stderr.contains("never-echoed-release-password"));
}

/// A succeeded Android debug export on `session`.
fn android_export(session: &mut StdioSession, id: u64) -> String {
    let (export, status) = export(session, id, "Android", json!({ "mode": "debug" }));
    assert_eq!(status["export"]["outcome"], json!("succeeded"), "{status}");
    export
}

fn deploy(session: &mut StdioSession, id: u64, export: &str, extra: Value) -> Value {
    let mut arguments = json!({
        "export": export,
        "serial": "emulator-5554",
        "runningTimeoutMs": 1000,
        "idempotencyId": format!("deploy-{id}"),
    });
    if let (Value::Object(arguments), Value::Object(extra)) = (&mut arguments, extra) {
        arguments.extend(extra);
    }
    session.call_tool(id, "android.deploy", arguments)
}

fn phase_outcomes(phases: &Value) -> Vec<(String, String)> {
    phases
        .as_array()
        .expect("phases")
        .iter()
        .map(|phase| {
            (
                phase["phase"].as_str().expect("phase").to_owned(),
                phase["outcome"].as_str().expect("outcome").to_owned(),
            )
        })
        .collect()
}

fn outcomes(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(phase, outcome)| ((*phase).to_owned(), (*outcome).to_owned()))
        .collect()
}

#[test]
fn the_device_list_is_what_adb_reports() {
    let tree = project("devices");
    let data = data_home();
    let mut session = server(&tree, &data, None);
    let missing = session.call_tool(2, "android.devices.list", json!({}));
    assert_eq!(error_code(&missing), json!("unavailable"), "{missing}");
    let _ = session.finish();

    let android = FakeAndroid::new("scenario=success");
    let mut session = server(&tree, &data, Some(&android));
    let listed = structured(&session.call_tool(2, "android.devices.list", json!({})));
    assert_eq!(listed["started_server"], json!(true), "{listed}");
    let devices = listed["devices"].as_array().expect("devices");
    assert_eq!(devices.len(), 1, "{listed}");
    assert_eq!(devices[0]["serial"], json!("emulator-5554"));
    assert_eq!(devices[0]["state"], json!("device"));
    assert_eq!(devices[0]["ready"], json!(true));
    assert_eq!(devices[0]["model"], json!("sdk_gphone64_x86_64"));
    assert_eq!(devices[0]["transport"], json!("emulator"));
    // The server already runs now: adb says nothing about starting it.
    let again = structured(&session.call_tool(3, "android.devices.list", json!({})));
    assert_eq!(again["started_server"], json!(false), "{again}");
    // adb runs with the server's private adb home, never the user's.
    let adb = android.runs_of("adb");
    assert!(
        adb.iter().all(|line| line.contains("/android/adb-home ")),
        "{adb:?}"
    );

    android.set("scenario=unauthorized");
    let unauthorized = structured(&session.call_tool(4, "android.devices.list", json!({})));
    assert_eq!(unauthorized["devices"][0]["state"], json!("unauthorized"));
    assert_eq!(unauthorized["devices"][0]["ready"], json!(false));
    assert_eq!(unauthorized["devices"][0]["transport"], json!("usb"));
    android.set("scenario=none");
    let none = structured(&session.call_tool(5, "android.devices.list", json!({})));
    assert_eq!(none["devices"], json!([]), "{none}");
    let _ = session.finish();
}

#[test]
fn a_deploy_succeeds_only_when_every_phase_did() {
    let tree = project("deploy");
    let data = data_home();
    let android = FakeAndroid::new("scenario=success");
    let mut session = server(&tree, &data, Some(&android));
    let export = android_export(&mut session, 2);

    let deployed = structured(&deploy(
        &mut session,
        10,
        &export,
        json!({ "logcatLines": 2 }),
    ));
    assert_eq!(deployed["installed"], json!(true), "{deployed}");
    assert_eq!(deployed["package"], json!("org.godotmcp.deploy"));
    assert_eq!(
        deployed["component"],
        json!("org.godotmcp.deploy/com.godot.game.GodotAppLauncher")
    );
    assert_eq!(deployed["pid"], json!(4242));
    assert_eq!(deployed["failed_phase"], Value::Null);
    assert_eq!(deployed["mutation_status"], json!("committed"));
    assert_eq!(
        phase_outcomes(&deployed["phases"]),
        outcomes(&[
            ("device", "succeeded"),
            ("install", "succeeded"),
            ("launch", "succeeded"),
            ("running", "succeeded"),
            ("logcat", "succeeded"),
        ]),
        "{deployed}"
    );
    let lines = deployed["logcat"]["lines"].as_array().expect("lines");
    assert_eq!(lines.len(), 2, "{deployed}");
    assert!(
        lines[1]
            .as_str()
            .is_some_and(|line| line.contains("fake game ready"))
    );
    assert_eq!(
        android.file("fake-android.installed").as_deref(),
        Some("org.godotmcp.deploy")
    );
    assert_eq!(
        android.file("fake-android.launched").as_deref(),
        Some("org.godotmcp.deploy/com.godot.game.GodotAppLauncher")
    );
    let installs = android
        .runs_of("adb")
        .into_iter()
        .filter(|line| line.contains(" install -r "))
        .count();
    assert_eq!(installs, 1);
    // The same request again is the recorded deploy, not a second install.
    let replayed = structured(&deploy(
        &mut session,
        10,
        &export,
        json!({ "logcatLines": 2 }),
    ));
    assert_eq!(replayed["replayed"], json!(true), "{replayed}");
    let installs_after = android
        .runs_of("adb")
        .into_iter()
        .filter(|line| line.contains(" install -r "))
        .count();
    assert_eq!(installs_after, 1, "{:?}", android.log());
    let _ = session.finish();
}

/// One failing deploy: the fake's scenario, the outcome code, the phase it
/// stops at, whether the APK is installed, the mutation status, and every
/// phase's outcome.
type FailureCase<'a> = (
    &'a str,
    &'a str,
    &'a str,
    bool,
    &'a str,
    &'a [(&'a str, &'a str)],
);

#[test]
fn a_deploy_that_fails_names_its_phase_and_what_ran() {
    let tree = project("deploy-phases");
    let data = data_home();
    let android = FakeAndroid::new("scenario=success");
    let mut session = server(&tree, &data, Some(&android));
    let export = android_export(&mut session, 2);
    let cases: [FailureCase<'_>; 7] = [
        (
            "none",
            "not_found",
            "device",
            false,
            "not_started",
            &[
                ("device", "failed"),
                ("install", "not_run"),
                ("launch", "not_run"),
                ("running", "not_run"),
                ("logcat", "not_run"),
            ],
        ),
        (
            "unauthorized",
            "unavailable",
            "device",
            false,
            "not_started",
            &[
                ("device", "failed"),
                ("install", "not_run"),
                ("launch", "not_run"),
                ("running", "not_run"),
                ("logcat", "not_run"),
            ],
        ),
        (
            "offline",
            "unavailable",
            "device",
            false,
            "not_started",
            &[
                ("device", "failed"),
                ("install", "not_run"),
                ("launch", "not_run"),
                ("running", "not_run"),
                ("logcat", "not_run"),
            ],
        ),
        (
            "install-failure",
            "conflict",
            "install",
            false,
            "not_started",
            &[
                ("device", "succeeded"),
                ("install", "failed"),
                ("launch", "not_run"),
                ("running", "not_run"),
                ("logcat", "not_run"),
            ],
        ),
        (
            "launch-failure",
            "unavailable",
            "launch",
            true,
            "committed",
            &[
                ("device", "succeeded"),
                ("install", "succeeded"),
                ("launch", "failed"),
                ("running", "not_run"),
                ("logcat", "not_run"),
            ],
        ),
        (
            "no-process",
            "timeout",
            "running",
            true,
            "committed",
            &[
                ("device", "succeeded"),
                ("install", "succeeded"),
                ("launch", "succeeded"),
                ("running", "failed"),
                ("logcat", "not_run"),
            ],
        ),
        (
            "logcat-failure",
            "unavailable",
            "logcat",
            true,
            "committed",
            &[
                ("device", "succeeded"),
                ("install", "succeeded"),
                ("launch", "succeeded"),
                ("running", "succeeded"),
                ("logcat", "failed"),
            ],
        ),
    ];
    for (index, (scenario, code, phase, installed, status, expected)) in
        cases.into_iter().enumerate()
    {
        android.set(&format!("scenario={scenario}"));
        let refused = deploy(&mut session, 20 + index as u64, &export, json!({}));
        assert_eq!(error_code(&refused), json!(code), "{scenario}: {refused}");
        assert!(
            message(&refused).contains(&format!("at the {phase} phase")),
            "{scenario}: {refused}"
        );
        let facts = details(&refused);
        assert_eq!(facts["failed_phase"], json!(phase), "{scenario}: {facts}");
        assert_eq!(facts["installed"], json!(installed), "{scenario}: {facts}");
        assert_eq!(
            facts["mutation_status"],
            json!(status),
            "{scenario}: {facts}"
        );
        assert_eq!(
            phase_outcomes(&facts["phases"]),
            outcomes(expected),
            "{scenario}: {facts}"
        );
        // Each phase that ran carries its bounded adb output.
        let ran = facts["phases"]
            .as_array()
            .expect("phases")
            .iter()
            .find(|reported| reported["phase"] == json!(phase))
            .expect("the failed phase is reported")
            .clone();
        assert!(ran["exit"].is_string(), "{scenario}: {ran}");
    }
    // A device that adb never reached got no install.
    let installs = android
        .runs_of("adb")
        .into_iter()
        .filter(|line| line.contains(" install -r "))
        .count();
    assert_eq!(installs, 4, "{:?}", android.log());

    // An install adb gave no verdict for is uncertain, never a failure that
    // proves nothing changed.
    android.set("scenario=install-lost");
    let lost = deploy(&mut session, 40, &export, json!({}));
    assert_eq!(error_code(&lost), json!("uncertain"), "{lost}");
    assert_eq!(
        details(&lost)["mutation_status"],
        json!("unresolved"),
        "{lost}"
    );
    assert_eq!(details(&lost)["failed_phase"], json!("install"));
    // Whether the device installed it is unknown, so `installed` says so.
    assert_eq!(details(&lost)["installed"], Value::Null, "{lost}");
    let _ = session.finish();
}

#[test]
fn a_deploy_of_what_is_not_a_finished_android_export_is_refused_unsent() {
    let tree = project("deploy-refusals");
    let data = data_home();
    let android = FakeAndroid::new("scenario=success");
    let mut session = server(&tree, &data, Some(&android));
    let (pack, _) = export(&mut session, 2, "Linux", json!({ "kind": "pack" }));
    let not_android = deploy(&mut session, 10, &pack, json!({}));
    assert_eq!(
        error_code(&not_android),
        json!("invalid_input"),
        "{not_android}"
    );
    let unknown = deploy(
        &mut session,
        11,
        &format!("export_{}", "0".repeat(32)),
        json!({}),
    );
    assert_eq!(error_code(&unknown), json!("not_found"), "{unknown}");
    let export = android_export(&mut session, 3);
    let bad_serial = deploy(&mut session, 12, &export, json!({ "serial": "a;reboot" }));
    assert_eq!(
        error_code(&bad_serial),
        json!("invalid_input"),
        "{bad_serial}"
    );
    behave(&tree, "apk:no-launcher");
    let quiet = android_export(&mut session, 4);
    let no_launcher = deploy(&mut session, 13, &quiet, json!({}));
    assert_eq!(
        error_code(&no_launcher),
        json!("unsupported"),
        "{no_launcher}"
    );
    assert!(message(&no_launcher).contains("launcher"), "{no_launcher}");
    assert!(android.runs_of("adb").is_empty(), "{:?}", android.log());
    let _ = session.finish();

    // Without a configured SDK there is no adb to deploy with.
    let mut session = server(&tree, &data, None);
    let missing = deploy(&mut session, 2, &export, json!({}));
    assert_eq!(error_code(&missing), json!("unavailable"), "{missing}");
    let _ = session.finish();
}

#[test]
fn a_deploy_a_dead_server_left_mid_install_is_reconciled_as_uncertain() {
    let tree = project("deploy-crash");
    let data = data_home();
    let android = FakeAndroid::new("scenario=success");
    let mut session = server(&tree, &data, Some(&android));
    let export = android_export(&mut session, 2);
    android.set("scenario=install-hang");
    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "android.deploy",
            "arguments": {
                "export": export,
                "serial": "emulator-5554",
                "idempotencyId": "deploy-crash",
            }
        }
    }));
    let pid_file = android.root.join("fake-android.hang-pid");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !pid_file.exists() {
        assert!(Instant::now() < deadline, "the install never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    // The server dies while adb is installing.
    session.kill();
    std::fs::write(android.root.join("fake-android.release"), b"").expect("release adb");
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .expect("pid")
        .trim()
        .parse()
        .expect("a pid");
    let deadline = Instant::now() + Duration::from_secs(30);
    while Path::new(&format!("/proc/{pid}")).exists() {
        assert!(Instant::now() < deadline, "the fake adb never ended");
        std::thread::sleep(Duration::from_millis(20));
    }

    // The next server proves the project untouched but cannot know what the
    // device did: the recorded deploy is uncertain, and is never sent again.
    android.set("scenario=success");
    let mut session = server(&tree, &data, Some(&android));
    let replayed = session.call_tool(
        2,
        "android.deploy",
        json!({
            "export": export,
            "serial": "emulator-5554",
            "idempotencyId": "deploy-crash",
        }),
    );
    assert_eq!(error_code(&replayed), json!("uncertain"), "{replayed}");
    assert!(message(&replayed).contains("device"), "{replayed}");
    let installs = android
        .runs_of("adb")
        .into_iter()
        .filter(|line| line.contains(" install -r "))
        .count();
    assert_eq!(installs, 1, "{:?}", android.log());
    // The project stays writable: nothing of it was in doubt.
    let written = session.call_tool(
        3,
        "file.create",
        json!({ "path": "after.txt", "content": "ok\n", "idempotencyId": "after-crash" }),
    );
    assert!(written["result"]["isError"] != json!(true), "{written}");
    let _ = session.finish();
}
