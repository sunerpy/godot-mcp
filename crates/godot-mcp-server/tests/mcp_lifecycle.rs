//! The MCP lifecycle this server serves: the `initialize` handshake of the
//! revisions up to 2025-11-25, and nothing else.
//!
//! rmcp 3 also speaks the 2026-07-28 revision, in which a request carries the
//! client context in its own `_meta` instead of an `initialize` handshake, and an
//! HTTP request is answered without a session. The HTTP session binding and the
//! stdio bootstrap both rest on the handshake, so a request in that lifecycle is
//! refused on both transports, before the kernel sees it. `server/discover`, the
//! probe that opens it, is answered "method not found", as a server that predates
//! the revision answers it, so a client that probes falls back to `initialize`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::config::Limits;
use godot_mcp_domain::contracts::{PrincipalId, ProjectIdentity, SessionId};
use godot_mcp_domain::objid::{LeaseKey, StableObjectId};
use godot_mcp_server::binding::SessionBindingPolicy;
use godot_mcp_server::http::{
    HttpTransportOptions, LaunchSecret, MCP_SESSION_HEADER, bind_http_transport,
};
use godot_mcp_server::kernel::{AccessContext, ServiceKernel};
use godot_mcp_server::rmcp_adapter::RmcpAdapter;
use rmcp::ServiceExt;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

const RESOURCE: &str = "http://127.0.0.1/mcp";
const ANSWER_DEADLINE: Duration = Duration::from_secs(10);

/// JSON-RPC "method not found".
const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC "invalid request".
const INVALID_REQUEST: i64 = -32600;

fn fixture_project() -> ProjectIdentity {
    ProjectIdentity::from_lease_key(
        LeaseKey {
            root: StableObjectId::Unix { dev: 41, ino: 5 },
            project_file: StableObjectId::Unix { dev: 41, ino: 6 },
        },
        1,
    )
}

fn fixture_access() -> AccessContext {
    AccessContext::new(
        fixture_project(),
        PrincipalId::parse(format!("principal_{}", "4".repeat(32))).expect("principal"),
        Some(SessionId::parse(format!("session_{}", "5".repeat(32))).expect("session")),
        1_000,
    )
}

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn initialize(id: u64, version: &str) -> Value {
    request(
        id,
        "initialize",
        json!({
            "protocolVersion": version,
            "capabilities": {},
            "clientInfo": { "name": "lifecycle-probe", "version": "0" }
        }),
    )
}

/// The request `_meta` of the 2026-07-28 lifecycle: the client context
/// `initialize` would otherwise have carried, here naming `version`.
fn inline_meta(version: &str) -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": version,
        "io.modelcontextprotocol/clientInfo": { "name": "lifecycle-probe", "version": "0" },
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

/// `method` with `params`, sent in the inline lifecycle under `version`.
fn inline_request(id: u64, version: &str, method: &str, mut params: Value) -> Value {
    params["_meta"] = inline_meta(version);
    request(id, method, params)
}

fn error_code(frame: &Value) -> Option<i64> {
    frame.pointer("/error/code").and_then(Value::as_i64)
}

fn assert_refused_as_unsupported(frame: &Value, what: &str) {
    assert!(
        frame.get("result").is_none(),
        "{what} must not be served: {frame}"
    );
    assert_eq!(error_code(frame), Some(INVALID_REQUEST), "{what}: {frame}");
    assert_eq!(
        frame.pointer("/error/data/code").and_then(Value::as_str),
        Some("unsupported"),
        "{what}: {frame}"
    );
}

/// One connection to the server over an in-memory stdio pipe, spoken in raw
/// newline-delimited JSON-RPC so a test can send what an SDK client would not.
struct RawStdio {
    lines: tokio::io::Lines<BufReader<ReadHalf<DuplexStream>>>,
    writer: WriteHalf<DuplexStream>,
    server: tokio::task::JoinHandle<()>,
}

impl RawStdio {
    fn start(kernel: ServiceKernel) -> Self {
        let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
        let server = RmcpAdapter::new(Arc::new(kernel), fixture_access());
        let server = tokio::spawn(async move {
            if let Ok(service) = server.serve(server_transport).await {
                let _ = service.waiting().await;
            }
        });
        let (reader, writer) = tokio::io::split(client_transport);
        Self {
            lines: BufReader::new(reader).lines(),
            writer,
            server,
        }
    }

    async fn send(&mut self, frame: &Value) -> anyhow::Result<()> {
        let mut line = serde_json::to_vec(frame)?;
        line.push(b'\n');
        self.writer.write_all(&line).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Send a request and return the frame that answers it, passing over any
    /// notification in between.
    async fn request(&mut self, frame: Value) -> anyhow::Result<Value> {
        let id = frame["id"].clone();
        self.send(&frame).await?;
        let answer = tokio::time::timeout(ANSWER_DEADLINE, async {
            while let Some(line) = self.lines.next_line().await? {
                let answer: Value = serde_json::from_str(&line)?;
                if answer.get("id") == Some(&id) {
                    return Ok(answer);
                }
            }
            anyhow::bail!("the server closed the connection before answering request {id}")
        })
        .await;
        answer.unwrap_or_else(|_| {
            anyhow::bail!("no answer to request {id} within {ANSWER_DEADLINE:?}")
        })
    }

    /// End the connection the way a client does, by closing its side.
    async fn close(self) -> anyhow::Result<()> {
        drop(self.writer);
        drop(self.lines);
        tokio::time::timeout(ANSWER_DEADLINE, self.server).await??;
        Ok(())
    }
}

#[tokio::test]
async fn stdio_serves_only_the_initialize_lifecycle() -> anyhow::Result<()> {
    // The probe is answered as a server that predates it answers, and the same
    // connection then completes the handshake and is served.
    let mut peer = RawStdio::start(ServiceKernel::new(CapabilityProfile::Core));
    let discover = peer
        .request(inline_request(
            1,
            "2026-07-28",
            "server/discover",
            json!({}),
        ))
        .await?;
    assert_eq!(
        error_code(&discover),
        Some(METHOD_NOT_FOUND),
        "server/discover: {discover}"
    );
    let initialized = peer.request(initialize(2, "2026-07-28")).await?;
    assert_eq!(
        initialized
            .pointer("/result/protocolVersion")
            .and_then(Value::as_str),
        Some("2025-11-25"),
        "a client asking for a revision without a handshake is given the newest with one: \
         {initialized}"
    );
    peer.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .await?;
    let tools = peer.request(request(3, "tools/list", json!({}))).await?;
    assert!(
        tools
            .pointer("/result/tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty()),
        "the handshake's session is served: {tools}"
    );
    peer.close().await?;

    // A connection opened by a request in the inline lifecycle is refused, whichever
    // revision the request names.
    for (version, method, params) in [
        ("2025-11-25", "tools/list", json!({})),
        (
            "2026-07-28",
            "tools/call",
            json!({ "name": "capabilities.get", "arguments": {} }),
        ),
        ("2025-11-25", "resources/list", json!({})),
        (
            "2026-07-28",
            "resources/read",
            json!({ "uri": "godot-mcp://resources/capabilities" }),
        ),
    ] {
        let mut peer = RawStdio::start(ServiceKernel::new(CapabilityProfile::Core));
        let answer = peer
            .request(inline_request(1, version, method, params))
            .await?;
        assert_refused_as_unsupported(&answer, &format!("{method} in the {version} lifecycle"));
        peer.close().await?;
    }
    Ok(())
}

/// One answer over HTTP: the JSON-RPC envelope, and the session handle, if any.
struct Answer {
    envelope: Value,
    session: Option<String>,
}

async fn post(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    protocol_version: Option<&str>,
    body: &Value,
) -> anyhow::Result<Answer> {
    let mut request = client
        .post(endpoint)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream");
    if let Some(version) = protocol_version {
        request = request.header("MCP-Protocol-Version", version);
        // The 2026-07-28 revision also names the method in a header (SEP-2243).
        if version >= "2026-07-28" {
            request = request.header("Mcp-Method", body["method"].as_str().unwrap_or_default());
            if let Some(name) = body.pointer("/params/name").and_then(Value::as_str) {
                request = request.header("Mcp-Name", name);
            }
        }
    }
    let response = request.body(serde_json::to_vec(body)?).send().await?;
    let session = response
        .headers()
        .get(MCP_SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let status = response.status();
    let text = response.text().await?;
    let envelope = serde_json::from_str::<Value>(&text)
        .ok()
        .or_else(|| {
            text.lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .find_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        })
        .ok_or_else(|| anyhow::anyhow!("HTTP {status} carried no JSON-RPC envelope: {text:?}"))?;
    Ok(Answer { envelope, session })
}

#[tokio::test(flavor = "multi_thread")]
async fn http_serves_only_the_initialize_lifecycle() -> anyhow::Result<()> {
    let state = tempfile::tempdir()?;
    // Bound the way a configured listener is: every session bound to its subject.
    let policy = SessionBindingPolicy::new(
        CapabilityProfile::Full,
        RESOURCE,
        fixture_project(),
        &Limits::default(),
    )?;
    let transport = bind_http_transport(
        HttpTransportOptions::trusted_local("127.0.0.1:0".parse::<SocketAddr>()?, state.path())
            .with_session_binding(policy),
        Arc::new(ServiceKernel::new(CapabilityProfile::Full)),
        fixture_access(),
    )
    .await?;
    let endpoint = transport.endpoint();
    let token = LaunchSecret::read_token(transport.secret_path().expect("secret path"))?;
    let client = reqwest::Client::builder().no_proxy().build()?;

    // The probe is answered as a server that predates it answers.
    let discover = post(
        &client,
        &endpoint,
        &token,
        Some("2026-07-28"),
        &inline_request(1, "2026-07-28", "server/discover", json!({})),
    )
    .await?;
    assert_eq!(
        error_code(&discover.envelope),
        Some(METHOD_NOT_FOUND),
        "server/discover: {}",
        discover.envelope
    );
    assert!(discover.session.is_none(), "a probe opens no session");

    // An authenticated request in the inline lifecycle, which rmcp would answer
    // without a session, is refused before the kernel sees it.
    for (version, method, params) in [
        (
            "2025-11-25",
            "tools/call",
            json!({ "name": "capabilities.get", "arguments": {} }),
        ),
        ("2026-07-28", "tools/list", json!({})),
        (
            "2025-11-25",
            "resources/read",
            json!({ "uri": "godot-mcp://resources/capabilities" }),
        ),
        ("2025-11-25", "resources/list", json!({})),
    ] {
        let answer = post(
            &client,
            &endpoint,
            &token,
            Some(version),
            &inline_request(2, version, method, params),
        )
        .await?;
        assert_refused_as_unsupported(
            &answer.envelope,
            &format!("{method} in the {version} lifecycle"),
        );
        assert!(answer.session.is_none(), "{method} opened a session");
    }
    assert_eq!(
        transport.kernel_dispatches(),
        0,
        "no request in the inline lifecycle reached the kernel"
    );

    // The handshake still opens a session, at the newest revision with one.
    let opened = post(
        &client,
        &endpoint,
        &token,
        None,
        &initialize(3, "2026-07-28"),
    )
    .await?;
    assert_eq!(
        opened
            .envelope
            .pointer("/result/protocolVersion")
            .and_then(Value::as_str),
        Some("2025-11-25"),
        "{}",
        opened.envelope
    );
    assert!(opened.session.is_some(), "initialize opens a session");

    transport.shutdown().await?;
    Ok(())
}
