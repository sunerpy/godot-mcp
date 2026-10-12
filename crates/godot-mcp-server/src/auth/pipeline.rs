//! Steps 4 and 5 of the ordered pipeline, and the boundary that runs all five.
//!
//! # Step 4 is the first moment any cryptography happens
//!
//! [`SignatureVerified::verify`] consumes a [`MetadataChecked`] — the token of
//! progress steps 1 through 3 produce — and only then computes a signature check,
//! using the key those steps validated and the algorithm step 1 validated. It
//! cannot be reached any other way: `MetadataChecked` has no public constructor,
//! and the only thing that produces one is [`super::jwks::KeySelected::check_metadata`],
//! which in turn needs an [`super::header::UnverifiedHeader`].
//!
//! Every real verification attempt increments a counter readable through
//! [`signature_checks`]. That turns "the rejection happened before any effective
//! signature check" from a claim into an assertion: the acceptance suite records the
//! counter, feeds in an `alg: none` token (or an HMAC substitution, a non-allowlisted
//! algorithm, a `crit` header, a token-supplied `jku`, an ambiguous `kid`, …), and
//! asserts the counter did not move.
//!
//! # Step 5 is the first moment any claim is read
//!
//! [`ClaimsValidated::validate`] consumes a [`SignatureVerified`]. The payload bytes
//! are carried through steps 1–4 *undecoded*: nothing before this point deserializes
//! them, so "claims are never trusted before the signature is checked" is a property
//! of where the parse happens, not a rule someone has to remember. An unsigned
//! attacker-chosen `iss` therefore cannot influence key selection, algorithm policy,
//! or which JWKS is fetched, because by the time it is legible the key has already
//! been fixed and the signature has already passed.
//!
//! Validation is exact and total: the issuer must equal the configured issuer, the
//! subject must equal the *one* configured subject, the audience must contain the
//! configured audience, `exp`/`nbf`/`iat` must be consistent with the clock inside
//! the configured leeway, and every required scope must be present.
//!
//! # Why the clock is injected
//!
//! [`OidcClock`] exists so expiry, not-yet-valid and issued-in-the-future cases are
//! deterministic. A test that had to sleep, or that pinned a timestamp into a
//! fixture, would either be slow or would start failing on a future date.
//!
//! # What reaches the kernel
//!
//! [`ClaimsValidated::principal`] is a [`ModePrincipal::Oidc`] holding exactly the
//! `(iss, sub)` pair — the shape Todo 27 froze for this transport mode. The token
//! itself stays inside [`TokenValue`], whose formatting is the redaction marker, and
//! is not part of the identity: nothing downstream, including a Godot process or an
//! editor plugin, can observe it.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock, Weak};

use serde_json::Value;

use super::b64;
use super::challenge::AuthRejection;
use super::header::{SignatureAlgorithm, UnverifiedHeader};
use super::hop::ProxyHop;
use super::jwks::{JwkSet, MetadataChecked};
use super::metadata::ResourceServerConfig;
use crate::idempotency::{ModePrincipal, TransportAuthMode};
use crate::observability::TokenValue;

/// How many real signature verifications this process has attempted.
///
/// Exists so a test can prove a rejection happened *before* any effective
/// signature check. Process-global and monotonic: a test records it, drives one
/// case, and compares.
static SIGNATURE_CHECKS: AtomicUsize = AtomicUsize::new(0);

/// The number of signature verifications attempted so far, process-wide.
#[must_use]
pub fn signature_checks() -> usize {
    SIGNATURE_CHECKS.load(Ordering::SeqCst)
}

/// A per-boundary count of real signature verifications.
///
/// The process-wide [`signature_checks`] counter cannot carry an assertion on its
/// own: several boundaries can be live at once (and a test suite runs cases in
/// parallel), so a delta observed across one case would include increments another
/// case caused. A probe scoped to one [`ResourceServer`] makes "this request
/// performed no signature check" exact.
#[derive(Debug, Clone, Default)]
pub struct SignatureProbe(std::sync::Arc<AtomicUsize>);

impl SignatureProbe {
    /// A probe that has observed nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many verifications this probe has observed.
    #[must_use]
    pub fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }

    fn record(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        SIGNATURE_CHECKS.fetch_add(1, Ordering::SeqCst);
    }
}

/// A monotonic source of "now", in seconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OidcClock {
    /// The real wall clock.
    System,
    /// A fixed instant, for deterministic `exp`/`nbf`/`iat` cases.
    Fixed(u64),
}

impl OidcClock {
    /// The current second.
    #[must_use]
    pub fn now(self) -> u64 {
        match self {
            Self::System => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs()),
            Self::Fixed(instant) => instant,
        }
    }
}

/// Why step 5 refused a token's claims.
///
/// The `Display` rendering is the internal label, not a message for a caller: the
/// wire form is the single indistinguishable challenge in [`super::challenge`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{}", self.as_str())]
pub enum ClaimRejection {
    /// The payload is not valid base64url, or not a JSON object.
    UndecodablePayload,
    /// A required claim is absent or has the wrong JSON type.
    MissingClaim,
    /// `iss` is not the configured issuer.
    WrongIssuer,
    /// `sub` is not the one configured subject.
    WrongSubject,
    /// `aud` does not name the configured audience.
    WrongAudience,
    /// `exp` is in the past.
    Expired,
    /// `nbf` is in the future.
    NotYetValid,
    /// `iat` is in the future.
    IssuedInFuture,
    /// A required scope is absent.
    InsufficientScope,
    /// The token names a capability profile this build does not serve.
    UnknownCapabilityProfile,
}

impl ClaimRejection {
    /// Whether this is an authorization failure (`403`) rather than an
    /// authentication failure (`401`).
    ///
    /// Only a token that is otherwise entirely valid but lacks authority earns a
    /// `403`; a wrong subject or a wrong audience is not "this user needs more
    /// scope", it is "this is not a token for this resource".
    #[must_use]
    pub const fn is_authorization_failure(self) -> bool {
        matches!(
            self,
            Self::InsufficientScope | Self::UnknownCapabilityProfile
        )
    }

    /// A stable snake_case label, for tests and the internal diagnostic sink.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UndecodablePayload => "undecodable_payload",
            Self::MissingClaim => "missing_claim",
            Self::WrongIssuer => "wrong_issuer",
            Self::WrongSubject => "wrong_subject",
            Self::WrongAudience => "wrong_audience",
            Self::Expired => "expired",
            Self::NotYetValid => "not_yet_valid",
            Self::IssuedInFuture => "issued_in_future",
            Self::InsufficientScope => "insufficient_scope",
            Self::UnknownCapabilityProfile => "unknown_capability_profile",
        }
    }
}

/// The output of step 4: a token whose signature verified under the validated key
/// and the validated algorithm.
///
/// The payload is still raw bytes here. That is deliberate — it is what makes
/// "claims are not read before step 4 completes" structural.
#[derive(Debug)]
pub struct SignatureVerified {
    algorithm: SignatureAlgorithm,
    kid: String,
    payload: String,
}

impl SignatureVerified {
    /// Step 4. Verify `token`'s signature with the step-3 key.
    ///
    /// Consuming `key` by value means the caller cannot hold a `MetadataChecked`
    /// and retry it against a different algorithm after a failure: a bounded
    /// old/new key overlap works by each key being selected on its own exact
    /// `kid`, never by falling back after a verification failure.
    ///
    /// # Errors
    /// Returns [`AuthRejection::Signature`] when the signature does not verify, and
    /// [`AuthRejection::Claims`] with [`ClaimRejection::UndecodablePayload`] when the
    /// token is not three parts.
    pub fn verify(
        token: &str,
        key: MetadataChecked<'_>,
        probe: &SignatureProbe,
    ) -> Result<Self, AuthRejection> {
        let mut parts = token.rsplitn(2, '.');
        let (Some(signature), Some(message)) = (parts.next(), parts.next()) else {
            return Err(AuthRejection::Claims(ClaimRejection::UndecodablePayload));
        };
        let payload = message
            .split_once('.')
            .map(|(_, payload)| payload.to_owned())
            .ok_or(AuthRejection::Claims(ClaimRejection::UndecodablePayload))?;

        probe.record();
        let verified = jsonwebtoken::crypto::verify(
            signature,
            message.as_bytes(),
            key.decoding_key(),
            key.algorithm().jwt_algorithm(),
        )
        .unwrap_or(false);
        if !verified {
            return Err(AuthRejection::Signature);
        }
        Ok(Self {
            algorithm: key.algorithm(),
            kid: key.kid().to_owned(),
            payload,
        })
    }

    /// The algorithm the signature was verified under.
    #[must_use]
    pub const fn algorithm(&self) -> SignatureAlgorithm {
        self.algorithm
    }

    /// The `kid` of the key that verified it.
    #[must_use]
    pub fn kid(&self) -> &str {
        &self.kid
    }
}

/// The output of step 5: a token whose claims satisfy the frozen configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimsValidated {
    principal: ModePrincipal,
    scopes: BTreeSet<String>,
    algorithm: SignatureAlgorithm,
    kid: String,
}

impl ClaimsValidated {
    /// Step 5. Read and validate the claims of an already-verified token.
    ///
    /// # Errors
    /// Returns [`AuthRejection::Claims`] with the [`ClaimRejection`] that stopped it.
    pub fn validate(
        verified: SignatureVerified,
        config: &ResourceServerConfig,
        clock: OidcClock,
    ) -> Result<Self, AuthRejection> {
        let claim_error = |rejection: ClaimRejection| AuthRejection::Claims(rejection);
        let decoded = b64::decode(&verified.payload)
            .ok_or_else(|| claim_error(ClaimRejection::UndecodablePayload))?;
        let claims: Value = serde_json::from_slice(&decoded)
            .map_err(|_| claim_error(ClaimRejection::UndecodablePayload))?;
        let claims = claims
            .as_object()
            .ok_or_else(|| claim_error(ClaimRejection::UndecodablePayload))?;

        let issuer = claims
            .get("iss")
            .and_then(Value::as_str)
            .ok_or_else(|| claim_error(ClaimRejection::MissingClaim))?;
        if issuer.trim_end_matches('/') != config.issuer() {
            return Err(claim_error(ClaimRejection::WrongIssuer));
        }
        let subject = claims
            .get("sub")
            .and_then(Value::as_str)
            .ok_or_else(|| claim_error(ClaimRejection::MissingClaim))?;
        if subject != config.subject() {
            return Err(claim_error(ClaimRejection::WrongSubject));
        }
        if !audience_contains(claims.get("aud"), config.audience()) {
            return Err(claim_error(ClaimRejection::WrongAudience));
        }

        let now = clock.now();
        let leeway = config.leeway_seconds();
        let expiry = claims
            .get("exp")
            .and_then(Value::as_u64)
            .ok_or_else(|| claim_error(ClaimRejection::MissingClaim))?;
        if expiry.saturating_add(leeway) <= now {
            return Err(claim_error(ClaimRejection::Expired));
        }
        if let Some(not_before) = claims.get("nbf").and_then(Value::as_u64)
            && not_before > now.saturating_add(leeway)
        {
            return Err(claim_error(ClaimRejection::NotYetValid));
        }
        if let Some(issued_at) = claims.get("iat").and_then(Value::as_u64)
            && issued_at > now.saturating_add(leeway)
        {
            return Err(claim_error(ClaimRejection::IssuedInFuture));
        }

        let scopes = parse_scopes(claims.get("scope"));
        if !config.required_scopes().is_subset(&scopes) {
            return Err(claim_error(ClaimRejection::InsufficientScope));
        }

        Ok(Self {
            principal: ModePrincipal::Oidc {
                issuer: config.issuer().to_owned(),
                subject: config.subject().to_owned(),
            },
            scopes,
            algorithm: verified.algorithm,
            kid: verified.kid,
        })
    }

    /// The `(iss, sub)` principal, in the shape Todo 27 froze for this mode.
    #[must_use]
    pub const fn principal(&self) -> &ModePrincipal {
        &self.principal
    }

    /// The transport mode this principal belongs to. Always `proxied_oidc`.
    #[must_use]
    pub const fn mode(&self) -> TransportAuthMode {
        TransportAuthMode::ProxiedOidc
    }

    /// The scopes the token carried.
    #[must_use]
    pub const fn scopes(&self) -> &BTreeSet<String> {
        &self.scopes
    }

    /// The algorithm the token was verified under.
    #[must_use]
    pub const fn algorithm(&self) -> SignatureAlgorithm {
        self.algorithm
    }

    /// The `kid` that verified it, which is how a rotation case observes *which*
    /// key of an overlapping pair was used.
    #[must_use]
    pub fn kid(&self) -> &str {
        &self.kid
    }
}

fn audience_contains(claim: Option<&Value>, expected: &str) -> bool {
    match claim {
        Some(Value::String(single)) => single == expected,
        Some(Value::Array(entries)) => entries
            .iter()
            .any(|entry| entry.as_str().is_some_and(|value| value == expected)),
        _ => false,
    }
}

fn parse_scopes(claim: Option<&Value>) -> BTreeSet<String> {
    match claim {
        Some(Value::String(text)) => text.split_whitespace().map(str::to_owned).collect(),
        Some(Value::Array(entries)) => entries
            .iter()
            .filter_map(|entry| entry.as_str().map(str::to_owned))
            .collect(),
        _ => BTreeSet::new(),
    }
}

/// The `proxied_oidc` boundary: proxy-hop identity plus the five-step pipeline.
///
/// One of these is built per launch from validated configuration and an
/// ACL-verified hop credential. It holds the fetched JWKS, which
/// [`ResourceServer::install_keys`] replaces when keys rotate; a bounded old/new
/// overlap is simply a set holding both, each selected by its own exact `kid`.
#[derive(Debug)]
pub struct ResourceServer {
    config: ResourceServerConfig,
    hop: ProxyHop,
    keys: Arc<RwLock<JwkSet>>,
    clock: OidcClock,
    probe: SignatureProbe,
}

impl ResourceServer {
    /// Assemble a boundary from configuration, a hop credential and a fetched JWKS.
    #[must_use]
    pub fn new(
        config: ResourceServerConfig,
        hop: ProxyHop,
        keys: JwkSet,
        clock: OidcClock,
    ) -> Self {
        Self {
            config,
            hop,
            keys: Arc::new(RwLock::new(keys)),
            clock,
            probe: SignatureProbe::new(),
        }
    }

    /// How many signature verifications *this* boundary has performed.
    #[must_use]
    pub fn signature_checks(&self) -> usize {
        self.probe.count()
    }

    /// The frozen configuration.
    #[must_use]
    pub const fn config(&self) -> &ResourceServerConfig {
        &self.config
    }

    /// Replace the key set, for a rotation.
    ///
    /// Rotation never relaxes anything: a token still has to name an existing `kid`
    /// exactly, and a failure under one key is never retried under another.
    pub fn install_keys(&self, keys: JwkSet) {
        *self
            .keys
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = keys;
    }

    /// The key set currently in use.
    #[must_use]
    pub fn keys(&self) -> JwkSet {
        self.keys
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn weak_keys(&self) -> Weak<RwLock<JwkSet>> {
        Arc::downgrade(&self.keys)
    }

    /// Authenticate one request: layer 1 (the hop), then layer 2 (the token).
    ///
    /// Both must pass. The hop is checked first because an unauthenticated hop means
    /// the request did not come from the proxy at all, and in that case no amount of
    /// token validity should let it through — nor should the server spend a
    /// signature check on it.
    ///
    /// `token` is taken as [`TokenValue`] so it cannot be logged by accident on any
    /// path through this function.
    ///
    /// # Errors
    /// Returns the [`AuthRejection`] that stopped it. Every variant renders
    /// identically to a caller; see [`super::challenge`].
    pub fn authenticate(
        &self,
        hop_credential: Option<&str>,
        token: Option<&TokenValue>,
    ) -> Result<ClaimsValidated, AuthRejection> {
        self.hop
            .authenticate(hop_credential)
            .map_err(AuthRejection::Hop)?;
        let token = token.ok_or(AuthRejection::Header(
            super::header::HeaderRejection::Malformed,
        ))?;
        let token = token.expose();

        // Step 1: header policy. Nothing has been fetched or verified yet.
        let header = UnverifiedHeader::parse(token, self.config.allowlist())
            .map_err(AuthRejection::Header)?;
        // Step 2: exact kid match against the server-fetched key set.
        let checked = {
            let keys = self
                .keys
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let selected = keys.select(header).map_err(AuthRejection::Key)?;
            // Step 3: the key's own metadata must agree with the validated algorithm.
            selected.check_metadata().map_err(AuthRejection::Key)?
        };
        // Step 4: the first cryptography.
        let verified = SignatureVerified::verify(token, checked, &self.probe)?;
        // Step 5: the first time any claim is legible.
        ClaimsValidated::validate(verified, &self.config, self.clock)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::header::{AlgorithmAllowlist, HeaderRejection};
    use crate::auth::hop::HopCredential;
    use crate::auth::jwks::KeyRejection;
    use crate::auth::testkeys;

    const NOW: u64 = 1_700_000_000;

    fn config() -> ResourceServerConfig {
        ResourceServerConfig::new(
            "https://issuer.example",
            "https://issuer.example/jwks",
            "subject-1",
            "https://mcp.example.com/mcp",
            "https://mcp.example.com/mcp",
        )
        .expect("valid configuration")
        .with_required_scopes(["godot.read"])
    }

    fn claims() -> Value {
        serde_json::json!({
            "iss": "https://issuer.example",
            "sub": "subject-1",
            "aud": "https://mcp.example.com/mcp",
            "exp": NOW + 300,
            "nbf": NOW - 10,
            "iat": NOW - 10,
            "scope": "godot.read godot.analyze",
        })
    }

    struct Fixture {
        server: ResourceServer,
        credential: String,
        _dir: std::path::PathBuf,
    }

    /// Distinct scratch directories per fixture. Tests run in parallel, so a name
    /// derived from anything shared would let two fixtures overwrite one credential.
    static SCRATCH_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    fn scratch(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "godot-mcp-pipeline-{}-{label}-{}",
            std::process::id(),
            SCRATCH_SEQUENCE.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn fixture(keys: &[testkeys::TestKey]) -> Fixture {
        let dir = scratch("boundary");
        let path = dir.join("hop-credential");
        let credential = ProxyHop::publish(&path).expect("publish");
        let hop = ProxyHop::load(HopCredential::File { path }).expect("load");
        let keys = JwkSet::parse(&testkeys::jwks_document(keys)).expect("parse");
        Fixture {
            server: ResourceServer::new(config(), hop, keys, OidcClock::Fixed(NOW)),
            credential,
            _dir: dir,
        }
    }

    #[test]
    fn a_valid_hop_and_token_resolve_to_the_configured_principal() {
        let fixture = fixture(&[testkeys::RSA_OLD, testkeys::RSA_NEW]);
        let token = TokenValue::new(testkeys::sign(
            &testkeys::RSA_OLD,
            SignatureAlgorithm::Rs256,
            Some(testkeys::RSA_OLD.kid),
            &claims(),
        ));
        let validated = fixture
            .server
            .authenticate(Some(&fixture.credential), Some(&token))
            .expect("authenticated");
        assert_eq!(
            validated.principal(),
            &ModePrincipal::Oidc {
                issuer: "https://issuer.example".to_owned(),
                subject: "subject-1".to_owned(),
            }
        );
        assert_eq!(validated.mode(), TransportAuthMode::ProxiedOidc);
        assert_eq!(validated.kid(), testkeys::RSA_OLD.kid);
        assert_eq!(validated.algorithm(), SignatureAlgorithm::Rs256);
        assert!(validated.scopes().contains("godot.read"));
    }

    #[test]
    fn both_keys_of_an_overlapping_rotation_work_on_their_own_kid() {
        let fixture = fixture(&[testkeys::RSA_OLD, testkeys::RSA_NEW]);
        for key in [testkeys::RSA_OLD, testkeys::RSA_NEW] {
            let token = TokenValue::new(testkeys::sign(
                &key,
                SignatureAlgorithm::Rs256,
                Some(key.kid),
                &claims(),
            ));
            let validated = fixture
                .server
                .authenticate(Some(&fixture.credential), Some(&token))
                .expect("authenticated");
            assert_eq!(validated.kid(), key.kid);
        }
    }

    #[test]
    fn an_es256_token_verifies_against_its_p256_key() {
        let dir = scratch("es256");
        let path = dir.join("hop-credential");
        let credential = ProxyHop::publish(&path).expect("publish");
        let hop = ProxyHop::load(HopCredential::File { path }).expect("load");
        let keys = JwkSet::parse(&testkeys::jwks_document(&[testkeys::EC_P256])).expect("parse");
        let server = ResourceServer::new(config(), hop, keys, OidcClock::Fixed(NOW));
        assert_eq!(server.signature_checks(), 0);
        let token = TokenValue::new(testkeys::sign(
            &testkeys::EC_P256,
            SignatureAlgorithm::Es256,
            Some(testkeys::EC_P256.kid),
            &claims(),
        ));
        let validated = server
            .authenticate(Some(&credential), Some(&token))
            .expect("authenticated");
        assert_eq!(validated.algorithm(), SignatureAlgorithm::Es256);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_rs256_token_verifies_against_its_rsa_key() {
        let fixture = fixture(&[testkeys::RSA_OLD]);
        assert_eq!(fixture.server.signature_checks(), 0);
        let token = TokenValue::new(testkeys::sign(
            &testkeys::RSA_OLD,
            SignatureAlgorithm::Rs256,
            Some(testkeys::RSA_OLD.kid),
            &claims(),
        ));
        let validated = fixture
            .server
            .authenticate(Some(&fixture.credential), Some(&token))
            .expect("authenticated");
        assert_eq!(validated.algorithm(), SignatureAlgorithm::Rs256);
        assert_eq!(validated.kid(), testkeys::RSA_OLD.kid);
        assert_eq!(fixture.server.signature_checks(), 1);
    }

    #[test]
    fn a_ps256_token_verifies_against_its_rsa_key() {
        let fixture = fixture(&[testkeys::RSA_PSS]);
        assert_eq!(fixture.server.signature_checks(), 0);
        let token = TokenValue::new(testkeys::sign(
            &testkeys::RSA_PSS,
            SignatureAlgorithm::Ps256,
            Some(testkeys::RSA_PSS.kid),
            &claims(),
        ));
        let validated = fixture
            .server
            .authenticate(Some(&fixture.credential), Some(&token))
            .expect("authenticated");
        assert_eq!(validated.algorithm(), SignatureAlgorithm::Ps256);
        assert_eq!(validated.kid(), testkeys::RSA_PSS.kid);
        assert_eq!(fixture.server.signature_checks(), 1);
    }

    #[test]
    fn a_header_policy_rejection_performs_no_signature_check() {
        let fixture = fixture(&[testkeys::RSA_OLD]);
        let cases: Vec<(Value, HeaderRejection)> = vec![
            (
                serde_json::json!({"alg": "none", "kid": "key-old"}),
                HeaderRejection::NoneAlgorithm,
            ),
            (
                serde_json::json!({"alg": "HS256", "kid": "key-old"}),
                HeaderRejection::HmacAlgorithm,
            ),
            (
                serde_json::json!({"alg": "RS512", "kid": "key-old"}),
                HeaderRejection::NonAllowlistedAlgorithm,
            ),
            (
                serde_json::json!({"alg": "RS256", "kid": "key-old", "crit": ["b64"]}),
                HeaderRejection::CriticalHeader,
            ),
            (
                serde_json::json!({"alg": "RS256", "kid": "key-old", "jku": "https://evil.example/jwks"}),
                HeaderRejection::JkuHeader,
            ),
            (
                serde_json::json!({"alg": "RS256", "kid": "key-old", "x5u": "https://evil.example/c"}),
                HeaderRejection::X5uHeader,
            ),
            (
                serde_json::json!({"alg": "RS256", "kid": "key-old", "x5c": ["MIIB"]}),
                HeaderRejection::EmbeddedX5c,
            ),
            (
                serde_json::json!({"alg": "RS256"}),
                HeaderRejection::MissingKid,
            ),
        ];
        let before = fixture.server.signature_checks();
        for (header, expected) in cases {
            let token = TokenValue::new(testkeys::sign_with_literal_header(
                &testkeys::RSA_OLD,
                &header,
                &claims(),
            ));
            assert_eq!(
                fixture
                    .server
                    .authenticate(Some(&fixture.credential), Some(&token))
                    .err(),
                Some(AuthRejection::Header(expected)),
                "{header} must be refused in step 1"
            );
        }
        assert_eq!(
            fixture.server.signature_checks(),
            before,
            "a header-policy rejection must never reach a signature check"
        );
    }

    #[test]
    fn an_unknown_kid_performs_no_signature_check() {
        let fixture = fixture(&[testkeys::RSA_OLD]);
        let before = fixture.server.signature_checks();
        let token = TokenValue::new(testkeys::sign(
            &testkeys::RSA_NEW,
            SignatureAlgorithm::Rs256,
            Some("key-absent"),
            &claims(),
        ));
        assert_eq!(
            fixture
                .server
                .authenticate(Some(&fixture.credential), Some(&token))
                .err(),
            Some(AuthRejection::Key(KeyRejection::UnknownKid))
        );
        assert_eq!(fixture.server.signature_checks(), before);
    }

    #[test]
    fn key_confusion_is_refused_before_a_signature_check() {
        // An ES256 header against an RSA key: step 3 must stop it.
        let fixture = fixture(&[testkeys::RSA_OLD]);
        let before = fixture.server.signature_checks();
        let token = TokenValue::new(testkeys::sign_with_literal_header(
            &testkeys::EC_P256,
            &serde_json::json!({"alg": "ES256", "kid": "key-old"}),
            &claims(),
        ));
        assert_eq!(
            fixture
                .server
                .authenticate(Some(&fixture.credential), Some(&token))
                .err(),
            Some(AuthRejection::Key(KeyRejection::KeyTypeMismatch))
        );
        assert_eq!(fixture.server.signature_checks(), before);
    }

    #[test]
    fn a_hop_failure_is_refused_before_any_token_work() {
        let fixture = fixture(&[testkeys::RSA_OLD]);
        let token = TokenValue::new(testkeys::sign(
            &testkeys::RSA_OLD,
            SignatureAlgorithm::Rs256,
            Some(testkeys::RSA_OLD.kid),
            &claims(),
        ));
        let before = fixture.server.signature_checks();
        assert!(matches!(
            fixture.server.authenticate(None, Some(&token)),
            Err(AuthRejection::Hop(_))
        ));
        assert!(matches!(
            fixture
                .server
                .authenticate(Some("0".repeat(64).as_str()), Some(&token)),
            Err(AuthRejection::Hop(_))
        ));
        assert_eq!(
            fixture.server.signature_checks(),
            before,
            "an unauthenticated hop must not spend a signature check"
        );
    }

    #[test]
    fn a_wrong_signature_is_refused_after_exactly_one_check() {
        let fixture = fixture(&[testkeys::RSA_OLD]);
        // Signed by the *new* key but naming the old key's kid: the selected key is
        // genuine, so this reaches step 4 and fails there.
        let token = TokenValue::new(testkeys::sign(
            &testkeys::RSA_NEW,
            SignatureAlgorithm::Rs256,
            Some(testkeys::RSA_OLD.kid),
            &claims(),
        ));
        let before = fixture.server.signature_checks();
        assert_eq!(
            fixture
                .server
                .authenticate(Some(&fixture.credential), Some(&token))
                .err(),
            Some(AuthRejection::Signature)
        );
        assert_eq!(fixture.server.signature_checks(), before + 1);
    }

    #[test]
    fn claims_are_validated_exactly() {
        let fixture = fixture(&[testkeys::RSA_OLD]);
        let cases: Vec<(Value, ClaimRejection)> = vec![
            (
                serde_json::json!({"iss": "https://elsewhere.example", "sub": "subject-1", "aud": "https://mcp.example.com/mcp", "exp": NOW + 300, "scope": "godot.read"}),
                ClaimRejection::WrongIssuer,
            ),
            (
                serde_json::json!({"iss": "https://issuer.example", "sub": "subject-2", "aud": "https://mcp.example.com/mcp", "exp": NOW + 300, "scope": "godot.read"}),
                ClaimRejection::WrongSubject,
            ),
            (
                serde_json::json!({"iss": "https://issuer.example", "sub": "subject-1", "aud": "https://other.example/mcp", "exp": NOW + 300, "scope": "godot.read"}),
                ClaimRejection::WrongAudience,
            ),
            (
                serde_json::json!({"iss": "https://issuer.example", "sub": "subject-1", "exp": NOW + 300, "scope": "godot.read"}),
                ClaimRejection::WrongAudience,
            ),
            (
                serde_json::json!({"iss": "https://issuer.example", "sub": "subject-1", "aud": "https://mcp.example.com/mcp", "exp": NOW - 1, "scope": "godot.read"}),
                ClaimRejection::Expired,
            ),
            (
                serde_json::json!({"iss": "https://issuer.example", "sub": "subject-1", "aud": "https://mcp.example.com/mcp", "exp": NOW + 300, "nbf": NOW + 60, "scope": "godot.read"}),
                ClaimRejection::NotYetValid,
            ),
            (
                serde_json::json!({"iss": "https://issuer.example", "sub": "subject-1", "aud": "https://mcp.example.com/mcp", "exp": NOW + 300, "iat": NOW + 60, "scope": "godot.read"}),
                ClaimRejection::IssuedInFuture,
            ),
            (
                serde_json::json!({"iss": "https://issuer.example", "sub": "subject-1", "aud": "https://mcp.example.com/mcp", "exp": NOW + 300, "scope": "godot.analyze"}),
                ClaimRejection::InsufficientScope,
            ),
            (
                serde_json::json!({"iss": "https://issuer.example", "sub": "subject-1", "aud": "https://mcp.example.com/mcp", "exp": NOW + 300}),
                ClaimRejection::InsufficientScope,
            ),
            (
                serde_json::json!({"sub": "subject-1", "aud": "https://mcp.example.com/mcp", "exp": NOW + 300}),
                ClaimRejection::MissingClaim,
            ),
            (
                serde_json::json!({"iss": "https://issuer.example", "aud": "https://mcp.example.com/mcp", "exp": NOW + 300}),
                ClaimRejection::MissingClaim,
            ),
            (
                serde_json::json!({"iss": "https://issuer.example", "sub": "subject-1", "aud": "https://mcp.example.com/mcp"}),
                ClaimRejection::MissingClaim,
            ),
        ];
        for (claims, expected) in cases {
            let token = TokenValue::new(testkeys::sign(
                &testkeys::RSA_OLD,
                SignatureAlgorithm::Rs256,
                Some(testkeys::RSA_OLD.kid),
                &claims,
            ));
            assert_eq!(
                fixture
                    .server
                    .authenticate(Some(&fixture.credential), Some(&token))
                    .err(),
                Some(AuthRejection::Claims(expected)),
                "{claims} must be refused"
            );
        }
    }

    #[test]
    fn an_audience_array_and_a_scope_array_are_accepted() {
        let fixture = fixture(&[testkeys::RSA_OLD]);
        let token = TokenValue::new(testkeys::sign(
            &testkeys::RSA_OLD,
            SignatureAlgorithm::Rs256,
            Some(testkeys::RSA_OLD.kid),
            &serde_json::json!({
                "iss": "https://issuer.example/",
                "sub": "subject-1",
                "aud": ["https://other.example", "https://mcp.example.com/mcp"],
                "exp": NOW + 300,
                "scope": ["godot.read", "godot.analyze"],
            }),
        ));
        let validated = fixture
            .server
            .authenticate(Some(&fixture.credential), Some(&token))
            .expect("authenticated");
        assert!(validated.scopes().contains("godot.analyze"));
    }

    #[test]
    fn leeway_is_applied_to_expiry_and_not_before() {
        let dir = scratch("leeway");
        let path = dir.join("hop-credential");
        let credential = ProxyHop::publish(&path).expect("publish");
        let hop = ProxyHop::load(HopCredential::File { path }).expect("load");
        let keys = JwkSet::parse(&testkeys::jwks_document(&[testkeys::RSA_OLD])).expect("parse");
        let server = ResourceServer::new(
            config().with_leeway_seconds(120),
            hop,
            keys,
            OidcClock::Fixed(NOW),
        );
        let token = TokenValue::new(testkeys::sign(
            &testkeys::RSA_OLD,
            SignatureAlgorithm::Rs256,
            Some(testkeys::RSA_OLD.kid),
            &serde_json::json!({
                "iss": "https://issuer.example",
                "sub": "subject-1",
                "aud": "https://mcp.example.com/mcp",
                "exp": NOW - 30,
                "nbf": NOW + 30,
                "scope": "godot.read",
            }),
        ));
        assert!(
            server.authenticate(Some(&credential), Some(&token)).is_ok(),
            "a token inside the configured leeway is accepted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_replaces_the_key_set_without_relaxing_selection() {
        let fixture = fixture(&[testkeys::RSA_OLD, testkeys::RSA_NEW]);
        assert_eq!(fixture.server.keys().len(), 2);
        fixture.server.install_keys(
            JwkSet::parse(&testkeys::jwks_document(&[testkeys::RSA_NEW])).expect("parse"),
        );
        assert_eq!(fixture.server.keys().len(), 1);
        // The retired key's kid is now unknown, not "tried and failed".
        let retired = TokenValue::new(testkeys::sign(
            &testkeys::RSA_OLD,
            SignatureAlgorithm::Rs256,
            Some(testkeys::RSA_OLD.kid),
            &claims(),
        ));
        assert_eq!(
            fixture
                .server
                .authenticate(Some(&fixture.credential), Some(&retired))
                .err(),
            Some(AuthRejection::Key(KeyRejection::UnknownKid))
        );
        let current = TokenValue::new(testkeys::sign(
            &testkeys::RSA_NEW,
            SignatureAlgorithm::Rs256,
            Some(testkeys::RSA_NEW.kid),
            &claims(),
        ));
        assert!(
            fixture
                .server
                .authenticate(Some(&fixture.credential), Some(&current))
                .is_ok()
        );
    }

    #[test]
    fn a_narrowed_allowlist_refuses_an_otherwise_valid_algorithm() {
        let dir = scratch("narrow");
        let path = dir.join("hop-credential");
        let credential = ProxyHop::publish(&path).expect("publish");
        let hop = ProxyHop::load(HopCredential::File { path }).expect("load");
        let keys = JwkSet::parse(&testkeys::jwks_document(&[testkeys::EC_P256])).expect("parse");
        let server = ResourceServer::new(
            config().with_allowlist(
                AlgorithmAllowlist::narrowed([SignatureAlgorithm::Rs256]).expect("subset"),
            ),
            hop,
            keys,
            OidcClock::Fixed(NOW),
        );
        let token = TokenValue::new(testkeys::sign(
            &testkeys::EC_P256,
            SignatureAlgorithm::Es256,
            Some(testkeys::EC_P256.kid),
            &claims(),
        ));
        assert_eq!(
            server.authenticate(Some(&credential), Some(&token)).err(),
            Some(AuthRejection::Header(HeaderRejection::UnselectedAlgorithm))
        );
        assert_eq!(
            server.signature_checks(),
            0,
            "an unselected algorithm is refused in step 1, before any signature check"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_token_is_refused_without_a_signature_check() {
        let fixture = fixture(&[testkeys::RSA_OLD]);
        let before = fixture.server.signature_checks();
        assert_eq!(
            fixture
                .server
                .authenticate(Some(&fixture.credential), None)
                .err(),
            Some(AuthRejection::Header(HeaderRejection::Malformed))
        );
        assert_eq!(fixture.server.signature_checks(), before);
    }

    #[test]
    fn the_clock_and_rejection_labels_are_stable() {
        assert_eq!(OidcClock::Fixed(42).now(), 42);
        assert!(OidcClock::System.now() > 1_600_000_000);
        assert!(ClaimRejection::InsufficientScope.is_authorization_failure());
        assert!(ClaimRejection::UnknownCapabilityProfile.is_authorization_failure());
        for rejection in [
            ClaimRejection::WrongIssuer,
            ClaimRejection::WrongSubject,
            ClaimRejection::WrongAudience,
            ClaimRejection::Expired,
            ClaimRejection::NotYetValid,
            ClaimRejection::IssuedInFuture,
            ClaimRejection::MissingClaim,
            ClaimRejection::UndecodablePayload,
        ] {
            assert!(!rejection.is_authorization_failure(), "{rejection:?}");
        }
        assert_eq!(ClaimRejection::WrongSubject.as_str(), "wrong_subject");
        assert_eq!(ClaimRejection::WrongAudience.as_str(), "wrong_audience");
        assert_eq!(ClaimRejection::Expired.as_str(), "expired");
        assert_eq!(ClaimRejection::NotYetValid.as_str(), "not_yet_valid");
        assert_eq!(ClaimRejection::IssuedInFuture.as_str(), "issued_in_future");
        assert_eq!(ClaimRejection::MissingClaim.as_str(), "missing_claim");
        assert_eq!(
            ClaimRejection::UndecodablePayload.as_str(),
            "undecodable_payload"
        );
        assert_eq!(ClaimRejection::WrongIssuer.as_str(), "wrong_issuer");
        assert_eq!(
            ClaimRejection::InsufficientScope.as_str(),
            "insufficient_scope"
        );
        assert_eq!(
            ClaimRejection::UnknownCapabilityProfile.as_str(),
            "unknown_capability_profile"
        );
    }

    #[test]
    fn a_validated_identity_never_renders_the_token() {
        let fixture = fixture(&[testkeys::RSA_OLD]);
        let raw = testkeys::sign(
            &testkeys::RSA_OLD,
            SignatureAlgorithm::Rs256,
            Some(testkeys::RSA_OLD.kid),
            &claims(),
        );
        let token = TokenValue::new(raw.clone());
        assert_eq!(format!("{token}"), "[REDACTED]");
        let validated = fixture
            .server
            .authenticate(Some(&fixture.credential), Some(&token))
            .expect("authenticated");
        let rendered = format!("{validated:?}");
        assert!(
            !rendered.contains(&raw),
            "the validated identity must not carry the token"
        );
        let server_rendered = format!("{:?}", fixture.server);
        assert!(
            !server_rendered.contains(&fixture.credential),
            "the hop credential must never be rendered"
        );
    }
}
