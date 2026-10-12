//! Production startup assembly for the `proxied_oidc` resource-server boundary.

use std::sync::{RwLock, Weak};
use std::time::Duration;

use godot_mcp_domain::config::{HttpConfig, HttpMode, OidcSigningAlgorithm, ProxyHopCredential};
use serde::Deserialize;

use super::header::{AlgorithmAllowlist, HeaderRejection, SignatureAlgorithm};
use super::hop::{HopCredential, HopRejection, ProxyHop};
use super::jwks::{JwkSet, KeyRejection};
use super::metadata::{ConfigRejection, ResourceServerConfig};
use super::pipeline::{OidcClock, ResourceServer};
use super::resolver::{
    DiscoveryClient, SsrfRejection, SystemResolver, VettedEndpoint, reject_environment_proxy,
};

#[derive(Debug, thiserror::Error)]
pub enum ResourceServerStartupError {
    #[error("the HTTP configuration is not proxied_oidc")]
    WrongMode,
    #[error("the proxied_oidc configuration is incomplete")]
    IncompleteConfiguration,
    #[error("socket_path cannot authenticate requests accepted by the TCP listener")]
    UnverifiableSocketHop,
    #[error("ambient proxy configuration is forbidden for OIDC discovery")]
    EnvironmentProxy,
    #[error("OIDC endpoint vetting or fetch failed: {0}")]
    Discovery(#[from] SsrfRejection),
    #[error("the OIDC discovery document is invalid")]
    InvalidDiscoveryDocument,
    #[error("the OIDC discovery issuer does not match the configured issuer")]
    IssuerMismatch,
    #[error("the resource-server configuration is invalid: {0}")]
    ResourceConfig(#[from] ConfigRejection),
    #[error("the proxy-hop credential is invalid: {0}")]
    ProxyHop(#[from] HopRejection),
    #[error("the fetched JWKS is invalid: {0}")]
    Jwks(#[from] KeyRejection),
    #[error("the selected OIDC algorithm policy is invalid: {0:?}")]
    AlgorithmPolicy(HeaderRejection),
}

const JWKS_REFRESH_INTERVAL: Duration = Duration::from_secs(300);

#[derive(Debug, Deserialize)]
struct DiscoveryDocument {
    issuer: String,
    jwks_uri: String,
}

pub async fn build_resource_server(
    http: &HttpConfig,
) -> Result<ResourceServer, ResourceServerStartupError> {
    if http.mode != HttpMode::ProxiedOidc {
        return Err(ResourceServerStartupError::WrongMode);
    }
    let principal = http
        .principal
        .as_ref()
        .ok_or(ResourceServerStartupError::IncompleteConfiguration)?;
    let resource = http
        .external_resource_url
        .as_deref()
        .ok_or(ResourceServerStartupError::IncompleteConfiguration)?;
    let configured_hop = http
        .proxy_hop
        .as_ref()
        .ok_or(ResourceServerStartupError::IncompleteConfiguration)?;
    if matches!(configured_hop, ProxyHopCredential::SocketPath { .. }) {
        return Err(ResourceServerStartupError::UnverifiableSocketHop);
    }

    let environment = std::env::vars().collect::<Vec<_>>();
    reject_environment_proxy(
        environment
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
    )
    .map_err(|_| ResourceServerStartupError::EnvironmentProxy)?;

    let issuer = principal.issuer.trim_end_matches('/');
    let discovery_url = format!("{issuer}/.well-known/openid-configuration");
    let resolver = SystemResolver;
    let discovery_endpoint = VettedEndpoint::vet(&discovery_url, &resolver)?;
    let discovery_body = DiscoveryClient::new(&discovery_endpoint)?
        .fetch(&discovery_endpoint, issuer, &resolver)
        .await?;
    let discovery: DiscoveryDocument = serde_json::from_str(&discovery_body)
        .map_err(|_| ResourceServerStartupError::InvalidDiscoveryDocument)?;
    if discovery.issuer.trim_end_matches('/') != issuer {
        return Err(ResourceServerStartupError::IssuerMismatch);
    }

    let allowlist = AlgorithmAllowlist::narrowed(
        http.effective_allowed_algorithms()
            .into_iter()
            .map(signature_algorithm),
    )
    .map_err(ResourceServerStartupError::AlgorithmPolicy)?;
    let config = ResourceServerConfig::new(
        issuer,
        &discovery.jwks_uri,
        &principal.subject,
        resource,
        resource,
    )?
    .with_allowlist(allowlist)
    .with_required_scopes(http.effective_required_scopes())
    .with_required_capabilities(http.effective_required_capabilities());
    let jwks_endpoint = VettedEndpoint::vet(config.jwks_endpoint(), &resolver)?;
    let jwks_body = DiscoveryClient::new(&jwks_endpoint)?
        .fetch(&jwks_endpoint, issuer, &resolver)
        .await?;
    let keys = JwkSet::parse(&jwks_body)?;
    let hop = ProxyHop::load(HopCredential::from_config(configured_hop))?;
    let server = ResourceServer::new(config, hop, keys, OidcClock::System);
    spawn_jwks_refresh(&server);
    Ok(server)
}

fn signature_algorithm(algorithm: OidcSigningAlgorithm) -> SignatureAlgorithm {
    match algorithm {
        OidcSigningAlgorithm::Rs256 => SignatureAlgorithm::Rs256,
        OidcSigningAlgorithm::Ps256 => SignatureAlgorithm::Ps256,
        OidcSigningAlgorithm::Es256 => SignatureAlgorithm::Es256,
    }
}

fn spawn_jwks_refresh(server: &ResourceServer) {
    let keys = server.weak_keys();
    let endpoint = server.config().jwks_endpoint().to_owned();
    let issuer = server.config().issuer().to_owned();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(JWKS_REFRESH_INTERVAL).await;
            if keys.upgrade().is_none() {
                break;
            }
            let resolver = SystemResolver;
            let result = async {
                let vetted = VettedEndpoint::vet(&endpoint, &resolver)?;
                DiscoveryClient::new(&vetted)?
                    .fetch(&vetted, &issuer, &resolver)
                    .await
            }
            .await;
            if let Err(error) = apply_jwks_refresh_to_cache(&keys, result) {
                tracing::warn!(
                    event = "oidc_jwks_refresh_failed",
                    reason = error.to_string(),
                    "keeping the last verified OIDC key set"
                );
            }
        }
    });
}

fn apply_jwks_refresh_to_cache(
    keys: &Weak<RwLock<JwkSet>>,
    fetched: Result<String, SsrfRejection>,
) -> Result<(), ResourceServerStartupError> {
    let document = fetched?;
    let replacement = JwkSet::parse(&document)?;
    if let Some(keys) = keys.upgrade() {
        *keys
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = replacement;
    }
    Ok(())
}

/// Install a successfully fetched JWKS while retaining the current set on failure.
///
/// # Errors
/// Returns the fetch or parse failure without changing the active key set.
pub fn apply_jwks_refresh_result(
    server: &ResourceServer,
    fetched: Result<String, SsrfRejection>,
) -> Result<(), ResourceServerStartupError> {
    let document = fetched?;
    server.install_keys(JwkSet::parse(&document)?);
    Ok(())
}
