//! Strict, layered startup configuration for `godot-mcp`.
//!
//! The loader applies built-in defaults, strict TOML, recognized environment
//! variables, and typed CLI overrides in that order. Secret material is never
//! accepted inline: configuration stores only environment/file indirections.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
#[cfg(not(any(unix, windows)))]
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
#[cfg(any(unix, windows))]
use std::path::Path;

use serde::Deserialize;

use crate::contracts::Capability;
use crate::{CapabilityProfile, ErrorKind};

const REDACTED: &str = "[REDACTED]";
const ENV_PREFIX: &str = "GODOT_MCP_";

/// Resolved secret material. `Debug` and `Display` are always redacted.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    /// Wrap raw secret material.
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// Explicitly expose secret material to an authentication consumer.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the secret contains no material.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

/// A permitted secret indirection. Inline secret strings cannot deserialize as
/// this externally tagged enum.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SecretRef {
    /// Resolve the secret from an environment variable at the point of use.
    Env { var: String },
    /// Resolve the secret from a protected regular file at the point of use.
    File { path: String },
}

impl SecretRef {
    /// Resolve this indirection without placing secret material in diagnostics.
    ///
    /// # Errors
    /// Returns a sanitized error for a missing/empty environment value or an
    /// unsafe/unreadable secret file.
    pub fn resolve(&self, env: &BTreeMap<String, String>) -> Result<Secret, ConfigError> {
        match self {
            Self::Env { var } => {
                require_nonempty("local_secret.env.var", var)?;
                let raw = env.get(var).ok_or_else(|| ConfigError::SecretResolution {
                    location: format!("environment variable {var}"),
                    reason: "is not set".to_string(),
                })?;
                if raw.trim().is_empty() {
                    return Err(ConfigError::SecretResolution {
                        location: format!("environment variable {var}"),
                        reason: "is empty".to_string(),
                    });
                }
                Ok(Secret::new(raw.clone()))
            }
            Self::File { path } => resolve_secret_file(path),
        }
    }
}

fn resolve_secret_file(path: &str) -> Result<Secret, ConfigError> {
    require_nonempty("local_secret.file.path", path)?;
    let raw = read_secret_file(path)?;
    let trimmed = raw.trim_end_matches(['\r', '\n']);
    if trimmed.trim().is_empty() {
        return Err(ConfigError::SecretResolution {
            location: format!("secret file {path}"),
            reason: "is empty".to_string(),
        });
    }
    Ok(Secret::new(trimmed))
}

#[cfg(any(unix, windows))]
fn read_secret_file(path: &str) -> Result<String, ConfigError> {
    let file = crate::sysfs::open_private_regular_file(Path::new(path)).map_err(|error| {
        ConfigError::SecretResolution {
            location: format!("secret file {path}"),
            reason: error.to_string(),
        }
    })?;
    let raw = crate::sysfs::read_file(&file).map_err(|_| ConfigError::SecretResolution {
        location: format!("secret file {path}"),
        reason: "cannot be read".to_string(),
    })?;
    String::from_utf8(raw).map_err(|_| ConfigError::SecretResolution {
        location: format!("secret file {path}"),
        reason: "is not valid UTF-8".to_string(),
    })
}

#[cfg(not(any(unix, windows)))]
fn read_secret_file(path: &str) -> Result<String, ConfigError> {
    let mut file = std::fs::File::open(path).map_err(|_| ConfigError::SecretResolution {
        location: format!("secret file {path}"),
        reason: "cannot be opened".to_string(),
    })?;
    let metadata = file.metadata().map_err(|_| ConfigError::SecretResolution {
        location: format!("secret file {path}"),
        reason: "cannot be inspected".to_string(),
    })?;
    if !metadata.is_file() {
        return Err(ConfigError::SecretResolution {
            location: format!("secret file {path}"),
            reason: "is not a regular file".to_string(),
        });
    }
    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .map_err(|_| ConfigError::SecretResolution {
            location: format!("secret file {path}"),
            reason: "cannot be read".to_string(),
        })?;
    Ok(raw)
}

/// Transport selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    /// Newline-delimited JSON-RPC over stdio.
    #[default]
    Stdio,
    /// Streamable HTTP on a loopback socket.
    Http,
}

/// HTTP authentication mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpMode {
    /// User-scoped per-launch secret.
    TrustedLocal,
    /// Authenticated proxy hop plus one frozen OIDC principal.
    ProxiedOidc,
}

/// An asymmetric OIDC signing algorithm selectable by deployment configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
pub enum OidcSigningAlgorithm {
    #[serde(rename = "RS256")]
    Rs256,
    #[serde(rename = "PS256")]
    Ps256,
    #[serde(rename = "ES256")]
    Es256,
}

impl OidcSigningAlgorithm {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rs256 => "RS256",
            Self::Ps256 => "PS256",
            Self::Es256 => "ES256",
        }
    }
}

const DEFAULT_OIDC_ALGORITHM: OidcSigningAlgorithm = OidcSigningAlgorithm::Rs256;
const DEFAULT_OIDC_SCOPE: &str = "godot.read";
const DEFAULT_OIDC_CAPABILITY: Capability = Capability::Read;

/// A frozen OIDC issuer/subject principal.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcPrincipal {
    /// Expected issuer URL.
    pub issuer: String,
    /// Expected subject.
    pub subject: String,
}

/// Authenticated same-host proxy-hop identity.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProxyHopCredential {
    /// Private Unix socket or named-pipe path.
    SocketPath { path: String },
    /// Protected credential-file path.
    CredentialFile { path: String },
}

/// HTTP transport configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    /// Loopback listener address.
    pub bind: SocketAddr,
    /// Authentication mode.
    pub mode: HttpMode,
    /// Exact expected Host authority.
    #[serde(default)]
    pub host: Option<String>,
    /// Exact expected HTTP(S) Origin.
    #[serde(default)]
    pub origin: Option<String>,
    /// Trusted-local secret indirection. Required under
    /// [`HttpMode::TrustedLocal`] and forbidden under
    /// [`HttpMode::ProxiedOidc`], but **not resolved by the production `serve`
    /// path**: that path authenticates against a secret the server itself
    /// publishes per launch. Its only runtime consequence is at load time, where
    /// the [`SecretRef::Env`] form's variable name is exempted from the
    /// unknown-`GODOT_MCP_`-variable rejection. See
    /// `docs/operator/configuration.md`.
    #[serde(default)]
    pub local_secret: Option<SecretRef>,
    /// Proxied-OIDC proxy-hop identity.
    #[serde(default)]
    pub proxy_hop: Option<ProxyHopCredential>,
    /// Proxied-OIDC frozen principal.
    #[serde(default)]
    pub principal: Option<OidcPrincipal>,
    /// Externally visible HTTPS MCP resource URL.
    #[serde(default)]
    pub external_resource_url: Option<String>,
    #[serde(default)]
    pub allowed_algorithms: Option<Vec<OidcSigningAlgorithm>>,
    #[serde(default)]
    pub required_scopes: Option<Vec<String>>,
    #[serde(default)]
    pub required_capabilities: Option<Vec<Capability>>,
}

impl HttpConfig {
    /// Effective algorithm policy. Omission narrows to RS256 rather than widening.
    #[must_use]
    pub fn effective_allowed_algorithms(&self) -> Vec<OidcSigningAlgorithm> {
        self.allowed_algorithms
            .clone()
            .unwrap_or_else(|| vec![DEFAULT_OIDC_ALGORITHM])
    }

    /// Effective scope policy. Omission still requires the fixture's read scope.
    #[must_use]
    pub fn effective_required_scopes(&self) -> Vec<String> {
        self.required_scopes
            .clone()
            .unwrap_or_else(|| vec![DEFAULT_OIDC_SCOPE.to_owned()])
    }

    /// Effective capability policy. Omission still requires read capability.
    #[must_use]
    pub fn effective_required_capabilities(&self) -> Vec<Capability> {
        self.required_capabilities
            .clone()
            .unwrap_or_else(|| vec![DEFAULT_OIDC_CAPABILITY])
    }
}

fn default_bridge_endpoint() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

/// Editor/LSP/DAP bridge configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeConfig {
    /// Loopback-only bridge endpoint.
    #[serde(default = "default_bridge_endpoint")]
    pub endpoint: SocketAddr,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            endpoint: default_bridge_endpoint(),
        }
    }
}

/// Which containment boundary every supervised Godot launch is created in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessBoundary {
    /// The trusted-Unix locator: a documented weak boundary, for a trusted
    /// project only.
    Weak,
    /// A cgroup v2 sub-tree delegated to the cgroup this server runs in (Linux).
    Cgroup,
    /// The configured container or virtual-machine adapter.
    Adapter,
    /// A Windows Job Object with no breakaway and kill-on-close.
    JobObject,
}

/// Whether the project's code is trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessTrust {
    /// The operator trusts the project; a weak boundary may contain it.
    #[default]
    Trusted,
    /// Anything else: only a strong boundary may contain it.
    Untrusted,
}

/// The external container or virtual-machine adapter program.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessAdapter {
    /// Absolute path of the adapter executable.
    pub program: String,
    /// Absolute boundary root the adapter keeps its boundaries under.
    pub root: String,
}

/// Process containment.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessConfig {
    /// The boundary every launch uses; the host's own when unset.
    #[serde(default)]
    pub boundary: Option<ProcessBoundary>,
    /// Whether the project's code is trusted.
    #[serde(default)]
    pub trust: ProcessTrust,
    /// The adapter, exactly when `boundary` is `adapter`.
    #[serde(default)]
    pub adapter: Option<ProcessAdapter>,
}

/// Diagnostics destination.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum LogSink {
    /// Diagnostics on stderr.
    #[default]
    Stderr,
    /// Diagnostics in a rolling file.
    File { path: String },
}

const fn default_deadline_ms() -> u64 {
    30_000
}
const fn default_max_output_bytes() -> u64 {
    8 * 1024 * 1024
}
const fn default_max_body_bytes() -> u64 {
    1024 * 1024
}
const fn default_max_queue() -> u32 {
    64
}

/// Operational resource limits.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Per-operation wall-clock deadline.
    #[serde(default = "default_deadline_ms")]
    pub deadline_ms: u64,
    /// Maximum captured output bytes.
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: u64,
    /// Maximum accepted request-body bytes.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: u64,
    /// Maximum queued effect operations.
    #[serde(default = "default_max_queue")]
    pub max_queue: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            deadline_ms: default_deadline_ms(),
            max_output_bytes: default_max_output_bytes(),
            max_body_bytes: default_max_body_bytes(),
            max_queue: default_max_queue(),
        }
    }
}

/// Complete server startup configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Candidate project root; secure ownership is established separately.
    #[serde(default)]
    pub project_root: Option<String>,
    /// Godot executable path.
    #[serde(default)]
    pub godot_binary: Option<String>,
    /// Explicit capability profile.
    #[serde(default)]
    pub profile: Option<CapabilityProfile>,
    /// Active transport.
    #[serde(default)]
    pub transport: Transport,
    /// HTTP configuration, forbidden under stdio.
    #[serde(default)]
    pub http: Option<HttpConfig>,
    /// Local editor bridge.
    #[serde(default)]
    pub bridge: BridgeConfig,
    /// Diagnostics destination.
    #[serde(default)]
    pub log_sink: LogSink,
    /// Whether the project is disposable.
    #[serde(default)]
    pub disposable: bool,
    /// Process containment.
    #[serde(default)]
    pub process: ProcessConfig,
    /// Resource limits.
    #[serde(default)]
    pub limits: Limits,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            project_root: None,
            godot_binary: None,
            profile: None,
            transport: Transport::Stdio,
            http: None,
            bridge: BridgeConfig::default(),
            log_sink: LogSink::default(),
            disposable: false,
            process: ProcessConfig::default(),
            limits: Limits::default(),
        }
    }
}

/// Typed CLI layer. Every configuration leaf can be overridden explicitly.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub project_root: Option<String>,
    pub godot_binary: Option<String>,
    pub profile: Option<CapabilityProfile>,
    pub transport: Option<Transport>,
    pub http_bind: Option<SocketAddr>,
    pub http_mode: Option<HttpMode>,
    pub http_host: Option<String>,
    pub http_origin: Option<String>,
    pub http_local_secret: Option<SecretRef>,
    pub http_proxy_hop: Option<ProxyHopCredential>,
    pub http_principal: Option<OidcPrincipal>,
    pub http_external_resource_url: Option<String>,
    pub http_allowed_algorithms: Option<Vec<OidcSigningAlgorithm>>,
    pub http_required_scopes: Option<Vec<String>>,
    pub http_required_capabilities: Option<Vec<Capability>>,
    pub bridge_endpoint: Option<SocketAddr>,
    pub log_sink: Option<LogSink>,
    pub disposable: Option<bool>,
    pub process_boundary: Option<ProcessBoundary>,
    pub process_trust: Option<ProcessTrust>,
    pub process_adapter: Option<ProcessAdapter>,
    pub deadline_ms: Option<u64>,
    pub max_output_bytes: Option<u64>,
    pub max_body_bytes: Option<u64>,
    pub max_queue: Option<u32>,
}

/// A sanitized configuration failure.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("configuration parse/shape error: {0}")]
    Parse(String),
    #[error("unknown godot-mcp environment variable: {name}")]
    UnknownEnvVar { name: String },
    #[error("invalid value for environment variable {name}: {reason}")]
    InvalidEnvValue { name: String, reason: String },
    #[error("configuration field {field} must not be empty")]
    EmptyValue { field: String },
    #[error("http override is incomplete; missing: {missing:?}")]
    IncompleteHttpOverride { missing: Vec<&'static str> },
    #[error("http transport selected but no [http] section was provided")]
    MissingHttpSection,
    #[error("[http] is forbidden when the final transport is stdio")]
    HttpSectionForbidden,
    #[error("http Host is required")]
    MissingHost,
    #[error("http Origin is required")]
    MissingOrigin,
    #[error("invalid http Host authority")]
    InvalidHost { reason: &'static str },
    #[error("invalid http Origin")]
    InvalidOrigin { reason: &'static str },
    #[error("http bind address {addr} is not loopback")]
    NonLoopbackBind { addr: SocketAddr },
    #[error("http mode trusted_local requires local_secret indirection")]
    MissingLocalSecret,
    #[error("http mode proxied_oidc requires an authenticated proxy_hop credential")]
    MissingProxyHop,
    #[error("http mode proxied_oidc requires exactly one issuer/subject principal")]
    MissingPrincipal,
    #[error("http mode proxied_oidc requires an external HTTPS resource URL")]
    MissingExternalResourceUrl,
    #[error("field {field} is forbidden for http mode {mode:?}")]
    ForbiddenFieldForMode { field: String, mode: HttpMode },
    #[error("invalid OIDC issuer URL")]
    InvalidIssuer { reason: &'static str },
    #[error("invalid external resource URL")]
    InvalidResourceUrl { reason: &'static str },
    #[error("OIDC policy field {field} must contain at least one value")]
    EmptyOidcPolicy { field: &'static str },
    #[error("OIDC scope {scope:?} is not an RFC 6749 scope-token")]
    InvalidOidcScope { scope: String },
    #[error("OIDC policy field {field} contains duplicate value {value:?}")]
    DuplicateOidcPolicyValue { field: &'static str, value: String },
    #[error("bridge endpoint {addr} is not loopback")]
    NonLoopbackBridge { addr: SocketAddr },
    #[error("operational limit {field} must be greater than zero")]
    ZeroLimit { field: &'static str },
    #[error("process boundary adapter requires a [process.adapter] section")]
    MissingProcessAdapter,
    #[error("[process.adapter] is forbidden unless the process boundary is adapter")]
    ProcessAdapterForbidden,
    #[error("configuration field {field} must be an absolute path")]
    InvalidProcessAdapter { field: &'static str },
    #[error("cannot resolve {location}: {reason}")]
    SecretResolution { location: String, reason: String },
}

impl ConfigError {
    /// Map to the stable domain error taxonomy.
    #[must_use]
    pub fn error_kind(&self) -> ErrorKind {
        match self {
            Self::NonLoopbackBind { .. } | Self::NonLoopbackBridge { .. } => {
                ErrorKind::PermissionDenied
            }
            _ => ErrorKind::InvalidInput,
        }
    }
}

#[derive(Default)]
struct HttpPartial {
    bind: Option<SocketAddr>,
    mode: Option<HttpMode>,
    host: Option<String>,
    origin: Option<String>,
    local_secret: Option<SecretRef>,
    proxy_hop: Option<ProxyHopCredential>,
    principal: Option<OidcPrincipal>,
    external_resource_url: Option<String>,
    allowed_algorithms: Option<Vec<OidcSigningAlgorithm>>,
    required_scopes: Option<Vec<String>>,
    required_capabilities: Option<Vec<Capability>>,
}

impl From<HttpConfig> for HttpPartial {
    fn from(value: HttpConfig) -> Self {
        Self {
            bind: Some(value.bind),
            mode: Some(value.mode),
            host: value.host,
            origin: value.origin,
            local_secret: value.local_secret,
            proxy_hop: value.proxy_hop,
            principal: value.principal,
            external_resource_url: value.external_resource_url,
            allowed_algorithms: value.allowed_algorithms,
            required_scopes: value.required_scopes,
            required_capabilities: value.required_capabilities,
        }
    }
}

impl HttpPartial {
    fn finish(self, override_touched: bool) -> Result<HttpConfig, ConfigError> {
        let mut missing = Vec::new();
        if self.bind.is_none() {
            missing.push("http.bind");
        }
        if self.mode.is_none() {
            missing.push("http.mode");
        }
        if self.host.is_none() {
            missing.push("http.host");
        }
        if self.origin.is_none() {
            missing.push("http.origin");
        }
        if let Some(mode) = self.mode {
            match mode {
                HttpMode::TrustedLocal if self.local_secret.is_none() => {
                    missing.push("http.local_secret")
                }
                HttpMode::ProxiedOidc => {
                    if self.proxy_hop.is_none() {
                        missing.push("http.proxy_hop");
                    }
                    if self.principal.is_none() {
                        missing.push("http.principal");
                    }
                    if self.external_resource_url.is_none() {
                        missing.push("http.external_resource_url");
                    }
                }
                HttpMode::TrustedLocal => {}
            }
        }
        if !missing.is_empty() {
            if override_touched {
                return Err(ConfigError::IncompleteHttpOverride { missing });
            }
            return match missing[0] {
                "http.host" => Err(ConfigError::MissingHost),
                "http.origin" => Err(ConfigError::MissingOrigin),
                "http.local_secret" => Err(ConfigError::MissingLocalSecret),
                "http.proxy_hop" => Err(ConfigError::MissingProxyHop),
                "http.principal" => Err(ConfigError::MissingPrincipal),
                "http.external_resource_url" => Err(ConfigError::MissingExternalResourceUrl),
                _ => Err(ConfigError::MissingHttpSection),
            };
        }
        Ok(HttpConfig {
            bind: self.bind.expect("checked"),
            mode: self.mode.expect("checked"),
            host: self.host,
            origin: self.origin,
            local_secret: self.local_secret,
            proxy_hop: self.proxy_hop,
            principal: self.principal,
            external_resource_url: self.external_resource_url,
            allowed_algorithms: self.allowed_algorithms,
            required_scopes: self.required_scopes,
            required_capabilities: self.required_capabilities,
        })
    }
}

/// Load and validate configuration with precedence CLI > env > TOML > defaults.
///
/// # Errors
/// Returns a sanitized parse, layering, secret-indirection, or validation error.
pub fn load_config(
    toml_text: Option<&str>,
    env: &BTreeMap<String, String>,
    cli: &CliOverrides,
) -> Result<ServerConfig, ConfigError> {
    let mut config = match toml_text {
        Some(text) => ServerConfig::from_toml_str(text)?,
        None => ServerConfig::default(),
    };
    let had_http = config.http.is_some();
    let mut http = config
        .http
        .take()
        .map(HttpPartial::from)
        .unwrap_or_default();
    let env_http = apply_env(&mut config, &mut http, env)?;
    let cli_http = apply_cli(&mut config, &mut http, cli);
    let http_override = env_http || cli_http;
    let active_secret_env = match http.local_secret.as_ref() {
        Some(SecretRef::Env { var }) => Some(var.as_str()),
        _ => None,
    };
    reject_unknown_env(env, active_secret_env)?;

    if had_http || http_override {
        config.http = Some(http.finish(http_override)?);
    }
    config.validate()?;
    Ok(config)
}

fn reject_unknown_env(
    env: &BTreeMap<String, String>,
    active_secret_env: Option<&str>,
) -> Result<(), ConfigError> {
    const KNOWN: &[&str] = &[
        "GODOT_MCP_PROJECT_ROOT",
        "GODOT_MCP_GODOT_BINARY",
        "GODOT_MCP_PROFILE",
        "GODOT_MCP_TRANSPORT",
        "GODOT_MCP_HTTP_BIND",
        "GODOT_MCP_HTTP_MODE",
        "GODOT_MCP_HTTP_HOST",
        "GODOT_MCP_HTTP_ORIGIN",
        "GODOT_MCP_HTTP_LOCAL_SECRET_ENV",
        "GODOT_MCP_HTTP_LOCAL_SECRET_FILE",
        "GODOT_MCP_HTTP_PROXY_SOCKET_PATH",
        "GODOT_MCP_HTTP_PROXY_CREDENTIAL_FILE",
        "GODOT_MCP_HTTP_PRINCIPAL_ISSUER",
        "GODOT_MCP_HTTP_PRINCIPAL_SUBJECT",
        "GODOT_MCP_HTTP_EXTERNAL_RESOURCE_URL",
        "GODOT_MCP_HTTP_ALLOWED_ALGORITHMS",
        "GODOT_MCP_HTTP_REQUIRED_SCOPES",
        "GODOT_MCP_HTTP_REQUIRED_CAPABILITIES",
        "GODOT_MCP_BRIDGE_ENDPOINT",
        "GODOT_MCP_LOG_SINK",
        "GODOT_MCP_DISPOSABLE",
        "GODOT_MCP_PROCESS_BOUNDARY",
        "GODOT_MCP_PROCESS_TRUST",
        "GODOT_MCP_DEADLINE_MS",
        "GODOT_MCP_MAX_OUTPUT_BYTES",
        "GODOT_MCP_MAX_BODY_BYTES",
        "GODOT_MCP_MAX_QUEUE",
    ];
    for name in env.keys() {
        if name.starts_with(ENV_PREFIX)
            && !KNOWN.contains(&name.as_str())
            && active_secret_env != Some(name.as_str())
        {
            return Err(ConfigError::UnknownEnvVar { name: name.clone() });
        }
    }
    Ok(())
}

fn apply_env(
    config: &mut ServerConfig,
    http: &mut HttpPartial,
    env: &BTreeMap<String, String>,
) -> Result<bool, ConfigError> {
    let mut touched = false;
    if let Some(value) = env.get("GODOT_MCP_PROJECT_ROOT") {
        config.project_root = Some(value.clone());
    }
    if let Some(value) = env.get("GODOT_MCP_GODOT_BINARY") {
        config.godot_binary = Some(value.clone());
    }
    if let Some(value) = env.get("GODOT_MCP_PROFILE") {
        config.profile = Some(parse_profile_env("GODOT_MCP_PROFILE", value)?);
    }
    if let Some(value) = env.get("GODOT_MCP_TRANSPORT") {
        config.transport = match value.as_str() {
            "stdio" => Transport::Stdio,
            "http" => {
                touched = true;
                Transport::Http
            }
            _ => return Err(invalid_env("GODOT_MCP_TRANSPORT", "expected stdio or http")),
        };
    }
    if let Some(value) = env.get("GODOT_MCP_BRIDGE_ENDPOINT") {
        config.bridge.endpoint = parse_env("GODOT_MCP_BRIDGE_ENDPOINT", value, "socket address")?;
    }
    if let Some(value) = env.get("GODOT_MCP_LOG_SINK") {
        config.log_sink = parse_log_sink_env(value)?;
    }
    if let Some(value) = env.get("GODOT_MCP_DISPOSABLE") {
        config.disposable = parse_bool_env("GODOT_MCP_DISPOSABLE", value)?;
    }
    if let Some(value) = env.get("GODOT_MCP_PROCESS_BOUNDARY") {
        config.process.boundary = Some(match value.as_str() {
            "weak" => ProcessBoundary::Weak,
            "cgroup" => ProcessBoundary::Cgroup,
            "adapter" => ProcessBoundary::Adapter,
            "job_object" => ProcessBoundary::JobObject,
            _ => {
                return Err(invalid_env(
                    "GODOT_MCP_PROCESS_BOUNDARY",
                    "expected weak, cgroup, adapter or job_object",
                ));
            }
        });
    }
    if let Some(value) = env.get("GODOT_MCP_PROCESS_TRUST") {
        config.process.trust = match value.as_str() {
            "trusted" => ProcessTrust::Trusted,
            "untrusted" => ProcessTrust::Untrusted,
            _ => {
                return Err(invalid_env(
                    "GODOT_MCP_PROCESS_TRUST",
                    "expected trusted or untrusted",
                ));
            }
        };
    }
    if let Some(value) = env.get("GODOT_MCP_DEADLINE_MS") {
        config.limits.deadline_ms = parse_env("GODOT_MCP_DEADLINE_MS", value, "positive integer")?;
    }
    if let Some(value) = env.get("GODOT_MCP_MAX_OUTPUT_BYTES") {
        config.limits.max_output_bytes =
            parse_env("GODOT_MCP_MAX_OUTPUT_BYTES", value, "positive integer")?;
    }
    if let Some(value) = env.get("GODOT_MCP_MAX_BODY_BYTES") {
        config.limits.max_body_bytes =
            parse_env("GODOT_MCP_MAX_BODY_BYTES", value, "positive integer")?;
    }
    if let Some(value) = env.get("GODOT_MCP_MAX_QUEUE") {
        config.limits.max_queue = parse_env("GODOT_MCP_MAX_QUEUE", value, "positive integer")?;
    }

    if let Some(value) = env.get("GODOT_MCP_HTTP_BIND") {
        http.bind = Some(parse_env("GODOT_MCP_HTTP_BIND", value, "socket address")?);
        touched = true;
    }
    if let Some(value) = env.get("GODOT_MCP_HTTP_MODE") {
        http.mode = Some(match value.as_str() {
            "trusted_local" => HttpMode::TrustedLocal,
            "proxied_oidc" => HttpMode::ProxiedOidc,
            _ => {
                return Err(invalid_env(
                    "GODOT_MCP_HTTP_MODE",
                    "expected trusted_local or proxied_oidc",
                ));
            }
        });
        touched = true;
    }
    set_env_string(env, "GODOT_MCP_HTTP_HOST", &mut http.host, &mut touched);
    set_env_string(env, "GODOT_MCP_HTTP_ORIGIN", &mut http.origin, &mut touched);
    if let Some(value) = env.get("GODOT_MCP_HTTP_LOCAL_SECRET_ENV") {
        http.local_secret = Some(SecretRef::Env { var: value.clone() });
        touched = true;
    }
    if let Some(value) = env.get("GODOT_MCP_HTTP_LOCAL_SECRET_FILE") {
        ensure_exclusive_env(
            env,
            "GODOT_MCP_HTTP_LOCAL_SECRET_ENV",
            "GODOT_MCP_HTTP_LOCAL_SECRET_FILE",
        )?;
        http.local_secret = Some(SecretRef::File {
            path: value.clone(),
        });
        touched = true;
    }
    if let Some(value) = env.get("GODOT_MCP_HTTP_PROXY_SOCKET_PATH") {
        http.proxy_hop = Some(ProxyHopCredential::SocketPath {
            path: value.clone(),
        });
        touched = true;
    }
    if let Some(value) = env.get("GODOT_MCP_HTTP_PROXY_CREDENTIAL_FILE") {
        ensure_exclusive_env(
            env,
            "GODOT_MCP_HTTP_PROXY_SOCKET_PATH",
            "GODOT_MCP_HTTP_PROXY_CREDENTIAL_FILE",
        )?;
        http.proxy_hop = Some(ProxyHopCredential::CredentialFile {
            path: value.clone(),
        });
        touched = true;
    }
    let issuer = env.get("GODOT_MCP_HTTP_PRINCIPAL_ISSUER");
    let subject = env.get("GODOT_MCP_HTTP_PRINCIPAL_SUBJECT");
    if issuer.is_some() || subject.is_some() {
        http.principal = Some(OidcPrincipal {
            issuer: issuer.cloned().unwrap_or_default(),
            subject: subject.cloned().unwrap_or_default(),
        });
        touched = true;
    }
    set_env_string(
        env,
        "GODOT_MCP_HTTP_EXTERNAL_RESOURCE_URL",
        &mut http.external_resource_url,
        &mut touched,
    );
    if let Some(value) = env.get("GODOT_MCP_HTTP_ALLOWED_ALGORITHMS") {
        http.allowed_algorithms = Some(parse_oidc_algorithms_env(value)?);
        touched = true;
    }
    if let Some(value) = env.get("GODOT_MCP_HTTP_REQUIRED_SCOPES") {
        http.required_scopes = Some(parse_csv(value));
        touched = true;
    }
    if let Some(value) = env.get("GODOT_MCP_HTTP_REQUIRED_CAPABILITIES") {
        http.required_capabilities = Some(parse_capabilities_env(value)?);
        touched = true;
    }
    Ok(touched)
}

fn apply_cli(config: &mut ServerConfig, http: &mut HttpPartial, cli: &CliOverrides) -> bool {
    let mut touched = false;
    if let Some(value) = &cli.project_root {
        config.project_root = Some(value.clone());
    }
    if let Some(value) = &cli.godot_binary {
        config.godot_binary = Some(value.clone());
    }
    if let Some(value) = cli.profile {
        config.profile = Some(value);
    }
    if let Some(value) = cli.transport {
        config.transport = value;
        touched = value == Transport::Http;
    }
    if let Some(value) = cli.bridge_endpoint {
        config.bridge.endpoint = value;
    }
    if let Some(value) = &cli.log_sink {
        config.log_sink = value.clone();
    }
    if let Some(value) = cli.disposable {
        config.disposable = value;
    }
    if let Some(value) = cli.process_boundary {
        config.process.boundary = Some(value);
    }
    if let Some(value) = cli.process_trust {
        config.process.trust = value;
    }
    if let Some(value) = &cli.process_adapter {
        config.process.adapter = Some(value.clone());
    }
    if let Some(value) = cli.deadline_ms {
        config.limits.deadline_ms = value;
    }
    if let Some(value) = cli.max_output_bytes {
        config.limits.max_output_bytes = value;
    }
    if let Some(value) = cli.max_body_bytes {
        config.limits.max_body_bytes = value;
    }
    if let Some(value) = cli.max_queue {
        config.limits.max_queue = value;
    }

    macro_rules! set_copy {
        ($source:expr, $target:expr) => {
            if let Some(value) = $source {
                $target = Some(value);
                touched = true;
            }
        };
    }
    macro_rules! set_clone {
        ($source:expr, $target:expr) => {
            if let Some(value) = $source {
                $target = Some(value.clone());
                touched = true;
            }
        };
    }
    set_copy!(cli.http_bind, http.bind);
    set_copy!(cli.http_mode, http.mode);
    set_clone!(&cli.http_host, http.host);
    set_clone!(&cli.http_origin, http.origin);
    set_clone!(&cli.http_local_secret, http.local_secret);
    set_clone!(&cli.http_proxy_hop, http.proxy_hop);
    set_clone!(&cli.http_principal, http.principal);
    set_clone!(&cli.http_external_resource_url, http.external_resource_url);
    set_clone!(&cli.http_allowed_algorithms, http.allowed_algorithms);
    set_clone!(&cli.http_required_scopes, http.required_scopes);
    set_clone!(&cli.http_required_capabilities, http.required_capabilities);
    touched
}

fn set_env_string(
    env: &BTreeMap<String, String>,
    name: &str,
    target: &mut Option<String>,
    touched: &mut bool,
) {
    if let Some(value) = env.get(name) {
        *target = Some(value.clone());
        *touched = true;
    }
}

fn ensure_exclusive_env(
    env: &BTreeMap<String, String>,
    first: &str,
    second: &str,
) -> Result<(), ConfigError> {
    if env.contains_key(first) && env.contains_key(second) {
        return Err(invalid_env(second, "conflicts with another indirection"));
    }
    Ok(())
}

fn invalid_env(name: &str, reason: &str) -> ConfigError {
    ConfigError::InvalidEnvValue {
        name: name.to_string(),
        reason: reason.to_string(),
    }
}

fn parse_env<T: std::str::FromStr>(
    name: &str,
    value: &str,
    expected: &str,
) -> Result<T, ConfigError> {
    value.parse().map_err(|_| invalid_env(name, expected))
}

fn parse_bool_env(name: &str, value: &str) -> Result<bool, ConfigError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(invalid_env(name, "expected true or false")),
    }
}

fn parse_profile_env(name: &str, value: &str) -> Result<CapabilityProfile, ConfigError> {
    match value {
        "core" => Ok(CapabilityProfile::Core),
        "editor" => Ok(CapabilityProfile::Editor),
        "runtime" => Ok(CapabilityProfile::Runtime),
        "full" => Ok(CapabilityProfile::Full),
        _ => Err(invalid_env(name, "expected core, editor, runtime, or full")),
    }
}

fn parse_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|entry| entry.trim().to_owned())
        .collect()
}

fn parse_oidc_algorithms_env(value: &str) -> Result<Vec<OidcSigningAlgorithm>, ConfigError> {
    parse_csv(value)
        .into_iter()
        .map(|name| match name.as_str() {
            "RS256" => Ok(OidcSigningAlgorithm::Rs256),
            "PS256" => Ok(OidcSigningAlgorithm::Ps256),
            "ES256" => Ok(OidcSigningAlgorithm::Es256),
            _ => Err(invalid_env(
                "GODOT_MCP_HTTP_ALLOWED_ALGORITHMS",
                "expected a comma-separated nonempty subset of RS256, PS256, ES256",
            )),
        })
        .collect()
}

fn parse_capabilities_env(value: &str) -> Result<Vec<Capability>, ConfigError> {
    parse_csv(value)
        .into_iter()
        .map(|name| match name.as_str() {
            "read" => Ok(Capability::Read),
            "project_write" => Ok(Capability::ProjectWrite),
            "editor_mutate" => Ok(Capability::EditorMutate),
            "runtime_execute" => Ok(Capability::RuntimeExecute),
            "export_destructive" => Ok(Capability::ExportDestructive),
            _ => Err(invalid_env(
                "GODOT_MCP_HTTP_REQUIRED_CAPABILITIES",
                "expected comma-separated capability identifiers",
            )),
        })
        .collect()
}

fn parse_log_sink_env(value: &str) -> Result<LogSink, ConfigError> {
    if value == "stderr" {
        return Ok(LogSink::Stderr);
    }
    if let Some(path) = value.strip_prefix("file:")
        && !path.is_empty()
    {
        return Ok(LogSink::File {
            path: path.to_string(),
        });
    }
    Err(invalid_env(
        "GODOT_MCP_LOG_SINK",
        "expected stderr or file:<path>",
    ))
}

impl ServerConfig {
    /// Parse a strict TOML document while suppressing source excerpts that could
    /// contain rejected inline secret material.
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|_: toml::de::Error| {
            ConfigError::Parse("invalid TOML or unknown configuration field/value".to_string())
        })
    }

    /// Validate all startup invariants before any listener or side effect.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.bridge.endpoint.ip().is_loopback() {
            return Err(ConfigError::NonLoopbackBridge {
                addr: self.bridge.endpoint,
            });
        }
        validate_limits(&self.limits)?;
        validate_process(&self.process)?;
        if let LogSink::File { path } = &self.log_sink {
            require_nonempty("log_sink.file.path", path)?;
        }
        match self.transport {
            Transport::Stdio => {
                if self.http.is_some() {
                    return Err(ConfigError::HttpSectionForbidden);
                }
                Ok(())
            }
            Transport::Http => {
                let http = self.http.as_ref().ok_or(ConfigError::MissingHttpSection)?;
                validate_http(http)
            }
        }
    }
}

fn validate_process(process: &ProcessConfig) -> Result<(), ConfigError> {
    match (process.boundary, &process.adapter) {
        (Some(ProcessBoundary::Adapter), None) => Err(ConfigError::MissingProcessAdapter),
        (Some(ProcessBoundary::Adapter), Some(adapter)) => {
            for (field, value) in [
                ("process.adapter.program", &adapter.program),
                ("process.adapter.root", &adapter.root),
            ] {
                if !std::path::Path::new(value).is_absolute() {
                    return Err(ConfigError::InvalidProcessAdapter { field });
                }
            }
            Ok(())
        }
        (_, Some(_)) => Err(ConfigError::ProcessAdapterForbidden),
        (_, None) => Ok(()),
    }
}

fn validate_limits(limits: &Limits) -> Result<(), ConfigError> {
    for (field, zero) in [
        ("deadline_ms", limits.deadline_ms == 0),
        ("max_output_bytes", limits.max_output_bytes == 0),
        ("max_body_bytes", limits.max_body_bytes == 0),
        ("max_queue", limits.max_queue == 0),
    ] {
        if zero {
            return Err(ConfigError::ZeroLimit { field });
        }
    }
    Ok(())
}

fn validate_http(http: &HttpConfig) -> Result<(), ConfigError> {
    if !http.bind.ip().is_loopback() {
        return Err(ConfigError::NonLoopbackBind { addr: http.bind });
    }
    let host = http.host.as_deref().ok_or(ConfigError::MissingHost)?;
    require_nonempty("http.host", host)?;
    validate_authority(host).map_err(|reason| ConfigError::InvalidHost { reason })?;
    let origin = http.origin.as_deref().ok_or(ConfigError::MissingOrigin)?;
    require_nonempty("http.origin", origin)?;
    validate_origin(origin)?;

    match http.mode {
        HttpMode::TrustedLocal => {
            let secret = http
                .local_secret
                .as_ref()
                .ok_or(ConfigError::MissingLocalSecret)?;
            validate_secret_ref(secret)?;
            forbid(http.proxy_hop.is_some(), "proxy_hop", http.mode)?;
            forbid(http.principal.is_some(), "principal", http.mode)?;
            forbid(
                http.external_resource_url.is_some(),
                "external_resource_url",
                http.mode,
            )?;
            forbid(
                http.allowed_algorithms.is_some(),
                "allowed_algorithms",
                http.mode,
            )?;
            forbid(http.required_scopes.is_some(), "required_scopes", http.mode)?;
            forbid(
                http.required_capabilities.is_some(),
                "required_capabilities",
                http.mode,
            )
        }
        HttpMode::ProxiedOidc => {
            forbid(http.local_secret.is_some(), "local_secret", http.mode)?;
            validate_proxy_hop(
                http.proxy_hop
                    .as_ref()
                    .ok_or(ConfigError::MissingProxyHop)?,
            )?;
            let principal = http
                .principal
                .as_ref()
                .ok_or(ConfigError::MissingPrincipal)?;
            require_nonempty("http.principal.issuer", &principal.issuer)?;
            require_nonempty("http.principal.subject", &principal.subject)?;
            validate_https_url(&principal.issuer, false)
                .map_err(|reason| ConfigError::InvalidIssuer { reason })?;
            let resource = http
                .external_resource_url
                .as_deref()
                .ok_or(ConfigError::MissingExternalResourceUrl)?;
            require_nonempty("http.external_resource_url", resource)?;
            validate_https_url(resource, true)
                .map_err(|reason| ConfigError::InvalidResourceUrl { reason })?;
            validate_oidc_policy(http)
        }
    }
}

fn validate_oidc_policy(http: &HttpConfig) -> Result<(), ConfigError> {
    let algorithms = http.effective_allowed_algorithms();
    validate_nonempty_unique("http.allowed_algorithms", &algorithms, |algorithm| {
        algorithm.as_str().to_owned()
    })?;

    let scopes = http.effective_required_scopes();
    validate_nonempty_unique("http.required_scopes", &scopes, Clone::clone)?;
    for scope in scopes {
        if !is_scope_token(&scope) {
            return Err(ConfigError::InvalidOidcScope { scope });
        }
    }

    let capabilities = http.effective_required_capabilities();
    validate_nonempty_unique("http.required_capabilities", &capabilities, |capability| {
        capability_name(*capability).to_owned()
    })
}

fn validate_nonempty_unique<T: Ord>(
    field: &'static str,
    values: &[T],
    render: impl Fn(&T) -> String,
) -> Result<(), ConfigError> {
    if values.is_empty() {
        return Err(ConfigError::EmptyOidcPolicy { field });
    }
    let mut observed = BTreeSet::new();
    for value in values {
        if !observed.insert(value) {
            return Err(ConfigError::DuplicateOidcPolicyValue {
                field,
                value: render(value),
            });
        }
    }
    Ok(())
}

fn is_scope_token(scope: &str) -> bool {
    !scope.is_empty()
        && scope.bytes().all(|byte| {
            byte == b'!' || (b'#'..=b'[').contains(&byte) || (b']'..=b'~').contains(&byte)
        })
}

const fn capability_name(capability: Capability) -> &'static str {
    match capability {
        Capability::Read => "read",
        Capability::ProjectWrite => "project_write",
        Capability::EditorMutate => "editor_mutate",
        Capability::RuntimeExecute => "runtime_execute",
        Capability::ExportDestructive => "export_destructive",
    }
}

fn validate_secret_ref(secret: &SecretRef) -> Result<(), ConfigError> {
    match secret {
        SecretRef::Env { var } => require_nonempty("http.local_secret.env.var", var),
        SecretRef::File { path } => require_nonempty("http.local_secret.file.path", path),
    }
}

fn validate_proxy_hop(proxy: &ProxyHopCredential) -> Result<(), ConfigError> {
    match proxy {
        ProxyHopCredential::SocketPath { path } => {
            require_nonempty("http.proxy_hop.socket_path.path", path)
        }
        ProxyHopCredential::CredentialFile { path } => {
            require_nonempty("http.proxy_hop.credential_file.path", path)
        }
    }
}

fn forbid(present: bool, field: &str, mode: HttpMode) -> Result<(), ConfigError> {
    if present {
        Err(ConfigError::ForbiddenFieldForMode {
            field: field.to_string(),
            mode,
        })
    } else {
        Ok(())
    }
}

fn require_nonempty(field: &str, value: &str) -> Result<(), ConfigError> {
    if value.trim().is_empty() {
        Err(ConfigError::EmptyValue {
            field: field.to_string(),
        })
    } else {
        Ok(())
    }
}

fn validate_origin(origin: &str) -> Result<(), ConfigError> {
    let parsed =
        parse_absolute_url(origin).map_err(|reason| ConfigError::InvalidOrigin { reason })?;
    if !matches!(parsed.scheme, "http" | "https") {
        return Err(ConfigError::InvalidOrigin {
            reason: "scheme must be http or https",
        });
    }
    if !parsed.suffix.is_empty() && parsed.suffix != "/" {
        return Err(ConfigError::InvalidOrigin {
            reason: "path, query, and fragment are forbidden",
        });
    }
    Ok(())
}

fn validate_https_url(url: &str, reject_local_ip: bool) -> Result<(), &'static str> {
    let parsed = parse_absolute_url(url)?;
    if parsed.scheme != "https" {
        return Err("scheme must be https");
    }
    if parsed.suffix.contains('#') {
        return Err("fragment is forbidden");
    }
    if reject_local_ip {
        let host = parsed.host.trim_matches(['[', ']']);
        if let Ok(ip) = host.parse::<IpAddr>()
            && (ip.is_loopback() || ip.is_unspecified())
        {
            return Err("loopback/unspecified resource address is forbidden");
        }
    }
    Ok(())
}

struct ParsedUrl<'a> {
    scheme: &'a str,
    host: &'a str,
    suffix: &'a str,
}

fn parse_absolute_url(url: &str) -> Result<ParsedUrl<'_>, &'static str> {
    if url.chars().any(char::is_whitespace) {
        return Err("whitespace is forbidden");
    }
    let (scheme, remainder) = url.split_once("://").ok_or("absolute URL required")?;
    if scheme.is_empty() || remainder.is_empty() {
        return Err("scheme and authority are required");
    }
    let boundary = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let authority = &remainder[..boundary];
    let suffix = &remainder[boundary..];
    if authority.is_empty() || authority.contains('@') {
        return Err("authority must be nonempty and contain no credentials");
    }
    let host = validate_authority(authority)?;
    Ok(ParsedUrl {
        scheme,
        host,
        suffix,
    })
}

fn validate_authority(authority: &str) -> Result<&str, &'static str> {
    if authority.is_empty()
        || authority == "*"
        || authority.contains(['/', '?', '#', '@'])
        || authority.chars().any(char::is_whitespace)
    {
        return Err("invalid authority");
    }
    if authority.starts_with('[') {
        let close = authority.find(']').ok_or("invalid IPv6 authority")?;
        let host = &authority[1..close];
        host.parse::<std::net::Ipv6Addr>()
            .map_err(|_| "invalid IPv6 host")?;
        let rest = &authority[close + 1..];
        if !rest.is_empty() {
            let port = rest.strip_prefix(':').ok_or("invalid authority suffix")?;
            validate_port(port)?;
        }
        return Ok(host);
    }
    if authority.matches(':').count() > 1 {
        return Err("IPv6 hosts must use brackets");
    }
    let (host, port) = authority
        .split_once(':')
        .map_or((authority, None), |(host, port)| (host, Some(port)));
    if host.is_empty()
        || !host
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-'))
    {
        return Err("invalid host name");
    }
    if let Some(port) = port {
        validate_port(port)?;
    }
    Ok(host)
}

fn validate_port(port: &str) -> Result<(), &'static str> {
    let parsed = port.parse::<u16>().map_err(|_| "invalid port")?;
    if parsed == 0 {
        return Err("port must be nonzero");
    }
    Ok(())
}
