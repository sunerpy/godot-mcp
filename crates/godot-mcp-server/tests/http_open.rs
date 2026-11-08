//! ADR 0014 acceptance: the `open` HTTP mode for development networks.
//!
//! An open listener binds any address and asks for no credential unless one
//! fixed token is configured. It still refuses what a web page or a proxy
//! sends (an `Origin` header, forwarded headers, a query string) and reports
//! its mode as `open`. The `trusted_local` and `proxied_oidc` modes keep their
//! loopback-only bind; `http_transport.rs` covers them.
//!
//! The production cases drive the path `godot-mcp serve --http` takes: the
//! layered configuration loader, `bind_configured_transport` and the live
//! listener.

#![cfg(feature = "http")]

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::config::{
    CliOverrides, HttpMode, Limits, Secret, ServerConfig, Transport, load_config,
};
use godot_mcp_domain::contracts::{PrincipalId, ProjectIdentity, SessionId};
use godot_mcp_domain::objid::{LeaseKey, StableObjectId};
use godot_mcp_server::http::{
    GuardRejection, HttpTransportError, HttpTransportOptions, MCP_PATH, RunningHttpTransport,
    bind_configured_transport, bind_http_transport,
};
use godot_mcp_server::idempotency::TransportAuthMode;
use godot_mcp_server::kernel::{AccessContext, ServiceKernel};
use godot_mcp_server::stdio::{ProjectRuntime, StdioOptions};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

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
            "clientInfo": { "name": "open-mode-probe", "version": "0" }
        }
    })
}

/// POST an `initialize` to `url` with `headers`, and answer the status.
async fn probe(url: &str, headers: &[(&str, &str)]) -> u16 {
    let mut request = raw_client()
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request
        .body(serde_json::to_vec(&initialize_body()).expect("serialize the probe"))
        .send()
        .await
        .expect("the probe is answered")
        .status()
        .as_u16()
}

/// An rmcp client of `endpoint`, presenting `token` when one is given.
async fn client(
    endpoint: &str,
    token: Option<&str>,
) -> anyhow::Result<RunningService<RoleClient, ()>> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(endpoint);
    if let Some(token) = token {
        config = config.auth_header(token);
    }
    let transport = StreamableHttpClientTransport::with_client(raw_client(), config);
    Ok(().serve(transport).await?)
}

/// The address this host sends from to reach another host: the local address
/// of a UDP socket connected to the documentation address 192.0.2.1, which
/// sends no packet. `None` on a host without such a route.
fn lan_address() -> Option<IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    let local = socket.local_addr().ok()?.ip();
    (!local.is_unspecified() && !local.is_loopback()).then_some(local)
}

/// One project served over an open listener built from `config`.
struct Served {
    transport: RunningHttpTransport,
    _runtime: ProjectRuntime,
    _temp: tempfile::TempDir,
}

impl Served {
    /// The URL a client on this host dials, whatever address was bound.
    fn local_endpoint(&self) -> String {
        let port = self.transport.local_addr().port();
        format!("http://127.0.0.1:{port}{MCP_PATH}")
    }
}

async fn serve(config: &ServerConfig) -> anyhow::Result<Served> {
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
        project_commands: true,
        android: Default::default(),
    })?;
    let transport = bind_configured_transport(&runtime, config, CapabilityProfile::Core)
        .await
        .map_err(anyhow::Error::msg)?;
    Ok(Served {
        transport,
        _runtime: runtime,
        _temp: temp,
    })
}

/// The configuration `godot-mcp serve --http <bind>` builds.
fn open_config(bind: &str) -> ServerConfig {
    load_config(
        None,
        &BTreeMap::new(),
        &CliOverrides {
            transport: Some(Transport::Http),
            http_bind: Some(bind.parse().expect("socket address")),
            http_mode: Some(HttpMode::Open),
            ..CliOverrides::default()
        },
    )
    .expect("the open listener's configuration validates")
}

#[tokio::test(flavor = "multi_thread")]
async fn an_open_listener_on_every_interface_serves_any_host_without_a_credential()
-> anyhow::Result<()> {
    let served = serve(&open_config("0.0.0.0:0")).await?;
    let transport = &served.transport;
    assert!(transport.local_addr().ip().is_unspecified());
    assert_eq!(transport.auth_mode(), TransportAuthMode::Open);
    assert!(
        transport.secret_path().is_none(),
        "an open listener publishes no secret"
    );
    assert!(!transport.requires_open_token());

    // A client presents nothing and is served; health names the mode.
    let endpoint = served.local_endpoint();
    let mcp = client(&endpoint, None).await?;
    let health = mcp
        .call_tool(CallToolRequestParams::new("health.get"))
        .await?
        .structured_content
        .expect("health structured content");
    assert_eq!(health["transport"], "http", "{health}");
    assert_eq!(health["http"]["mode"], "open", "{health}");
    assert_eq!(
        health.as_object().map(serde_json::Map::len),
        Some(26),
        "the open mode keeps the frozen 26-key health shape"
    );
    let tools = mcp.list_all_tools().await?;
    assert!(tools.iter().any(|tool| tool.name == "health.get"));
    mcp.cancel().await?;

    // Any Host is accepted: the network may name this machine anything.
    for host in [
        "build-box.example.invalid:8765",
        "192.0.2.44",
        "[2001:db8::7]:80",
    ] {
        assert_eq!(probe(&endpoint, &[("Host", host)]).await, 200, "{host}");
    }
    // Another host on the network reaches it by this host's LAN address.
    if let Some(lan) = lan_address() {
        let url = format!(
            "http://{}{MCP_PATH}",
            SocketAddr::new(lan, transport.local_addr().port())
        );
        assert_eq!(probe(&url, &[]).await, 200, "{url}");
    } else {
        eprintln!("no non-loopback route on this host; the LAN dial is not exercised");
    }
    assert!(transport.kernel_dispatches() > 0);
    served.transport.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn an_open_listener_with_a_token_admits_that_token_only() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let secrets = tempfile::tempdir()?;
    std::fs::set_permissions(secrets.path(), std::fs::Permissions::from_mode(0o700))?;
    let token_file = secrets.path().join("lan-token");
    std::fs::write(&token_file, b"lan-token-1\n")?;
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600))?;
    let config = load_config(
        Some(&format!(
            "transport = \"http\"\n\n[http]\nbind = \"127.0.0.1:0\"\nmode = \"open\"\n\n\
             [http.token.file]\npath = \"{}\"\n",
            token_file.display()
        )),
        &BTreeMap::new(),
        &CliOverrides::default(),
    )?;
    let served = serve(&config).await?;
    assert!(served.transport.requires_open_token());
    let endpoint = served.transport.endpoint();

    for (credential, status) in [
        (None, 401),
        (Some("Bearer lan-token-2"), 401),
        (Some("Bearer LAN-TOKEN-1"), 401),
        (Some("lan-token-1"), 401),
        (Some("Bearer lan-token-1"), 200),
    ] {
        let headers: Vec<(&str, &str)> = credential
            .map(|value| vec![("Authorization", value)])
            .unwrap_or_default();
        assert_eq!(probe(&endpoint, &headers).await, status, "{credential:?}");
    }
    let mcp = client(&endpoint, Some("lan-token-1")).await?;
    let health = mcp
        .call_tool(CallToolRequestParams::new("health.get"))
        .await?
        .structured_content
        .expect("health structured content");
    assert_eq!(health["http"]["mode"], "open", "{health}");
    assert!(!health.to_string().contains("lan-token-1"), "{health}");
    mcp.cancel().await?;
    served.transport.shutdown().await?;
    Ok(())
}

fn fixture_access() -> AccessContext {
    AccessContext::new(
        ProjectIdentity::from_lease_key(
            LeaseKey {
                root: StableObjectId::Unix { dev: 21, ino: 34 },
                project_file: StableObjectId::Unix { dev: 21, ino: 55 },
            },
            4,
        ),
        PrincipalId::parse("principal_00112233445566778899aabbccddeeff").expect("principal"),
        Some(SessionId::parse("session_00112233445566778899aabbccddeeff").expect("session")),
        4_000,
    )
}

async fn bind_open(options: HttpTransportOptions) -> anyhow::Result<RunningHttpTransport> {
    Ok(bind_http_transport(
        options,
        Arc::new(ServiceKernel::new(CapabilityProfile::Core)),
        fixture_access(),
    )
    .await?)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_open_listener_refuses_what_a_web_page_or_a_proxy_sends() -> anyhow::Result<()> {
    let transport = bind_open(HttpTransportOptions::open("127.0.0.1:0".parse()?)).await?;
    let endpoint = transport.endpoint();

    // A web page the developer visits sends an Origin; an MCP client does not.
    for origin in [
        "https://attacker.example.invalid",
        "http://127.0.0.1",
        "null",
    ] {
        assert_eq!(
            probe(&endpoint, &[("Origin", origin)]).await,
            GuardRejection::OriginNotAllowed.status_code(),
            "{origin}"
        );
    }
    for header in [
        "X-Forwarded-For",
        "X-Forwarded-Host",
        "X-Forwarded-Proto",
        "Forwarded",
    ] {
        assert_eq!(
            probe(&endpoint, &[(header, "127.0.0.1")]).await,
            GuardRejection::ForwardedHeader.status_code(),
            "{header}"
        );
    }
    assert_eq!(
        probe(&format!("{endpoint}?token=lan-token-1"), &[]).await,
        GuardRejection::QueryCredential.status_code()
    );
    assert_eq!(
        probe(&endpoint, &[("MCP-Protocol-Version", "1999-01-01")]).await,
        GuardRejection::UnsupportedProtocolVersion.status_code()
    );
    assert_eq!(
        transport.kernel_dispatches(),
        0,
        "every refusal precedes dispatch"
    );
    transport.shutdown().await?;

    // A configured Origin and Host are matched exactly.
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let transport = bind_open(
        HttpTransportOptions::open(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
            .with_allowed_origins(["http://tools.example.invalid:3000"])
            .with_allowed_hosts([format!("127.0.0.1:{port}")]),
    )
    .await?;
    let endpoint = transport.endpoint();
    assert_eq!(
        probe(
            &endpoint,
            &[("Origin", "http://tools.example.invalid:3000")]
        )
        .await,
        200
    );
    assert_eq!(
        probe(
            &endpoint,
            &[("Origin", "http://tools.example.invalid:4000")]
        )
        .await,
        GuardRejection::OriginNotAllowed.status_code()
    );
    assert_eq!(
        probe(&endpoint, &[("Host", "build-box.example.invalid:8765")]).await,
        GuardRejection::HostNotAllowed.status_code()
    );
    transport.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_open_mode_binds_beyond_loopback() -> anyhow::Result<()> {
    let state = tempfile::tempdir()?;
    let refused = bind_open(HttpTransportOptions::trusted_local(
        "0.0.0.0:0".parse()?,
        state.path(),
    ))
    .await
    .expect_err("trusted_local stays on loopback");
    assert!(
        matches!(
            refused.downcast_ref::<HttpTransportError>(),
            Some(HttpTransportError::NonLoopbackBind { .. })
        ),
        "{refused}"
    );
    let open = bind_open(
        HttpTransportOptions::open("0.0.0.0:0".parse()?).with_token(Secret::new("lan-token-1")),
    )
    .await?;
    assert!(open.local_addr().ip().is_unspecified());
    assert!(open.requires_open_token());
    open.shutdown().await?;
    Ok(())
}
