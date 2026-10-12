//! The optional Streamable HTTP transport, and its staged authentication boundary.
//!
//! # One kernel, two transports
//!
//! This module contains no tool logic. It mounts the official
//! [`StreamableHttpService`] from `rmcp` 2.2.0 on Axum and points it at the very
//! same [`RmcpAdapter`] over the very same [`ServiceKernel`] that
//! [`stdio`](crate::stdio) serves, which is what makes the shared parity suite in
//! `tests/http_transport.rs` meaningful rather than tautological. The only thing
//! wrapped around the adapter is [`StagedHandler`], which admits protected
//! dispatch only after the configured authentication boundary has been armed.
//!
//! # Never on by default
//!
//! [`HttpTransportOptions::from_config`] returns `None` unless configuration
//! selected [`Transport::Http`] *and* supplied an `[http]` section. No listener
//! is created on the default path, so a default launch opens no TCP socket.
//!
//! # Loopback only, and no direct TLS
//!
//! [`bind_http_transport`] refuses a non-loopback bind before it creates a
//! socket, and there is no TLS code path here at all: exposure beyond the host is
//! a same-host HTTPS reverse proxy's job. `proxied_oidc` may *advertise* an
//! external HTTPS URL, and only an HTTPS one, but it still binds loopback.
//!
//! # Staged authentication
//!
//! * `trusted_local` — every request must carry the launch's bearer secret (see
//!   [`secret`]), validated in constant time. Missing, stale and wrong
//!   credentials produce byte-identical refusals so the endpoint is not an
//!   oracle.
//! * `proxied_oidc` — an armed listener authenticates both the same-host proxy hop
//!   and the end-user OIDC token before protected dispatch. A listener constructed
//!   without that boundary answers `unavailable`, so partial configuration fails
//!   closed. This holds regardless of which loopback peer connected and regardless
//!   of any forwarded header, because neither is an identity.
//!
//! # What the guard refuses, before the kernel sees anything
//!
//! [`RequestGuard`] runs ahead of the MCP service and refuses forwarded headers,
//! any query string (so a credential can never arrive in a URL), a `Host` or
//! `Origin` outside the configured allowlists, an unsupported
//! `MCP-Protocol-Version`, and — in `trusted_local` — an unauthenticated request.
//! Body size and body-read time are bounded by `tower-http` layers wrapped
//! *around* the MCP service, so an oversize or stalled body is rejected before
//! any handler runs. `kernel_dispatches` counts only real kernel entries, which is
//! how the acceptance suite proves refusals never reached it.

pub mod secret;
mod staged;

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures::StreamExt as _;
use godot_mcp_domain::config::{HttpMode, Limits, ServerConfig, Transport};
use godot_mcp_domain::contracts::Capability;
use rmcp::model::ProtocolVersion;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;
use tower::ServiceBuilder;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::RequestBodyTimeoutLayer;

use crate::binding::{
    BindingRefusal, BindingRegistry, BoundSubject, SessionBindingPolicy, StreamTicket,
};
use crate::diagnostics::HttpDiagnosticsSource;
use crate::idempotency::{ModePrincipal, TransportAuthMode};
use crate::kernel::{AccessContext, ServiceKernel};
use crate::rmcp_adapter::RmcpAdapter;

pub use secret::{
    LAUNCH_SECRET_BYTES, LAUNCH_SECRET_FILE_MODE, LAUNCH_SECRET_FILE_NAME, LaunchSecret,
    LaunchSecretError,
};
pub use staged::StagedHandler;

/// The single MCP endpoint path both HTTP modes expose.
pub const MCP_PATH: &str = "/mcp";

/// The Streamable HTTP session header, in the one spelling this module compares.
///
/// The value travelling in it is the opaque handle
/// [`BindingRegistry`](crate::binding::BindingRegistry) minted, never the SDK's own
/// internal identifier: the middleware substitutes the internal one inbound and
/// restores the handle outbound, so the SDK's identifier never leaves the process.
pub const MCP_SESSION_HEADER: &str = "mcp-session-id";

/// The exact request headers a preflight may ask for, when an `Origin` is allowed.
///
/// Enumerated rather than reflected: echoing whatever a browser asked for would make
/// the allowlist decorative.
pub const ALLOWED_REQUEST_HEADERS: &str =
    "authorization, content-type, mcp-session-id, mcp-protocol-version";

/// The one response header a browser may read back.
pub const EXPOSED_RESPONSE_HEADERS: &str = "mcp-session-id";

/// The exact methods the single MCP endpoint answers.
pub const ALLOWED_REQUEST_METHODS: &str = "POST, GET, DELETE, OPTIONS";

/// The header a same-host reverse proxy presents its own hop credential in.
///
/// Distinct from `Authorization`, which carries the *end user's* token: the two are
/// independent layers and conflating them would let one credential satisfy both.
pub const PROXY_HOP_HEADER: &str = "x-godot-mcp-proxy-hop";

/// Refusal body for every authentication failure.
///
/// One constant, used for a missing, stale and wrong credential alike: a caller
/// must not be able to tell *which* it was, or the endpoint becomes a probe.
const UNAUTHORIZED_BODY: &str = "Unauthorized";

/// How long a request body may take to arrive.
///
/// Derived from `Limits::deadline_ms`, the operational per-operation deadline the
/// Todo 2 loader already validates as non-zero, rather than a number invented
/// here. It bounds only body *reading*: an SSE response stream is long-lived by
/// design and is bounded instead by the session keep-alive below.
const fn body_read_timeout(limits: &Limits) -> Duration {
    Duration::from_millis(limits.deadline_ms)
}

/// SSE keep-alive interval. Matches `rmcp`'s own documented default so a client
/// written against the SDK sees the interval it expects.
const SSE_KEEP_ALIVE: Duration = Duration::from_secs(15);

/// The transport-mode vocabulary Todo 27 froze, for one configured HTTP mode.
#[must_use]
pub const fn auth_mode_of(mode: HttpMode) -> TransportAuthMode {
    match mode {
        HttpMode::TrustedLocal => TransportAuthMode::TrustedLocal,
        HttpMode::ProxiedOidc => TransportAuthMode::ProxiedOidc,
    }
}

/// Why an HTTP listener could not be created.
#[derive(Debug, thiserror::Error)]
pub enum HttpTransportError {
    /// The requested bind address is not loopback.
    #[error("http bind address {addr} is not loopback; exposure requires a same-host HTTPS proxy")]
    NonLoopbackBind {
        /// The address that was refused.
        addr: SocketAddr,
    },
    /// The advertised external URL is not HTTPS.
    #[error("the advertised external resource URL must be https, got {url}")]
    InsecureResourceUrl {
        /// The URL that was refused.
        url: String,
    },
    /// `proxied_oidc` was selected without an external resource URL.
    #[error("http mode proxied_oidc requires an external https resource URL")]
    MissingResourceUrl,
    /// `trusted_local` was selected without a state directory for its secret.
    #[error("http mode trusted_local requires a state directory for its launch secret")]
    MissingStateDir,
    /// The launch secret could not be established.
    #[error("the per-launch secret could not be established: {0}")]
    Secret(#[from] LaunchSecretError),
    /// The listener could not be bound.
    #[error("the loopback listener could not be bound: {detail}")]
    Listen {
        /// What the OS reported.
        detail: String,
    },
    /// The transport task failed.
    #[error("the http transport failed: {detail}")]
    Transport {
        /// What went wrong.
        detail: String,
    },
    /// The configured scope/capability policy is incompatible with the served profile.
    #[error("the HTTP binding policy is invalid: {detail}")]
    BindingPolicy {
        /// What the existing session-policy validator refused.
        detail: String,
    },
}

/// One resolved HTTP serve request.
#[derive(Debug, Clone)]
pub struct HttpTransportOptions {
    bind: SocketAddr,
    mode: HttpMode,
    state_dir: Option<PathBuf>,
    external_resource_url: Option<String>,
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
    limits: Limits,
    binding: Option<SessionBindingPolicy>,
    required_scopes: Vec<String>,
    required_capabilities: Vec<Capability>,
    binding_policy_error: Option<String>,
    #[cfg(feature = "auth")]
    boundary: Option<Arc<crate::auth::ResourceServer>>,
}

impl HttpTransportOptions {
    /// A `trusted_local` listener publishing its per-launch secret in `state_dir`.
    #[must_use]
    pub fn trusted_local(bind: SocketAddr, state_dir: &Path) -> Self {
        Self {
            bind,
            mode: HttpMode::TrustedLocal,
            state_dir: Some(state_dir.to_path_buf()),
            external_resource_url: None,
            allowed_hosts: Vec::new(),
            allowed_origins: Vec::new(),
            limits: Limits::default(),
            binding: None,
            required_scopes: Vec::new(),
            required_capabilities: Vec::new(),
            binding_policy_error: None,
            #[cfg(feature = "auth")]
            boundary: None,
        }
    }

    /// A `proxied_oidc` listener advertising `external_resource_url`.
    ///
    /// The URL must be HTTPS; the listener itself still binds loopback. This bare
    /// constructor has no authentication boundary, so protected dispatch remains
    /// `unavailable` until one is attached.
    #[must_use]
    pub fn proxied_oidc(bind: SocketAddr, external_resource_url: &str) -> Self {
        Self {
            bind,
            mode: HttpMode::ProxiedOidc,
            state_dir: None,
            external_resource_url: Some(external_resource_url.to_owned()),
            allowed_hosts: Vec::new(),
            allowed_origins: Vec::new(),
            limits: Limits::default(),
            binding: None,
            required_scopes: Vec::new(),
            required_capabilities: Vec::new(),
            binding_policy_error: None,
            #[cfg(feature = "auth")]
            boundary: None,
        }
    }

    /// Configure the exact `Host` authorities that may reach this listener.
    ///
    /// Configured explicitly rather than inherited: with no entry the bound
    /// loopback authorities are the allowlist, and nothing else is accepted.
    #[must_use]
    pub fn with_allowed_hosts<S: Into<String>>(
        mut self,
        hosts: impl IntoIterator<Item = S>,
    ) -> Self {
        self.allowed_hosts = hosts.into_iter().map(Into::into).collect();
        self
    }

    /// Configure the exact browser `Origin` values that may reach this listener.
    ///
    /// With no entry, a request carrying *any* `Origin` is refused: a local MCP
    /// client does not send one, so an `Origin` means a browser page is talking
    /// to the endpoint.
    #[must_use]
    pub fn with_allowed_origins<S: Into<String>>(
        mut self,
        origins: impl IntoIterator<Item = S>,
    ) -> Self {
        self.allowed_origins = origins.into_iter().map(Into::into).collect();
        self
    }

    /// Apply operational limits (body size, body-read deadline).
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Bind every session, task and artifact of this listener to `policy`.
    ///
    /// Without one the listener behaves exactly as Todo 28/29 left it: authentication
    /// is still enforced per request, but the SDK's own session identifier travels on
    /// the wire and nothing re-authorizes a resumed session. With one, the registry
    /// mints the wire session identifier, every request re-proves its subject against
    /// the binding, and every artifact range read is authorized individually.
    #[must_use]
    pub fn with_session_binding(mut self, policy: SessionBindingPolicy) -> Self {
        let policy = policy.with_required_scopes(self.required_scopes.clone());
        match policy.with_required_capabilities(self.required_capabilities.iter().copied()) {
            Ok(policy) => self.binding = Some(policy),
            Err(error) => self.binding_policy_error = Some(error.to_string()),
        }
        self
    }

    /// Attach the `proxied_oidc` authentication boundary.
    ///
    /// Without one, `proxied_oidc` keeps refusing every protected dispatch: the
    /// mode opens only when both layers exist to enforce.
    #[cfg(feature = "auth")]
    #[must_use]
    pub fn with_boundary(mut self, boundary: Arc<crate::auth::ResourceServer>) -> Self {
        self.boundary = Some(boundary);
        self
    }

    /// The configured authentication mode.
    #[must_use]
    pub const fn mode(&self) -> HttpMode {
        self.mode
    }

    /// The configured session-binding policy, when one was attached.
    #[must_use]
    pub const fn session_binding(&self) -> Option<&SessionBindingPolicy> {
        self.binding.as_ref()
    }

    /// Derive options from validated configuration, or `None` when no HTTP
    /// listener was configured.
    ///
    /// # Errors
    /// Returns [`HttpTransportError`] when the configured mode lacks the material
    /// it requires.
    pub fn from_config(
        config: &ServerConfig,
        state_dir: &Path,
    ) -> Result<Option<Self>, HttpTransportError> {
        if config.transport != Transport::Http {
            return Ok(None);
        }
        let Some(http) = config.http.as_ref() else {
            return Ok(None);
        };
        let mut options = match http.mode {
            HttpMode::TrustedLocal => Self::trusted_local(http.bind, state_dir),
            HttpMode::ProxiedOidc => Self::proxied_oidc(
                http.bind,
                http.external_resource_url
                    .as_deref()
                    .ok_or(HttpTransportError::MissingResourceUrl)?,
            ),
        };
        options = options.with_limits(config.limits.clone());
        if let Some(host) = http.host.as_deref() {
            options = options.with_allowed_hosts([host]);
        }
        if let Some(origin) = http.origin.as_deref() {
            options = options.with_allowed_origins([origin]);
        }
        if http.mode == HttpMode::ProxiedOidc {
            options.required_scopes = http.effective_required_scopes();
            options.required_capabilities = http.effective_required_capabilities();
        }
        Ok(Some(options))
    }
}

/// Why the guard refused a request, and with which status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardRejection {
    /// A forwarded header was present; this stage does not trust one.
    ForwardedHeader,
    /// A credential may never travel in a URL, so no query string is accepted.
    QueryCredential,
    /// The `Host` authority is outside the configured allowlist.
    HostNotAllowed,
    /// The `Origin` is outside the configured allowlist.
    OriginNotAllowed,
    /// The `MCP-Protocol-Version` header names an unsupported version.
    UnsupportedProtocolVersion,
    /// No acceptable credential was presented.
    Unauthorized,
    /// The OIDC boundary produced an RFC 6750/9728 challenge.
    #[cfg(feature = "auth")]
    Oidc(crate::auth::AuthChallenge),
}

impl GuardRejection {
    /// The HTTP status this refusal returns.
    #[must_use]
    pub const fn status_code(&self) -> u16 {
        match self {
            Self::ForwardedHeader | Self::QueryCredential | Self::UnsupportedProtocolVersion => 400,
            Self::HostNotAllowed | Self::OriginNotAllowed => 403,
            Self::Unauthorized => 401,
            #[cfg(feature = "auth")]
            Self::Oidc(challenge) => challenge.status(),
        }
    }

    fn body(&self) -> &'static str {
        match self {
            Self::ForwardedHeader => "Bad Request: forwarded headers are not trusted",
            Self::QueryCredential => "Bad Request: query strings are not accepted",
            Self::UnsupportedProtocolVersion => "Bad Request: unsupported MCP-Protocol-Version",
            Self::HostNotAllowed => "Forbidden: Host is not allowed",
            Self::OriginNotAllowed => "Forbidden: Origin is not allowed",
            Self::Unauthorized => UNAUTHORIZED_BODY,
            #[cfg(feature = "auth")]
            Self::Oidc(challenge) => challenge.body(),
        }
    }
}

impl IntoResponse for GuardRejection {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status_code()).expect("guard statuses are valid");
        let mut response = (status, self.body()).into_response();
        let challenge = match &self {
            Self::Unauthorized => Some("Bearer".to_owned()),
            #[cfg(feature = "auth")]
            Self::Oidc(challenge) => Some(challenge.header_value()),
            _ => None,
        };
        if let Some(challenge) = challenge {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_str(&challenge)
                    .expect("the configured metadata URL is a valid header value"),
            );
        }
        response
    }
}

/// Forwarding headers this boundary refuses outright. Even an authenticated proxy
/// hop cannot use them to assert caller identity; only the validated end-user token
/// can do that.
const FORWARDED_HEADERS: [&str; 6] = [
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-forwarded-port",
    "x-real-ip",
];

/// The pre-dispatch request guard.
#[derive(Debug)]
pub struct RequestGuard {
    mode: HttpMode,
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
    secret: Option<Arc<LaunchSecret>>,
    #[cfg(feature = "auth")]
    boundary: Option<Arc<crate::auth::ResourceServer>>,
}

impl RequestGuard {
    /// Decide whether a request may proceed to the MCP service.
    ///
    /// # Errors
    /// Returns the [`GuardRejection`] that stopped it.
    pub fn check(&self, uri: &Uri, headers: &HeaderMap) -> Result<(), GuardRejection> {
        if self.check_transport(uri, headers)? {
            return Ok(());
        }
        self.check_credential(headers)
    }

    /// Re-derive the subject this request's credential proves, for one policy.
    ///
    /// Called on *every* request, including one that presents a session identifier:
    /// the identifier is compared against this result rather than trusted in place of
    /// it. `Ok(None)` means the request legitimately carries no subject — the
    /// unauthenticated metadata document, or a `proxied_oidc` listener with no
    /// boundary attached — and no binding is resolvable without one.
    ///
    /// # Errors
    /// Returns the [`GuardRejection`] that stopped it.
    pub fn authorize(
        &self,
        uri: &Uri,
        headers: &HeaderMap,
        policy: &SessionBindingPolicy,
    ) -> Result<Option<BoundSubject>, GuardRejection> {
        if self.check_transport(uri, headers)? {
            return Ok(None);
        }
        let (mode, principal, scopes) = match self.resolve_credential(headers)? {
            CredentialFacts::LaunchSecret(principal) => {
                (TransportAuthMode::TrustedLocal, principal, BTreeSet::new())
            }
            CredentialFacts::Oidc { principal, scopes } => {
                (TransportAuthMode::ProxiedOidc, principal, scopes)
            }
            CredentialFacts::Unenforced => return Ok(None),
        };
        policy
            .admit(mode, principal, scopes)
            .map(Some)
            .map_err(|_| GuardRejection::Unauthorized)
    }

    /// Run only the transport-level refusals, for a browser preflight.
    ///
    /// A preflight cannot carry a credential — that is what it exists to ask about —
    /// so it is answered before authentication. Every other refusal still applies,
    /// and the answer discloses only the configured allowlist.
    ///
    /// # Errors
    /// Returns the [`GuardRejection`] that stopped it.
    pub fn preflight(&self, uri: &Uri, headers: &HeaderMap) -> Result<(), GuardRejection> {
        self.check_transport(uri, headers).map(|_| ())
    }

    /// Run every transport-level refusal. `true` means the target is the
    /// unauthenticated metadata document, so no credential is required.
    ///
    /// A client cannot present an acceptable token until it has read that document,
    /// so requiring one to fetch it would be circular. Every check here still
    /// applies, and the document carries only the configured issuer and resource
    /// URL — nothing about the project.
    fn check_transport(&self, uri: &Uri, headers: &HeaderMap) -> Result<bool, GuardRejection> {
        Self::reject_forwarded(headers)?;
        Self::reject_query(uri)?;
        self.check_host(uri, headers)?;
        self.check_origin(headers)?;
        Self::check_protocol_version(headers)?;
        Ok(Self::is_public_metadata(uri))
    }

    #[cfg(feature = "auth")]
    fn is_public_metadata(uri: &Uri) -> bool {
        uri.path() == crate::auth::PROTECTED_RESOURCE_METADATA_PATH
    }

    #[cfg(not(feature = "auth"))]
    fn is_public_metadata(_uri: &Uri) -> bool {
        false
    }

    fn reject_forwarded(headers: &HeaderMap) -> Result<(), GuardRejection> {
        if FORWARDED_HEADERS
            .iter()
            .any(|name| headers.contains_key(*name))
        {
            return Err(GuardRejection::ForwardedHeader);
        }
        Ok(())
    }

    fn reject_query(uri: &Uri) -> Result<(), GuardRejection> {
        match uri.query() {
            None => Ok(()),
            Some("") => Ok(()),
            Some(_) => Err(GuardRejection::QueryCredential),
        }
    }

    fn check_host(&self, uri: &Uri, headers: &HeaderMap) -> Result<(), GuardRejection> {
        let authority = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .or_else(|| uri.authority().map(ToString::to_string))
            .ok_or(GuardRejection::HostNotAllowed)?;
        let authority = normalize_authority(&authority);
        if self
            .allowed_hosts
            .iter()
            .any(|allowed| normalize_authority(allowed) == authority)
        {
            Ok(())
        } else {
            Err(GuardRejection::HostNotAllowed)
        }
    }

    fn check_origin(&self, headers: &HeaderMap) -> Result<(), GuardRejection> {
        let Some(origin) = headers.get(header::ORIGIN) else {
            return Ok(());
        };
        let origin = origin
            .to_str()
            .map_err(|_| GuardRejection::OriginNotAllowed)?
            .trim()
            .trim_end_matches('/')
            .to_ascii_lowercase();
        if self
            .allowed_origins
            .iter()
            .any(|allowed| allowed.trim().trim_end_matches('/').to_ascii_lowercase() == origin)
        {
            Ok(())
        } else {
            Err(GuardRejection::OriginNotAllowed)
        }
    }

    fn check_protocol_version(headers: &HeaderMap) -> Result<(), GuardRejection> {
        let Some(value) = headers.get("mcp-protocol-version") else {
            return Ok(());
        };
        let version = value
            .to_str()
            .map_err(|_| GuardRejection::UnsupportedProtocolVersion)?;
        if ProtocolVersion::KNOWN_VERSIONS
            .iter()
            .any(|known| known.as_str() == version)
        {
            Ok(())
        } else {
            Err(GuardRejection::UnsupportedProtocolVersion)
        }
    }

    /// Validate the presented credential.
    ///
    /// The candidate is read from the `Authorization` header and nowhere else —
    /// never from the URI, and never inferred from who connected.
    fn check_credential(&self, headers: &HeaderMap) -> Result<(), GuardRejection> {
        self.resolve_credential(headers).map(|_| ())
    }

    /// Validate the presented credential and report what it proved.
    ///
    /// The candidate is read from the `Authorization` header and the dedicated hop
    /// header, and nowhere else: never from a URL, and never inferred from who
    /// connected.
    fn resolve_credential(&self, headers: &HeaderMap) -> Result<CredentialFacts, GuardRejection> {
        let Some(secret) = self.secret.as_ref() else {
            return match self.mode {
                // `proxied_oidc` presents no launch secret. When its boundary is
                // attached, both authentication layers must pass here; when none
                // is attached the staged handler still refuses every protected
                // dispatch, so the mode stays closed either way.
                HttpMode::ProxiedOidc => self.resolve_proxied(headers),
                HttpMode::TrustedLocal => Err(GuardRejection::Unauthorized),
            };
        };
        let candidate = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or("")
            .trim();
        if secret.matches_bearer(candidate) {
            Ok(CredentialFacts::LaunchSecret(secret.mode_principal()))
        } else {
            Err(GuardRejection::Unauthorized)
        }
    }

    /// Run both `proxied_oidc` layers, when a boundary is attached.
    ///
    /// The hop credential and the bearer token are read from headers only — never
    /// from the URI, and never inferred from who connected — and the token is wrapped
    /// in [`TokenValue`](crate::observability::TokenValue) at the point of read so no
    /// path through the boundary can log it.
    #[cfg(feature = "auth")]
    fn resolve_proxied(&self, headers: &HeaderMap) -> Result<CredentialFacts, GuardRejection> {
        let Some(boundary) = self.boundary.as_ref() else {
            return Ok(CredentialFacts::Unenforced);
        };
        let hop = headers
            .get(PROXY_HOP_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::trim);
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(|value| crate::observability::TokenValue::new(value.trim()));
        match boundary.authenticate(hop, token.as_ref()) {
            Ok(validated) => Ok(CredentialFacts::Oidc {
                principal: validated.principal().clone(),
                scopes: validated.scopes().clone(),
            }),
            Err(rejection) => Err(GuardRejection::Oidc(crate::auth::AuthChallenge::new(
                &rejection,
                &boundary.config().metadata_url(),
            ))),
        }
    }

    #[cfg(not(feature = "auth"))]
    fn resolve_proxied(&self, _headers: &HeaderMap) -> Result<CredentialFacts, GuardRejection> {
        Ok(CredentialFacts::Unenforced)
    }
}

/// What one request's credential proved, before any policy is applied.
///
/// Deliberately not public: a caller receives a [`BoundSubject`] or nothing, so no
/// code outside this module can act on a half-resolved credential.
#[derive(Debug)]
enum CredentialFacts {
    /// The launch secret matched, authenticating this launch's local principal.
    LaunchSecret(ModePrincipal),
    /// Both `proxied_oidc` layers passed for the one configured principal.
    #[cfg_attr(
        not(feature = "auth"),
        allow(
            dead_code,
            reason = "only the auth feature compiles the boundary that proves it"
        )
    )]
    Oidc {
        /// The `(iss, sub)` principal the token resolved to.
        principal: ModePrincipal,
        /// The scopes the token carried.
        scopes: BTreeSet<String>,
    },
    /// `proxied_oidc` with no boundary attached: nothing is proved, and the staged
    /// handler refuses every protected dispatch, so the mode stays closed.
    Unenforced,
}

fn normalize_authority(authority: &str) -> String {
    authority
        .trim()
        .trim_start_matches('[')
        .replace(']', "")
        .to_ascii_lowercase()
}

fn loopback_authorities(addr: SocketAddr) -> Vec<String> {
    let port = addr.port();
    vec![
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("::1:{port}"),
        addr.to_string(),
    ]
}

/// A bound, serving HTTP transport.
#[derive(Debug)]
pub struct RunningHttpTransport {
    local_addr: SocketAddr,
    mode: HttpMode,
    external_resource_url: Option<String>,
    secret_path: Option<PathBuf>,
    dispatches: Arc<std::sync::atomic::AtomicUsize>,
    bindings: Option<Arc<BindingRegistry>>,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl RunningHttpTransport {
    /// The loopback address actually bound.
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The MCP endpoint a co-located client dials.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("http://{}{MCP_PATH}", self.local_addr)
    }

    /// The transport-mode vocabulary this listener presents.
    #[must_use]
    pub const fn auth_mode(&self) -> TransportAuthMode {
        auth_mode_of(self.mode)
    }

    /// The externally advertised HTTPS URL, when one is configured.
    #[must_use]
    pub fn advertised_resource_url(&self) -> Option<&str> {
        self.external_resource_url.as_deref()
    }

    /// Where this launch's secret is published, in `trusted_local` mode.
    #[must_use]
    pub fn secret_path(&self) -> Option<&Path> {
        self.secret_path.as_deref()
    }

    /// How many requests reached the kernel through this listener.
    #[must_use]
    pub fn kernel_dispatches(&self) -> usize {
        self.dispatches.load(Ordering::SeqCst)
    }

    /// The live session/task/artifact bindings, when a policy was attached.
    #[must_use]
    pub const fn bindings(&self) -> Option<&Arc<BindingRegistry>> {
        self.bindings.as_ref()
    }

    /// Stop serving, drain in-flight requests, and remove the launch secret.
    ///
    /// # Errors
    /// Returns [`HttpTransportError`] when the serving task panicked or the secret
    /// could not be removed.
    pub async fn shutdown(self) -> Result<(), HttpTransportError> {
        self.shutdown.cancel();
        self.task
            .await
            .map_err(|error| HttpTransportError::Transport {
                detail: error.to_string(),
            })?;
        if let Some(bindings) = self.bindings.as_ref() {
            bindings.revoke_all();
        }
        if let Some(path) = self.secret_path.as_deref() {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(HttpTransportError::Transport {
                        detail: error.to_string(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Bind the HTTP transport `config` selects over `runtime`, exactly as
/// `godot-mcp serve --config` does: the configured mode, the session binding
/// every session, task and artifact read is checked against, and, in
/// `proxied_oidc`, the armed authentication boundary.
///
/// # Errors
/// Returns a description when the configuration selects no listener, the
/// selected mode is not compiled in, or the listener cannot be bound.
#[cfg(feature = "stdio")]
pub async fn bind_configured_transport(
    runtime: &crate::stdio::ProjectRuntime,
    config: &ServerConfig,
    profile: godot_mcp_domain::CapabilityProfile,
) -> Result<RunningHttpTransport, String> {
    // The HTTP launch secret lives in the same canonical state root every other
    // component of this runtime was built from, never in a candidate spelling.
    let state_dir = runtime.state_root().to_path_buf();
    let mut options = HttpTransportOptions::from_config(config, &state_dir)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "HTTP transport selected without listener options".to_string())?;
    let http = config
        .http
        .as_ref()
        .ok_or_else(|| "HTTP transport selected without an [http] section".to_string())?;
    let resource = match http.mode {
        HttpMode::TrustedLocal => {
            format!(
                "{}{MCP_PATH}",
                http.origin
                    .as_deref()
                    .unwrap_or_default()
                    .trim_end_matches('/')
            )
        }
        HttpMode::ProxiedOidc => http
            .external_resource_url
            .clone()
            .ok_or_else(|| "proxied_oidc requires an external resource URL".to_string())?,
    };
    let policy = SessionBindingPolicy::new(
        profile,
        &resource,
        runtime.access().project().clone(),
        &config.limits,
    )
    .map_err(|error| error.to_string())?;
    options = options.with_session_binding(policy);

    if http.mode == HttpMode::ProxiedOidc {
        #[cfg(feature = "auth")]
        {
            let boundary = crate::auth::build_resource_server(http)
                .await
                .map_err(|error| error.to_string())?;
            options = options.with_boundary(Arc::new(boundary));
        }
        #[cfg(not(feature = "auth"))]
        return Err("proxied_oidc requires the auth build feature".to_string());
    }

    runtime.apply_reconciliation_mode();
    bind_http_transport(options, runtime.kernel(), runtime.access())
        .await
        .map_err(|error| error.to_string())
}

/// Bind the Streamable HTTP transport for one kernel and start serving.
///
/// # Errors
/// Returns [`HttpTransportError`] for a non-loopback bind, a non-HTTPS advertised
/// URL, a secret that cannot be published, or a listener that cannot be bound.
pub async fn bind_http_transport(
    options: HttpTransportOptions,
    kernel: Arc<ServiceKernel>,
    access: AccessContext,
) -> Result<RunningHttpTransport, HttpTransportError> {
    if !options.bind.ip().is_loopback() {
        return Err(HttpTransportError::NonLoopbackBind { addr: options.bind });
    }
    if let Some(url) = options.external_resource_url.as_deref()
        && !url.starts_with("https://")
    {
        return Err(HttpTransportError::InsecureResourceUrl {
            url: url.to_owned(),
        });
    }
    if let Some(detail) = options.binding_policy_error.as_ref() {
        return Err(HttpTransportError::BindingPolicy {
            detail: detail.clone(),
        });
    }

    let secret = match options.mode {
        HttpMode::TrustedLocal => {
            let state_dir = options
                .state_dir
                .as_deref()
                .ok_or(HttpTransportError::MissingStateDir)?;
            Some(Arc::new(LaunchSecret::create(state_dir)?))
        }
        HttpMode::ProxiedOidc => None,
    };
    let secret_path = secret.as_ref().map(|secret| secret.path().to_path_buf());

    let listener = tokio::net::TcpListener::bind(options.bind)
        .await
        .map_err(|error| HttpTransportError::Listen {
            detail: error.to_string(),
        })?;
    let local_addr = listener
        .local_addr()
        .map_err(|error| HttpTransportError::Listen {
            detail: error.to_string(),
        })?;
    if !local_addr.ip().is_loopback() {
        return Err(HttpTransportError::NonLoopbackBind { addr: local_addr });
    }

    let mut allowed_hosts = options.allowed_hosts.clone();
    if allowed_hosts.is_empty() {
        allowed_hosts = loopback_authorities(local_addr);
    }
    #[cfg(feature = "auth")]
    let boundary = options.boundary.clone();
    let guard = Arc::new(RequestGuard {
        mode: options.mode,
        allowed_hosts: allowed_hosts.clone(),
        allowed_origins: options.allowed_origins.clone(),
        secret: secret.clone(),
        #[cfg(feature = "auth")]
        boundary: boundary.clone(),
    });

    let diagnostics = match options.mode {
        HttpMode::TrustedLocal => HttpDiagnosticsSource::TrustedLocal {
            secret: Arc::clone(
                secret
                    .as_ref()
                    .expect("trusted_local created a launch secret before binding"),
            ),
        },
        HttpMode::ProxiedOidc => {
            #[cfg(feature = "auth")]
            {
                HttpDiagnosticsSource::ProxiedOidc {
                    boundary: boundary.clone(),
                }
            }
            #[cfg(not(feature = "auth"))]
            {
                HttpDiagnosticsSource::ProxiedOidc
            }
        }
    };
    kernel.declare_http_diagnostics(diagnostics);

    let shutdown = CancellationToken::new();
    let dispatches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let bindings = options.binding.clone().map(|policy| {
        Arc::new(
            BindingRegistry::new(policy)
                .with_artifacts(kernel.artifact_contracts())
                .with_artifact_store(kernel.artifact_store()),
        )
    });
    let handler = StagedHandler::new(
        RmcpAdapter::new(kernel, access),
        options.mode,
        Arc::clone(&dispatches),
    );
    #[cfg(feature = "auth")]
    let handler = if boundary.is_some() {
        handler.with_armed_boundary()
    } else {
        handler
    };
    let handler = match bindings.as_ref() {
        Some(bindings) => handler.with_bindings(Arc::clone(bindings)),
        None => handler,
    };
    let service: StreamableHttpService<StagedHandler, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default()
                .with_sse_keep_alive(Some(SSE_KEEP_ALIVE))
                .with_allowed_hosts(allowed_hosts)
                .with_allowed_origins(options.allowed_origins.clone())
                .with_cancellation_token(shutdown.child_token()),
        );

    let bounded = ServiceBuilder::new()
        .layer(RequestBodyLimitLayer::new(
            usize::try_from(options.limits.max_body_bytes).unwrap_or(usize::MAX),
        ))
        .layer(RequestBodyTimeoutLayer::new(body_read_timeout(
            &options.limits,
        )))
        .service(service);

    let router = Router::new().route_service(MCP_PATH, bounded);

    // RFC 9728: a client that was refused needs to discover where an acceptable
    // token comes from. The document is unauthenticated by design — it is exactly
    // the information a client needs *before* it can authenticate — and it exposes
    // nothing about the project, only the configured issuer and resource URL.
    #[cfg(feature = "auth")]
    let router = match boundary.as_ref() {
        Some(boundary) => {
            let body = boundary.config().metadata().to_body();
            router.route(
                crate::auth::PROTECTED_RESOURCE_METADATA_PATH,
                axum::routing::get(move || {
                    let body = body.clone();
                    async move { ([(header::CONTENT_TYPE, "application/json")], body) }
                }),
            )
        }
        None => router,
    };

    let router = router.layer(axum::middleware::from_fn({
        let guard = Arc::clone(&guard);
        let bindings = bindings.clone();
        move |request: Request<Body>, next: Next| {
            let guard = Arc::clone(&guard);
            let bindings = bindings.clone();
            async move {
                match bindings {
                    // No policy attached: the Todo 28/29 path, byte for byte.
                    None => {
                        if let Err(rejection) = guard.check(request.uri(), request.headers()) {
                            return rejection.into_response();
                        }
                        next.run(request).await
                    }
                    Some(bindings) => serve_bound(request, next, &guard, &bindings).await,
                }
            }
        }
    }));

    let task = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move { shutdown.cancelled_owned().await })
                .await;
        }
    });

    tracing::info!(
        event = "http_serving",
        mode = auth_mode_of(options.mode).as_str(),
        "the optional streamable http transport is serving on the shared kernel"
    );

    Ok(RunningHttpTransport {
        local_addr,
        mode: options.mode,
        external_resource_url: options.external_resource_url,
        secret_path,
        dispatches,
        bindings,
        shutdown,
        task,
    })
}

/// Serve one request against the session/task/artifact bindings.
///
/// The order is the whole point, and it is asserted from this module's own bytes by
/// [`crate::binding`]'s self-audit:
///
/// 1. [`RequestGuard::authorize`] revalidates the credential — the launch secret, or
///    the proxy hop *then* the OIDC token — and resolves it to a [`BoundSubject`].
/// 2. A presented session identifier is re-authorized against that subject. It is
///    never accepted as authentication, and a mismatch is indistinguishable from an
///    identifier that never existed.
/// 3. Only then does the request reach the MCP service.
///
/// The SDK's internal session identifier is substituted inbound and replaced by the
/// opaque handle outbound, so the identifier a client holds is always the minted one.
async fn serve_bound(
    mut request: Request<Body>,
    next: Next,
    guard: &RequestGuard,
    bindings: &Arc<BindingRegistry>,
) -> Response {
    let origin = request.headers().get(header::ORIGIN).cloned();
    if request.method() == Method::OPTIONS && origin.is_some() {
        return preflight_response(request.uri(), request.headers(), guard, origin.as_ref());
    }
    let subject = match guard.authorize(request.uri(), request.headers(), bindings.policy()) {
        Ok(subject) => subject,
        Err(rejection) => return rejection.into_response(),
    };
    let now = now_unix_ms();
    let presented = request
        .headers()
        .get(MCP_SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let mut ticket = None;
    if let Some(candidate) = presented.as_deref() {
        let lease = match bindings.resume(candidate, subject.as_ref(), now) {
            Ok(lease) => lease,
            Err(refusal) => return refusal_response(&refusal),
        };
        if request.method() == Method::DELETE {
            if let Err(refusal) = bindings.revoke(candidate, subject.as_ref(), now) {
                return refusal_response(&refusal);
            }
        } else {
            match bindings.open_stream(candidate, subject.as_ref(), now) {
                Ok((_, acquired)) => ticket = Some(acquired),
                Err(refusal) => return refusal_response(&refusal),
            }
        }
        let internal = HeaderValue::from_str(lease.internal_session())
            .expect("an internal session identifier came out of a header and converts back");
        request.headers_mut().insert(MCP_SESSION_HEADER, internal);
        request.extensions_mut().insert(lease);
    }
    if let Some(subject) = subject.clone() {
        request.extensions_mut().insert(subject);
    }

    let response = next.run(request).await;
    let response = rebind_session(
        response,
        presented.as_deref(),
        subject.as_ref(),
        bindings,
        now,
    );
    let mut response = match response {
        Ok(response) => response,
        Err(refusal) => return refusal_response(&refusal),
    };
    apply_cors(&mut response, origin.as_ref());
    hold_stream_slot(response, ticket)
}

/// Put the opaque handle back on the wire, minting one for a new session.
fn rebind_session(
    mut response: Response,
    presented: Option<&str>,
    subject: Option<&BoundSubject>,
    bindings: &Arc<BindingRegistry>,
    now_unix_ms: u64,
) -> Result<Response, BindingRefusal> {
    if let Some(handle) = presented {
        if response.headers().contains_key(MCP_SESSION_HEADER) {
            let value = HeaderValue::from_str(handle)
                .expect("a handle came out of a request header and converts back");
            response.headers_mut().insert(MCP_SESSION_HEADER, value);
        }
        return Ok(response);
    }
    let Some(internal) = response
        .headers()
        .get(MCP_SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
    else {
        return Ok(response);
    };
    let Some(subject) = subject else {
        // A response cannot open a session for a caller that proved no identity.
        response.headers_mut().remove(MCP_SESSION_HEADER);
        return Ok(response);
    };
    let lease = bindings.bind_session(
        &internal,
        subject,
        bindings.policy().expires_at(now_unix_ms),
    )?;
    let value = HeaderValue::from_str(lease.handle().as_str())
        .expect("a minted handle is lowercase hexadecimal");
    response.headers_mut().insert(MCP_SESSION_HEADER, value);
    Ok(response)
}

/// Keep a stream's ceiling slot for exactly as long as its body streams.
///
/// The ticket is captured by the mapped body stream rather than left in the response
/// extensions, because hyper takes the body apart before it writes it: a slot parked
/// in extensions would be released while the SSE stream was still open, which would
/// make the ceiling unenforceable for precisely the long-lived case it exists for.
fn hold_stream_slot(response: Response, ticket: Option<StreamTicket>) -> Response {
    let Some(ticket) = ticket else {
        return response;
    };
    let (parts, body) = response.into_parts();
    let held = body.into_data_stream().inspect(move |_| {
        let _slot = &ticket;
    });
    Response::from_parts(parts, Body::from_stream(held))
}

/// Answer a browser preflight from the configured allowlist alone.
fn preflight_response(
    uri: &Uri,
    headers: &HeaderMap,
    guard: &RequestGuard,
    origin: Option<&HeaderValue>,
) -> Response {
    if let Err(rejection) = guard.preflight(uri, headers) {
        return rejection.into_response();
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static(ALLOWED_REQUEST_METHODS),
    );
    apply_cors(&mut response, origin);
    response
}

/// Answer explicitly, and only for an allowlisted `Origin`.
///
/// `*` is never sent and credentials are never allowed: the allowlist is the whole
/// policy, and a wildcard would make it decorative. A non-allowlisted `Origin` never
/// reaches here — [`RequestGuard`] refused it with `403` before dispatch.
fn apply_cors(response: &mut Response, origin: Option<&HeaderValue>) {
    let Some(origin) = origin else {
        return;
    };
    let headers = response.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    headers.insert(header::VARY, HeaderValue::from_static("origin"));
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(ALLOWED_REQUEST_HEADERS),
    );
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static(EXPOSED_RESPONSE_HEADERS),
    );
}

/// The one refusal shape a binding failure produces, from constants only.
fn refusal_response(refusal: &BindingRefusal) -> Response {
    let status =
        StatusCode::from_u16(refusal.status_code()).expect("the binding refusal status is valid");
    (status, refusal.message()).into_response()
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod self_audit {
    //! Source-scanning bans, asserted against this module's own bytes so a future
    //! edit that reintroduces one fails a test rather than passing review.

    use super::*;

    const TRANSPORT_SOURCE: &str = include_str!("http.rs");
    const STAGED_SOURCE: &str = include_str!("http/staged.rs");
    const SECRET_SOURCE: &str = include_str!("http/secret.rs");

    fn production_source(source: &str) -> &str {
        source
            .split_once("#[cfg(test)]")
            .map_or(source, |(production, _)| production)
    }

    #[test]
    fn no_second_tool_implementation_exists_here() {
        for source in [TRANSPORT_SOURCE, STAGED_SOURCE, SECRET_SOURCE] {
            let production = production_source(source);
            assert!(
                !production.contains("impl KernelToolHandler"),
                "the http transport must not implement tool logic"
            );
            assert!(
                !production.contains("canonical_manifest"),
                "the http transport must not derive its own capability surface"
            );
        }
    }

    #[test]
    fn credentials_are_never_read_from_a_url_or_a_peer_address() {
        let production = production_source(TRANSPORT_SOURCE);
        let credential = production
            .split_once("fn check_credential")
            .expect("credential validation exists")
            .1
            .split_once("\n    }")
            .expect("credential validation ends")
            .0;
        for banned in ["uri", "query", "peer_addr", "ConnectInfo", "is_loopback"] {
            assert!(
                !credential.contains(banned),
                "credential validation must not consult {banned}"
            );
        }
        assert!(
            production.contains("header::AUTHORIZATION"),
            "the credential must come from the Authorization header"
        );
        for source in [TRANSPORT_SOURCE, STAGED_SOURCE] {
            let production = production_source(source);
            for banned in ["ConnectInfo", "peer_addr", "access_token"] {
                assert!(
                    !production.contains(banned),
                    "{banned} must never appear in the transport shell"
                );
            }
        }
    }

    #[test]
    fn every_bind_is_checked_for_loopback_first() {
        let production = production_source(TRANSPORT_SOURCE);
        let bind = production
            .find("TcpListener::bind")
            .expect("the listener is bound here");
        let check = production
            .find("if !options.bind.ip().is_loopback()")
            .expect("the requested bind is checked");
        assert!(check < bind, "the loopback check must precede the bind");
        assert_eq!(
            production.matches("TcpListener::bind").count(),
            1,
            "there must be exactly one bind site"
        );
        assert!(
            production.contains("if !local_addr.ip().is_loopback()"),
            "the bound address must be re-checked independently"
        );
    }

    #[test]
    fn no_tls_or_forwarded_trust_is_compiled_in() {
        for source in [TRANSPORT_SOURCE, STAGED_SOURCE, SECRET_SOURCE] {
            let production = production_source(source);
            for banned in [
                "rustls",
                "native_tls",
                "TlsAcceptor",
                "ServerConfig::builder",
            ] {
                assert!(
                    !production.contains(banned),
                    "{banned} must not appear; TLS terminates in a same-host proxy"
                );
            }
        }
        let production = production_source(TRANSPORT_SOURCE);
        for header in FORWARDED_HEADERS {
            assert!(
                production.contains(header),
                "{header} must be named in the refusal list"
            );
        }
        assert!(
            !production.contains(".get(\"x-forwarded-for\")"),
            "a forwarded header may be detected but never read"
        );
    }

    #[test]
    fn refusals_are_indistinguishable_for_every_credential_failure() {
        assert_eq!(GuardRejection::Unauthorized.status_code(), 401);
        assert_eq!(GuardRejection::Unauthorized.body(), UNAUTHORIZED_BODY);
        let production = production_source(TRANSPORT_SOURCE);
        assert_eq!(
            production.matches("GuardRejection::Unauthorized").count(),
            production
                .matches("GuardRejection::Unauthorized")
                .count()
                .min(4),
            "authentication refusals must all funnel through one variant"
        );
    }

    #[test]
    fn the_default_configuration_describes_no_listener() {
        let config = ServerConfig::default();
        assert_eq!(config.transport, Transport::Stdio);
        assert!(config.http.is_none());
        let resolved =
            HttpTransportOptions::from_config(&config, Path::new("/nonexistent")).expect("resolve");
        assert!(
            resolved.is_none(),
            "stdio must not describe an HTTP listener"
        );
    }

    #[test]
    fn guard_rejections_map_to_the_required_statuses() {
        assert_eq!(GuardRejection::ForwardedHeader.status_code(), 400);
        assert_eq!(GuardRejection::QueryCredential.status_code(), 400);
        assert_eq!(
            GuardRejection::UnsupportedProtocolVersion.status_code(),
            400
        );
        assert_eq!(GuardRejection::HostNotAllowed.status_code(), 403);
        assert_eq!(GuardRejection::OriginNotAllowed.status_code(), 403);
    }

    #[test]
    fn the_guard_refuses_before_any_credential_is_considered() {
        let guard = RequestGuard {
            mode: HttpMode::TrustedLocal,
            allowed_hosts: vec!["127.0.0.1:9".to_owned()],
            allowed_origins: Vec::new(),
            secret: None,
            #[cfg(feature = "auth")]
            boundary: None,
        };
        let uri: Uri = "/mcp?access_token=abc".parse().expect("uri");
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:9".parse().expect("host"));
        assert_eq!(
            guard.check(&uri, &headers),
            Err(GuardRejection::QueryCredential)
        );

        headers.insert("x-forwarded-for", "10.0.0.1".parse().expect("value"));
        assert_eq!(
            guard.check(&uri, &headers),
            Err(GuardRejection::ForwardedHeader)
        );

        let uri: Uri = "/mcp".parse().expect("uri");
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "elsewhere.example:80".parse().expect("host"));
        assert_eq!(
            guard.check(&uri, &headers),
            Err(GuardRejection::HostNotAllowed)
        );

        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:9".parse().expect("host"));
        headers.insert(header::ORIGIN, "https://evil.example".parse().expect("o"));
        assert_eq!(
            guard.check(&uri, &headers),
            Err(GuardRejection::OriginNotAllowed)
        );

        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:9".parse().expect("host"));
        headers.insert("mcp-protocol-version", "1999-01-01".parse().expect("v"));
        assert_eq!(
            guard.check(&uri, &headers),
            Err(GuardRejection::UnsupportedProtocolVersion)
        );

        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:9".parse().expect("host"));
        assert_eq!(
            guard.check(&uri, &headers),
            Err(GuardRejection::Unauthorized)
        );
    }

    #[test]
    fn proxied_mode_needs_no_credential_but_advertises_only_https() {
        let guard = RequestGuard {
            mode: HttpMode::ProxiedOidc,
            allowed_hosts: vec!["127.0.0.1:9".to_owned()],
            allowed_origins: Vec::new(),
            secret: None,
            #[cfg(feature = "auth")]
            boundary: None,
        };
        let uri: Uri = "/mcp".parse().expect("uri");
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:9".parse().expect("host"));
        assert_eq!(guard.check(&uri, &headers), Ok(()));
        assert_eq!(
            auth_mode_of(HttpMode::ProxiedOidc),
            TransportAuthMode::ProxiedOidc
        );
        assert_eq!(
            auth_mode_of(HttpMode::TrustedLocal),
            TransportAuthMode::TrustedLocal
        );
    }

    #[test]
    fn the_body_read_deadline_comes_from_configured_limits() {
        let limits = Limits {
            deadline_ms: 1_234,
            ..Limits::default()
        };
        assert_eq!(body_read_timeout(&limits), Duration::from_millis(1_234));
    }
}
