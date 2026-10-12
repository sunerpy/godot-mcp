//! Todo 30 acceptance: HTTP sessions, tasks and artifacts bound to the subject that
//! authenticated, and real authenticated-HTTP artifact parity with stdio.
//!
//! Exactly two cases exist, matching `qa/tasks/30.json`:
//!
//! * `http_session_binding_happy_path` — an authenticated subject resumes its own
//!   session, opens its own SSE stream, dispatches its own task and reads its own
//!   opaque artifact range, and that authenticated-HTTP artifact read is
//!   byte-identical to the stdio read of the same vector in *both* `trusted_local`
//!   and `proxied_oidc`.
//! * `http_session_binding_failure_paths` — a missing proxy-hop identity, a second
//!   subject reusing an identifier, a stolen or fabricated identifier, a changed
//!   audience/profile/capability/project generation, a forbidden `Origin`/`Host`, an
//!   unauthenticated forwarded host, an invalid range, excessive streams and a
//!   replayed destructive task are all refused through the non-enumerating contract,
//!   with zero task/artifact disclosure and zero kernel dispatch.
//!
//! # The artifact parity vector is not a screenshot, and this file says so
//!
//! The plan text for this todo asks to replay Todo 24's stdio screenshot/recording
//! parity vectors over authenticated HTTP. **Todo 24 was cancelled and never
//! implemented**, so those vectors do not exist and nothing here pretends otherwise.
//! What is asserted instead is the same set of invariants over an artifact channel
//! that *does* exist — Todo 5's `register_artifact` / `read_resource` pair against
//! Todo 3's frozen `ArtifactContract` — using a deterministic, controlled byte
//! vector: the normalized result of a bounded range read is byte-identical across
//! stdio, authenticated `trusted_local` HTTP and authenticated `proxied_oidc` HTTP;
//! every range read is authorized; and no raw storage path appears in any response.
//! Screenshot and recording parity specifically cannot be verified until the runtime
//! artifact plane exists.
//!
//! # Why the comparison is meaningful rather than tautological
//!
//! All three channels drive the *same* [`RmcpAdapter`] over the *same*
//! [`ServiceKernel`] built by one function, so a difference can only come from a
//! transport. The snapshot excludes session identifiers, socket addresses and
//! wall-clock values by construction.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::config::{Limits, ProxyHopCredential};
use godot_mcp_domain::contracts::{
    ArtifactContract, ArtifactId, ArtifactKind, ArtifactMetadata, ArtifactResourceUri, Capability,
    OutcomeError, PrincipalId, ProjectIdentity, RangeSemantics, SessionId,
};
use godot_mcp_domain::objid::{LeaseKey, StableObjectId};
use godot_mcp_server::auth::pipeline::OidcClock;
use godot_mcp_server::auth::testkeys;
use godot_mcp_server::auth::{
    HopCredential, JwkSet, PROTECTED_RESOURCE_METADATA_PATH, ProxyHop, ResourceServer,
    ResourceServerConfig, SignatureAlgorithm,
};
use godot_mcp_server::binding::{
    BINDING_REFUSAL_MESSAGE, BINDING_REFUSAL_STATUS, BindingRegistry, HandleKind,
    OPAQUE_HANDLE_HEX_CHARS, OpaqueHandle, SessionBindingPolicy, binding_refusal,
};
use godot_mcp_server::http::{
    ALLOWED_REQUEST_HEADERS, ALLOWED_REQUEST_METHODS, EXPOSED_RESPONSE_HEADERS,
    HttpTransportOptions, MCP_SESSION_HEADER, PROXY_HOP_HEADER, RunningHttpTransport,
    bind_http_transport,
};
use godot_mcp_server::idempotency::{ModePrincipal, TransportAuthMode};
use godot_mcp_server::kernel::{AccessContext, KernelToolCall, KernelToolHandler, ServiceKernel};
use godot_mcp_server::rmcp_adapter::{ARTIFACT_RANGE_META_KEY, RmcpAdapter};
use rmcp::model::{ReadResourceRequestParams, RequestMetaObject, ResourceContents};
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Deterministic fixtures. One kernel builder, three transports.
// ---------------------------------------------------------------------------

const ISSUER: &str = "https://issuer.example";
const JWKS: &str = "https://issuer.example/jwks";
const SUBJECT: &str = "subject-1";
const SECOND_SUBJECT: &str = "subject-2";
const RESOURCE: &str = "https://mcp.example.com/mcp";
const NOW_SECONDS: u64 = 1_700_000_000;
const REQUIRED_SCOPE: &str = "godot.read";
const PROTOCOL_VERSION: &str = "2025-11-25";

/// The controlled artifact vector. Sixteen ascending bytes: every byte differs from
/// its neighbours, so a range that was resolved even one byte off produces different
/// output rather than accidentally matching.
const ARTIFACT_BYTES: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

/// The digest of [`ARTIFACT_BYTES`], which the frozen contract requires.
const ARTIFACT_SHA256: &str = "be45cb2605bf36bebde684841a28f0fd43c69850a3dce5fedba69928ee3a8991";

/// The bounded range every channel reads, chosen to be neither the whole artifact
/// nor anchored at either end.
const PARITY_RANGE: (u64, u64) = (4, 11);

fn fixture_project() -> ProjectIdentity {
    project_at_generation(4)
}

fn project_at_generation(generation: u64) -> ProjectIdentity {
    ProjectIdentity::from_lease_key(
        LeaseKey {
            root: StableObjectId::Unix { dev: 30, ino: 71 },
            project_file: StableObjectId::Unix { dev: 30, ino: 72 },
        },
        generation,
    )
}

fn fixture_principal() -> PrincipalId {
    PrincipalId::parse(format!("principal_{}", "3".repeat(32))).expect("principal")
}

fn fixture_session() -> SessionId {
    SessionId::parse(format!("session_{}", "7".repeat(32))).expect("session")
}

/// The kernel-facing access context, identical for every channel so the kernel's own
/// owner/session/retention checks answer the same way in all three.
fn fixture_access() -> AccessContext {
    AccessContext::new(
        fixture_project(),
        fixture_principal(),
        Some(fixture_session()),
        4_000,
    )
}

fn fixture_artifact() -> ArtifactContract {
    let artifact_id =
        ArtifactId::parse(format!("artifact_{}", "5".repeat(64))).expect("artifact id");
    ArtifactContract::new(
        artifact_id.clone(),
        ArtifactResourceUri::for_artifact(&artifact_id),
        fixture_project(),
        None,
        ArtifactMetadata {
            kind: ArtifactKind::Report,
            media_type: "application/octet-stream".to_owned(),
            byte_length: ARTIFACT_BYTES.len() as u64,
            sha256: ARTIFACT_SHA256.to_owned(),
            created_at_unix_ms: 1_000,
        },
        u64::MAX / 2,
        RangeSemantics::new(ARTIFACT_BYTES.len() as u64, 8, true).expect("range semantics"),
    )
    .expect("artifact contract")
}

fn fixture_artifact_uri() -> String {
    fixture_artifact().resource_uri().as_str().to_owned()
}

struct AccessEchoHandler;

#[async_trait]
impl KernelToolHandler for AccessEchoHandler {
    async fn call(&self, call: KernelToolCall) -> Result<Value, OutcomeError> {
        Ok(json!({
            "transport": call.access.transport_mode().as_str(),
            "principal": call.access.mode_principal().to_journal_string(),
            "session": call.access.connection_session(),
        }))
    }
}

fn fixture_kernel() -> anyhow::Result<ServiceKernel> {
    let mut kernel = ServiceKernel::new(CapabilityProfile::Full);
    kernel.register_artifact(
        fixture_artifact(),
        ARTIFACT_BYTES.to_vec(),
        &fixture_access(),
    )?;
    kernel.register_tool_handler(
        godot_mcp_domain::catalog::descriptor("health.get")
            .and_then(|tool| tool.handler)
            .expect("a catalog tool"),
        Arc::new(AccessEchoHandler),
    )?;
    Ok(kernel)
}

/// The `proxied_oidc` policy. Scopes are an OIDC concept, so only this mode
/// requires them.
fn oidc_binding_policy() -> SessionBindingPolicy {
    local_binding_policy().with_required_scopes([REQUIRED_SCOPE])
}

/// The `trusted_local` policy.
///
/// It requires no scopes because a launch secret carries none: it authenticates the
/// user who started this process, and demanding a scope it structurally cannot
/// present would close the mode rather than tighten it.
fn local_binding_policy() -> SessionBindingPolicy {
    SessionBindingPolicy::new(
        CapabilityProfile::Full,
        RESOURCE,
        fixture_project(),
        &Limits::default(),
    )
    .expect("policy")
}

// ---------------------------------------------------------------------------
// The OIDC boundary a `proxied_oidc` listener is armed with.
// ---------------------------------------------------------------------------

struct Boundary {
    server: Arc<ResourceServer>,
    hop_credential: String,
    token: String,
    _directory: tempfile::TempDir,
}

impl Boundary {
    fn build(subject: &str) -> anyhow::Result<Self> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("proxy-hop-credential");
        let hop_credential = ProxyHop::publish(&path)?;
        let hop = ProxyHop::load(HopCredential::from_config(
            &ProxyHopCredential::CredentialFile {
                path: path.display().to_string(),
            },
        ))?;
        let config = ResourceServerConfig::new(ISSUER, JWKS, subject, RESOURCE, RESOURCE)?
            .with_required_scopes([REQUIRED_SCOPE]);
        let keys = JwkSet::parse(&testkeys::jwks_document(&[testkeys::RSA_OLD]))?;
        Ok(Self {
            server: Arc::new(ResourceServer::new(
                config,
                hop,
                keys,
                OidcClock::Fixed(NOW_SECONDS),
            )),
            hop_credential,
            token: testkeys::sign(
                &testkeys::RSA_OLD,
                SignatureAlgorithm::Rs256,
                Some(testkeys::RSA_OLD.kid),
                &claims_for(subject),
            ),
            _directory: directory,
        })
    }
}

fn claims_for(subject: &str) -> Value {
    json!({
        "iss": ISSUER,
        "sub": subject,
        "aud": RESOURCE,
        "exp": NOW_SECONDS + 300,
        "nbf": NOW_SECONDS - 30,
        "iat": NOW_SECONDS - 30,
        "scope": format!("{REQUIRED_SCOPE} godot.analyze"),
    })
}

// ---------------------------------------------------------------------------
// Raw HTTP driving. Every header is explicit, so nothing is trusted by accident.
// ---------------------------------------------------------------------------

fn raw_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("reqwest client")
}

/// One request's credential material, as a proxy or a local client would present it.
#[derive(Clone)]
enum Credential {
    LaunchSecret(String),
    Proxied {
        hop: String,
        token: String,
    },
    /// A `proxied_oidc` request whose hop credential is absent: the case a caller
    /// reaching the loopback socket directly would produce.
    MissingHop {
        token: String,
    },
    None,
}

impl Credential {
    fn apply(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Self::LaunchSecret(secret) => {
                request.header("Authorization", format!("Bearer {secret}"))
            }
            Self::Proxied { hop, token } => request
                .header("Authorization", format!("Bearer {token}"))
                .header(PROXY_HOP_HEADER, hop),
            Self::MissingHop { token } => {
                request.header("Authorization", format!("Bearer {token}"))
            }
            Self::None => request,
        }
    }
}

struct Rpc {
    method: reqwest::Method,
    body: Option<Value>,
    session: Option<String>,
    credential: Credential,
    headers: Vec<(&'static str, String)>,
    url: Option<String>,
}

impl Rpc {
    fn post(body: Value) -> Self {
        Self {
            method: reqwest::Method::POST,
            body: Some(body),
            session: None,
            credential: Credential::None,
            headers: Vec::new(),
            url: None,
        }
    }

    fn get() -> Self {
        Self {
            method: reqwest::Method::GET,
            body: None,
            session: None,
            credential: Credential::None,
            headers: Vec::new(),
            url: None,
        }
    }

    fn delete() -> Self {
        Self {
            method: reqwest::Method::DELETE,
            body: None,
            session: None,
            credential: Credential::None,
            headers: Vec::new(),
            url: None,
        }
    }

    fn options() -> Self {
        Self {
            method: reqwest::Method::OPTIONS,
            body: None,
            session: None,
            credential: Credential::None,
            headers: Vec::new(),
            url: None,
        }
    }

    fn session(mut self, handle: impl Into<String>) -> Self {
        self.session = Some(handle.into());
        self
    }

    fn credential(mut self, credential: Credential) -> Self {
        self.credential = credential;
        self
    }

    fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    async fn send(self, transport: &RunningHttpTransport) -> reqwest::Response {
        let url = self.url.unwrap_or_else(|| transport.endpoint());
        // A `Host` a case names must *replace* the bound authority rather than be
        // appended beside it: two `Host` values would let the guard read the allowed
        // one and the forbidden-host case would pass without proving anything.
        let host = self
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("host"))
            .map_or_else(
                || transport.local_addr().to_string(),
                |(_, value)| value.clone(),
            );
        let mut request = raw_client()
            .request(self.method, url)
            .header("Accept", "application/json, text/event-stream")
            .header("Host", host);
        if self.body.is_some() {
            request = request.header("Content-Type", "application/json");
        }
        if let Some(session) = &self.session {
            request = request.header(MCP_SESSION_HEADER, session);
        }
        request = self.credential.apply(request);
        for (name, value) in &self.headers {
            if name.eq_ignore_ascii_case("host") {
                continue;
            }
            request = request.header(*name, value);
        }
        if let Some(body) = self.body {
            request = request.body(serde_json::to_vec(&body).expect("serialize body"));
        }
        request.send().await.expect("http request")
    }
}

fn initialize_body(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "todo-30-probe", "version": "0" }
        }
    })
}

fn call_tool_body(id: u64, name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    })
}

fn read_artifact_body(id: u64, range: Option<(u64, u64)>) -> Value {
    let mut params = json!({ "uri": fixture_artifact_uri() });
    if let Some((start, end_inclusive)) = range {
        params.as_object_mut().expect("object").insert(
            "_meta".to_owned(),
            json!({
                ARTIFACT_RANGE_META_KEY: { "start": start, "endInclusive": end_inclusive }
            }),
        );
    }
    json!({ "jsonrpc": "2.0", "id": id, "method": "resources/read", "params": params })
}

/// Pull the JSON-RPC envelope out of either a JSON body or a one-event SSE body.
fn rpc_envelope(body: &str) -> anyhow::Result<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        return Ok(value);
    }
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data:")
            && let Ok(value) = serde_json::from_str::<Value>(data.trim())
        {
            return Ok(value);
        }
    }
    anyhow::bail!("no JSON-RPC envelope in {body:?}")
}

/// Open a session over HTTP and return the opaque handle the server minted.
async fn open_session(
    transport: &RunningHttpTransport,
    credential: &Credential,
    id: u64,
) -> anyhow::Result<String> {
    let response = Rpc::post(initialize_body(id))
        .credential(credential.clone())
        .send(transport)
        .await;
    anyhow::ensure!(
        response.status().is_success(),
        "the handshake must succeed, got {}",
        response.status()
    );
    let handle = response
        .headers()
        .get(MCP_SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("a handshake must advertise a session identifier"))?;
    // Draining the handshake body releases its stream slot before the caller
    // continues, so a later ceiling assertion measures only what it opened itself.
    let _ = response.text().await?;
    Ok(handle)
}

// ---------------------------------------------------------------------------
// The normalized artifact observation the three channels are compared on.
// ---------------------------------------------------------------------------

/// One bounded artifact range read, with nothing transport-specific in it.
#[derive(Debug, PartialEq, Eq)]
struct ArtifactObservation {
    uri: String,
    media_type: Option<String>,
    blob: String,
}

impl ArtifactObservation {
    /// Every field the frozen contract publishes, and no field a path could hide in.
    fn assert_discloses_no_storage_location(&self) {
        for leak in [
            "/tmp", "/home", "/var", "C:\\", "\\\\", ".png", ".ogv", ".log", "file://", "target/",
        ] {
            assert!(
                !self.uri.contains(leak) && !self.blob.contains(leak),
                "an artifact read must not disclose {leak}"
            );
        }
        // The frozen URI shape is the prefix plus the identifier's 64 hex payload.
        let payload = self
            .uri
            .strip_prefix("godot-mcp://artifacts/")
            .unwrap_or_else(|| {
                panic!("an artifact URI must carry the frozen prefix: {}", self.uri)
            });
        assert_eq!(
            payload.len(),
            OPAQUE_HANDLE_HEX_CHARS,
            "an artifact is addressed only by its full-width opaque payload, got {payload}"
        );
        assert!(
            payload
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "an artifact payload is lowercase hexadecimal, got {payload}"
        );
    }
}

fn observation_from_contents(contents: &Value) -> anyhow::Result<ArtifactObservation> {
    let entry = contents
        .get("result")
        .and_then(|result| result.get("contents"))
        .and_then(Value::as_array)
        .and_then(|entries| entries.first())
        .ok_or_else(|| anyhow::anyhow!("no resource contents in {contents}"))?;
    Ok(ArtifactObservation {
        uri: entry
            .get("uri")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        media_type: entry
            .get("mimeType")
            .and_then(Value::as_str)
            .map(str::to_owned),
        blob: entry
            .get("blob")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("an artifact read must return blob bytes: {entry}"))?
            .to_owned(),
    })
}

#[derive(Clone, Default)]
struct StdioClient;

impl ClientHandler for StdioClient {}

/// Read the parity range over stdio, through the very same adapter and kernel.
async fn stdio_observation() -> anyhow::Result<ArtifactObservation> {
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let adapter = RmcpAdapter::new(Arc::new(fixture_kernel()?), fixture_access());
    let server = tokio::spawn(async move {
        if let Ok(service) = adapter.serve(server_transport).await {
            let _ = service.waiting().await;
        }
    });
    let client: rmcp::service::RunningService<RoleClient, StdioClient> =
        StdioClient.serve(client_transport).await?;
    let mut meta = RequestMetaObject::new();
    meta.0.insert(
        ARTIFACT_RANGE_META_KEY.to_owned(),
        json!({ "start": PARITY_RANGE.0, "endInclusive": PARITY_RANGE.1 }),
    );
    let read = client
        .read_resource(ReadResourceRequestParams::new(fixture_artifact_uri()).with_meta(meta))
        .await?;
    let observation = match &read.contents[0] {
        ResourceContents::BlobResourceContents {
            uri,
            mime_type,
            blob,
            ..
        } => ArtifactObservation {
            uri: uri.clone(),
            media_type: mime_type.clone(),
            blob: blob.clone(),
        },
        other => anyhow::bail!("expected a bounded artifact blob, got {other:?}"),
    };
    client.cancel().await?;
    server.abort();
    Ok(observation)
}

/// Read the parity range over one authenticated HTTP listener.
async fn http_observation(
    transport: &RunningHttpTransport,
    credential: &Credential,
    session: &str,
) -> anyhow::Result<ArtifactObservation> {
    let body = Rpc::post(read_artifact_body(9, Some(PARITY_RANGE)))
        .credential(credential.clone())
        .session(session)
        .send(transport)
        .await
        .text()
        .await?;
    observation_from_contents(&rpc_envelope(&body)?)
}

// ---------------------------------------------------------------------------
// Case 1 — happy path.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn http_session_binding_happy_path() -> anyhow::Result<()> {
    let state = tempfile::tempdir()?;
    let local = bind_http_transport(
        HttpTransportOptions::trusted_local("127.0.0.1:0".parse::<SocketAddr>()?, state.path())
            .with_session_binding(local_binding_policy()),
        Arc::new(fixture_kernel()?),
        fixture_access(),
    )
    .await?;
    let secret = godot_mcp_server::http::LaunchSecret::read_token(
        local
            .secret_path()
            .expect("trusted_local publishes a secret"),
    )?;
    let local_credential = Credential::LaunchSecret(secret);

    let boundary = Boundary::build(SUBJECT)?;
    let proxied = bind_http_transport(
        HttpTransportOptions::proxied_oidc("127.0.0.1:0".parse::<SocketAddr>()?, RESOURCE)
            .with_boundary(Arc::clone(&boundary.server))
            .with_session_binding(oidc_binding_policy()),
        Arc::new(fixture_kernel()?),
        fixture_access(),
    )
    .await?;
    let proxied_credential = Credential::Proxied {
        hop: boundary.hop_credential.clone(),
        token: boundary.token.clone(),
    };

    // --- The advertised session identifier is a minted opaque handle. ---
    let local_session = open_session(&local, &local_credential, 1).await?;
    let proxied_session = open_session(&proxied, &proxied_credential, 1).await?;
    for handle in [&local_session, &proxied_session] {
        let parsed = OpaqueHandle::parse(HandleKind::Session, handle)
            .unwrap_or_else(|| panic!("{handle} must be a minted opaque handle"));
        assert_eq!(parsed.as_str(), handle);
        assert_eq!(
            handle.len(),
            HandleKind::Session.prefix().len() + OPAQUE_HANDLE_HEX_CHARS
        );
        assert!(
            !handle.contains('-'),
            "the SDK's own identifier must never reach the wire, got {handle}"
        );
    }
    assert_ne!(
        local_session, proxied_session,
        "two listeners never mint the same handle"
    );
    assert_eq!(local.bindings().expect("registry").open_sessions(), 1);
    assert_eq!(proxied.bindings().expect("registry").open_sessions(), 1);

    let local_access = Rpc::post(call_tool_body(2, "health.get", json!({})))
        .credential(local_credential.clone())
        .session(&local_session)
        .send(&local)
        .await
        .text()
        .await?;
    let local_access = rpc_envelope(&local_access)?;
    let local_access = local_access
        .pointer("/result/structuredContent")
        .ok_or_else(|| anyhow::anyhow!("trusted_local access facts are missing"))?;
    assert_eq!(local_access["transport"], "trusted_local");
    assert_eq!(local_access["session"], local_session);
    assert_ne!(local_access["principal"], fixture_principal().as_str());

    let proxied_access = Rpc::post(call_tool_body(3, "health.get", json!({})))
        .credential(proxied_credential.clone())
        .session(&proxied_session)
        .send(&proxied)
        .await
        .text()
        .await?;
    let proxied_access = rpc_envelope(&proxied_access)?;
    let proxied_access = proxied_access
        .pointer("/result/structuredContent")
        .ok_or_else(|| anyhow::anyhow!("proxied_oidc access facts are missing"))?;
    assert_eq!(proxied_access["transport"], "proxied_oidc");
    assert_eq!(proxied_access["session"], proxied_session);
    assert_eq!(
        proxied_access["principal"],
        ModePrincipal::Oidc {
            issuer: ISSUER.to_owned(),
            subject: SUBJECT.to_owned(),
        }
        .to_journal_string()
    );

    // --- The authenticated subject resumes its own session and dispatches. ---
    for (transport, credential, session) in [
        (&local, &local_credential, &local_session),
        (&proxied, &proxied_credential, &proxied_session),
    ] {
        let dispatched = Rpc::post(call_tool_body(2, "capabilities.get", json!({})))
            .credential(credential.clone())
            .session(session)
            .send(transport)
            .await;
        assert!(
            dispatched.status().is_success(),
            "an authenticated subject's own task is served, got {}",
            dispatched.status()
        );
        // The SDK sets a session header only on the handshake response, so the rule
        // asserted here is the invariant that matters: whenever one *is* present it
        // is the opaque handle, and the internal identifier never appears.
        assert!(
            dispatched
                .headers()
                .get(MCP_SESSION_HEADER)
                .and_then(|value| value.to_str().ok())
                .is_none_or(|echoed| echoed == session.as_str()),
            "a session header on a response must be the opaque handle"
        );
        let served = rpc_envelope(&dispatched.text().await?)?;
        assert!(
            served.get("result").is_some(),
            "the kernel answered the resumed task: {served}"
        );
    }

    // --- Its own SSE stream opens, holds one ceiling slot, and releases it. ---
    for (transport, credential, session) in [
        (&local, &local_credential, &local_session),
        (&proxied, &proxied_credential, &proxied_session),
    ] {
        let registry = transport.bindings().expect("registry");
        assert_eq!(registry.open_streams(session), Some(0));
        let stream = Rpc::get()
            .credential(credential.clone())
            .session(session)
            .send(transport)
            .await;
        assert!(
            stream.status().is_success(),
            "an authenticated subject opens its own stream, got {}",
            stream.status()
        );
        assert_eq!(
            stream
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.starts_with("text/event-stream")),
            Some(true)
        );
        assert_eq!(
            registry.open_streams(session),
            Some(1),
            "an open stream holds exactly one ceiling slot"
        );
        drop(stream);
        for _ in 0..200 {
            if registry.open_streams(session) == Some(0) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            registry.open_streams(session),
            Some(0),
            "a closed stream releases its slot"
        );
    }

    // --- The one load-bearing parity assertion. ---
    let stdio = stdio_observation().await?;
    let over_local = http_observation(&local, &local_credential, &local_session).await?;
    let over_proxied = http_observation(&proxied, &proxied_credential, &proxied_session).await?;

    assert_eq!(
        stdio, over_local,
        "an authenticated trusted_local artifact read must be byte-identical to stdio"
    );
    assert_eq!(
        stdio, over_proxied,
        "an authenticated proxied_oidc artifact read must be byte-identical to stdio"
    );
    for observation in [&stdio, &over_local, &over_proxied] {
        observation.assert_discloses_no_storage_location();
        assert_eq!(
            observation.media_type.as_deref(),
            Some("application/octet-stream")
        );
    }
    // The vector really is the requested window, not the whole artifact.
    let expected = base64_encode(
        &ARTIFACT_BYTES[usize::try_from(PARITY_RANGE.0)?..=usize::try_from(PARITY_RANGE.1)?],
    );
    assert_eq!(stdio.blob, expected, "the bounded window is what came back");
    assert_ne!(
        stdio.blob,
        base64_encode(&ARTIFACT_BYTES),
        "a bounded read must not return the whole artifact"
    );

    // Every read went through authorization, not just the first.
    assert_eq!(
        local.bindings().expect("registry").authorized_reads(),
        1,
        "the trusted_local read was authorized"
    );
    assert_eq!(
        proxied.bindings().expect("registry").authorized_reads(),
        1,
        "the proxied_oidc read was authorized"
    );
    for (transport, credential, session) in [
        (&local, &local_credential, &local_session),
        (&proxied, &proxied_credential, &proxied_session),
    ] {
        let again = http_observation(transport, credential, session).await?;
        assert_eq!(again, stdio, "a repeated read stays byte-identical");
        assert_eq!(
            transport.bindings().expect("registry").authorized_reads(),
            2,
            "every range read is authorized, not only the first"
        );
    }

    // --- An allowlisted browser origin is answered explicitly, never with `*`. ---
    let origin = "https://console.example";
    let cors_state = tempfile::tempdir()?;
    let cors = bind_http_transport(
        HttpTransportOptions::trusted_local(
            "127.0.0.1:0".parse::<SocketAddr>()?,
            cors_state.path(),
        )
        .with_allowed_origins([origin])
        .with_session_binding(local_binding_policy()),
        Arc::new(fixture_kernel()?),
        fixture_access(),
    )
    .await?;
    let cors_secret = godot_mcp_server::http::LaunchSecret::read_token(
        cors.secret_path().expect("a secret is published"),
    )?;
    let preflight = Rpc::options().header("Origin", origin).send(&cors).await;
    assert_eq!(preflight.status().as_u16(), 204);
    let headers = preflight.headers().clone();
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some(origin),
        "the exact configured origin is echoed, never a wildcard"
    );
    assert_eq!(
        headers
            .get("access-control-allow-methods")
            .and_then(|value| value.to_str().ok()),
        Some(ALLOWED_REQUEST_METHODS)
    );
    assert_eq!(
        headers
            .get("access-control-allow-headers")
            .and_then(|value| value.to_str().ok()),
        Some(ALLOWED_REQUEST_HEADERS)
    );
    assert_eq!(
        headers
            .get("access-control-expose-headers")
            .and_then(|value| value.to_str().ok()),
        Some(EXPOSED_RESPONSE_HEADERS)
    );
    assert!(
        headers.get("access-control-allow-credentials").is_none(),
        "credentials are never allowed for a browser origin"
    );
    let allowed = Rpc::post(initialize_body(1))
        .credential(Credential::LaunchSecret(cors_secret))
        .header("Origin", origin)
        .send(&cors)
        .await;
    assert!(allowed.status().is_success());
    assert_eq!(
        allowed
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some(origin)
    );

    // --- The subject may terminate its own binding, and it stops resolving. ---
    for (transport, credential, session) in [
        (&local, &local_credential, &local_session),
        (&proxied, &proxied_credential, &proxied_session),
    ] {
        let terminated = Rpc::delete()
            .credential(credential.clone())
            .session(session)
            .send(transport)
            .await;
        assert!(
            terminated.status().is_success() || terminated.status().as_u16() == 202,
            "a subject terminates its own session, got {}",
            terminated.status()
        );
        assert_eq!(transport.bindings().expect("registry").open_sessions(), 0);
        let after = Rpc::post(call_tool_body(3, "capabilities.get", json!({})))
            .credential(credential.clone())
            .session(session)
            .send(transport)
            .await;
        assert_eq!(
            after.status().as_u16(),
            BINDING_REFUSAL_STATUS,
            "a terminated binding is indistinguishable from one that never existed"
        );
    }

    // The public metadata document stays reachable and still names nothing local.
    let metadata = Rpc::get()
        .url(format!(
            "http://{}{PROTECTED_RESOURCE_METADATA_PATH}",
            proxied.local_addr()
        ))
        .send(&proxied)
        .await;
    assert_eq!(metadata.status().as_u16(), 200);
    let metadata: Value = metadata.json().await?;
    assert_eq!(
        metadata.get("resource").and_then(Value::as_str),
        Some(RESOURCE)
    );

    cors.shutdown().await?;
    proxied.shutdown().await?;
    local.shutdown().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Case 2 — failure paths.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn http_session_binding_failure_paths() -> anyhow::Result<()> {
    let boundary = Boundary::build(SUBJECT)?;
    let owner = Credential::Proxied {
        hop: boundary.hop_credential.clone(),
        token: boundary.token.clone(),
    };
    let proxied = bind_http_transport(
        HttpTransportOptions::proxied_oidc("127.0.0.1:0".parse::<SocketAddr>()?, RESOURCE)
            .with_boundary(Arc::clone(&boundary.server))
            .with_session_binding(oidc_binding_policy()),
        Arc::new(fixture_kernel()?),
        fixture_access(),
    )
    .await?;
    let session = open_session(&proxied, &owner, 1).await?;
    let dispatches_after_handshake = proxied.kernel_dispatches();
    let reads_after_handshake = proxied.bindings().expect("registry").authorized_reads();

    // --- A missing proxy-hop identity never resolves a real binding. ---
    let missing_hop = Rpc::post(call_tool_body(2, "capabilities.get", json!({})))
        .credential(Credential::MissingHop {
            token: boundary.token.clone(),
        })
        .session(&session)
        .send(&proxied)
        .await;
    assert_eq!(
        missing_hop.status().as_u16(),
        401,
        "a request with no hop identity is not authenticated, whatever it holds"
    );
    let missing_hop_body = missing_hop.text().await?;
    let no_credential = Rpc::post(call_tool_body(3, "capabilities.get", json!({})))
        .credential(Credential::None)
        .session(&session)
        .send(&proxied)
        .await;
    assert_eq!(no_credential.status().as_u16(), 401);
    assert_eq!(
        no_credential.text().await?,
        missing_hop_body,
        "a missing hop and a missing token are byte-identical refusals"
    );
    for body in [&missing_hop_body] {
        for leak in ["artifact", "task", "subject-", "issuer", "session_"] {
            assert!(
                !body.contains(leak),
                "an unauthenticated refusal must not mention {leak}: {body}"
            );
        }
    }

    // --- A second subject holding a real identifier is refused, not informed. ---
    let second = Boundary::build(SECOND_SUBJECT)?;
    let second_transport = bind_http_transport(
        HttpTransportOptions::proxied_oidc("127.0.0.1:0".parse::<SocketAddr>()?, RESOURCE)
            .with_boundary(Arc::clone(&second.server))
            .with_session_binding(oidc_binding_policy()),
        Arc::new(fixture_kernel()?),
        fixture_access(),
    )
    .await?;
    let second_credential = Credential::Proxied {
        hop: second.hop_credential.clone(),
        token: second.token.clone(),
    };
    let second_session = open_session(&second_transport, &second_credential, 1).await?;

    // The first subject's own token is refused outright by the second listener,
    // because a listener accepts exactly one subject.
    let wrong_subject = Rpc::post(call_tool_body(4, "capabilities.get", json!({})))
        .credential(owner.clone())
        .session(&second_session)
        .send(&second_transport)
        .await;
    assert_eq!(wrong_subject.status().as_u16(), 401);

    // And a fully authenticated *second* subject presenting the first subject's
    // identifier gets the non-enumerating refusal, with nothing disclosed.
    let stolen = Rpc::post(call_tool_body(5, "capabilities.get", json!({})))
        .credential(second_credential.clone())
        .session(&session)
        .send(&second_transport)
        .await;
    assert_eq!(
        stolen.status().as_u16(),
        BINDING_REFUSAL_STATUS,
        "a second subject reusing an identifier is refused"
    );
    let stolen_body = stolen.text().await?;
    let fabricated = Rpc::post(call_tool_body(6, "capabilities.get", json!({})))
        .credential(second_credential.clone())
        .session(
            OpaqueHandle::mint(HandleKind::Session)
                .expect("os entropy")
                .as_str(),
        )
        .send(&second_transport)
        .await;
    assert_eq!(fabricated.status().as_u16(), BINDING_REFUSAL_STATUS);
    let malformed = Rpc::post(call_tool_body(7, "capabilities.get", json!({})))
        .credential(second_credential.clone())
        .session("session_not-a-handle")
        .send(&second_transport)
        .await;
    assert_eq!(malformed.status().as_u16(), BINDING_REFUSAL_STATUS);
    let fabricated_body = fabricated.text().await?;
    let malformed_body = malformed.text().await?;
    assert_eq!(
        stolen_body, fabricated_body,
        "a stolen identifier and one that never existed are byte-identical"
    );
    assert_eq!(stolen_body, malformed_body);
    assert_eq!(stolen_body, BINDING_REFUSAL_MESSAGE);
    for leak in [
        "artifact",
        "task",
        "principal",
        "subject",
        "generation",
        "revoked",
        "expired",
        "exists",
    ] {
        assert!(
            !stolen_body.contains(leak),
            "the binding refusal must not mention {leak}: {stolen_body}"
        );
    }
    // Nothing above reached the kernel, and nothing above authorized a read.
    assert_eq!(
        proxied.kernel_dispatches(),
        dispatches_after_handshake,
        "a refused request never dispatches"
    );
    assert_eq!(
        proxied.bindings().expect("registry").authorized_reads(),
        reads_after_handshake,
        "a refused request never authorizes an artifact read"
    );
    assert_eq!(
        second_transport
            .bindings()
            .expect("registry")
            .authorized_reads(),
        0
    );

    // The owner's binding is untouched by every one of those attempts.
    let still_mine = Rpc::post(call_tool_body(8, "capabilities.get", json!({})))
        .credential(owner.clone())
        .session(&session)
        .send(&proxied)
        .await;
    assert!(still_mine.status().is_success());
    let _ = still_mine.text().await?;

    // --- A stranger cannot terminate, stream from, or read through a binding. ---
    for probe in [
        Rpc::delete()
            .credential(second_credential.clone())
            .session(&session),
        Rpc::get()
            .credential(second_credential.clone())
            .session(&session),
        Rpc::post(read_artifact_body(9, Some(PARITY_RANGE)))
            .credential(second_credential.clone())
            .session(&session),
    ] {
        let refused = probe.send(&second_transport).await;
        assert_eq!(refused.status().as_u16(), BINDING_REFUSAL_STATUS);
        assert_eq!(refused.text().await?, BINDING_REFUSAL_MESSAGE);
    }
    assert_eq!(
        proxied.bindings().expect("registry").open_sessions(),
        1,
        "the owner's binding survives every stranger's attempt"
    );

    // --- An invalid range is refused before the kernel sees it. ---
    let dispatches_before_range = proxied.kernel_dispatches();
    for range in [
        (0_u64, ARTIFACT_BYTES.len() as u64),
        (0, ARTIFACT_BYTES.len() as u64 - 1),
    ] {
        let refused = Rpc::post(read_artifact_body(10, Some(range)))
            .credential(owner.clone())
            .session(&session)
            .send(&proxied)
            .await;
        let body = refused.text().await?;
        let envelope = rpc_envelope(&body)?;
        assert!(
            envelope.get("error").is_some(),
            "range {range:?} must be refused: {envelope}"
        );
        assert_eq!(
            envelope
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str),
            Some(BINDING_REFUSAL_MESSAGE),
            "an out-of-contract range refusal is the non-enumerating one"
        );
    }
    assert_eq!(
        proxied.kernel_dispatches(),
        dispatches_before_range,
        "a refused range never reaches the kernel"
    );

    // --- A forbidden Host or Origin, and a forwarded header, never get that far. ---
    let host_refused = Rpc::post(initialize_body(11))
        .credential(owner.clone())
        .header("Host", "elsewhere.example:80")
        .send(&proxied)
        .await;
    assert_eq!(host_refused.status().as_u16(), 403);
    let origin_refused = Rpc::post(initialize_body(12))
        .credential(owner.clone())
        .header("Origin", "https://evil.example")
        .send(&proxied)
        .await;
    assert_eq!(origin_refused.status().as_u16(), 403);
    let preflight_refused = Rpc::options()
        .header("Origin", "https://evil.example")
        .send(&proxied)
        .await;
    assert_eq!(
        preflight_refused.status().as_u16(),
        403,
        "a preflight from a forbidden origin is refused too"
    );
    for header in [
        "x-forwarded-host",
        "x-forwarded-for",
        "x-forwarded-proto",
        "forwarded",
        "x-real-ip",
    ] {
        let refused = Rpc::post(initialize_body(13))
            .credential(owner.clone())
            .header(header, "attacker.example")
            .send(&proxied)
            .await;
        assert_eq!(
            refused.status().as_u16(),
            400,
            "{header} must be refused rather than believed"
        );
    }

    // --- A replayed destructive task is refused. ---
    let first_mutation = Rpc::post(call_tool_body(
        14,
        "node.delete",
        json!({ "idempotencyId": "todo-30-destructive" }),
    ))
    .credential(owner.clone())
    .session(&session)
    .send(&proxied)
    .await;
    assert!(first_mutation.status().is_success());
    let _ = first_mutation.text().await?;
    let replayed = Rpc::post(call_tool_body(
        15,
        "node.delete",
        json!({ "idempotencyId": "todo-30-destructive" }),
    ))
    .credential(owner.clone())
    .session(&session)
    .send(&proxied)
    .await;
    let replayed = rpc_envelope(&replayed.text().await?)?;
    assert_eq!(
        replayed
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str),
        Some(BINDING_REFUSAL_MESSAGE),
        "a replayed destructive task is refused with the non-enumerating envelope: {replayed}"
    );

    // --- Excessive streams are refused, over real HTTP, against a ceiling of one. ---
    let narrow_boundary = Boundary::build(SUBJECT)?;
    let narrow_limits = Limits {
        max_queue: 1,
        ..Limits::default()
    };
    let narrow = bind_http_transport(
        HttpTransportOptions::proxied_oidc("127.0.0.1:0".parse::<SocketAddr>()?, RESOURCE)
            .with_boundary(Arc::clone(&narrow_boundary.server))
            .with_limits(narrow_limits.clone())
            .with_session_binding(
                SessionBindingPolicy::new(
                    CapabilityProfile::Full,
                    RESOURCE,
                    fixture_project(),
                    &narrow_limits,
                )?
                .with_required_scopes([REQUIRED_SCOPE]),
            ),
        Arc::new(fixture_kernel()?),
        fixture_access(),
    )
    .await?;
    let narrow_credential = Credential::Proxied {
        hop: narrow_boundary.hop_credential.clone(),
        token: narrow_boundary.token.clone(),
    };
    let narrow_session = open_session(&narrow, &narrow_credential, 1).await?;
    assert_eq!(
        narrow
            .bindings()
            .expect("registry")
            .policy()
            .max_concurrent_streams(),
        1,
        "the ceiling comes from the configured queue bound"
    );
    let held = Rpc::get()
        .credential(narrow_credential.clone())
        .session(&narrow_session)
        .send(&narrow)
        .await;
    assert!(held.status().is_success());
    for _ in 0..200 {
        if narrow
            .bindings()
            .expect("registry")
            .open_streams(&narrow_session)
            == Some(1)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        narrow
            .bindings()
            .expect("registry")
            .open_streams(&narrow_session),
        Some(1)
    );
    let excessive = Rpc::post(call_tool_body(16, "capabilities.get", json!({})))
        .credential(narrow_credential.clone())
        .session(&narrow_session)
        .send(&narrow)
        .await;
    assert_eq!(
        excessive.status().as_u16(),
        BINDING_REFUSAL_STATUS,
        "exceeding the configured stream ceiling is the same refusal as anything else"
    );
    assert_eq!(excessive.text().await?, BINDING_REFUSAL_MESSAGE);
    drop(held);

    // --- A changed audience, profile, capability or generation stops resolution. ---
    assert_changed_facts_never_resume();

    narrow.shutdown().await?;
    second_transport.shutdown().await?;
    proxied.shutdown().await?;
    Ok(())
}

/// The same production `resume` the transport calls, driven with subjects admitted
/// under a changed audience, profile, capability set and project generation.
///
/// This runs against the registry rather than over a socket because each listener
/// admits exactly one `(issuer, subject)` principal for one resource: changing the
/// audience or the profile *is* changing the deployment, and a second listener would
/// prove only that two registries hold different maps. Driving the same entry point
/// the middleware drives is what proves the comparison is total.
fn assert_changed_facts_never_resume() {
    let policy = oidc_binding_policy();
    let registry = BindingRegistry::new(policy.clone());
    let principal = ModePrincipal::Oidc {
        issuer: ISSUER.to_owned(),
        subject: SUBJECT.to_owned(),
    };
    let scopes: BTreeSet<String> = [REQUIRED_SCOPE.to_owned()].into_iter().collect();
    let owner = policy
        .admit(
            TransportAuthMode::ProxiedOidc,
            principal.clone(),
            scopes.clone(),
        )
        .expect("admitted");
    let lease = registry
        .bind_session("internal-1", &owner, 1_000_000)
        .expect("bound");
    let handle = lease.handle().as_str();

    let changed_audience = SessionBindingPolicy::new(
        CapabilityProfile::Full,
        "https://other.example/mcp",
        fixture_project(),
        &Limits::default(),
    )
    .expect("policy")
    .with_required_scopes([REQUIRED_SCOPE]);
    let changed_profile = SessionBindingPolicy::new(
        CapabilityProfile::Core,
        RESOURCE,
        fixture_project(),
        &Limits::default(),
    )
    .expect("policy")
    .with_required_scopes([REQUIRED_SCOPE]);
    let changed_generation = SessionBindingPolicy::new(
        CapabilityProfile::Full,
        RESOURCE,
        project_at_generation(5),
        &Limits::default(),
    )
    .expect("policy")
    .with_required_scopes([REQUIRED_SCOPE]);
    let changed_capability = policy
        .clone()
        .with_required_capabilities([Capability::Read])
        .expect("full grants read");

    let mut refusals: BTreeMap<&str, Vec<u8>> = BTreeMap::new();
    for (label, candidate) in [
        ("audience", changed_audience),
        ("profile", changed_profile),
        ("generation", changed_generation),
        ("capability", changed_capability),
    ] {
        let subject = candidate
            .admit(
                TransportAuthMode::ProxiedOidc,
                principal.clone(),
                scopes.clone(),
            )
            .expect("admitted");
        assert!(
            !subject.matches_exactly(&owner),
            "a changed {label} must produce a different subject"
        );
        let refusal = registry
            .resume(handle, Some(&subject), 1)
            .expect_err(&format!("a changed {label} must never resume"));
        refusals.insert(label, refusal.as_bytes());
    }
    let distinct: BTreeSet<Vec<u8>> = refusals.values().cloned().collect();
    assert_eq!(
        distinct.len(),
        1,
        "every changed-fact refusal must be byte-identical, got {refusals:?}"
    );
    assert_eq!(
        distinct.into_iter().next().expect("one refusal shape"),
        binding_refusal().as_bytes()
    );
    // The unchanged subject still resumes, so the refusals above were about the
    // changed fact rather than about the registry having forgotten the binding.
    assert!(registry.resume(handle, Some(&owner), 1).is_ok());
    // An authentication or policy change discards every binding rather than
    // repairing it, and a generation bump discards exactly the stale ones.
    assert_eq!(registry.revoke_other_generations(5), 1);
    assert!(registry.resume(handle, Some(&owner), 1).is_err());
    let lease = registry
        .bind_session("internal-2", &owner, 1_000_000)
        .expect("bound");
    assert_eq!(registry.revoke_all(), 1);
    assert!(
        registry
            .resume(lease.handle().as_str(), Some(&owner), 1)
            .is_err()
    );
}

/// Standard base64, matching what the adapter emits for artifact bytes.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(char::from(ALPHABET[(first >> 2) as usize]));
        encoded.push(char::from(
            ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize],
        ));
        if chunk.len() > 1 {
            encoded.push(char::from(
                ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize],
            ));
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(char::from(ALPHABET[(third & 0x3f) as usize]));
        } else {
            encoded.push('=');
        }
    }
    encoded
}
