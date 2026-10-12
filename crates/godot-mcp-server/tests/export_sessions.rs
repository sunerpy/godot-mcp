//! M6 acceptance: managed exports over the real stdio assembly.
//!
//! The production server runs out of process with the non-shipping
//! `qa-fake-game` binary as its Godot executable. Given `--export-release` or
//! `--export-debug`, the stand-in writes the file a template export would — a
//! copy of itself, so the exported build can be smoke-run — or, as scripted,
//! fails, hangs mid-export or produces an oversize file. Exact templates are
//! looked up where Godot looks for them, so each case builds its own template
//! directory under a private data home. Every server-side path is the shipped
//! one: preset reading, the exact-template check, the supervised export, the
//! artifact registration, the smoke run, cancellation and reconciliation by
//! the next server. The same server binds authenticated `trusted_local`
//! HTTP through the configuration path the shipping CLI uses, so an export's
//! artifact is read over both transports. Real Godot builds are exercised by
//! `scripts/real_client/m6_smoke.py`.
#![cfg(target_os = "linux")]

mod support;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use godot_mcp_server::http::LaunchSecret;
use godot_mcp_server::rmcp_adapter::ARTIFACT_RANGE_META_KEY;
use rmcp::model::{
    CallToolRequestParams, ReadResourceRequestParams, RequestMetaObject, ResourceContents,
};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use support::{ProjectTree, StdioSession, error_code, sha256_hex};

const PRESETS: &str = r#"[preset.0]

name="Linux"
platform="Linux"
runnable=true
export_path="build/game.x86_64"

[preset.0.options]

custom_template/debug=""
custom_template/release=""
binary_format/embed_pck=true
binary_format/architecture="x86_64"

[preset.1]

name="Secret"
platform="Linux"
runnable=false
export_path="build/secret.x86_64"
script_encryption_key="7f0e5e3bd4c1a9b8e2f6d0c4b3a2918f7e6d5c4b3a291807f6e5d4c3b2a19180"

[preset.1.options]

binary_format/architecture="x86_64"

[preset.2]

name="Phone"
platform="Android"
runnable=false
export_path="build/game.apk"

[preset.2.options]
"#;

/// A macOS preset whose export path names a bundle directory, not a zip.
const MAC_PRESETS: &str = r#"[preset.0]

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

fn fake_godot() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_qa-fake-game"))
}

/// A data home holding the stand-in build's exact templates, or none.
struct DataHome {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

fn data_home(with_templates: bool) -> DataHome {
    let directory = tempfile::tempdir().expect("data home");
    let root = directory.path().to_path_buf();
    if with_templates {
        let templates = root.join("godot/export_templates/4.7.1.stable");
        std::fs::create_dir_all(&templates).expect("templates");
        std::fs::write(templates.join("version.txt"), "4.7.1.stable\n").expect("version");
        std::fs::write(templates.join("linux_release.x86_64"), b"template").expect("template");
        std::fs::write(templates.join("linux_debug.x86_64"), b"template").expect("template");
    }
    DataHome {
        _directory: directory,
        root,
    }
}

/// A data home holding the stand-in build's macOS template.
fn mac_data_home() -> DataHome {
    let directory = tempfile::tempdir().expect("data home");
    let root = directory.path().to_path_buf();
    let templates = root.join("godot/export_templates/4.7.1.stable");
    std::fs::create_dir_all(&templates).expect("templates");
    std::fs::write(templates.join("version.txt"), "4.7.1.stable\n").expect("version");
    std::fs::write(templates.join("macos.zip"), b"template").expect("template");
    DataHome {
        _directory: directory,
        root,
    }
}

fn server_command(tree: &ProjectTree, data: &DataHome) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_qa-journal-foundation"));
    command
        .arg("--project")
        .arg(&tree.root)
        .arg("--profile")
        .arg("full")
        .arg("--godot-binary")
        .arg(fake_godot())
        .env("XDG_DATA_HOME", &data.root)
        .env_remove("GODOT_MCP_TEST_LAUNCH_CRASH_STAGE");
    command
}

fn server(tree: &ProjectTree, data: &DataHome) -> StdioSession {
    StdioSession::handshake(StdioSession::start_command(server_command(tree, data))).0
}

/// A server whose startup clock reads `observed_at_unix_ms`.
fn server_at(tree: &ProjectTree, data: &DataHome, observed_at_unix_ms: u64) -> StdioSession {
    let mut command = server_command(tree, data);
    command
        .arg("--observed-at-unix-ms")
        .arg(observed_at_unix_ms.to_string());
    StdioSession::handshake(StdioSession::start_command(command)).0
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// The same server over configured, authenticated `trusted_local` HTTP.
struct HttpServer {
    child: Child,
    endpoint: String,
    token: String,
    _config: tempfile::TempDir,
}

impl HttpServer {
    fn start(tree: &ProjectTree, data: &DataHome) -> Self {
        let config_dir = tempfile::tempdir().expect("config directory");
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve a loopback port");
        let address = probe.local_addr().expect("reserved address");
        drop(probe);
        let config = config_dir.path().join("http.toml");
        std::fs::write(
            &config,
            format!(
                r#"transport = "http"

[http]
bind = "{address}"
mode = "trusted_local"
host = "{address}"
origin = "http://{address}"

[http.local_secret.env]
var = "GODOT_MCP_TEST_LOCAL_SECRET"
"#
            ),
        )
        .expect("HTTP configuration");
        let mut command = server_command(tree, data);
        command
            .arg("--config")
            .arg(&config)
            .env(
                "GODOT_MCP_TEST_LOCAL_SECRET",
                "config-indirection-is-not-the-launch-secret",
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn the HTTP server");
        let stderr = child.stderr.take().expect("stderr");
        let (lines, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = lines.send(line);
            }
        });
        let deadline = Instant::now() + Duration::from_secs(60);
        let (mut endpoint, mut secret) = (None, None);
        while endpoint.is_none() || secret.is_none() {
            let line = received
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("the HTTP server announces its endpoint and secret");
            if let Some((_, rest)) = line.split_once("HTTP listening at ") {
                endpoint = Some(rest.trim().to_owned());
            }
            if let Some((_, rest)) = line.split_once("launch secret published at ") {
                secret = Some(PathBuf::from(rest.trim()));
            }
        }
        let token = LaunchSecret::read_token(&secret.expect("secret path")).expect("token");
        Self {
            child,
            endpoint: endpoint.expect("endpoint"),
            token,
            _config: config_dir,
        }
    }

    /// One more authenticated MCP session.
    async fn client(&self) -> RunningService<RoleClient, ()> {
        let transport = StreamableHttpClientTransport::with_client(
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("HTTP client"),
            StreamableHttpClientTransportConfig::with_uri(self.endpoint.as_str())
                .auth_header(self.token.clone()),
        );
        ().serve(transport)
            .await
            .expect("an authenticated session opens")
    }

    /// Close stdin and wait for the server to shut down.
    fn finish(mut self) {
        drop(self.child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.child.try_wait().expect("inspect the server").is_none() {
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("the HTTP server did not shut down when its stdin closed");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// What one ranged read answered: the bytes, or that it was refused.
type ReadAnswer = Result<Vec<u8>, String>;

fn stdio_read(
    session: &mut StdioSession,
    id: u64,
    uri: &str,
    range: Option<(u64, u64)>,
) -> ReadAnswer {
    let mut params = json!({ "uri": uri });
    if let Some((start, end)) = range {
        params["_meta"] =
            json!({ ARTIFACT_RANGE_META_KEY: { "start": start, "endInclusive": end } });
    }
    let response = session.request(id, "resources/read", params);
    match response["result"]["contents"][0]["blob"].as_str() {
        Some(blob) => Ok(decode_base64(blob)),
        None => Err(response.to_string()),
    }
}

async fn http_read(
    client: &RunningService<RoleClient, ()>,
    uri: &str,
    range: Option<(u64, u64)>,
) -> ReadAnswer {
    let mut request = ReadResourceRequestParams::new(uri);
    if let Some((start, end)) = range {
        let mut meta = RequestMetaObject::new();
        meta.0.insert(
            ARTIFACT_RANGE_META_KEY.to_owned(),
            json!({ "start": start, "endInclusive": end }),
        );
        request = request.with_meta(meta);
    }
    match client.read_resource(request).await {
        Ok(result) => match &result.contents[0] {
            ResourceContents::BlobResourceContents { blob, .. } => Ok(decode_base64(blob)),
            other => Err(format!("{other:?}")),
        },
        Err(error) => Err(error.to_string()),
    }
}

async fn http_call(client: &RunningService<RoleClient, ()>, tool: &str, arguments: Value) -> Value {
    let Value::Object(arguments) = arguments else {
        panic!("tool arguments are an object");
    };
    let result = client
        .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments))
        .await
        .expect("the call is answered");
    assert_ne!(
        result.is_error,
        Some(true),
        "{tool} must succeed: {result:?}"
    );
    result.structured_content.expect("structured content")
}

async fn http_artifacts(client: &RunningService<RoleClient, ()>) -> Vec<String> {
    client
        .list_all_resources()
        .await
        .expect("listing")
        .into_iter()
        .map(|resource| resource.uri)
        .filter(|uri| uri.starts_with("godot-mcp://artifacts/"))
        .collect()
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

fn start(session: &mut StdioSession, id: u64, preset: &str, extra: Value) -> Value {
    let mut arguments = json!({ "preset": preset, "idempotencyId": format!("export-{id}") });
    if let (Value::Object(arguments), Value::Object(extra)) = (&mut arguments, extra) {
        arguments.extend(extra);
    }
    session.call_tool(id, "project.export", arguments)
}

/// Poll the export's status until it leaves `running`.
fn ended(session: &mut StdioSession, first_id: u64, export: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
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

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_preset_exports_through_its_exact_template_into_verified_artifacts() {
    let tree = project("m6-export", None);
    let data = data_home(true);
    let mut session = server(&tree, &data);

    let listed = structured(&session.call_tool(2, "export.presets.list", json!({})));
    let presets = listed["presets"].as_array().expect("presets");
    assert_eq!(presets.len(), 3, "{listed}");
    assert_eq!(presets[0]["name"], json!("Linux"));
    assert_eq!(presets[0]["platform"], json!("Linux"));
    assert_eq!(presets[0]["runnable"], json!(true));
    assert_eq!(
        presets[0]["template"]["file"],
        json!("linux_release.x86_64")
    );
    assert_eq!(presets[0]["template"]["label"], json!("4.7.1.stable"));
    assert_eq!(presets[0]["template"]["installed"], json!(true), "{listed}");
    assert_eq!(presets[0]["secret_options"], json!([]));
    assert_eq!(
        presets[1]["secret_options"],
        json!(["script_encryption_key"]),
        "{listed}"
    );
    assert!(
        presets[2]["template"]["unsupported"].is_string(),
        "{listed}"
    );

    let started = structured(&start(&mut session, 3, "Linux", json!({ "smoke": true })));
    let export = started["export"]["session"]
        .as_str()
        .expect("export id")
        .to_owned();
    assert!(
        export.starts_with("export_") && export.len() == 39,
        "{started}"
    );
    let replayed = structured(&start(&mut session, 3, "Linux", json!({ "smoke": true })));
    assert_eq!(replayed["export"]["session"], json!(export));
    assert_eq!(replayed["replayed"], json!(true));

    let status = ended(&mut session, 10, &export);
    assert_eq!(status["state"], json!("exited"), "{status}");
    assert_eq!(status["export"]["outcome"], json!("succeeded"), "{status}");
    assert_eq!(status["export"]["template"], json!("linux_release.x86_64"));
    let outputs = status["export"]["outputs"].as_array().expect("outputs");
    assert_eq!(outputs.len(), 1, "{status}");
    assert_eq!(outputs[0]["name"], json!("game.x86_64"));
    assert_eq!(status["export"]["smoke"]["passed"], json!(true), "{status}");

    // The artifact reads back as exactly the bytes the status hashed.
    let uri = outputs[0]["artifact"]
        .as_str()
        .expect("artifact uri")
        .to_owned();
    assert!(uri.starts_with("godot-mcp://artifacts/"), "{uri}");
    let read = session.request(200, "resources/read", json!({ "uri": uri }));
    let blob = read["result"]["contents"][0]["blob"]
        .as_str()
        .expect("blob")
        .to_owned();
    let bytes = decode_base64(&blob);
    assert!(bytes.starts_with(b"#!/bin/sh"), "{read}");
    assert_eq!(outputs[0]["bytes"], json!(bytes.len()));
    assert_eq!(outputs[0]["sha256"], json!(sha256_hex(&bytes)));

    // Nothing was written where the preset's export path points.
    assert!(!tree.root.join("build").exists());
    assert!(
        !session
            .transcript
            .contains(&tree.root.display().to_string()),
        "no answer names a private path"
    );
    let _ = session.finish();
}

/// A macOS preset exports its app bundle as one zip, whatever extension its
/// export path names; a host that is not a Mac exports it but does not run it.
#[test]
fn a_macos_preset_exports_its_app_bundle_as_one_zip() {
    let tree = ProjectTree::new("m6-macos");
    tree.write("export_presets.cfg", MAC_PRESETS);
    tree.write(
        "main.tscn",
        "[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node2D\"]\n",
    );
    let data = mac_data_home();
    let mut session = server(&tree, &data);

    let listed = structured(&session.call_tool(2, "export.presets.list", json!({})));
    let template = &listed["presets"][0]["template"];
    assert_eq!(template["file"], json!("macos.zip"), "{listed}");
    assert_eq!(template["installed"], json!(true), "{listed}");
    assert_eq!(template["runnable_here"], json!(false), "{listed}");

    let started = structured(&start(&mut session, 3, "Mac", json!({ "smoke": true })));
    let export = started["export"]["session"]
        .as_str()
        .expect("export id")
        .to_owned();
    let status = ended(&mut session, 10, &export);
    assert_eq!(status["export"]["outcome"], json!("succeeded"), "{status}");
    assert_eq!(status["export"]["template"], json!("macos.zip"));
    let outputs = status["export"]["outputs"].as_array().expect("outputs");
    assert_eq!(outputs.len(), 1, "{status}");
    assert_eq!(outputs[0]["name"], json!("Game.zip"), "{status}");
    let uri = outputs[0]["artifact"].as_str().expect("artifact uri");
    let read = session.request(200, "resources/read", json!({ "uri": uri }));
    let bytes = decode_base64(
        read["result"]["contents"][0]["blob"]
            .as_str()
            .expect("blob"),
    );
    assert!(bytes.starts_with(b"PK\x03\x04"), "a zip archive");
    assert_eq!(outputs[0]["sha256"], json!(sha256_hex(&bytes)));
    let smoke = &status["export"]["smoke"];
    assert_eq!(smoke["ran"], json!(false), "{status}");
    assert!(
        smoke["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("macOS")),
        "{status}"
    );
    assert!(!tree.root.join("build").exists());
    let _ = session.finish();
}

#[test]
fn an_export_that_cannot_be_exact_or_safe_is_refused_before_anything_runs() {
    let tree = project("m6-refused", None);
    let empty = data_home(false);
    let mut session = server(&tree, &empty);
    let listed = structured(&session.call_tool(2, "export.presets.list", json!({})));
    assert_eq!(
        listed["presets"][0]["template"]["installed"],
        json!(false),
        "{listed}"
    );
    let missing = start(&mut session, 3, "Linux", json!({}));
    assert_eq!(error_code(&missing), json!("unavailable"), "{missing}");
    assert!(missing.to_string().contains("4.7.1.stable"), "{missing}");
    let _ = session.finish();

    let data = data_home(true);
    let mut session = server(&tree, &data);
    let secret = start(&mut session, 2, "Secret", json!({}));
    assert_eq!(error_code(&secret), json!("invalid_input"), "{secret}");
    assert!(
        secret.to_string().contains("script_encryption_key"),
        "{secret}"
    );
    let unsupported = start(&mut session, 3, "Phone", json!({}));
    assert_eq!(
        error_code(&unsupported),
        json!("unsupported"),
        "{unsupported}"
    );
    let unknown = start(&mut session, 4, "Nope", json!({}));
    assert_eq!(error_code(&unknown), json!("not_found"), "{unknown}");
    let transcript = session.transcript.clone();
    assert!(
        !transcript.contains("7f0e5e3bd4c1a9b8"),
        "a secret value never leaves the server"
    );
    // No export ever started.
    assert!(!tree.root.join("fake-export.started").exists());
    let listed = structured(&session.call_tool(5, "project.export.status", json!({})));
    assert_eq!(listed["exports"], json!([]), "{listed}");
    let _ = session.finish();
}

#[test]
fn a_cancelled_export_leaves_nothing_behind() {
    let tree = project("m6-cancel", Some("export:hang"));
    let data = data_home(true);
    let mut session = server(&tree, &data);
    let started = structured(&start(&mut session, 2, "Linux", json!({})));
    let export = started["export"]["session"]
        .as_str()
        .expect("export id")
        .to_owned();
    wait_for(&tree.root.join("fake-export.started"));
    let cancelled = structured(&session.call_tool(
        3,
        "project.export.cancel",
        json!({ "export": export, "idempotencyId": "cancel-1" }),
    ));
    assert_eq!(cancelled["state"], json!("cancelled"), "{cancelled}");
    let status =
        structured(&session.call_tool(4, "project.export.status", json!({ "export": export })));
    assert_eq!(status["export"]["outcome"], json!("cancelled"), "{status}");
    assert_eq!(status["export"]["outputs"], json!([]), "{status}");
    assert_eq!(status["exit"]["boundary_empty"], json!(true), "{status}");
    let again = structured(&session.call_tool(
        5,
        "project.export.cancel",
        json!({ "export": export, "idempotencyId": "cancel-2" }),
    ));
    assert_eq!(again["already_ended"], json!(true));
    let _ = session.finish();
}

#[test]
fn an_export_that_fails_or_overflows_registers_nothing() {
    for (label, behaviour, reason) in [
        ("m6-fail", "export:fail", "failed"),
        ("m6-huge", "export:huge", "artifact budget"),
    ] {
        let tree = project(label, Some(behaviour));
        let data = data_home(true);
        let mut session = server(&tree, &data);
        let started = structured(&start(&mut session, 2, "Linux", json!({})));
        let export = started["export"]["session"]
            .as_str()
            .expect("export id")
            .to_owned();
        let status = ended(&mut session, 10, &export);
        assert_eq!(status["export"]["outcome"], json!("failed"), "{status}");
        assert_eq!(status["export"]["outputs"], json!([]), "{status}");
        assert!(
            status["export"]["failure"]
                .as_str()
                .is_some_and(|failure| failure.contains(reason)),
            "{label}: {status}"
        );
        let _ = session.finish();
    }
}

#[test]
fn an_export_a_killed_server_left_running_keeps_its_output_private() {
    let tree = project("m6-orphan", Some("export:hang"));
    let data = data_home(true);
    let mut first = server(&tree, &data);
    let started = structured(&start(&mut first, 2, "Linux", json!({})));
    let export = started["export"]["session"]
        .as_str()
        .expect("export id")
        .to_owned();
    wait_for(&tree.root.join("fake-export.started"));
    first.kill();

    let mut second = server(&tree, &data);
    let status =
        structured(&second.call_tool(2, "project.export.status", json!({ "export": export })));
    assert_eq!(status["state"], json!("reconciled"), "{status}");
    assert_eq!(status["export"]["outputs"], json!([]), "{status}");
    assert!(
        status["export"]["recovery_evidence"]["bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0),
        "the partial output is kept as private recovery evidence: {status}"
    );
    let _ = second.finish();
}

/// An export's artifact reads the same over stdio and authenticated HTTP:
/// the same bytes for the ranges its contract admits, a refusal for the one it
/// does not, and nothing at all for another session or another principal.
#[test]
fn an_export_artifact_reads_the_same_over_stdio_and_authenticated_http() {
    let tree = project("m6-parity", None);
    let data = data_home(true);
    let reads = [None, Some((3, 10)), Some((0, 4096))];

    let mut stdio = server(&tree, &data);
    let started = structured(&start(&mut stdio, 2, "Linux", json!({})));
    let export = started["export"]["session"]
        .as_str()
        .expect("export id")
        .to_owned();
    let status = ended(&mut stdio, 10, &export);
    assert_eq!(status["export"]["outcome"], json!("succeeded"), "{status}");
    let stdio_uri = status["export"]["outputs"][0]["artifact"]
        .as_str()
        .expect("artifact uri")
        .to_owned();
    let over_stdio: Vec<ReadAnswer> = reads
        .iter()
        .enumerate()
        .map(|(index, range)| stdio_read(&mut stdio, 300 + index as u64, &stdio_uri, *range))
        .collect();
    let _ = stdio.finish();
    let whole = over_stdio[0]
        .clone()
        .expect("the owner reads the whole artifact over stdio");
    assert!(whole.starts_with(b"#!/bin/sh"));
    assert_eq!(over_stdio[1], Ok(whole[3..=10].to_vec()));
    assert!(
        over_stdio[2].is_err(),
        "a range past the end is refused over stdio"
    );

    let http = HttpServer::start(&tree, &data);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let owner = http.client().await;
        let started = http_call(
            &owner,
            "project.export",
            json!({ "preset": "Linux", "idempotencyId": "export-http" }),
        )
        .await;
        let export = started["export"]["session"]
            .as_str()
            .expect("export id")
            .to_owned();
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            let status =
                http_call(&owner, "project.export.status", json!({ "export": export })).await;
            if status["state"] != json!("running") && status["state"] != json!("starting") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "the export never ended: {status}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(status["export"]["outcome"], json!("succeeded"), "{status}");
        let uri = status["export"]["outputs"][0]["artifact"]
            .as_str()
            .expect("artifact uri")
            .to_owned();

        let mut over_http = Vec::new();
        for range in reads {
            over_http.push(http_read(&owner, &uri, range).await);
        }
        assert_eq!(
            over_http[0], over_stdio[0],
            "the whole artifact reads the same"
        );
        assert_eq!(
            over_http[1], over_stdio[1],
            "a bounded range reads the same"
        );
        assert!(
            over_http[2].is_err(),
            "a range past the end is refused over HTTP too"
        );
        assert_eq!(http_artifacts(&owner).await, vec![uri.clone()]);

        // Another session of the same subject, and the artifact the stdio
        // principal produced, are refused and never listed.
        let sibling = http.client().await;
        assert!(
            http_read(&sibling, &uri, None).await.is_err(),
            "another session never reads it"
        );
        assert!(
            http_artifacts(&sibling).await.is_empty(),
            "nor sees it listed"
        );
        assert!(
            http_read(&owner, &stdio_uri, None).await.is_err(),
            "another principal's artifact is not readable over HTTP"
        );
        // The owner still reads it after a refusal elsewhere.
        assert_eq!(http_read(&owner, &uri, Some((3, 10))).await, over_stdio[1]);
        let _ = owner.cancel().await;
        let _ = sibling.cancel().await;
    });
    http.finish();
}

/// A restart after an artifact's retention removes it, while an export a
/// killed server left unresolved keeps its partial output, privately.
#[test]
fn a_restart_past_retention_removes_the_artifact_and_keeps_unresolved_evidence_private() {
    let tree = project("m6-expiry", None);
    let data = data_home(true);
    let mut first = server(&tree, &data);
    let started = structured(&start(&mut first, 2, "Linux", json!({})));
    let finished = started["export"]["session"]
        .as_str()
        .expect("export id")
        .to_owned();
    let status = ended(&mut first, 10, &finished);
    let uri = status["export"]["outputs"][0]["artifact"]
        .as_str()
        .expect("artifact uri")
        .to_owned();
    let expires = status["export"]["outputs"][0]["expires_at_unix_ms"]
        .as_u64()
        .expect("expiry");
    assert!(stdio_read(&mut first, 100, &uri, None).is_ok());

    // A second export hangs, and its server dies under it.
    std::fs::remove_file(tree.root.join("fake-export.started")).expect("marker");
    tree.write("fake-game.conf", "export:hang\n");
    let started = structured(&start(&mut first, 101, "Linux", json!({})));
    let unresolved = started["export"]["session"]
        .as_str()
        .expect("export id")
        .to_owned();
    wait_for(&tree.root.join("fake-export.started"));
    first.kill();

    let state = tree.root.join(".godot-mcp");
    let evidence = state.join("sessions").join(&unresolved).join("out");
    let mut second = server_at(&tree, &data, expires.max(now_unix_ms()) + 1);
    let status =
        structured(&second.call_tool(2, "project.export.status", json!({ "export": finished })));
    assert_eq!(
        status["export"]["outputs"][0]["artifact"],
        json!(null),
        "{status}"
    );
    assert_eq!(
        status["export"]["outputs"][0]["expired"],
        json!(true),
        "{status}"
    );
    assert!(
        stdio_read(&mut second, 3, &uri, None).is_err(),
        "an expired artifact reads as absent"
    );
    let listed = second.request(4, "resources/list", json!({}));
    assert!(
        !listed.to_string().contains(&uri),
        "nor is it listed: {listed}"
    );
    let stored: Vec<String> = std::fs::read_dir(state.join("artifacts"))
        .expect("artifact directory")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(
        stored.is_empty(),
        "the expired artifact's bytes are gone: {stored:?}"
    );

    let status =
        structured(&second.call_tool(5, "project.export.status", json!({ "export": unresolved })));
    assert_eq!(status["state"], json!("reconciled"), "{status}");
    assert_eq!(status["export"]["outputs"], json!([]), "{status}");
    let kept = status["export"]["recovery_evidence"]["bytes"]
        .as_u64()
        .expect("evidence bytes");
    assert!(kept > 0, "{status}");
    let on_disk: u64 = std::fs::read_dir(&evidence)
        .expect("the evidence stays in the private session directory")
        .map(|entry| entry.expect("entry").metadata().expect("metadata").len())
        .sum();
    assert_eq!(
        on_disk, kept,
        "the retention sweep leaves recovery evidence alone"
    );
    assert!(
        !second.transcript.contains(&evidence.display().to_string()),
        "no answer names where the evidence is kept"
    );
    let _ = second.finish();
}
