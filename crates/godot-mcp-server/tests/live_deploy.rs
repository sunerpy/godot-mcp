//! Pack, patch and Android exports through real Godot builds.
//!
//! Each case runs the production stdio server with one real editor build of
//! `GODOT_MCP_TEST_EDITORS` (or `GODOT_MCP_LOCKED_ARTIFACT`) as its Godot
//! executable, once per build. The server runs Godot in its own private home
//! and reads nothing of the user's Godot settings but the installed export
//! templates. No case runs `adb`: deploys are exercised against the
//! non-shipping fake in `deploy_sessions.rs`, and against a real device not at
//! all.
//!
//! The Android case exports a real APK when the build's export templates are
//! installed and `GODOT_MCP_TEST_ANDROID_SDK` and
//! `GODOT_MCP_TEST_ANDROID_JAVA_HOME` name an Android SDK and a JDK; without
//! templates it proves the export is refused naming them, and without an SDK
//! that it is refused naming the SDK.
#![cfg(target_os = "linux")]

#[path = "support/live.rs"]
mod live;
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use godot_mcp_server::rmcp_adapter::ARTIFACT_RANGE_META_KEY;
use serde_json::{Value, json};
use support::{ProjectTree, StdioSession, error_code, sha256_hex};

use live::{Ids, ok};

pub const SDK: &str = "GODOT_MCP_TEST_ANDROID_SDK";
pub const JAVA_HOME: &str = "GODOT_MCP_TEST_ANDROID_JAVA_HOME";

/// Every Godot build writes temporaries of an export under fixed names in the
/// host's temporary directory, so two of this suite's servers must not
/// export at once.
static ONE_EXPORT: Mutex<()> = Mutex::new(());

/// The longest one real export may take on a loaded host.
const EXPORT_DEADLINE: Duration = Duration::from_secs(600);
/// The default largest range one artifact read answers.
const READ_CHUNK: u64 = 8 * 1024 * 1024;

const PRESETS: &str = r#"[preset.0]

name="Linux"
platform="Linux"
runnable=true
export_filter="all_resources"
include_filter="data/*.json"
exclude_filter=""
export_path="build/game.x86_64"

[preset.0.options]

custom_template/debug=""
custom_template/release=""
binary_format/embed_pck=false
binary_format/architecture="x86_64"

[preset.1]

name="Android"
platform="Android"
runnable=true
export_filter="all_resources"
include_filter="data/*.json"
exclude_filter=""
export_path="build/game.apk"

[preset.1.options]

custom_template/debug=""
custom_template/release=""
gradle_build/use_gradle_build=false
package/unique_name="org.godotmcp.live"
package/name="Live"
"#;

/// A small project: a scene, a script and two data files the presets pack.
fn project(label: &str) -> ProjectTree {
    let tree = ProjectTree::new(label);
    tree.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Live\"\n\
         run/main_scene=\"res://main.tscn\"\n\n[rendering]\n\n\
         textures/vram_compression/import_etc2_astc=true\n",
    );
    tree.write(
        "main.tscn",
        "[gd_scene load_steps=2 format=3]\n\n\
         [ext_resource type=\"Script\" path=\"res://scripts/player.gd\" id=\"1\"]\n\n\
         [node name=\"Main\" type=\"Node2D\"]\nscript = ExtResource(\"1\")\n",
    );
    tree.write(
        "scripts/player.gd",
        "extends Node2D\n\nfunc _ready() -> void:\n\tprint(\"live\")\n",
    );
    tree.write("data/levels.json", "{\"levels\": 3}\n");
    tree.write("export_presets.cfg", PRESETS);
    tree
}

/// The production server with `binary` as its Godot executable and, when
/// given, an Android SDK and JDK.
fn server(tree: &ProjectTree, binary: &Path, android: Option<(&Path, &Path)>) -> StdioSession {
    let mut command = Command::new(env!("CARGO_BIN_EXE_qa-journal-foundation"));
    command
        .arg("--project")
        .arg(&tree.root)
        .arg("--profile")
        .arg("full")
        .arg("--godot-binary")
        .arg(binary)
        .env_remove("GODOT_MCP_TEST_LAUNCH_CRASH_STAGE");
    if let Some((sdk, java_home)) = android {
        command
            .arg("--android-sdk")
            .arg(sdk)
            .arg("--android-java-home")
            .arg(java_home);
    }
    StdioSession::handshake(StdioSession::start_command(command)).0
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

/// Start an export of `preset` with `extra` arguments.
fn start(session: &mut StdioSession, ids: &mut Ids, preset: &str, extra: Value) -> Value {
    let id = ids.next();
    let mut arguments = json!({ "preset": preset, "idempotencyId": format!("live-export-{id}") });
    if let (Value::Object(arguments), Value::Object(extra)) = (&mut arguments, extra) {
        arguments.extend(extra);
    }
    session.call_tool(id, "project.export", arguments)
}

/// Export `preset` and wait for its end: the export's status.
fn export(
    session: &mut StdioSession,
    ids: &mut Ids,
    preset: &str,
    extra: Value,
    context: &str,
) -> Value {
    let started = ok(&start(session, ids, preset, extra), context);
    let export = started["export"]["session"]
        .as_str()
        .expect("export id")
        .to_owned();
    let begun = Instant::now();
    loop {
        let status = ok(
            &session.call_tool(
                ids.next(),
                "project.export.status",
                json!({ "export": export }),
            ),
            context,
        );
        if status["state"] != json!("running") && status["state"] != json!("starting") {
            eprintln!(
                "{context}: {preset} {} export ended {} in {} ms",
                status["export"]["kind"],
                status["export"]["outcome"],
                begun.elapsed().as_millis()
            );
            return status;
        }
        assert!(
            begun.elapsed() < EXPORT_DEADLINE,
            "{context}: the export never ended: {status}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Read the whole artifact `uri` of `total` bytes, range by range.
fn read_artifact(session: &mut StdioSession, ids: &mut Ids, uri: &str, total: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut start = 0;
    while start < total {
        let end = (start + READ_CHUNK).min(total) - 1;
        let response = session.request(
            ids.next(),
            "resources/read",
            json!({
                "uri": uri,
                "_meta": { ARTIFACT_RANGE_META_KEY: { "start": start, "endInclusive": end } },
            }),
        );
        let blob = response["result"]["contents"][0]["blob"]
            .as_str()
            .unwrap_or_else(|| panic!("the artifact reads: {response}"));
        bytes.extend(live::decode_base64(blob));
        start = end + 1;
    }
    bytes
}

/// The build a status names, as `4.7.1`.
fn build_of(status: &Value) -> String {
    status["export"]["template_label"]
        .as_str()
        .and_then(|label| label.strip_suffix(".stable"))
        .unwrap_or_else(|| panic!("the export names its build: {status}"))
        .to_owned()
}

/// The pack format each locked build writes, as measured with that build.
fn pack_format_of(build: &str) -> u64 {
    match build {
        "4.4.1" => 2,
        "4.5.1" | "4.6.1" => 3,
        "4.7.1" | "4.7.2" => 4,
        other => panic!("no pack format of Godot {other} has been measured"),
    }
}

fn entry<'a>(pack: &'a Value, path: &str) -> Option<&'a Value> {
    pack["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|entry| entry["path"] == json!(path))
}

/// The names of a finished export's outputs.
fn output_names(status: &Value) -> Vec<String> {
    status["export"]["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .map(|output| output["name"].as_str().expect("name").to_owned())
        .collect()
}

/// The output `name` of a finished export, with its artifact read back and
/// its digest checked.
fn output(
    session: &mut StdioSession,
    ids: &mut Ids,
    status: &Value,
    name: &str,
) -> (String, Vec<u8>) {
    let outputs = status["export"]["outputs"].as_array().expect("outputs");
    let output = outputs
        .iter()
        .find(|output| output["name"] == json!(name))
        .unwrap_or_else(|| panic!("the export wrote {name}: {status}"));
    let uri = output["artifact"].as_str().expect("artifact").to_owned();
    let total = output["bytes"].as_u64().expect("bytes");
    let bytes = read_artifact(session, ids, &uri, total);
    assert_eq!(
        sha256_hex(&bytes),
        output["sha256"].as_str().expect("sha256"),
        "{status}"
    );
    (uri, bytes)
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

/// Pack, then patch against that pack: with nothing changed, with a changed
/// and a new file, and with a file removed against the base named by its
/// project path. Every build packs without export templates.
#[test]
#[ignore = "needs GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn packs_and_patches_are_read_back_on_every_build() {
    let _one = ONE_EXPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (binaries, _work) = live::editors();
    for binary in &binaries {
        let context = binary.display().to_string();
        let tree = project("live-pack");
        let mut session = server(&tree, binary, None);
        let mut ids = Ids(1);

        // The base pack.
        let base = export(
            &mut session,
            &mut ids,
            "Linux",
            json!({ "kind": "pack" }),
            &context,
        );
        assert_eq!(
            base["export"]["outcome"],
            json!("succeeded"),
            "{context}: {base}"
        );
        let build = build_of(&base);
        let pack = &base["export"]["pack"];
        assert_eq!(pack["format"], json!("pck"), "{context}: {base}");
        assert_eq!(pack["readable"], json!(true), "{context}: {base}");
        assert_eq!(
            pack["format_version"],
            json!(pack_format_of(&build)),
            "{context}: {base}"
        );
        assert_eq!(pack["engine_version"], json!(build), "{context}: {base}");
        assert_eq!(pack["truncated"], json!(false));
        for path in ["data/levels.json", "project.binary"] {
            assert!(
                entry(pack, path).is_some(),
                "{context}: {path} is packed: {base}"
            );
        }
        assert!(
            pack["entries"]
                .as_array()
                .expect("entries")
                .iter()
                .all(|entry| entry["digest"] == json!("verified")),
            "{context}: every packed file matches its MD5: {base}"
        );
        assert_eq!(output_names(&base), ["game.pck"], "{context}: {base}");
        let (base_uri, base_bytes) = output(&mut session, &mut ids, &base, "game.pck");
        assert_eq!(&base_bytes[..4], b"GDPC", "{context}");
        let base_sha256 = sha256_hex(&base_bytes);

        // A ZIP pack holds the same files.
        let zipped = export(
            &mut session,
            &mut ids,
            "Linux",
            json!({ "kind": "pack", "packFormat": "zip" }),
            &context,
        );
        assert_eq!(
            zipped["export"]["outcome"],
            json!("succeeded"),
            "{context}: {zipped}"
        );
        assert_eq!(
            zipped["export"]["pack"]["format"],
            json!("zip"),
            "{context}: {zipped}"
        );
        assert!(
            entry(&zipped["export"]["pack"], "data/levels.json").is_some(),
            "{zipped}"
        );
        assert_eq!(output_names(&zipped), ["game.zip"], "{context}: {zipped}");
        let (_, zip_bytes) = output(&mut session, &mut ids, &zipped, "game.zip");
        assert_eq!(&zip_bytes[..2], b"PK", "{context}");

        // Nothing changed since the base: Godot writes no patch.
        let same = export(
            &mut session,
            &mut ids,
            "Linux",
            json!({ "kind": "patch", "patches": [base_uri] }),
            &context,
        );
        assert_eq!(
            same["export"]["outcome"],
            json!("failed"),
            "{context}: {same}"
        );
        assert_eq!(
            same["export"]["nothing_changed"],
            json!(true),
            "{context}: {same}"
        );
        assert_eq!(same["export"]["outputs"], json!([]), "{context}: {same}");
        assert_eq!(
            same["export"]["bases"][0]["sha256"],
            json!(base_sha256),
            "{same}"
        );

        // A changed and a new file.
        tree.write("data/levels.json", "{\"levels\": 4, \"boss\": true}\n");
        tree.write("data/extra.json", "{\"extra\": 1}\n");
        let patch = export(
            &mut session,
            &mut ids,
            "Linux",
            json!({ "kind": "patch", "patches": [base_uri] }),
            &context,
        );
        assert_eq!(
            patch["export"]["outcome"],
            json!("succeeded"),
            "{context}: {patch}"
        );
        assert_eq!(
            patch["export"]["bases"][0]["source"],
            json!(base_uri),
            "{patch}"
        );
        assert_eq!(
            patch["export"]["bases"][0]["sha256"],
            json!(base_sha256),
            "{patch}"
        );
        let facts = &patch["export"]["pack"];
        assert_eq!(
            facts["format_version"],
            json!(pack_format_of(&build)),
            "{patch}"
        );
        assert_eq!(facts["patch"]["new"], json!(1), "{context}: {patch}");
        assert_eq!(facts["patch"]["changed"], json!(1), "{context}: {patch}");
        assert_eq!(facts["patch"]["removed"], json!(0), "{context}: {patch}");
        assert_eq!(facts["patch"]["identical"], json!(0), "{context}: {patch}");
        assert_eq!(
            entry(facts, "data/extra.json").map(|e| &e["change"]),
            Some(&json!("new"))
        );
        assert_eq!(
            entry(facts, "data/levels.json").map(|e| &e["change"]),
            Some(&json!("changed")),
            "{context}: {patch}"
        );
        assert_eq!(output_names(&patch), ["game.pck"], "{context}: {patch}");
        let (_, patch_bytes) = output(&mut session, &mut ids, &patch, "game.pck");
        assert_eq!(&patch_bytes[..4], b"GDPC", "{context}");

        // A removed file, against the base named by its path in the project.
        std::fs::create_dir_all(tree.root.join("bases")).expect("bases directory");
        std::fs::write(tree.root.join("bases/base.pck"), &base_bytes).expect("base copy");
        std::fs::remove_file(tree.root.join("data/levels.json")).expect("remove levels");
        let removal = export(
            &mut session,
            &mut ids,
            "Linux",
            json!({ "kind": "patch", "patches": ["bases/base.pck"] }),
            &context,
        );
        assert_eq!(
            removal["export"]["outcome"],
            json!("succeeded"),
            "{context}: {removal}"
        );
        assert_eq!(
            removal["export"]["bases"][0]["source"],
            json!("bases/base.pck")
        );
        assert_eq!(removal["export"]["bases"][0]["sha256"], json!(base_sha256));
        let facts = &removal["export"]["pack"];
        assert_eq!(facts["patch"]["removed"], json!(1), "{context}: {removal}");
        assert_eq!(facts["patch"]["new"], json!(1), "{context}: {removal}");
        let removed = entry(facts, "data/levels.json").expect("the removal entry");
        assert_eq!(removed["change"], json!("removed"), "{context}: {removal}");
        assert_eq!(removed["flags"], json!(["removal"]), "{context}: {removal}");

        // Nothing an export staged is left, and no answer names a host path.
        for directory in session_dirs(&tree) {
            assert!(!directory.join("bases").exists(), "{}", directory.display());
        }
        for status in [&base, &zipped, &same, &patch, &removal] {
            assert!(
                !status
                    .to_string()
                    .contains(&tree.base.display().to_string()),
                "{context}: {status}"
            );
        }
        eprintln!(
            "{context}: Godot {build} pack format {} ({} bytes), patch new/changed {}/{}, \
             removal patch removed {}",
            pack["format_version"],
            base_bytes.len(),
            patch["export"]["pack"]["patch"]["new"],
            patch["export"]["pack"]["patch"]["changed"],
            removal["export"]["pack"]["patch"]["removed"]
        );
        let _ = session.finish();
    }
}

/// The SDK and JDK this run may export with, when both are named.
fn android_tools() -> Option<(PathBuf, PathBuf)> {
    let sdk = std::env::var_os(SDK).map(PathBuf::from)?;
    let java_home = std::env::var_os(JAVA_HOME).map(PathBuf::from)?;
    assert!(sdk.is_dir(), "{SDK} names no directory: {}", sdk.display());
    assert!(
        java_home.is_dir(),
        "{JAVA_HOME} names no directory: {}",
        java_home.display()
    );
    Some((sdk, java_home))
}

/// The certificate digest the real `apksigner` reads from `apk`, run with a
/// private home.
fn signer_of(sdk: &Path, java_home: &Path, apk: &[u8]) -> String {
    let scratch = tempfile::tempdir().expect("scratch");
    let file = scratch.path().join("game.apk");
    std::fs::write(&file, apk).expect("apk copy");
    let (_, apksigner) = godot_mcp_engine::android::sdk::AndroidSdk::new(sdk)
        .apksigner()
        .expect("the SDK's apksigner");
    let output = Command::new(apksigner)
        .args(["verify", "--print-certs"])
        .arg(&file)
        .env_clear()
        .env("HOME", scratch.path())
        .env("JAVA_HOME", java_home)
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", java_home.join("bin").display()),
        )
        .output()
        .expect("run apksigner");
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(output.status.success(), "apksigner verify failed: {text}");
    // build-tools 35.0.0 and 36.0.0 print `Signer #1 certificate SHA-256
    // digest:`, 37.0.0 `V3.0 Signer: certificate SHA-256 digest:`.
    text.lines()
        .find_map(|line| line.split_once("certificate SHA-256 digest:"))
        .map(|(_, digest)| digest.trim().to_owned())
        .unwrap_or_else(|| panic!("apksigner names a signer: {text}"))
}

/// An Android debug export: a verified APK on a build with its templates,
/// a refusal naming what is missing otherwise.
#[test]
#[ignore = "needs GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT; exports an APK when the \
            build's templates are installed and GODOT_MCP_TEST_ANDROID_SDK and \
            GODOT_MCP_TEST_ANDROID_JAVA_HOME name an Android SDK and a JDK"]
fn an_android_debug_export_is_verified_or_refused_naming_what_is_missing() {
    let _one = ONE_EXPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (binaries, _work) = live::editors();
    let tools = android_tools();
    for binary in &binaries {
        let context = binary.display().to_string();
        let tree = project("live-android");
        let android = tools
            .as_ref()
            .map(|(sdk, java_home)| (sdk.as_path(), java_home.as_path()));
        let mut session = server(&tree, binary, android);
        let mut ids = Ids(1);

        let listed = ok(
            &session.call_tool(ids.next(), "export.presets.list", json!({})),
            &context,
        );
        let preset = listed["presets"]
            .as_array()
            .expect("presets")
            .iter()
            .find(|preset| preset["name"] == json!("Android"))
            .unwrap_or_else(|| panic!("{context}: the Android preset is listed: {listed}"))
            .clone();
        let label = preset["template"]["label"]
            .as_str()
            .unwrap_or_else(|| panic!("{context}: the build is resolved: {listed}"))
            .to_owned();
        let installed = preset["template"]["installed"] == json!(true);

        match (installed, &tools) {
            (false, _) => {
                let refused = start(
                    &mut session,
                    &mut ids,
                    "Android",
                    json!({ "mode": "debug" }),
                );
                assert_eq!(
                    error_code(&refused),
                    json!("unavailable"),
                    "{context}: {refused}"
                );
                assert!(
                    message(&refused).contains(&format!("no export templates for Godot {label}")),
                    "{context}: {refused}"
                );
                assert!(
                    session_dirs(&tree).is_empty(),
                    "{context}: nothing was staged"
                );
                eprintln!(
                    "{context}: {label}: refused before launch: {}",
                    message(&refused)
                );
            }
            (true, None) => {
                let refused = start(
                    &mut session,
                    &mut ids,
                    "Android",
                    json!({ "mode": "debug" }),
                );
                assert_eq!(
                    error_code(&refused),
                    json!("unavailable"),
                    "{context}: {refused}"
                );
                assert!(
                    message(&refused).contains("no Android SDK is configured"),
                    "{context}: {refused}"
                );
                eprintln!(
                    "{context}: {label}: refused before launch: {}",
                    message(&refused)
                );
            }
            (true, Some((sdk, java_home))) => {
                let status = export(
                    &mut session,
                    &mut ids,
                    "Android",
                    json!({ "mode": "debug" }),
                    &context,
                );
                assert_eq!(
                    status["export"]["outcome"],
                    json!("succeeded"),
                    "{context}: {status}"
                );
                assert_eq!(status["export"]["template"], json!("android_debug.apk"));
                let apk = status["export"]["apk"].clone();
                assert_eq!(
                    apk["package"],
                    json!("org.godotmcp.live"),
                    "{context}: {status}"
                );
                assert!(apk["launcher"].is_string(), "{context}: {status}");
                assert_eq!(apk["debuggable"], json!(true), "{context}: {status}");
                assert!(apk["asset_entries"].as_u64().is_some_and(|count| count > 0));
                assert_eq!(
                    apk["signature"]["verified"],
                    json!(true),
                    "{context}: {status}"
                );
                assert_eq!(
                    apk["signature"]["keystore"],
                    json!("server-debug"),
                    "{status}"
                );
                let signer = apk["signature"]["signer_sha256"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{context}: the signer is named: {status}"))
                    .to_owned();

                // The artifact is the APK the facts describe, and the real
                // apksigner reads the same signer from it.
                // Godot writes the APK and, beside it, the APK Signature
                // Scheme v4 file apksigner signs it with.
                assert_eq!(
                    output_names(&status),
                    ["game.apk", "game.apk.idsig"],
                    "{context}: {status}"
                );
                let (_, bytes) = output(&mut session, &mut ids, &status, "game.apk");
                let facts = godot_mcp_engine::export::apk::read_apk(&bytes)
                    .unwrap_or_else(|error| panic!("{context}: the APK reads: {error}"));
                assert_eq!(facts.manifest.package, "org.godotmcp.live");
                assert_eq!(signer_of(sdk, java_home, &bytes), signer, "{context}");

                // A second debug export signs with the server's same key.
                let again = export(
                    &mut session,
                    &mut ids,
                    "Android",
                    json!({ "mode": "debug" }),
                    &context,
                );
                assert_eq!(
                    again["export"]["outcome"],
                    json!("succeeded"),
                    "{context}: {again}"
                );
                assert_eq!(
                    again["export"]["apk"]["signature"]["signer_sha256"],
                    json!(signer),
                    "{context}: {again}"
                );
                // The export's private home and SDK view are gone with it.
                for directory in session_dirs(&tree) {
                    for staged in ["android-home", "android-sdk", "scratch"] {
                        assert!(
                            !directory.join(staged).exists(),
                            "{}/{staged}",
                            directory.display()
                        );
                    }
                }
                eprintln!(
                    "{context}: {label}: APK of {} bytes, package {}, launcher {}, signed by \
                     {signer}, verified by {}",
                    bytes.len(),
                    apk["package"],
                    apk["launcher"],
                    apk["signature"]["verifier"]
                );
            }
        }
        let _ = session.finish();
    }
}
