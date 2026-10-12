//! RFC 9728 protected-resource metadata, and the fixed server-side configuration
//! the token pipeline validates against.
//!
//! # Why the configuration lives in one immutable struct
//!
//! Every value the pipeline compares a token against — the issuer, the single
//! accepted `(iss, sub)` principal, the audience, the JWKS endpoint, the selected
//! algorithm subset — is *server-side configuration*. Collecting them into one
//! [`ResourceServerConfig`] that is validated once at construction and then only
//! read makes a whole class of mistake impossible: no request handler can widen the
//! algorithm set, redirect the JWKS endpoint, or add a second subject, because
//! there is no method that mutates any of it.
//!
//! Construction is where the invariants are enforced, and it fails closed:
//!
//! * The issuer and the advertised resource URL must be HTTPS. An `http://` issuer
//!   would make the whole chain unauthenticated, so it is refused rather than
//!   warned about.
//! * The JWKS endpoint must be HTTPS *and* must sit under the issuer's authority,
//!   which is what keeps "fetch the keys" from becoming "fetch anything".
//! * Exactly one subject is accepted. There is no list, so there is nothing to
//!   append to.
//! * The algorithm allowlist can only narrow the hard `{RS256, PS256, ES256}` set
//!   (see [`super::header::AlgorithmAllowlist`]).
//!
//! # What the metadata document is for
//!
//! A client that gets a `401` needs to know *where* to obtain an acceptable token.
//! RFC 9728 answers that with a document at
//! [`PROTECTED_RESOURCE_METADATA_PATH`] naming the resource identifier, its
//! authorization servers, and the scopes it understands; RFC 6750 §3 carries a
//! pointer to that document in the `WWW-Authenticate` challenge. Both use the
//! *externally visible* HTTPS URL, because the loopback address this process
//! actually binds is meaningless to a client on the other side of the proxy.
//!
//! The document is deliberately thin. It advertises no token endpoint, no
//! registration endpoint and no grant types, because this server issues nothing:
//! it is a Resource Server, and login, consent, PKCE, client registration, token
//! issuance and refresh all live in the identity provider.

use std::collections::BTreeSet;

use godot_mcp_domain::contracts::Capability;
use serde_json::{Value, json};

use super::header::AlgorithmAllowlist;

/// The RFC 9728 well-known path this server answers.
pub const PROTECTED_RESOURCE_METADATA_PATH: &str = "/.well-known/oauth-protected-resource";

/// Why a resource-server configuration was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigRejection {
    /// A configured URL is not HTTPS.
    #[error("{field} must be an https url")]
    InsecureUrl {
        /// Which field was refused.
        field: &'static str,
    },
    /// A configured value is empty.
    #[error("{field} must not be empty")]
    Empty {
        /// Which field was refused.
        field: &'static str,
    },
    /// The JWKS endpoint is not under the issuer's authority.
    #[error("the jwks endpoint must share the issuer's authority")]
    ForeignJwksEndpoint,
}

/// The frozen server-side configuration the token pipeline validates against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceServerConfig {
    issuer: String,
    jwks_endpoint: String,
    subject: String,
    audience: String,
    external_resource_url: String,
    allowlist: AlgorithmAllowlist,
    required_scopes: BTreeSet<String>,
    required_capabilities: BTreeSet<Capability>,
    leeway_seconds: u64,
}

impl ResourceServerConfig {
    /// Build and validate one configuration.
    ///
    /// # Errors
    /// Returns [`ConfigRejection`] for a non-HTTPS issuer, JWKS endpoint or
    /// resource URL, for an empty subject or audience, or for a JWKS endpoint
    /// outside the issuer's authority.
    pub fn new(
        issuer: &str,
        jwks_endpoint: &str,
        subject: &str,
        audience: &str,
        external_resource_url: &str,
    ) -> Result<Self, ConfigRejection> {
        for (field, url) in [
            ("issuer", issuer),
            ("jwks_endpoint", jwks_endpoint),
            ("external_resource_url", external_resource_url),
        ] {
            if url.trim().is_empty() {
                return Err(ConfigRejection::Empty { field });
            }
            if !url.starts_with("https://") {
                return Err(ConfigRejection::InsecureUrl { field });
            }
        }
        if subject.trim().is_empty() {
            return Err(ConfigRejection::Empty { field: "subject" });
        }
        if audience.trim().is_empty() {
            return Err(ConfigRejection::Empty { field: "audience" });
        }
        if authority_of(jwks_endpoint) != authority_of(issuer) {
            return Err(ConfigRejection::ForeignJwksEndpoint);
        }
        Ok(Self {
            issuer: issuer.trim_end_matches('/').to_owned(),
            jwks_endpoint: jwks_endpoint.to_owned(),
            subject: subject.to_owned(),
            audience: audience.to_owned(),
            external_resource_url: external_resource_url.to_owned(),
            allowlist: AlgorithmAllowlist::hard(),
            required_scopes: BTreeSet::new(),
            required_capabilities: BTreeSet::new(),
            leeway_seconds: 0,
        })
    }

    /// Narrow the algorithms this deployment accepts.
    #[must_use]
    pub fn with_allowlist(mut self, allowlist: AlgorithmAllowlist) -> Self {
        self.allowlist = allowlist;
        self
    }

    /// Require every named scope to be present in a token.
    #[must_use]
    pub fn with_required_scopes<S: Into<String>>(
        mut self,
        scopes: impl IntoIterator<Item = S>,
    ) -> Self {
        self.required_scopes = scopes.into_iter().map(Into::into).collect();
        self
    }

    /// Require the deployment to grant every named capability.
    #[must_use]
    pub fn with_required_capabilities(
        mut self,
        capabilities: impl IntoIterator<Item = Capability>,
    ) -> Self {
        self.required_capabilities = capabilities.into_iter().collect();
        self
    }

    /// Allow `seconds` of clock skew on `exp`/`nbf`/`iat`.
    #[must_use]
    pub const fn with_leeway_seconds(mut self, seconds: u64) -> Self {
        self.leeway_seconds = seconds;
        self
    }

    /// The exact issuer a token's `iss` must equal.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// The fixed JWKS endpoint, the only location keys are ever fetched from.
    #[must_use]
    pub fn jwks_endpoint(&self) -> &str {
        &self.jwks_endpoint
    }

    /// The one subject this deployment accepts.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// The exact audience a token must name.
    #[must_use]
    pub fn audience(&self) -> &str {
        &self.audience
    }

    /// The externally visible HTTPS resource URL.
    #[must_use]
    pub fn external_resource_url(&self) -> &str {
        &self.external_resource_url
    }

    /// The selected algorithm subset.
    #[must_use]
    pub const fn allowlist(&self) -> &AlgorithmAllowlist {
        &self.allowlist
    }

    /// The scopes every token must carry.
    #[must_use]
    pub const fn required_scopes(&self) -> &BTreeSet<String> {
        &self.required_scopes
    }

    /// The capabilities the deployment policy must grant.
    #[must_use]
    pub const fn required_capabilities(&self) -> &BTreeSet<Capability> {
        &self.required_capabilities
    }

    /// The permitted clock skew, in seconds.
    #[must_use]
    pub const fn leeway_seconds(&self) -> u64 {
        self.leeway_seconds
    }

    /// The absolute URL of this resource's metadata document.
    #[must_use]
    pub fn metadata_url(&self) -> String {
        format!(
            "{}{PROTECTED_RESOURCE_METADATA_PATH}",
            origin_of(&self.external_resource_url)
        )
    }

    /// The metadata document this resource publishes.
    #[must_use]
    pub fn metadata(&self) -> ProtectedResourceMetadata {
        ProtectedResourceMetadata {
            resource: self.external_resource_url.clone(),
            authorization_servers: vec![self.issuer.clone()],
            scopes_supported: self.required_scopes.iter().cloned().collect(),
            algorithms_supported: self
                .allowlist
                .selected()
                .into_iter()
                .map(|algorithm| algorithm.as_str().to_owned())
                .collect(),
            capabilities_required: self
                .required_capabilities
                .iter()
                .map(|capability| capability_name(*capability).to_owned())
                .collect(),
        }
    }
}

/// The RFC 9728 protected-resource metadata document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
    scopes_supported: Vec<String>,
    algorithms_supported: Vec<String>,
    capabilities_required: Vec<String>,
}

impl ProtectedResourceMetadata {
    /// The resource identifier clients request a token for (RFC 8707 `resource`).
    #[must_use]
    pub fn resource(&self) -> &str {
        &self.resource
    }

    /// The issuers whose tokens this resource accepts.
    #[must_use]
    pub fn authorization_servers(&self) -> &[String] {
        &self.authorization_servers
    }

    /// The JSON body served at [`PROTECTED_RESOURCE_METADATA_PATH`].
    ///
    /// Byte-stable for a given configuration: the field order is fixed and every
    /// list is already sorted, so a client and a test see the same bytes each time.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "resource": self.resource,
            "authorization_servers": self.authorization_servers,
            "scopes_supported": self.scopes_supported,
            "bearer_methods_supported": ["header"],
            "resource_signing_alg_values_supported": self.algorithms_supported,
            "godot_capabilities_required": self.capabilities_required,
        })
    }

    /// The serialized body, as served.
    ///
    /// # Panics
    /// Panics only if `serde_json` cannot serialize a document it just built, which
    /// would be a bug in this module rather than a runtime condition.
    #[must_use]
    pub fn to_body(&self) -> String {
        serde_json::to_string(&self.to_json()).expect("the metadata document serializes")
    }
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

fn authority_of(url: &str) -> String {
    url.strip_prefix("https://")
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn origin_of(url: &str) -> String {
    format!("https://{}", authority_of(url))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::header::SignatureAlgorithm;

    fn config() -> ResourceServerConfig {
        ResourceServerConfig::new(
            "https://issuer.example",
            "https://issuer.example/jwks",
            "subject-1",
            "https://mcp.example.com/mcp",
            "https://mcp.example.com/mcp",
        )
        .expect("valid configuration")
    }

    #[test]
    fn a_valid_configuration_exposes_exactly_what_was_configured() {
        let config = config();
        assert_eq!(config.issuer(), "https://issuer.example");
        assert_eq!(config.jwks_endpoint(), "https://issuer.example/jwks");
        assert_eq!(config.subject(), "subject-1");
        assert_eq!(config.audience(), "https://mcp.example.com/mcp");
        assert_eq!(
            config.external_resource_url(),
            "https://mcp.example.com/mcp"
        );
        assert_eq!(config.leeway_seconds(), 0);
        assert!(config.required_scopes().is_empty());
        assert_eq!(config.allowlist().selected().len(), 3);
    }

    #[test]
    fn an_insecure_or_empty_url_is_refused() {
        assert_eq!(
            ResourceServerConfig::new(
                "http://issuer.example",
                "https://issuer.example/jwks",
                "s",
                "a",
                "https://mcp.example.com/mcp"
            )
            .err(),
            Some(ConfigRejection::InsecureUrl { field: "issuer" })
        );
        assert_eq!(
            ResourceServerConfig::new(
                "https://issuer.example",
                "http://issuer.example/jwks",
                "s",
                "a",
                "https://mcp.example.com/mcp"
            )
            .err(),
            Some(ConfigRejection::InsecureUrl {
                field: "jwks_endpoint"
            })
        );
        assert_eq!(
            ResourceServerConfig::new(
                "https://issuer.example",
                "https://issuer.example/jwks",
                "s",
                "a",
                "http://mcp.example.com/mcp"
            )
            .err(),
            Some(ConfigRejection::InsecureUrl {
                field: "external_resource_url"
            })
        );
        assert_eq!(
            ResourceServerConfig::new(
                "",
                "https://issuer.example/jwks",
                "s",
                "a",
                "https://mcp.example.com/mcp"
            )
            .err(),
            Some(ConfigRejection::Empty { field: "issuer" })
        );
        assert_eq!(
            ResourceServerConfig::new(
                "https://issuer.example",
                "https://issuer.example/jwks",
                "  ",
                "a",
                "https://mcp.example.com/mcp"
            )
            .err(),
            Some(ConfigRejection::Empty { field: "subject" })
        );
        assert_eq!(
            ResourceServerConfig::new(
                "https://issuer.example",
                "https://issuer.example/jwks",
                "s",
                "",
                "https://mcp.example.com/mcp"
            )
            .err(),
            Some(ConfigRejection::Empty { field: "audience" })
        );
    }

    #[test]
    fn a_jwks_endpoint_outside_the_issuer_authority_is_refused() {
        assert_eq!(
            ResourceServerConfig::new(
                "https://issuer.example",
                "https://evil.example/jwks",
                "s",
                "a",
                "https://mcp.example.com/mcp"
            )
            .err(),
            Some(ConfigRejection::ForeignJwksEndpoint)
        );
    }

    #[test]
    fn the_metadata_document_advertises_the_external_https_url() {
        let config = config()
            .with_required_scopes(["godot.read", "godot.analyze"])
            .with_allowlist(
                AlgorithmAllowlist::narrowed([SignatureAlgorithm::Rs256]).expect("subset"),
            );
        let metadata = config.metadata();
        assert_eq!(metadata.resource(), "https://mcp.example.com/mcp");
        assert_eq!(
            metadata.authorization_servers(),
            ["https://issuer.example".to_owned()]
        );
        assert_eq!(
            config.metadata_url(),
            "https://mcp.example.com/.well-known/oauth-protected-resource"
        );
        let body = metadata.to_json();
        assert_eq!(
            body.get("resource").and_then(Value::as_str),
            Some("https://mcp.example.com/mcp")
        );
        assert_eq!(
            body.get("resource_signing_alg_values_supported"),
            Some(&json!(["RS256"]))
        );
        assert_eq!(
            body.get("scopes_supported"),
            Some(&json!(["godot.analyze", "godot.read"])),
            "scopes are sorted so the document is byte-stable"
        );
        assert_eq!(
            body.get("bearer_methods_supported"),
            Some(&json!(["header"]))
        );
        assert_eq!(metadata.to_body(), config.metadata().to_body());
    }

    #[test]
    fn the_document_advertises_no_issuance_endpoint() {
        let body = config().metadata().to_body();
        for banned in [
            "token_endpoint",
            "registration_endpoint",
            "authorization_endpoint",
            "grant_types_supported",
            "code_challenge_methods_supported",
        ] {
            assert!(
                !body.contains(banned),
                "{banned} must not appear: this is a resource server"
            );
        }
    }

    #[test]
    fn the_well_known_path_is_the_rfc_9728_one() {
        assert_eq!(
            PROTECTED_RESOURCE_METADATA_PATH,
            "/.well-known/oauth-protected-resource"
        );
    }
}
