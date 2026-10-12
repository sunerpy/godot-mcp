//! The `proxied_oidc` boundary: proxy-hop identity plus the OIDC Resource Server.
//!
//! # Two independent layers, both required on every request
//!
//! `proxied_oidc` is the only mode that may be reached from outside the host, and
//! it is reached only through a same-host HTTPS reverse proxy. It completes when
//! **both** of the following pass, per request, in this order:
//!
//! 1. **Proxy-hop authentication** ([`hop`]) — the hop presents a dedicated
//!    high-entropy credential read from an ACL-protected file, or connects over an
//!    ACL-protected Unix socket / named pipe. The credential is compared in
//!    constant time.
//! 2. **End-user OIDC validation** ([`pipeline`]) — the bearer token is validated
//!    through the five-step fail-closed pipeline below, and must resolve to the one
//!    configured `(iss, sub)` principal.
//!
//! A loopback peer address authenticates *nothing*. Any local process can open a
//! loopback connection, and a proxy can assert any header it likes; neither is an
//! identity, so neither appears in the decision.
//!
//! # The five-step ordered pipeline
//!
//! Every token walks these steps in this exact order. A rejection at any step means
//! the later steps never run, and every value read from the header before signature
//! verification is a *match constraint to check against server configuration* —
//! never authority that selects a URL, a key, or an algorithm policy:
//!
//! 1. [`header::UnverifiedHeader::parse`] — refuse `alg: none`, every HMAC
//!    algorithm, anything outside the deployment-selected subset of
//!    `{RS256, PS256, ES256}`, and any header carrying `crit`, `jku`, `x5u`,
//!    embedded `x5c` or embedded `jwk`. Require a non-empty `kid`.
//! 2. [`jwks::JwkSet::select`] — fetch the JWKS *only* from the fixed
//!    server-configured endpoint, and select by exact `kid` match against it.
//!    Zero, ambiguous or unknown `kid` is refused unconditionally.
//! 3. [`jwks::KeySelected::check_metadata`] — `kty`/`crv`/`alg`/`use`/`key_ops`
//!    must agree with the algorithm step 1 validated, *before* the key is used.
//! 4. [`pipeline::SignatureVerified`] — only now is the signature checked, with
//!    the validated key and the validated algorithm.
//! 5. [`pipeline::ClaimsValidated`] — only after a successful signature check are
//!    the claims read at all: issuer, `(iss, sub)`, audience, `exp`/`nbf`/`iat`,
//!    scopes and profile.
//!
//! # The order is a property of the types, not a convention
//!
//! Each step consumes the previous step's token of progress and no step's output
//! has a public constructor. `ClaimsValidated` can only be produced from a
//! `SignatureVerified`, which can only be produced from a `MetadataChecked`, which
//! can only be produced from a `KeySelected`, which can only be produced from an
//! `UnverifiedHeader`. Skipping a step is unrepresentable rather than merely
//! discouraged, and [`pipeline::signature_checks`] lets a test assert that a
//! header-policy rejection performed *zero* signature checks.
//!
//! # Nothing here is logged, and nothing here is forwarded
//!
//! Credentials and tokens are held in Todo 4's redacting wrappers
//! ([`crate::observability::SecretValue`] / [`crate::observability::TokenValue`]),
//! whose `Debug`/`Display` are the redaction marker. The kernel receives the
//! resolved principal only — never the token, never the hop credential — so a
//! Godot process or an editor plugin can never observe either.
//!
//! # What this module is not
//!
//! There is no login, consent, PKCE, client registration, token issuance or
//! refresh here, and no user account is persisted: this is a Resource Server. It
//! validates tokens an identity provider already issued, and it accepts exactly one
//! subject.

pub mod b64;
pub mod challenge;
pub mod header;
pub mod hop;
pub mod jwks;
pub mod metadata;
pub mod pipeline;
pub mod resolver;
pub mod startup;
#[cfg(any(test, feature = "test-hooks"))]
pub mod testkeys;

pub use challenge::{AuthChallenge, AuthRejection};
pub use header::{
    AlgorithmAllowlist, HARD_ALLOWLIST, HeaderRejection, SignatureAlgorithm, UnverifiedHeader,
};
pub use hop::{HopCredential, HopRejection, ProxyHop};
pub use jwks::{Jwk, JwkSet, KeyRejection, KeySelected, MetadataChecked};
pub use metadata::{
    ConfigRejection, PROTECTED_RESOURCE_METADATA_PATH, ProtectedResourceMetadata,
    ResourceServerConfig,
};
pub use pipeline::{
    ClaimRejection, ClaimsValidated, OidcClock, ResourceServer, SignatureProbe, SignatureVerified,
    signature_checks,
};
pub use resolver::{DiscoveryClient, SsrfRejection, SystemResolver, VettedEndpoint, vet_address};
pub use startup::{ResourceServerStartupError, build_resource_server};
