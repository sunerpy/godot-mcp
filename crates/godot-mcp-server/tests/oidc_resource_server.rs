//! Todo 29 acceptance: the proxy-hop identity layer and the OIDC Resource Server
//! boundary, with its fixed JWT/JWK header policy.
//!
//! Exactly two cases exist, matching `qa/tasks/29.json`:
//!
//! * `oidc_resource_server_happy_path` — an authenticated same-host proxy hop
//!   carries the configured principal's token, with the exact resource audience and
//!   the exact required scopes, and calls allowed tools *before and after* an
//!   overlapping-key rotation, using an allowlisted algorithm and an unambiguous
//!   `kid`.
//! * `oidc_resource_server_failure_paths` — a loopback peer with no hop credential,
//!   a second subject, another audience, an HTTP issuer, a mixed/private resolution
//!   answer, a redirect to the metadata IP, an environment proxy, public-to-private
//!   lookup/connect rebinding, `alg: none`, an HMAC substitution, a non-allowlisted
//!   algorithm, key/algorithm confusion, a duplicate and a missing `kid`, a
//!   token-supplied `jku`/`x5u`/`x5c`/`jwk`, and an unsupported `crit` entry are all
//!   rejected — with no SSRF and no kernel dispatch.
//!
//! Everything here is deterministic. The keys are committed fixtures (see
//! `tests/fixtures/auth/`), the clock is pinned, and DNS answers are scripted, so no
//! case depends on a network or a live identity provider.
//!
//! The load-bearing ordering assertion is the per-boundary signature counter: for
//! every header-policy, key-selection and metadata rejection the suite asserts the
//! boundary performed *zero* signature verifications, which is what makes "rejected
//! before any effective signature check" a measurement rather than a claim.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::config::{
    CliOverrides, ConfigError, Limits, OidcSigningAlgorithm, ProxyHopCredential, load_config,
};
use godot_mcp_domain::contracts::{Capability, PrincipalId, ProjectIdentity, SessionId};
use godot_mcp_domain::objid::{LeaseKey, StableObjectId};
use godot_mcp_server::auth::metadata::ConfigRejection;
use godot_mcp_server::auth::pipeline::OidcClock;
use godot_mcp_server::auth::resolver::{ScriptedResolver, reject_environment_proxy};
use godot_mcp_server::auth::startup::apply_jwks_refresh_result;
use godot_mcp_server::auth::testkeys;
use godot_mcp_server::auth::{
    AlgorithmAllowlist, AuthChallenge, AuthRejection, ClaimRejection, HARD_ALLOWLIST,
    HeaderRejection, HopCredential, HopRejection, JwkSet, KeyRejection,
    PROTECTED_RESOURCE_METADATA_PATH, ProxyHop, ResourceServer, ResourceServerConfig,
    SignatureAlgorithm, SsrfRejection, UnverifiedHeader, VettedEndpoint,
};
use godot_mcp_server::http::{
    HttpTransportOptions, MCP_PATH, PROXY_HOP_HEADER, bind_http_transport,
};
use godot_mcp_server::idempotency::{ModePrincipal, TransportAuthMode};
use godot_mcp_server::kernel::{AccessContext, ServiceKernel};
use godot_mcp_server::observability::TokenValue;
use godot_mcp_server::stdio::{ProjectRuntime, StdioOptions};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Deterministic fixtures.
// ---------------------------------------------------------------------------

const ISSUER: &str = "https://issuer.example";
const JWKS: &str = "https://issuer.example/jwks";
const SUBJECT: &str = "subject-1";
const SECOND_SUBJECT: &str = "subject-2";
const RESOURCE: &str = "https://mcp.example.com/mcp";
const NOW: u64 = 1_700_000_000;
const REQUIRED_SCOPE: &str = "godot.read";

fn config() -> ResourceServerConfig {
    ResourceServerConfig::new(ISSUER, JWKS, SUBJECT, RESOURCE, RESOURCE)
        .expect("the fixture configuration is valid")
        .with_required_scopes([REQUIRED_SCOPE])
        .with_required_capabilities([Capability::Read])
}

fn claims_for(subject: &str, audience: &str) -> Value {
    json!({
        "iss": ISSUER,
        "sub": subject,
        "aud": audience,
        "exp": NOW + 300,
        "nbf": NOW - 30,
        "iat": NOW - 30,
        "scope": format!("{REQUIRED_SCOPE} godot.analyze"),
    })
}

fn valid_claims() -> Value {
    claims_for(SUBJECT, RESOURCE)
}

fn token_for(key: &testkeys::TestKey, algorithm: SignatureAlgorithm, claims: &Value) -> TokenValue {
    TokenValue::new(testkeys::sign(key, algorithm, Some(key.kid), claims))
}

/// One boundary plus the hop credential a proxy would present to it.
struct Boundary {
    server: ResourceServer,
    hop_credential: String,
    directory: tempfile::TempDir,
}

impl Boundary {
    fn build(keys: &[testkeys::TestKey], config: ResourceServerConfig) -> anyhow::Result<Self> {
        Self::from_document(&testkeys::jwks_document(keys), config)
    }

    fn from_document(document: &str, config: ResourceServerConfig) -> anyhow::Result<Self> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("proxy-hop-credential");
        let hop_credential = ProxyHop::publish(&path)?;
        let hop = ProxyHop::load(HopCredential::from_config(
            &ProxyHopCredential::CredentialFile {
                path: path.display().to_string(),
            },
        ))?;
        let keys = JwkSet::parse(document)?;
        Ok(Self {
            server: ResourceServer::new(config, hop, keys, OidcClock::Fixed(NOW)),
            hop_credential,
            directory,
        })
    }

    fn authenticate(&self, token: &TokenValue) -> Result<ModePrincipal, AuthRejection> {
        self.server
            .authenticate(Some(&self.hop_credential), Some(token))
            .map(|validated| validated.principal().clone())
    }

    fn path(&self) -> std::path::PathBuf {
        self.directory.path().join("proxy-hop-credential")
    }
}

fn fixture_project() -> ProjectIdentity {
    ProjectIdentity::from_lease_key(
        LeaseKey {
            root: StableObjectId::Unix { dev: 29, ino: 1 },
            project_file: StableObjectId::Unix { dev: 29, ino: 2 },
        },
        4,
    )
}

fn fixture_access() -> AccessContext {
    AccessContext::new(
        fixture_project(),
        PrincipalId::parse(format!("principal_{}", "c".repeat(32))).expect("principal"),
        Some(SessionId::parse(format!("session_{}", "d".repeat(32))).expect("session")),
        4_000,
    )
}

fn fixture_kernel() -> anyhow::Result<ServiceKernel> {
    Ok(ServiceKernel::new(CapabilityProfile::Full))
}

fn public_answer() -> Vec<IpAddr> {
    vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]
}

// ---------------------------------------------------------------------------
// Case 1 — happy path.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn oidc_resource_server_happy_path() -> anyhow::Result<()> {
    // --- The production configuration can carry every fixture restriction. ---
    let configured = load_config(
        Some(&format!(
            r#"
transport = "http"

[http]
bind = "127.0.0.1:9443"
mode = "proxied_oidc"
host = "127.0.0.1:9443"
origin = "https://mcp.example.com"
external_resource_url = "{RESOURCE}"
allowed_algorithms = ["RS256", "PS256", "ES256"]
required_scopes = ["{REQUIRED_SCOPE}"]
required_capabilities = ["read"]

[http.proxy_hop.credential_file]
path = "/tmp/godot-mcp-proxy-hop"

[http.principal]
issuer = "{ISSUER}"
subject = "{SUBJECT}"
"#
        )),
        &Default::default(),
        &CliOverrides::default(),
    )?;
    let configured = configured.http.expect("HTTP configuration");
    assert_eq!(
        configured.effective_allowed_algorithms(),
        vec![
            OidcSigningAlgorithm::Rs256,
            OidcSigningAlgorithm::Ps256,
            OidcSigningAlgorithm::Es256,
        ]
    );
    assert_eq!(
        configured.effective_required_scopes(),
        vec![REQUIRED_SCOPE.to_owned()]
    );
    assert_eq!(
        configured.effective_required_capabilities(),
        vec![Capability::Read]
    );

    // --- The hop credential is ACL-protected and constant-time compared. ---
    let boundary = Boundary::build(&[testkeys::RSA_OLD], config())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(boundary.path())?.permissions().mode() & 0o777,
            0o600,
            "the hop credential must be owner-only from the outset"
        );
    }
    let mut first_byte = boundary.hop_credential.clone().into_bytes();
    first_byte[0] = if first_byte[0] == b'a' { b'b' } else { b'a' };
    let mut last_byte = boundary.hop_credential.clone().into_bytes();
    let end = last_byte.len() - 1;
    last_byte[end] = if last_byte[end] == b'a' { b'b' } else { b'a' };
    let first_byte = String::from_utf8(first_byte)?;
    let last_byte = String::from_utf8(last_byte)?;
    assert_eq!(
        ProxyHop::load(HopCredential::File {
            path: boundary.path()
        })?
        .compare_probe(&first_byte),
        ProxyHop::load(HopCredential::File {
            path: boundary.path()
        })?
        .compare_probe(&last_byte),
        "the hop comparison must examine every byte"
    );

    // --- An allowlisted algorithm with an unambiguous kid authenticates. ---
    let token = token_for(
        &testkeys::RSA_OLD,
        SignatureAlgorithm::Rs256,
        &valid_claims(),
    );
    assert_eq!(
        boundary.authenticate(&token)?,
        ModePrincipal::Oidc {
            issuer: ISSUER.to_owned(),
            subject: SUBJECT.to_owned(),
        },
        "the resolved principal is the exact configured (iss, sub) pair"
    );
    assert_eq!(
        boundary.authenticate(&token)?.required_mode(),
        TransportAuthMode::ProxiedOidc
    );
    let validated = boundary
        .server
        .authenticate(Some(&boundary.hop_credential), Some(&token))?;
    assert_eq!(validated.kid(), testkeys::RSA_OLD.kid);
    assert_eq!(validated.algorithm(), SignatureAlgorithm::Rs256);
    assert!(validated.scopes().contains(REQUIRED_SCOPE));
    assert_eq!(validated.mode(), TransportAuthMode::ProxiedOidc);
    assert_eq!(
        boundary.server.signature_checks(),
        3,
        "each accepted token costs exactly one signature verification"
    );

    // --- Every allowlisted algorithm works with its own key kind. ---
    let es256 = Boundary::build(&[testkeys::EC_P256], config())?;
    let es256_token = token_for(
        &testkeys::EC_P256,
        SignatureAlgorithm::Es256,
        &valid_claims(),
    );
    assert_eq!(
        es256.authenticate(&es256_token)?,
        ModePrincipal::Oidc {
            issuer: ISSUER.to_owned(),
            subject: SUBJECT.to_owned(),
        }
    );
    // PS256 shares the RSA key kind with RS256, so the fixture key is republished
    // declaring `alg: PS256` — a JWK whose declared algorithm must match the token's
    // is exactly the step-3 rule, and it cuts both ways.
    let ps256_jwk: Value = serde_json::from_str(testkeys::RSA_NEW.jwk)?;
    let mut ps256_jwk = ps256_jwk.as_object().expect("jwk object").clone();
    ps256_jwk.insert("alg".to_owned(), json!("PS256"));
    let ps256 = Boundary::from_document(
        &json!({ "keys": [Value::Object(ps256_jwk)] }).to_string(),
        config(),
    )?;
    let ps256_token = token_for(
        &testkeys::RSA_NEW,
        SignatureAlgorithm::Ps256,
        &valid_claims(),
    );
    assert!(
        ps256.authenticate(&ps256_token).is_ok(),
        "PS256 is inside the hard allowlist"
    );

    // --- A bounded old/new overlap: both keys work, each on its own kid. ---
    let rotating = Boundary::build(&[testkeys::RSA_OLD, testkeys::RSA_NEW], config())?;
    assert_eq!(rotating.server.keys().len(), 2);
    for key in [testkeys::RSA_OLD, testkeys::RSA_NEW] {
        let during = token_for(&key, SignatureAlgorithm::Rs256, &valid_claims());
        let validated = rotating
            .server
            .authenticate(Some(&rotating.hop_credential), Some(&during))?;
        assert_eq!(
            validated.kid(),
            key.kid,
            "the selected key is the one the kid names, not whichever verifies"
        );
    }
    // After the overlap closes the old key is *unknown*, not "tried and failed":
    // rotation never becomes a second verification attempt.
    apply_jwks_refresh_result(
        &rotating.server,
        Ok(testkeys::jwks_document(&[testkeys::RSA_NEW])),
    )?;
    let after = rotating
        .server
        .authenticate(Some(&rotating.hop_credential), Some(&token))
        .expect_err("the retired kid is no longer selectable");
    assert_eq!(after, AuthRejection::Key(KeyRejection::UnknownKid));
    let checks_before_retired = rotating.server.signature_checks();
    let new_token = token_for(
        &testkeys::RSA_NEW,
        SignatureAlgorithm::Rs256,
        &valid_claims(),
    );
    assert!(
        rotating
            .server
            .authenticate(Some(&rotating.hop_credential), Some(&new_token))
            .is_ok(),
        "the current key still works after the overlap closes"
    );
    assert_eq!(
        rotating.server.signature_checks(),
        checks_before_retired + 1,
        "the retired-kid refusal spent no signature check"
    );

    // --- An audience array naming the resource, and array-form scopes. ---
    let array_form = token_for(
        &testkeys::RSA_OLD,
        SignatureAlgorithm::Rs256,
        &json!({
            "iss": format!("{ISSUER}/"),
            "sub": SUBJECT,
            "aud": ["https://other.example", RESOURCE],
            "exp": NOW + 300,
            "scope": [REQUIRED_SCOPE, "godot.analyze"],
        }),
    );
    assert!(boundary.authenticate(&array_form).is_ok());

    // --- RFC 9728 metadata advertises the external HTTPS resource URL. ---
    let metadata = config().metadata();
    assert_eq!(metadata.resource(), RESOURCE);
    assert_eq!(metadata.authorization_servers(), [ISSUER.to_owned()]);
    assert_eq!(
        config().metadata_url(),
        format!("https://mcp.example.com{PROTECTED_RESOURCE_METADATA_PATH}")
    );
    let document: Value = serde_json::from_str(&metadata.to_body())?;
    assert_eq!(
        document.get("resource").and_then(Value::as_str),
        Some(RESOURCE)
    );
    assert_eq!(
        document.get("scopes_supported"),
        Some(&json!([REQUIRED_SCOPE]))
    );
    assert_eq!(
        document.get("resource_signing_alg_values_supported"),
        Some(&json!(["RS256", "PS256", "ES256"])),
        "the document advertises the selected subset of the hard allowlist"
    );
    assert_eq!(
        document.get("godot_capabilities_required"),
        Some(&json!(["read"])),
        "the document makes the configured capability policy observable"
    );
    assert_eq!(
        metadata.to_body(),
        config().metadata().to_body(),
        "the document is byte-stable"
    );

    // --- The 401 challenge points a refused client at that document. ---
    let challenge = AuthChallenge::unauthenticated(&config().metadata_url());
    assert_eq!(challenge.status(), 401);
    assert_eq!(challenge.body(), "Unauthorized");
    assert!(
        challenge.header_value().contains(&format!(
            "resource_metadata=\"https://mcp.example.com{PROTECTED_RESOURCE_METADATA_PATH}\""
        )),
        "the challenge names the external metadata URL, not the loopback bind"
    );

    // --- A vetted endpoint pins the address and keeps the hostname. ---
    let resolver = ScriptedResolver::new(vec![public_answer()]);
    let endpoint = VettedEndpoint::vet(JWKS, &resolver)?;
    assert_eq!(endpoint.host(), "issuer.example");
    assert_eq!(
        endpoint.socket_addresses(),
        vec![SocketAddr::new(public_answer()[0], 443)],
        "the socket connects to the exact vetted address"
    );
    assert_eq!(
        resolver.lookups(),
        1,
        "DNS is consulted once; the vetted address is then pinned"
    );
    assert_eq!(reject_environment_proxy([("PATH", "/usr/bin")]), Ok(()));

    // --- The end-to-end listener: a real request over the real guard. ---
    let armed = Arc::new(ResourceServer::new(
        config(),
        ProxyHop::load(HopCredential::File {
            path: boundary.path(),
        })?,
        JwkSet::parse(&testkeys::jwks_document(&[testkeys::RSA_OLD]))?,
        OidcClock::Fixed(NOW),
    ));
    let project_dir = tempfile::tempdir()?;
    let project = project_dir.path().join("project");
    std::fs::create_dir_all(&project)?;
    std::fs::write(project.join("project.godot"), b"config_version=5\n")?;
    let runtime = ProjectRuntime::open(&StdioOptions {
        project,
        godot_binary: None,
        profile: CapabilityProfile::Full,
        principal: PrincipalId::parse(format!("principal_{}", "e".repeat(32)))?,
        observed_at_unix_ms: 4_000,
        limits: Limits::default(),
        process: Default::default(),
    })?;
    let transport = bind_http_transport(
        HttpTransportOptions::proxied_oidc("127.0.0.1:0".parse::<SocketAddr>()?, RESOURCE)
            .with_boundary(Arc::clone(&armed)),
        runtime.kernel(),
        runtime.access(),
    )
    .await?;
    assert!(
        transport.local_addr().ip().is_loopback(),
        "proxied_oidc still binds loopback only"
    );
    let endpoint_url = transport.endpoint();
    assert!(endpoint_url.ends_with(MCP_PATH));
    // Bounded, so a response that never arrives fails the test at the request
    // that lost it instead of holding the run: one gate run on 2026-10-10 hung in
    // this binary with a connection established and every thread parked.
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    // The metadata document is reachable without a credential, because a client
    // cannot obtain an acceptable token before reading it.
    let served = client
        .get(format!(
            "http://{}{PROTECTED_RESOURCE_METADATA_PATH}",
            transport.local_addr()
        ))
        .header("Host", transport.local_addr().to_string())
        .send()
        .await?;
    assert_eq!(served.status().as_u16(), 200);
    let served: Value = served.json().await?;
    assert_eq!(
        served.get("resource").and_then(Value::as_str),
        Some(RESOURCE)
    );

    // An authenticated hop carrying the configured principal's token gets through
    // the guard and reaches the MCP service.
    let authorized = client
        .post(&endpoint_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", format!("Bearer {}", token.expose()))
        .header(PROXY_HOP_HEADER, &boundary.hop_credential)
        .body(serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "todo-29-probe", "version": "0" }
            }
        }))?)
        .send()
        .await?;
    assert!(
        authorized.status().is_success(),
        "an authenticated hop with a valid token is admitted, got {}",
        authorized.status()
    );
    let session = authorized
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .expect("an admitted handshake establishes an MCP session");

    // The load-bearing assertion for this mode: protected dispatch actually reaches
    // the kernel. Before Todo 29 armed the boundary this answered `unavailable`
    // without dispatching, so a passing assertion here is what proves the mode is
    // genuinely open to an authenticated caller rather than merely reachable.
    let dispatched = client
        .post(&endpoint_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", format!("Bearer {}", token.expose()))
        .header(PROXY_HOP_HEADER, &boundary.hop_credential)
        .header("Mcp-Session-Id", &session)
        .body(serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "health.get", "arguments": {} }
        }))?)
        .send()
        .await?;
    assert!(
        dispatched.status().is_success(),
        "an authenticated tool call is served, got {}",
        dispatched.status()
    );
    let served = dispatched.text().await?;
    assert!(
        served.contains("\"transport\":\"http\""),
        "production health must name the bound HTTP lane: {served}"
    );
    assert!(
        served.contains("\"mode\":\"proxied_oidc\""),
        "production health must name the configured proxied_oidc mode: {served}"
    );
    assert!(
        !served.contains("jwks_unreachable") && !served.contains("jwks_empty"),
        "the live populated ResourceServer key cache must report ready: {served}"
    );
    assert_eq!(
        transport.kernel_dispatches(),
        1,
        "exactly one authenticated request reached the kernel"
    );
    transport.shutdown().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Case 2 — failure paths.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn oidc_resource_server_failure_paths() -> anyhow::Result<()> {
    // --- An omitted production policy takes documented fail-closed defaults. ---
    let defaults = load_config(
        Some(&format!(
            r#"
transport = "http"

[http]
bind = "127.0.0.1:9443"
mode = "proxied_oidc"
host = "127.0.0.1:9443"
origin = "https://mcp.example.com"
external_resource_url = "{RESOURCE}"

[http.proxy_hop.credential_file]
path = "/tmp/godot-mcp-proxy-hop"

[http.principal]
issuer = "{ISSUER}"
subject = "{SUBJECT}"
"#
        )),
        &Default::default(),
        &CliOverrides::default(),
    )?;
    let defaults = defaults.http.expect("HTTP configuration");
    assert_eq!(
        defaults.effective_allowed_algorithms(),
        vec![OidcSigningAlgorithm::Rs256],
        "an omitted subset narrows to RS256 instead of widening to the hard allowlist"
    );
    assert_eq!(
        defaults.effective_required_scopes(),
        vec![REQUIRED_SCOPE.to_owned()]
    );
    assert_eq!(
        defaults.effective_required_capabilities(),
        vec![Capability::Read]
    );
    for nonsensical in [
        "allowed_algorithms = []",
        "required_scopes = []",
        "required_scopes = [\"godot read\"]",
        "required_capabilities = []",
        "required_capabilities = [\"read\", \"read\"]",
    ] {
        let text = format!(
            r#"
transport = "http"

[http]
bind = "127.0.0.1:9443"
mode = "proxied_oidc"
host = "127.0.0.1:9443"
origin = "https://mcp.example.com"
external_resource_url = "{RESOURCE}"
{nonsensical}

[http.proxy_hop.credential_file]
path = "/tmp/godot-mcp-proxy-hop"

[http.principal]
issuer = "{ISSUER}"
subject = "{SUBJECT}"
"#
        );
        assert!(
            matches!(
                load_config(Some(&text), &Default::default(), &CliOverrides::default()),
                Err(ConfigError::EmptyOidcPolicy { .. })
                    | Err(ConfigError::InvalidOidcScope { .. })
                    | Err(ConfigError::DuplicateOidcPolicyValue { .. })
            ),
            "nonsensical policy must be rejected: {nonsensical}"
        );
    }

    let boundary = Boundary::build(&[testkeys::RSA_OLD], config())?;
    let token = token_for(
        &testkeys::RSA_OLD,
        SignatureAlgorithm::Rs256,
        &valid_claims(),
    );

    // --- Layer 1: a loopback peer with no hop credential is refused. ---
    let checks = boundary.server.signature_checks();
    assert_eq!(
        boundary.server.authenticate(None, Some(&token)).err(),
        Some(AuthRejection::Hop(HopRejection::Missing)),
        "loopback alone never authenticates the proxy hop"
    );
    assert_eq!(
        boundary.server.authenticate(Some(""), Some(&token)).err(),
        Some(AuthRejection::Hop(HopRejection::Missing))
    );
    assert_eq!(
        boundary
            .server
            .authenticate(Some(&"0".repeat(64)), Some(&token))
            .err(),
        Some(AuthRejection::Hop(HopRejection::Mismatch))
    );
    assert_eq!(
        boundary.server.signature_checks(),
        checks,
        "an unauthenticated hop must never reach a signature check"
    );

    // A relaxed credential file is refused rather than silently trusted.
    #[cfg(unix)]
    {
        let relaxed = tempfile::tempdir()?;
        let path = relaxed.path().join("hop");
        ProxyHop::publish(&path)?;
        ProxyHop::set_mode(&path, 0o644)?;
        assert_eq!(
            ProxyHop::load(HopCredential::File { path }).err(),
            Some(HopRejection::InsecurePermissions { mode: 0o644 })
        );
    }

    // --- Step 1: the header policy, all rejected with zero signature checks. ---
    let header_cases: Vec<(Value, HeaderRejection)> = vec![
        (
            json!({"alg": "none", "kid": testkeys::RSA_OLD.kid}),
            HeaderRejection::NoneAlgorithm,
        ),
        (
            json!({"alg": "HS256", "kid": testkeys::RSA_OLD.kid}),
            HeaderRejection::HmacAlgorithm,
        ),
        (
            json!({"alg": "HS384", "kid": testkeys::RSA_OLD.kid}),
            HeaderRejection::HmacAlgorithm,
        ),
        (
            json!({"alg": "HS512", "kid": testkeys::RSA_OLD.kid}),
            HeaderRejection::HmacAlgorithm,
        ),
        (
            json!({"alg": "RS384", "kid": testkeys::RSA_OLD.kid}),
            HeaderRejection::NonAllowlistedAlgorithm,
        ),
        (
            json!({"alg": "RS512", "kid": testkeys::RSA_OLD.kid}),
            HeaderRejection::NonAllowlistedAlgorithm,
        ),
        (
            json!({"alg": "ES384", "kid": testkeys::RSA_OLD.kid}),
            HeaderRejection::NonAllowlistedAlgorithm,
        ),
        (
            json!({"alg": "EdDSA", "kid": testkeys::RSA_OLD.kid}),
            HeaderRejection::NonAllowlistedAlgorithm,
        ),
        (
            json!({"alg": "RS256", "kid": testkeys::RSA_OLD.kid, "jku": "https://evil.example/jwks"}),
            HeaderRejection::JkuHeader,
        ),
        (
            json!({"alg": "RS256", "kid": testkeys::RSA_OLD.kid, "x5u": "https://evil.example/chain"}),
            HeaderRejection::X5uHeader,
        ),
        (
            json!({"alg": "RS256", "kid": testkeys::RSA_OLD.kid, "x5c": ["MIIBpayload"]}),
            HeaderRejection::EmbeddedX5c,
        ),
        (
            json!({"alg": "RS256", "kid": testkeys::RSA_OLD.kid, "jwk": {"kty": "RSA", "n": "AQAB", "e": "AQAB"}}),
            HeaderRejection::EmbeddedJwk,
        ),
        (
            json!({"alg": "RS256", "kid": testkeys::RSA_OLD.kid, "crit": ["b64"]}),
            HeaderRejection::CriticalHeader,
        ),
        (
            json!({"alg": "RS256", "kid": testkeys::RSA_OLD.kid, "crit": []}),
            HeaderRejection::CriticalHeader,
        ),
        (json!({"alg": "RS256"}), HeaderRejection::MissingKid),
        (
            json!({"alg": "RS256", "kid": ""}),
            HeaderRejection::MissingKid,
        ),
    ];
    let checks = boundary.server.signature_checks();
    for (header, expected) in &header_cases {
        let forged = TokenValue::new(testkeys::sign_with_literal_header(
            &testkeys::RSA_OLD,
            header,
            &valid_claims(),
        ));
        assert_eq!(
            boundary
                .server
                .authenticate(Some(&boundary.hop_credential), Some(&forged))
                .err(),
            Some(AuthRejection::Header(*expected)),
            "{header} must be refused by the header policy"
        );
    }
    assert_eq!(
        boundary.server.signature_checks(),
        checks,
        "every header-policy rejection happens before any effective signature check"
    );

    // A missing kid is refused even against a multi-key JWKS: a single candidate is
    // not an excuse to skip the exact match.
    let multi = Boundary::build(&[testkeys::RSA_OLD, testkeys::RSA_NEW], config())?;
    let no_kid = TokenValue::new(testkeys::sign(
        &testkeys::RSA_OLD,
        SignatureAlgorithm::Rs256,
        None,
        &valid_claims(),
    ));
    assert_eq!(
        multi
            .server
            .authenticate(Some(&multi.hop_credential), Some(&no_kid))
            .err(),
        Some(AuthRejection::Header(HeaderRejection::MissingKid))
    );
    assert_eq!(multi.server.signature_checks(), 0);

    // --- Step 2: kid selection against the server-fetched JWKS. ---
    // Two entries sharing a kid reject the whole set: "try each" would turn a
    // rotation window into a second verification attempt.
    let duplicate = format!(
        "{{\"keys\":[{},{}]}}",
        testkeys::RSA_OLD.jwk.trim(),
        testkeys::RSA_OLD.jwk.trim()
    );
    assert_eq!(
        JwkSet::parse(&duplicate).err(),
        Some(KeyRejection::DuplicateKid {
            kid: testkeys::RSA_OLD.kid.to_owned()
        })
    );
    assert_eq!(
        JwkSet::parse("{\"keys\":[{\"kty\":\"RSA\",\"n\":\"AQAB\",\"e\":\"AQAB\"}]}").err(),
        Some(KeyRejection::KeyWithoutKid),
        "a JWKS entry with no kid is unusable"
    );
    let unknown_kid = TokenValue::new(testkeys::sign(
        &testkeys::RSA_NEW,
        SignatureAlgorithm::Rs256,
        Some("key-that-does-not-exist"),
        &valid_claims(),
    ));
    let checks = boundary.server.signature_checks();
    assert_eq!(
        boundary
            .server
            .authenticate(Some(&boundary.hop_credential), Some(&unknown_kid))
            .err(),
        Some(AuthRejection::Key(KeyRejection::UnknownKid))
    );
    assert_eq!(boundary.server.signature_checks(), checks);

    // --- Step 3: key/algorithm confusion, before the key can verify anything. ---
    let confusion = TokenValue::new(testkeys::sign_with_literal_header(
        &testkeys::EC_P256,
        &json!({"alg": "ES256", "kid": testkeys::RSA_OLD.kid}),
        &valid_claims(),
    ));
    assert_eq!(
        boundary
            .server
            .authenticate(Some(&boundary.hop_credential), Some(&confusion))
            .err(),
        Some(AuthRejection::Key(KeyRejection::KeyTypeMismatch)),
        "an RSA key must never satisfy an ES256 token"
    );
    let p384 = Boundary::build(&[testkeys::EC_P384], config())?;
    let p384_token = TokenValue::new(testkeys::sign_with_literal_header(
        &testkeys::EC_P256,
        &json!({"alg": "ES256", "kid": testkeys::EC_P384.kid}),
        &valid_claims(),
    ));
    let rejection = p384
        .server
        .authenticate(Some(&p384.hop_credential), Some(&p384_token))
        .err();
    assert!(
        matches!(
            rejection,
            Some(AuthRejection::Key(
                KeyRejection::CurveMismatch | KeyRejection::AlgorithmMismatch
            ))
        ),
        "a P-384 key must never satisfy ES256, got {rejection:?}"
    );
    assert_eq!(
        p384.server.signature_checks(),
        0,
        "curve confusion is refused before any signature check"
    );
    for (key, expected) in [
        (
            json!({"kid": "k", "kty": "RSA", "alg": "RS512", "n": "AQAB", "e": "AQAB"}),
            KeyRejection::AlgorithmMismatch,
        ),
        (
            json!({"kid": "k", "kty": "RSA", "use": "enc", "n": "AQAB", "e": "AQAB"}),
            KeyRejection::NonSignatureUse,
        ),
        (
            json!({"kid": "k", "kty": "RSA", "key_ops": ["sign"], "n": "AQAB", "e": "AQAB"}),
            KeyRejection::KeyOpsMismatch,
        ),
    ] {
        let set = JwkSet::parse(&json!({"keys": [key]}).to_string())?;
        let header = godot_mcp_server::auth::b64::encode(
            serde_json::to_string(&json!({"alg": "RS256", "kid": "k"}))?.as_bytes(),
        );
        let parsed = UnverifiedHeader::parse(
            &format!("{header}.eyJhIjoxfQ.c2ln"),
            &AlgorithmAllowlist::hard(),
        )
        .expect("the probe header is allowlisted");
        assert_eq!(
            set.select(parsed)
                .expect("the kid matches")
                .check_metadata()
                .err(),
            Some(expected)
        );
    }

    // --- Step 4: a genuine key, a signature that does not belong to it. ---
    let wrong_signature = TokenValue::new(testkeys::sign(
        &testkeys::RSA_NEW,
        SignatureAlgorithm::Rs256,
        Some(testkeys::RSA_OLD.kid),
        &valid_claims(),
    ));
    let checks = boundary.server.signature_checks();
    assert_eq!(
        boundary
            .server
            .authenticate(Some(&boundary.hop_credential), Some(&wrong_signature))
            .err(),
        Some(AuthRejection::Signature)
    );
    assert_eq!(
        boundary.server.signature_checks(),
        checks + 1,
        "a signature failure costs exactly one check and is never retried"
    );

    // A failed refresh retains the last verified set; it neither installs an
    // unverified document nor makes an unknown rotated key acceptable.
    let stale = Boundary::build(&[testkeys::RSA_OLD], config())?;
    apply_jwks_refresh_result(&stale.server, Err(SsrfRejection::TransportFailure))
        .expect_err("a failed refresh is observable to the refresh worker");
    let rotated = token_for(
        &testkeys::RSA_NEW,
        SignatureAlgorithm::Rs256,
        &valid_claims(),
    );
    assert_eq!(
        stale.authenticate(&rotated).err(),
        Some(AuthRejection::Key(KeyRejection::UnknownKid)),
        "refresh failure must not accept a key that was never verified"
    );
    assert!(
        stale.authenticate(&token).is_ok(),
        "the last verified key set remains usable while refresh fails closed"
    );

    // --- Step 5: claims, each refused exactly. ---
    let claim_cases: Vec<(Value, ClaimRejection)> = vec![
        (
            claims_for(SECOND_SUBJECT, RESOURCE),
            ClaimRejection::WrongSubject,
        ),
        (
            claims_for(SUBJECT, "https://another.example/mcp"),
            ClaimRejection::WrongAudience,
        ),
        (
            json!({"iss": "https://elsewhere.example", "sub": SUBJECT, "aud": RESOURCE, "exp": NOW + 300, "scope": REQUIRED_SCOPE}),
            ClaimRejection::WrongIssuer,
        ),
        (
            json!({"iss": "http://issuer.example", "sub": SUBJECT, "aud": RESOURCE, "exp": NOW + 300, "scope": REQUIRED_SCOPE}),
            ClaimRejection::WrongIssuer,
        ),
        (
            json!({"iss": ISSUER, "sub": SUBJECT, "aud": RESOURCE, "exp": NOW - 1, "scope": REQUIRED_SCOPE}),
            ClaimRejection::Expired,
        ),
        (
            json!({"iss": ISSUER, "sub": SUBJECT, "aud": RESOURCE, "exp": NOW + 300, "nbf": NOW + 120, "scope": REQUIRED_SCOPE}),
            ClaimRejection::NotYetValid,
        ),
        (
            json!({"iss": ISSUER, "sub": SUBJECT, "aud": RESOURCE, "exp": NOW + 300, "iat": NOW + 120, "scope": REQUIRED_SCOPE}),
            ClaimRejection::IssuedInFuture,
        ),
        (
            json!({"iss": ISSUER, "sub": SUBJECT, "aud": RESOURCE, "exp": NOW + 300, "scope": "godot.analyze"}),
            ClaimRejection::InsufficientScope,
        ),
        (
            json!({"iss": ISSUER, "sub": SUBJECT, "aud": RESOURCE, "exp": NOW + 300}),
            ClaimRejection::InsufficientScope,
        ),
        (
            json!({"sub": SUBJECT, "aud": RESOURCE, "exp": NOW + 300, "scope": REQUIRED_SCOPE}),
            ClaimRejection::MissingClaim,
        ),
        (
            json!({"iss": ISSUER, "aud": RESOURCE, "exp": NOW + 300, "scope": REQUIRED_SCOPE}),
            ClaimRejection::MissingClaim,
        ),
        (
            json!({"iss": ISSUER, "sub": SUBJECT, "aud": RESOURCE, "scope": REQUIRED_SCOPE}),
            ClaimRejection::MissingClaim,
        ),
    ];
    for (claims, expected) in &claim_cases {
        let forged = token_for(&testkeys::RSA_OLD, SignatureAlgorithm::Rs256, claims);
        assert_eq!(
            boundary
                .server
                .authenticate(Some(&boundary.hop_credential), Some(&forged))
                .err(),
            Some(AuthRejection::Claims(*expected)),
            "{claims} must be refused"
        );
    }

    // An HTTP issuer cannot even be configured, so the chain is never unauthenticated.
    assert_eq!(
        ResourceServerConfig::new("http://issuer.example", JWKS, SUBJECT, RESOURCE, RESOURCE).err(),
        Some(ConfigRejection::InsecureUrl { field: "issuer" })
    );
    assert_eq!(
        ResourceServerConfig::new(
            ISSUER,
            "http://issuer.example/jwks",
            SUBJECT,
            RESOURCE,
            RESOURCE
        )
        .err(),
        Some(ConfigRejection::InsecureUrl {
            field: "jwks_endpoint"
        })
    );
    assert_eq!(
        ResourceServerConfig::new(
            ISSUER,
            "https://evil.example/jwks",
            SUBJECT,
            RESOURCE,
            RESOURCE
        )
        .err(),
        Some(ConfigRejection::ForeignJwksEndpoint),
        "the JWKS location is fixed to the issuer's authority"
    );

    // --- SSRF: no fetch may reach a prohibited address, ever. ---
    let mixed = ScriptedResolver::new(vec![vec![
        IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
        IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
    ]]);
    assert_eq!(
        VettedEndpoint::vet(JWKS, &mixed).err(),
        Some(SsrfRejection::ProhibitedAddress),
        "a mixed answer is refused rather than filtered"
    );
    for answer in [
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)),
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)),
        IpAddr::V4(Ipv4Addr::new(172, 16, 0, 9)),
        IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
        IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1)),
        "::1".parse()?,
        "::ffff:127.0.0.1".parse()?,
        "fd00::1".parse()?,
        "fe80::1".parse()?,
    ] {
        let private = ScriptedResolver::new(vec![vec![answer]]);
        assert_eq!(
            VettedEndpoint::vet(JWKS, &private).err(),
            Some(SsrfRejection::ProhibitedAddress),
            "{answer} must never be connected to"
        );
    }

    // A redirect is re-vetted in full: the metadata IP and a foreign issuer both fail.
    let to_metadata =
        ScriptedResolver::new(vec![vec![IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))]]);
    assert_eq!(
        VettedEndpoint::revet_redirect("https://issuer.example/rotated", ISSUER, &to_metadata)
            .err(),
        Some(SsrfRejection::ProhibitedAddress),
        "a redirect at the metadata address is refused"
    );
    let elsewhere = ScriptedResolver::new(vec![public_answer()]);
    assert_eq!(
        VettedEndpoint::revet_redirect("https://evil.example/jwks", ISSUER, &elsewhere).err(),
        Some(SsrfRejection::IssuerChanged)
    );
    assert_eq!(
        VettedEndpoint::revet_redirect("http://issuer.example/jwks", ISSUER, &elsewhere).err(),
        Some(SsrfRejection::InsecureScheme)
    );

    // An environment proxy would send the request somewhere vetting never saw.
    for name in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        assert_eq!(
            reject_environment_proxy([(name, "http://proxy.internal:8080")]),
            Err(SsrfRejection::EnvironmentProxy),
            "{name} must be refused"
        );
    }

    // Public-to-private lookup/connect rebinding: the vetted endpoint keeps the
    // address it vetted, and a second answer cannot move the socket.
    let rebinding = ScriptedResolver::new(vec![
        public_answer(),
        vec![IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))],
    ]);
    let pinned = VettedEndpoint::vet(JWKS, &rebinding)?;
    assert_eq!(
        pinned.socket_addresses(),
        vec![SocketAddr::new(public_answer()[0], 443)],
        "the connect target is the vetted address, not a re-resolved one"
    );
    assert_eq!(rebinding.lookups(), 1);
    assert_eq!(
        VettedEndpoint::vet(JWKS, &rebinding).err(),
        Some(SsrfRejection::ProhibitedAddress),
        "the rebound answer would have been refused had it been trusted"
    );
    assert_eq!(
        pinned.socket_addresses(),
        vec![SocketAddr::new(public_answer()[0], 443)],
        "the already-vetted endpoint is unaffected by a later DNS answer"
    );
    // A literal address is refused outright, so a URL cannot bypass name vetting.
    assert_eq!(
        VettedEndpoint::vet("https://169.254.169.254/jwks", &elsewhere).err(),
        Some(SsrfRejection::LiteralAddress)
    );

    // --- Every refusal is indistinguishable to a caller. ---
    let metadata_url = config().metadata_url();
    let wire: BTreeSet<String> = [
        AuthRejection::Hop(HopRejection::Missing),
        AuthRejection::Hop(HopRejection::Mismatch),
        AuthRejection::Header(HeaderRejection::NoneAlgorithm),
        AuthRejection::Header(HeaderRejection::HmacAlgorithm),
        AuthRejection::Header(HeaderRejection::JkuHeader),
        AuthRejection::Header(HeaderRejection::CriticalHeader),
        AuthRejection::Key(KeyRejection::UnknownKid),
        AuthRejection::Key(KeyRejection::KeyTypeMismatch),
        AuthRejection::Signature,
        AuthRejection::Claims(ClaimRejection::WrongSubject),
        AuthRejection::Claims(ClaimRejection::WrongAudience),
        AuthRejection::Claims(ClaimRejection::Expired),
        AuthRejection::Discovery(SsrfRejection::ProhibitedAddress),
    ]
    .iter()
    .map(|rejection| {
        let challenge = AuthChallenge::new(rejection, &metadata_url);
        format!(
            "{} {} {}",
            challenge.status(),
            challenge.body(),
            challenge.header_value()
        )
    })
    .collect();
    assert_eq!(
        wire.len(),
        1,
        "authentication refusals must not be distinguishable, got {wire:?}"
    );
    // Only a token that is valid but lacks authority is a 403.
    assert_eq!(
        AuthChallenge::new(
            &AuthRejection::Claims(ClaimRejection::InsufficientScope),
            &metadata_url
        )
        .status(),
        403
    );

    // --- Nothing is logged, and nothing is forwarded. ---
    let rendered = format!("{:?}", boundary.server);
    assert!(
        !rendered.contains(&boundary.hop_credential),
        "the hop credential must never be rendered"
    );
    assert_eq!(format!("{token}"), "[REDACTED]");
    assert_eq!(format!("{token:?}"), "[REDACTED]");
    for rejection in [
        AuthRejection::Hop(HopRejection::Mismatch),
        AuthRejection::Signature,
    ] {
        assert_eq!(
            rejection.to_string(),
            "Unauthorized",
            "even a diagnostic rendering must not identify which check fired"
        );
    }

    // --- The listener: no refused request reaches the kernel. ---
    let armed = Arc::new(ResourceServer::new(
        config(),
        ProxyHop::load(HopCredential::File {
            path: boundary.path(),
        })?,
        JwkSet::parse(&testkeys::jwks_document(&[testkeys::RSA_OLD]))?,
        OidcClock::Fixed(NOW),
    ));
    let transport = bind_http_transport(
        HttpTransportOptions::proxied_oidc("127.0.0.1:0".parse::<SocketAddr>()?, RESOURCE)
            .with_boundary(Arc::clone(&armed)),
        Arc::new(fixture_kernel()?),
        fixture_access(),
    )
    .await?;
    let endpoint_url = transport.endpoint();
    // Bounded, so a response that never arrives fails the test at the request
    // that lost it instead of holding the run: one gate run on 2026-10-10 hung in
    // this binary with a connection established and every thread parked.
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "todo-29-probe", "version": "0" }
        }
    }))?;
    let second_subject = token_for(
        &testkeys::RSA_OLD,
        SignatureAlgorithm::Rs256,
        &claims_for(SECOND_SUBJECT, RESOURCE),
    );
    let none_alg = TokenValue::new(testkeys::sign_with_literal_header(
        &testkeys::RSA_OLD,
        &json!({"alg": "none", "kid": testkeys::RSA_OLD.kid}),
        &valid_claims(),
    ));
    let expired = token_for(
        &testkeys::RSA_OLD,
        SignatureAlgorithm::Rs256,
        &json!({
            "iss": ISSUER,
            "sub": SUBJECT,
            "aud": RESOURCE,
            "exp": NOW - 1,
            "scope": REQUIRED_SCOPE,
        }),
    );
    let insufficient_scope = token_for(
        &testkeys::RSA_OLD,
        SignatureAlgorithm::Rs256,
        &json!({
            "iss": ISSUER,
            "sub": SUBJECT,
            "aud": RESOURCE,
            "exp": NOW + 300,
            "scope": "godot.analyze",
        }),
    );
    let metadata_url = armed.config().metadata_url();
    let forbidden = client
        .post(&endpoint_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(PROXY_HOP_HEADER, &boundary.hop_credential)
        .header(
            "Authorization",
            format!("Bearer {}", insufficient_scope.expose()),
        )
        .body(body.clone())
        .send()
        .await?;
    assert_eq!(forbidden.status().as_u16(), 403);
    assert_eq!(
        forbidden
            .headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok()),
        Some(
            format!("Bearer error=\"insufficient_scope\", resource_metadata=\"{metadata_url}\"")
                .as_str()
        )
    );
    assert_eq!(forbidden.text().await?, "Unauthorized");
    let probes: Vec<(&str, Option<&str>, Option<&TokenValue>)> = vec![
        ("no hop credential and no token", None, None),
        ("a valid token but no hop credential", None, Some(&token)),
        (
            "a valid hop but the second subject",
            Some(&boundary.hop_credential),
            Some(&second_subject),
        ),
        (
            "a valid hop but an alg:none token",
            Some(&boundary.hop_credential),
            Some(&none_alg),
        ),
        (
            "a valid hop but an expired token",
            Some(&boundary.hop_credential),
            Some(&expired),
        ),
        (
            "a valid hop but no token",
            Some(&boundary.hop_credential),
            None,
        ),
    ];
    let mut refusal_shapes = BTreeSet::new();
    for (label, hop, presented) in probes {
        let mut request = client
            .post(&endpoint_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        if let Some(hop) = hop {
            request = request.header(PROXY_HOP_HEADER, hop);
        }
        if let Some(presented) = presented {
            request = request.header("Authorization", format!("Bearer {}", presented.expose()));
        }
        let response = request.body(body.clone()).send().await?;
        assert_eq!(
            response.status().as_u16(),
            401,
            "{label} must be refused with 401"
        );
        let challenge = response
            .headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        refusal_shapes.insert((response.text().await?, challenge));
    }
    assert_eq!(
        refusal_shapes.len(),
        1,
        "every authentication refusal must be byte-identical, got {refusal_shapes:?}"
    );
    assert_eq!(
        refusal_shapes.into_iter().next(),
        Some((
            "Unauthorized".to_owned(),
            format!("Bearer error=\"invalid_token\", resource_metadata=\"{metadata_url}\"")
        ))
    );
    // A forwarded header is still refused, and still never read.
    for header in ["X-Forwarded-For", "X-Forwarded-Host", "Forwarded"] {
        let response = client
            .post(&endpoint_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header(header, "10.0.0.7")
            .header(PROXY_HOP_HEADER, &boundary.hop_credential)
            .header("Authorization", format!("Bearer {}", token.expose()))
            .body(body.clone())
            .send()
            .await?;
        assert_eq!(
            response.status().as_u16(),
            400,
            "{header} must be refused before authentication is even considered"
        );
    }
    // A credential may never travel in a URL.
    let query = client
        .post(format!("{endpoint_url}?access_token=abc"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(body.clone())
        .send()
        .await?;
    assert_eq!(query.status().as_u16(), 400);
    assert_eq!(
        transport.kernel_dispatches(),
        0,
        "no refused request may reach the kernel"
    );
    transport.shutdown().await?;

    // A proxied_oidc listener with *no* boundary stays closed rather than open.
    let unarmed = bind_http_transport(
        HttpTransportOptions::proxied_oidc("127.0.0.1:0".parse::<SocketAddr>()?, RESOURCE),
        Arc::new(fixture_kernel()?),
        fixture_access(),
    )
    .await?;
    let refusal = client
        .post(unarmed.endpoint())
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(PROXY_HOP_HEADER, &boundary.hop_credential)
        .header("Authorization", format!("Bearer {}", token.expose()))
        .body(serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "capabilities.get", "arguments": {} }
        }))?)
        .send()
        .await?;
    assert!(
        !refusal.status().is_server_error(),
        "an unarmed listener answers at the protocol layer, got {}",
        refusal.status()
    );
    assert_eq!(
        unarmed.kernel_dispatches(),
        0,
        "a proxied_oidc listener without a boundary must never dispatch"
    );
    unarmed.shutdown().await?;

    // The hard allowlist itself is exactly three SHA-256 asymmetric algorithms.
    assert_eq!(
        HARD_ALLOWLIST
            .iter()
            .map(|algorithm| algorithm.as_str())
            .collect::<Vec<_>>(),
        ["RS256", "PS256", "ES256"]
    );
    Ok(())
}
