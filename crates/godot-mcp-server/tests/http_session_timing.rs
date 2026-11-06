//! The two clocks that end an HTTP MCP session, `http.session_idle_timeout_ms` and
//! `http.session_lifetime_ms`, driven through the path `godot-mcp serve --config`
//! takes: the layered configuration loader, `bind_configured_transport` and the
//! live listener.
//!
//! The SDK closes a session that received nothing for its idle timeout, and the
//! binding refuses a session once its lifetime from opening has passed. Either
//! way a client receives HTTP 404 for the session and opens a new one; the two
//! are told apart by their bodies.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::config::{CliOverrides, Limits, load_config};
use godot_mcp_domain::contracts::PrincipalId;
use godot_mcp_server::binding::BINDING_REFUSAL_MESSAGE;
use godot_mcp_server::http::{
    LaunchSecret, MCP_SESSION_HEADER, RunningHttpTransport, bind_configured_transport,
};
use godot_mcp_server::stdio::{ProjectRuntime, StdioOptions};
use serde_json::{Value, json};

/// One served project on `trusted_local` HTTP and the token that authenticates to it.
struct Served {
    transport: RunningHttpTransport,
    token: String,
    _runtime: ProjectRuntime,
    _temp: tempfile::TempDir,
}

/// Serve a fresh project over `trusted_local` HTTP whose `[http]` table also
/// holds `http_keys`, exactly as `serve --config` does.
async fn serve(http_keys: &str) -> anyhow::Result<Served> {
    let temp = tempfile::tempdir()?;
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project)?;
    std::fs::write(project.join("project.godot"), b"config_version=5\n")?;
    let runtime = ProjectRuntime::open(&StdioOptions {
        project,
        godot_binary: None,
        profile: CapabilityProfile::Core,
        principal: PrincipalId::parse("principal_00112233445566778899aabbccddeeff")?,
        observed_at_unix_ms: 4_000,
        limits: Limits::default(),
        process: Default::default(),
        allow_scripts: false,
        android: Default::default(),
    })?;
    // The configured Host must name the port the listener binds, so the port is
    // chosen first, the way the export suite chooses it for its HTTP server.
    let address = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?;
    let config = load_config(
        Some(&format!(
            r#"transport = "http"

[http]
bind = "{address}"
mode = "trusted_local"
host = "{address}"
origin = "http://{address}"
{http_keys}

[http.local_secret.env]
var = "GODOT_MCP_TEST_LOCAL_SECRET"
"#
        )),
        &BTreeMap::new(),
        &CliOverrides::default(),
    )?;
    let transport = bind_configured_transport(&runtime, &config, CapabilityProfile::Core)
        .await
        .map_err(anyhow::Error::msg)?;
    let token = LaunchSecret::read_token(
        transport
            .secret_path()
            .expect("trusted_local publishes a launch secret"),
    )?;
    Ok(Served {
        transport,
        token,
        _runtime: runtime,
        _temp: temp,
    })
}

async fn post(served: &Served, session: Option<&str>, body: &Value) -> reqwest::Response {
    let mut request = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("HTTP client")
        .post(served.transport.endpoint())
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", format!("Bearer {}", served.token));
    if let Some(session) = session {
        request = request.header(MCP_SESSION_HEADER, session);
    }
    request
        .body(serde_json::to_vec(body).expect("serialize the body"))
        .send()
        .await
        .expect("the request is answered")
}

/// Open a session and return the identifier the server handed out.
async fn open(served: &Served) -> anyhow::Result<String> {
    let response = post(
        served,
        None,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "session-timing-probe", "version": "0" }
            }
        }),
    )
    .await;
    anyhow::ensure!(
        response.status().is_success(),
        "the handshake must succeed, got {}",
        response.status()
    );
    let session = response
        .headers()
        .get(MCP_SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("a handshake names its session"))?;
    let _ = response.text().await?;
    let initialized = post(
        served,
        Some(&session),
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;
    anyhow::ensure!(
        initialized.status().is_success(),
        "the initialized notification must be accepted, got {}",
        initialized.status()
    );
    let _ = initialized.text().await?;
    Ok(session)
}

fn list_tools(id: u64) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": "tools/list" })
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_session_closes_after_the_configured_idle_timeout() -> anyhow::Result<()> {
    // Idleness is the condition under test: any request would reset the timer, so
    // the session cannot be polled and the wait is a fixed one. 2.5 s is two and a
    // half short timeouts, room for the SDK to notice and drop the session; the
    // short timeout itself is long enough that the handshake never crosses it. The
    // control waits as long, so a 404 below comes from the timeout and nothing else.
    let idle = Duration::from_millis(2_500);

    let short = serve("session_idle_timeout_ms = 1000").await?;
    let session = open(&short).await?;
    tokio::time::sleep(idle).await;
    let closed = post(&short, Some(&session), &list_tools(2)).await;
    assert_eq!(
        closed.status().as_u16(),
        404,
        "a session idle past the configured timeout is gone"
    );
    assert_ne!(
        closed.text().await?,
        BINDING_REFUSAL_MESSAGE,
        "the SDK closed it, while its binding still holds"
    );
    short.transport.shutdown().await?;

    let long = serve("session_idle_timeout_ms = 30000").await?;
    let session = open(&long).await?;
    tokio::time::sleep(idle).await;
    let kept = post(&long, Some(&session), &list_tools(2)).await;
    assert_eq!(
        kept.status().as_u16(),
        200,
        "a session idle for less than the configured timeout stays open"
    );
    let _ = kept.text().await?;
    long.transport.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bound_session_ends_at_the_configured_lifetime() -> anyhow::Result<()> {
    let lifetime = Duration::from_millis(2_000);
    let served = serve(&format!("session_lifetime_ms = {}", lifetime.as_millis())).await?;
    // Taken before the handshake, so the binding cannot have been made earlier.
    let opened = Instant::now();
    let session = open(&served).await?;
    // Requests do not extend a lifetime, so the session is polled until it is
    // refused. Without the key the lifetime would be the derived 32 minutes, and
    // this deadline would end the test first.
    let deadline = opened + Duration::from_secs(20);
    let refused = loop {
        let response = post(&served, Some(&session), &list_tools(3)).await;
        if response.status().as_u16() == 404 {
            break response;
        }
        assert_eq!(
            response.status().as_u16(),
            200,
            "a live session answers until its lifetime ends"
        );
        let _ = response.text().await?;
        assert!(
            Instant::now() < deadline,
            "the configured lifetime never ended the session"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let elapsed = opened.elapsed();
    assert_eq!(
        refused.text().await?,
        BINDING_REFUSAL_MESSAGE,
        "the binding refused the session, not the SDK"
    );
    // The binding clock counts whole milliseconds, so allow for the one it rounds off.
    assert!(
        elapsed + Duration::from_millis(1) >= lifetime,
        "a session lives for its configured lifetime first, ended after {elapsed:?}"
    );
    served.transport.shutdown().await?;
    Ok(())
}
