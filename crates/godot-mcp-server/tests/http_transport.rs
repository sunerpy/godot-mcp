//! Todo 28 acceptance: the optional Streamable HTTP transport, its staged
//! authentication boundary, and its parity with stdio.
//!
//! Exactly two cases exist, matching `qa/tasks/28.json`:
//!
//! * `http_transport_happy_path` — an ephemeral `trusted_local` client reads the
//!   ACL-protected launch secret and drives initialize/list/call/stream/cancel to
//!   the *same* normalized results stdio produces.
//! * `http_transport_failure_paths` — missing/stale/wrong secret, a direct
//!   non-loopback bind, unarmed `proxied_oidc` dispatch, forwarded
//!   headers, an HTTP (non-HTTPS) external URL, invalid `Origin`/`Host`, an
//!   oversize body and a query-string token are all refused before the kernel
//!   dispatches anything.
//!
//! The parity comparison is a *shared* suite: [`parity_snapshot`] is written once
//! and executed against both transports, so a divergence in either direction
//! fails. Neither transport has its own tool logic — both drive
//! [`RmcpAdapter`] over one [`ServiceKernel`].

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::config::{HttpMode, Limits, ServerConfig, Transport};
use godot_mcp_domain::contracts::{
    ArtifactContract, ArtifactId, ArtifactKind, ArtifactMetadata, ArtifactOwner,
    ArtifactResourceUri, ErrorKind, OutcomeError, PrincipalId, ProjectIdentity, RangeSemantics,
    SessionId,
};
use godot_mcp_domain::objid::{LeaseKey, StableObjectId};
use godot_mcp_server::http::{
    GuardRejection, HttpTransportError, HttpTransportOptions, LAUNCH_SECRET_FILE_MODE,
    LaunchSecret, MCP_PATH, bind_http_transport,
};
use godot_mcp_server::idempotency::{ModePrincipal, TransportAuthMode};
use godot_mcp_server::kernel::{AccessContext, KernelToolCall, KernelToolHandler, ServiceKernel};
use godot_mcp_server::rmcp_adapter::{ARTIFACT_RANGE_META_KEY, RmcpAdapter};
use godot_mcp_server::stdio::{ProjectRuntime, StdioOptions, state_dir_for};
use rmcp::handler::client::progress::ProgressDispatcher;
use rmcp::model::{
    CallToolRequestParams, ClientRequest, GetPromptRequestParams, Meta, ReadResourceRequestParams,
    Request, ResourceContents,
};
use rmcp::service::{PeerRequestOptions, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientHandler, RoleClient, ServiceError, ServiceExt};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Fixture kernel: one builder, used by both transports.
// ---------------------------------------------------------------------------

const LIVE_ARTIFACT_EXPIRY_UNIX_MS: u64 = u64::MAX;

#[derive(Clone, Default)]
struct TestClient {
    progress: ProgressDispatcher,
}

impl ClientHandler for TestClient {
    async fn on_progress(
        &self,
        notification: rmcp::model::ProgressNotificationParam,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) {
        self.progress.handle_notification(notification).await;
    }
}

struct CancellationProbe {
    cancelled: mpsc::UnboundedSender<()>,
}

#[async_trait]
impl KernelToolHandler for CancellationProbe {
    async fn call(&self, call: KernelToolCall) -> Result<Value, OutcomeError> {
        call.progress
            .report(0.0, Some(1.0), Some("waiting for cancellation"))
            .await;
        call.cancellation.cancelled().await;
        let _ = self.cancelled.send(());
        Err(OutcomeError::new(
            ErrorKind::Cancelled,
            "request cancelled",
            false,
            BTreeMap::new(),
        ))
    }
}

fn fixture_project() -> ProjectIdentity {
    ProjectIdentity::from_lease_key(
        LeaseKey {
            root: StableObjectId::Unix { dev: 21, ino: 34 },
            project_file: StableObjectId::Unix { dev: 21, ino: 55 },
        },
        4,
    )
}

fn fixture_principal() -> PrincipalId {
    PrincipalId::parse("principal_00112233445566778899aabbccddeeff").expect("principal")
}

fn fixture_session() -> SessionId {
    SessionId::parse("session_00112233445566778899aabbccddeeff").expect("session")
}

fn fixture_access() -> AccessContext {
    AccessContext::new(
        fixture_project(),
        fixture_principal(),
        Some(fixture_session()),
        4_000,
    )
}

fn fixture_artifact() -> ArtifactContract {
    let artifact_id = ArtifactId::parse(format!("artifact_{}", "b".repeat(64))).expect("id");
    ArtifactContract::new(
        artifact_id.clone(),
        ArtifactResourceUri::for_artifact(&artifact_id),
        fixture_project(),
        Some(ArtifactOwner {
            principal: Some(fixture_principal()),
            session: Some(fixture_session()),
        }),
        ArtifactMetadata {
            kind: ArtifactKind::Report,
            media_type: "application/octet-stream".to_owned(),
            byte_length: 5,
            sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_owned(),
            created_at_unix_ms: 1_500,
        },
        LIVE_ARTIFACT_EXPIRY_UNIX_MS,
        RangeSemantics::new(5, 3, true).expect("range semantics"),
    )
    .expect("artifact contract")
}

fn fixture_artifact_uri() -> String {
    fixture_artifact().resource_uri().as_str().to_owned()
}

/// Build the fixture kernel. Called once per transport so both serve byte-identical
/// capability surfaces from the same construction path.
fn fixture_kernel(cancelled: mpsc::UnboundedSender<()>) -> anyhow::Result<ServiceKernel> {
    let mut kernel = ServiceKernel::new(CapabilityProfile::Full);
    kernel.register_artifact(fixture_artifact(), b"hello".to_vec(), &fixture_access())?;
    kernel.register_tool_handler(
        godot_mcp_domain::catalog::descriptor("project.validate")
            .and_then(|tool| tool.handler)
            .expect("a catalog tool"),
        Arc::new(CancellationProbe { cancelled }),
    )?;
    Ok(kernel)
}

// ---------------------------------------------------------------------------
// Shared parity suite.
// ---------------------------------------------------------------------------

/// One normalized observation of the whole MCP surface.
///
/// Every field is transport-independent by construction: session identifiers,
/// SSE event ids, socket addresses and wall-clock values are excluded, so two
/// snapshots may only differ if the *kernel* answered differently.
#[derive(Debug, PartialEq, Eq)]
struct ParitySnapshot {
    server_name: String,
    tools: BTreeSet<String>,
    resources: BTreeSet<String>,
    resource_templates: BTreeSet<String>,
    prompts: Vec<String>,
    prompt_messages: usize,
    profile_manifests: Value,
    capabilities_resource: String,
    artifact_range_blob: String,
    invalid_params_error: (i32, String),
    unknown_tool_error: (i32, String),
    cancelled_observed: bool,
}

async fn parity_snapshot(
    client: &RunningService<RoleClient, TestClient>,
    cancelled_rx: &mut mpsc::UnboundedReceiver<()>,
) -> anyhow::Result<ParitySnapshot> {
    let server_name = client
        .peer_info()
        .expect("initialize returned server info")
        .server_info
        .name
        .clone();

    let tools: BTreeSet<String> = client
        .list_all_tools()
        .await?
        .into_iter()
        .map(|tool| tool.name.into_owned())
        .collect();
    let resources: BTreeSet<String> = client
        .list_all_resources()
        .await?
        .into_iter()
        .map(|resource| resource.uri)
        .collect();
    let resource_templates: BTreeSet<String> = client
        .list_all_resource_templates()
        .await?
        .into_iter()
        .map(|template| template.uri_template)
        .collect();
    let listed_prompts = client.list_all_prompts().await?;
    let prompts: Vec<String> = listed_prompts
        .iter()
        .map(|prompt| prompt.name.clone())
        .collect();
    let prompt_messages = client
        .get_prompt(GetPromptRequestParams::new(listed_prompts[0].name.clone()))
        .await?
        .messages
        .len();

    let profile_manifests = client
        .call_tool(CallToolRequestParams::new("capabilities.get"))
        .await?
        .structured_content
        .expect("structured profile manifests");

    let capabilities_resource = match &client
        .read_resource(ReadResourceRequestParams::new(
            "godot-mcp://resources/capabilities",
        ))
        .await?
        .contents[0]
    {
        ResourceContents::TextResourceContents { text, .. } => text.clone(),
        other => anyhow::bail!("expected text capabilities resource, got {other:?}"),
    };

    let mut range_meta = Meta::new();
    range_meta.0.insert(
        ARTIFACT_RANGE_META_KEY.to_owned(),
        json!({ "start": 1, "endInclusive": 3 }),
    );
    let artifact_range_blob = match &client
        .read_resource(ReadResourceRequestParams::new(fixture_artifact_uri()).with_meta(range_meta))
        .await?
        .contents[0]
    {
        ResourceContents::BlobResourceContents { blob, .. } => blob.clone(),
        other => anyhow::bail!("expected bounded artifact blob, got {other:?}"),
    };

    let mut arguments = Map::new();
    arguments.insert("profile".to_owned(), json!(42));
    let invalid_params_error = protocol_error(
        client
            .call_tool(CallToolRequestParams::new("capabilities.get").with_arguments(arguments))
            .await
            .expect_err("malformed profile must be a protocol error"),
    )?;
    let unknown_tool_error = protocol_error(
        client
            .call_tool(CallToolRequestParams::new("tool.does.not.exist"))
            .await
            .expect_err("unknown tool must be a protocol error"),
    )?;

    let request = ClientRequest::CallToolRequest(Request::new(CallToolRequestParams::new(
        "project.validate",
    )));
    let handle = client
        .send_cancellable_request(request, PeerRequestOptions::no_options())
        .await?;
    let mut progress = client
        .service()
        .progress
        .subscribe(handle.progress_token.clone())
        .await;
    let update = tokio::time::timeout(Duration::from_secs(10), progress.next())
        .await?
        .expect("progress notification");
    assert_eq!(update.message.as_deref(), Some("waiting for cancellation"));
    handle
        .cancel(Some("parity cancellation".to_owned()))
        .await?;
    let cancelled_observed = tokio::time::timeout(Duration::from_secs(10), cancelled_rx.recv())
        .await?
        .is_some();

    Ok(ParitySnapshot {
        server_name,
        tools,
        resources,
        resource_templates,
        prompts,
        prompt_messages,
        profile_manifests,
        capabilities_resource,
        artifact_range_blob,
        invalid_params_error,
        unknown_tool_error,
        cancelled_observed,
    })
}

fn protocol_error(error: ServiceError) -> anyhow::Result<(i32, String)> {
    match error {
        ServiceError::McpError(error) => Ok((
            error.code.0,
            error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        )),
        other => anyhow::bail!("expected an MCP protocol error, got {other:?}"),
    }
}

async fn stdio_client(
    kernel: ServiceKernel,
) -> anyhow::Result<(
    RunningService<RoleClient, TestClient>,
    tokio::task::JoinHandle<()>,
)> {
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server = RmcpAdapter::new(Arc::new(kernel), fixture_access());
    let server_task = tokio::spawn(async move {
        if let Ok(service) = server.serve(server_transport).await {
            let _ = service.waiting().await;
        }
    });
    let client = TestClient::default().serve(client_transport).await?;
    Ok((client, server_task))
}

async fn http_client(
    endpoint: &str,
    token: &str,
) -> anyhow::Result<RunningService<RoleClient, TestClient>> {
    let transport = StreamableHttpClientTransport::with_client(
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("reqwest client"),
        StreamableHttpClientTransportConfig::with_uri(endpoint).auth_header(token),
    );
    Ok(TestClient::default().serve(transport).await?)
}

// ---------------------------------------------------------------------------
// Raw HTTP probes used by the failure suite.
// ---------------------------------------------------------------------------

fn raw_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("reqwest client")
}

fn initialize_body() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "todo-28-probe", "version": "0" }
        }
    })
}

fn call_tool_body() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "capabilities.get", "arguments": {} }
    })
}

struct Probe {
    url: String,
    body: Value,
    headers: Vec<(&'static str, String)>,
}

impl Probe {
    fn new(endpoint: &str) -> Self {
        Self {
            url: endpoint.to_owned(),
            body: initialize_body(),
            headers: Vec::new(),
        }
    }

    fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    fn body(mut self, body: Value) -> Self {
        self.body = body;
        self
    }

    fn url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    async fn send(self, client: &reqwest::Client) -> reqwest::Response {
        let mut request = client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        for (name, value) in &self.headers {
            request = request.header(*name, value);
        }
        request
            .body(serde_json::to_vec(&self.body).expect("serialize probe body"))
            .send()
            .await
            .expect("probe request")
    }
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

// ---------------------------------------------------------------------------
// Case 1 — happy path.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn http_transport_happy_path() -> anyhow::Result<()> {
    let state = tempfile::tempdir()?;

    // The launch secret is created fresh, ACL-protected, and readable only as a
    // whole document: an ephemeral client learns the token exactly the way a
    // co-located MCP client does.
    let secret = LaunchSecret::create(state.path())?;
    assert!(secret.path().is_file());
    assert_eq!(
        LaunchSecret::mode_of(secret.path())?,
        LAUNCH_SECRET_FILE_MODE
    );
    let token_from_file = LaunchSecret::read_token(secret.path())?;
    assert_eq!(token_from_file, secret.bearer_token());
    assert!(secret.matches_bearer(&token_from_file));
    assert_eq!(
        secret.mode_principal(),
        ModePrincipal::LaunchSecret {
            principal: secret.principal().as_str().to_owned(),
        }
    );
    assert_eq!(
        secret.mode_principal().required_mode(),
        TransportAuthMode::TrustedLocal
    );

    // Constant-time validation: a candidate differing in its first byte and one
    // differing in its last byte examine the same number of bytes.
    let mut first_differs: Vec<char> = token_from_file.chars().collect();
    first_differs[0] = if first_differs[0] == '0' { '1' } else { '0' };
    let mut last_differs: Vec<char> = token_from_file.chars().collect();
    let last = last_differs.len() - 1;
    last_differs[last] = if last_differs[last] == '0' { '1' } else { '0' };
    let first_differs: String = first_differs.into_iter().collect();
    let last_differs: String = last_differs.into_iter().collect();
    assert!(!secret.matches_bearer(&first_differs));
    assert!(!secret.matches_bearer(&last_differs));
    assert_eq!(
        secret.compare_probe(&first_differs),
        secret.compare_probe(&last_differs),
        "constant-time comparison must not short-circuit"
    );

    // Default configuration opens no TCP socket at all.
    let default_config = ServerConfig::default();
    assert_eq!(default_config.transport, Transport::Stdio);
    assert!(default_config.http.is_none());
    assert!(
        HttpTransportOptions::from_config(&default_config, state.path())?.is_none(),
        "the default configuration must not describe an HTTP listener"
    );

    // Both transports over the same kernel construction path.
    let (stdio_cancelled_tx, mut stdio_cancelled_rx) = mpsc::unbounded_channel();
    let (stdio, stdio_task) = stdio_client(fixture_kernel(stdio_cancelled_tx)?).await?;
    let stdio_snapshot = parity_snapshot(&stdio, &mut stdio_cancelled_rx).await?;
    stdio.cancel().await?;
    stdio_task.await?;

    let (http_cancelled_tx, mut http_cancelled_rx) = mpsc::unbounded_channel();
    let options =
        HttpTransportOptions::trusted_local("127.0.0.1:0".parse::<SocketAddr>()?, state.path());
    let transport = bind_http_transport(
        options,
        Arc::new(fixture_kernel(http_cancelled_tx)?),
        fixture_access(),
    )
    .await?;
    assert!(transport.local_addr().ip().is_loopback());
    let endpoint = transport.endpoint();
    assert!(endpoint.ends_with(MCP_PATH));
    let token = LaunchSecret::read_token(
        transport
            .secret_path()
            .expect("trusted_local publishes a secret file"),
    )?;

    let http = http_client(&endpoint, &token).await?;
    let http_snapshot = parity_snapshot(&http, &mut http_cancelled_rx).await?;
    http.cancel().await?;

    assert_eq!(
        http_snapshot, stdio_snapshot,
        "authenticated trusted_local HTTP and stdio must produce equivalent normalized results"
    );

    // Graceful shutdown removes the per-launch secret.
    let secret_path = transport.secret_path().map(Path::to_path_buf);
    transport.shutdown().await?;
    let secret_path = secret_path.expect("secret path");
    assert!(
        !secret_path.exists(),
        "shutdown must remove the per-launch secret"
    );

    // Restart rotates the material: the previous token is no longer this launch's.
    let rotated = LaunchSecret::create(state.path())?;
    assert_ne!(rotated.bearer_token(), secret.bearer_token());
    assert!(!rotated.matches_bearer(&secret.bearer_token()));
    rotated.remove()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn production_http_health_reports_the_bound_transport_and_live_auth_mode()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project)?;
    std::fs::write(project.join("project.godot"), b"config_version=5\n")?;
    let runtime = ProjectRuntime::open(&StdioOptions {
        project: project.clone(),
        godot_binary: None,
        profile: CapabilityProfile::Core,
        principal: fixture_principal(),
        observed_at_unix_ms: 4_000,
        limits: Limits::default(),
        process: Default::default(),
    })?;

    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server = RmcpAdapter::new(runtime.kernel(), runtime.access());
    let server_task = tokio::spawn(async move {
        if let Ok(service) = server.serve(server_transport).await {
            let _ = service.waiting().await;
        }
    });
    let stdio = TestClient::default().serve(client_transport).await?;
    let stdio_health = stdio
        .call_tool(CallToolRequestParams::new("health.get"))
        .await?
        .structured_content
        .expect("stdio health structured content");
    stdio.cancel().await?;
    server_task.await?;

    let state_dir = state_dir_for(&project);
    let transport = bind_http_transport(
        HttpTransportOptions::trusted_local("127.0.0.1:0".parse::<SocketAddr>()?, &state_dir),
        runtime.kernel(),
        runtime.access(),
    )
    .await?;
    let secret_path = transport
        .secret_path()
        .expect("trusted_local publishes a secret")
        .to_path_buf();
    let token = LaunchSecret::read_token(&secret_path)?;
    let http = http_client(&transport.endpoint(), &token).await?;
    let ready_health = http
        .call_tool(CallToolRequestParams::new("health.get"))
        .await?
        .structured_content
        .expect("HTTP health structured content");

    assert_eq!(
        ready_health["transport"], "http",
        "health.get reached over the bound HTTP listener must report transport=http, got {}",
        ready_health["transport"]
    );
    assert_eq!(
        ready_health["http"]["mode"], "trusted_local",
        "health.get reached over trusted_local HTTP must report that configured mode, got {}",
        ready_health["http"]["mode"]
    );
    assert_eq!(
        ready_health.as_object().map(serde_json::Map::len),
        Some(26),
        "binding HTTP must not change the frozen 26-key health shape"
    );
    assert_eq!(
        ready_health["components"], stdio_health["components"],
        "the five legacy components must stay byte-identical across the lane declaration"
    );
    assert!(
        !ready_health["codes"]
            .as_array()
            .expect("diagnostic codes")
            .contains(&json!("local_secret_missing")),
        "the published launch secret must be sampled as ready"
    );

    let replacement = LaunchSecret::create(&state_dir)?;
    let mismatched_health = http
        .call_tool(CallToolRequestParams::new("health.get"))
        .await?
        .structured_content
        .expect("HTTP health after secret replacement");
    assert!(
        mismatched_health["codes"]
            .as_array()
            .expect("diagnostic codes")
            .contains(&json!("local_secret_unreadable")),
        "a readable file that no longer contains the live guard secret must not report ready"
    );

    replacement.remove()?;
    let missing_health = http
        .call_tool(CallToolRequestParams::new("health.get"))
        .await?
        .structured_content
        .expect("HTTP health after secret removal");
    assert_eq!(
        missing_health["http"]["mode"], "trusted_local",
        "losing the live secret must not erase the configured HTTP mode"
    );
    assert!(
        missing_health["codes"]
            .as_array()
            .expect("diagnostic codes")
            .contains(&json!("local_secret_missing")),
        "health.get must sample the live auth state after the launch-secret file disappears"
    );

    http.cancel().await?;
    transport.shutdown().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Case 2 — failure paths.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn http_transport_failure_paths() -> anyhow::Result<()> {
    let state = tempfile::tempdir()?;
    let client = raw_client();

    // A direct non-loopback bind is refused before any socket is created.
    let non_loopback =
        HttpTransportOptions::trusted_local("0.0.0.0:0".parse::<SocketAddr>()?, state.path());
    let (sink, _drain) = mpsc::unbounded_channel();
    let refused = bind_http_transport(
        non_loopback,
        Arc::new(fixture_kernel(sink)?),
        fixture_access(),
    )
    .await
    .expect_err("a non-loopback bind must be refused");
    assert!(matches!(
        refused,
        HttpTransportError::NonLoopbackBind { .. }
    ));

    // An HTTP (non-HTTPS) external resource URL is refused for proxied_oidc.
    let insecure_external = HttpTransportOptions::proxied_oidc(
        "127.0.0.1:0".parse::<SocketAddr>()?,
        "http://mcp.example.com/mcp",
    );
    let (sink, _drain) = mpsc::unbounded_channel();
    let refused = bind_http_transport(
        insecure_external,
        Arc::new(fixture_kernel(sink)?),
        fixture_access(),
    )
    .await
    .expect_err("an http:// external resource URL must be refused");
    assert!(matches!(
        refused,
        HttpTransportError::InsecureResourceUrl { .. }
    ));

    // A relaxed secret file is refused rather than silently trusted.
    let relaxed = LaunchSecret::create(state.path())?;
    let relaxed_path = relaxed.path().to_path_buf();
    LaunchSecret::set_mode(&relaxed_path, 0o644)?;
    #[cfg(unix)]
    assert!(matches!(
        LaunchSecret::read_token(&relaxed_path),
        Err(godot_mcp_server::http::LaunchSecretError::InsecurePermissions { .. })
    ));
    relaxed.remove()?;

    // --- The authenticated trusted_local listener under attack. ---
    let (sink, _drain) = mpsc::unbounded_channel();
    let bounded_limits = Limits {
        max_body_bytes: 4_096,
        ..Limits::default()
    };
    let transport = bind_http_transport(
        HttpTransportOptions::trusted_local("127.0.0.1:0".parse::<SocketAddr>()?, state.path())
            .with_limits(bounded_limits),
        Arc::new(fixture_kernel(sink)?),
        fixture_access(),
    )
    .await?;
    let endpoint = transport.endpoint();
    let token = LaunchSecret::read_token(transport.secret_path().expect("secret path"))?;

    // Missing, wrong and stale credentials are indistinguishable from each other.
    let stale = LaunchSecret::create(state.path())?;
    let stale_token = stale.bearer_token();
    stale.remove()?;
    let mut unauthorized_bodies = BTreeSet::new();
    for credential in [None, Some(stale_token.clone()), Some("00".repeat(32))] {
        let mut probe = Probe::new(&endpoint);
        if let Some(credential) = &credential {
            probe = probe.header("Authorization", bearer(credential));
        }
        let response = probe.send(&client).await;
        assert_eq!(
            response.status().as_u16(),
            GuardRejection::Unauthorized.status_code(),
            "credential {credential:?} must be refused"
        );
        unauthorized_bodies.insert(response.text().await?);
    }
    assert_eq!(
        unauthorized_bodies.len(),
        1,
        "a missing, stale or wrong secret must produce byte-identical refusals"
    );

    // A query-string credential is never accepted.
    let response = Probe::new(&endpoint)
        .url(format!("{endpoint}?access_token={token}"))
        .send(&client)
        .await;
    assert_eq!(
        response.status().as_u16(),
        GuardRejection::QueryCredential.status_code()
    );

    // Forwarded headers are not trusted at this stage.
    for header in [
        "X-Forwarded-For",
        "X-Forwarded-Host",
        "X-Forwarded-Proto",
        "Forwarded",
    ] {
        let response = Probe::new(&endpoint)
            .header("Authorization", bearer(&token))
            .header(header, "127.0.0.1")
            .send(&client)
            .await;
        assert_eq!(
            response.status().as_u16(),
            GuardRejection::ForwardedHeader.status_code(),
            "{header} must be refused"
        );
    }

    // An unknown protocol version, a disallowed Host and a disallowed Origin.
    let response = Probe::new(&endpoint)
        .header("Authorization", bearer(&token))
        .header("MCP-Protocol-Version", "1999-01-01")
        .send(&client)
        .await;
    assert_eq!(
        response.status().as_u16(),
        GuardRejection::UnsupportedProtocolVersion.status_code()
    );

    let response = Probe::new(&endpoint)
        .header("Authorization", bearer(&token))
        .header("Host", "attacker.example.com")
        .send(&client)
        .await;
    assert_eq!(
        response.status().as_u16(),
        GuardRejection::HostNotAllowed.status_code()
    );

    let response = Probe::new(&endpoint)
        .header("Authorization", bearer(&token))
        .header("Origin", "https://attacker.example.com")
        .send(&client)
        .await;
    assert_eq!(
        response.status().as_u16(),
        GuardRejection::OriginNotAllowed.status_code()
    );

    // An oversize body is refused by the limit layer, outside the kernel: the
    // configured 4 KiB ceiling is exceeded by a declared Content-Length, so the
    // refusal happens without the body ever being read.
    let oversize = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "initialize",
        "params": { "padding": "x".repeat(8 * 1024) }
    });
    let response = Probe::new(&endpoint)
        .header("Authorization", bearer(&token))
        .body(oversize)
        .send(&client)
        .await;
    assert_eq!(response.status().as_u16(), 413);

    // An unknown session is refused before dispatch.
    let response = Probe::new(&endpoint)
        .header("Authorization", bearer(&token))
        .header("Mcp-Session-Id", "not-a-live-session")
        .body(call_tool_body())
        .send(&client)
        .await;
    assert_eq!(response.status().as_u16(), 404);

    // No refused request ever reached the kernel.
    assert_eq!(
        transport.kernel_dispatches(),
        0,
        "every refusal must happen before kernel dispatch"
    );
    transport.shutdown().await?;

    // --- proxied_oidc: a listener without an authentication boundary fails closed. ---
    let (sink, _drain) = mpsc::unbounded_channel();
    let proxied = bind_http_transport(
        HttpTransportOptions::proxied_oidc(
            "127.0.0.1:0".parse::<SocketAddr>()?,
            "https://mcp.example.com/mcp",
        ),
        Arc::new(fixture_kernel(sink)?),
        fixture_access(),
    )
    .await?;
    assert!(proxied.local_addr().ip().is_loopback());
    assert_eq!(
        proxied.advertised_resource_url(),
        Some("https://mcp.example.com/mcp")
    );
    assert!(
        proxied.secret_path().is_none(),
        "proxied_oidc must not publish a launch secret"
    );

    let proxied_endpoint = proxied.endpoint();
    let proxied_client = http_client(&proxied_endpoint, "unused-in-proxied-mode").await?;
    let refusal = proxied_client
        .call_tool(CallToolRequestParams::new("capabilities.get"))
        .await?;
    assert_eq!(refusal.is_error, Some(true));
    assert_eq!(
        refusal
            .structured_content
            .as_ref()
            .and_then(|value| value.get("code"))
            .and_then(Value::as_str),
        Some("unavailable"),
        "unarmed proxied_oidc tool dispatch must be unavailable"
    );
    let resource_refusal = proxied_client
        .read_resource(ReadResourceRequestParams::new(
            "godot-mcp://resources/capabilities",
        ))
        .await
        .expect_err("proxied_oidc resource dispatch must be unavailable");
    assert_eq!(protocol_error(resource_refusal)?.1, "unavailable");
    proxied_client.cancel().await?;

    // Even a loopback peer carrying forwarded headers gets no dispatch.
    for header in ["X-Forwarded-For", "X-Forwarded-Host", "Forwarded"] {
        let response = Probe::new(&proxied_endpoint)
            .header(header, "10.0.0.7")
            .send(&client)
            .await;
        assert_eq!(
            response.status().as_u16(),
            GuardRejection::ForwardedHeader.status_code(),
            "{header} must be refused in proxied_oidc too"
        );
    }
    assert_eq!(
        proxied.kernel_dispatches(),
        0,
        "unarmed proxied_oidc must never dispatch to the kernel"
    );

    // The configured mode, not the peer address, is what decides the boundary.
    assert_eq!(proxied.auth_mode(), TransportAuthMode::ProxiedOidc);
    assert_eq!(
        godot_mcp_server::http::auth_mode_of(HttpMode::TrustedLocal),
        TransportAuthMode::TrustedLocal
    );
    proxied.shutdown().await?;
    Ok(())
}
