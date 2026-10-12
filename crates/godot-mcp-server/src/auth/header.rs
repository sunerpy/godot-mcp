//! Step 1 of the ordered pipeline: the JWT header policy.
//!
//! # Everything here runs before a key is chosen and before a byte is verified
//!
//! A JWT header is attacker-controlled. It is read here *only* to produce match
//! constraints that server-side configuration must then satisfy — never as
//! authority over which key, which JWKS, or which algorithm policy applies. That
//! distinction is what defeats the classic JOSE attacks:
//!
//! * `alg: none` and the HMAC family are refused outright, so a token cannot ask
//!   to be verified with no key or with a public key repurposed as a shared
//!   secret. This holds under every configuration: [`AlgorithmAllowlist`] can only
//!   *narrow* [`HARD_ALLOWLIST`], never extend it, and there is no constructor
//!   that admits any other algorithm.
//! * `jku` and `x5u` are refused rather than dereferenced, and an embedded `jwk`
//!   or `x5c` is refused rather than trusted, so key resolution can never be
//!   redirected to material the caller supplied.
//! * `crit` is refused because this deployment implements no critical header
//!   extension, and RFC 7515 §4.1.11 requires rejecting a header whose critical
//!   extensions the recipient cannot process.
//! * `kid` must be present and non-empty, because the only supported way to pick
//!   a key is an exact match against the fetched JWKS. A JWKS with a single key is
//!   not an excuse to skip the match.
//!
//! # Why parsing is done here rather than with `decode_header`
//!
//! `jsonwebtoken::decode_header` deserializes into its own [`Header`] type, whose
//! `alg` field is a closed enum: a token carrying `"alg":"none"` or an unknown
//! algorithm name fails *deserialization*, which would collapse two different
//! rejections and — worse — leave "was `none` refused?" untestable. This module
//! parses the header as plain JSON so `none`, `HS256` and an unknown name are each
//! observed and each refused explicitly, and so the presence of `jku`/`x5u`/`jwk`/
//! `x5c`/`crit` is detected structurally rather than by a typed field that a
//! future dependency bump might rename.

use std::collections::BTreeSet;

use serde_json::Value;

use super::b64;

/// The algorithms this release will ever verify. Asymmetric, SHA-256 only.
///
/// `RS384`/`RS512`/`ES384`/`EdDSA` are deliberately absent: the smallest workable
/// set is the one with the fewest ways to be confused. Configuration selects a
/// nonempty subset of exactly this list.
pub const HARD_ALLOWLIST: [SignatureAlgorithm; 3] = [
    SignatureAlgorithm::Rs256,
    SignatureAlgorithm::Ps256,
    SignatureAlgorithm::Es256,
];

/// One acceptable asymmetric signature algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SignatureAlgorithm {
    /// RSASSA-PKCS1-v1_5 with SHA-256.
    Rs256,
    /// RSASSA-PSS with SHA-256.
    Ps256,
    /// ECDSA on P-256 with SHA-256.
    Es256,
}

impl SignatureAlgorithm {
    /// The `alg` spelling this algorithm has on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rs256 => "RS256",
            Self::Ps256 => "PS256",
            Self::Es256 => "ES256",
        }
    }

    /// The JWK `kty` a key for this algorithm must declare.
    #[must_use]
    pub const fn key_type(self) -> &'static str {
        match self {
            Self::Rs256 | Self::Ps256 => "RSA",
            Self::Es256 => "EC",
        }
    }

    /// The JWK `crv` an elliptic-curve key for this algorithm must declare.
    #[must_use]
    pub const fn curve(self) -> Option<&'static str> {
        match self {
            Self::Rs256 | Self::Ps256 => None,
            Self::Es256 => Some("P-256"),
        }
    }

    /// The `jsonwebtoken` algorithm this maps to, for the step-4 verifier.
    #[must_use]
    pub const fn jwt_algorithm(self) -> jsonwebtoken::Algorithm {
        match self {
            Self::Rs256 => jsonwebtoken::Algorithm::RS256,
            Self::Ps256 => jsonwebtoken::Algorithm::PS256,
            Self::Es256 => jsonwebtoken::Algorithm::ES256,
        }
    }

    /// Parse a wire spelling that is inside the hard allowlist.
    ///
    /// Returns `None` for `none`, every HMAC algorithm, every asymmetric
    /// algorithm outside the allowlist, and every unknown name alike: the caller
    /// refuses all of them identically, and nothing downstream can widen this.
    #[must_use]
    pub fn parse_allowlisted(name: &str) -> Option<Self> {
        HARD_ALLOWLIST
            .into_iter()
            .find(|candidate| candidate.as_str() == name)
    }
}

/// The deployment-selected subset of [`HARD_ALLOWLIST`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlgorithmAllowlist(BTreeSet<SignatureAlgorithm>);

impl AlgorithmAllowlist {
    /// Every algorithm in the hard allowlist.
    #[must_use]
    pub fn hard() -> Self {
        Self(HARD_ALLOWLIST.into_iter().collect())
    }

    /// Narrow the hard allowlist to `selection`.
    ///
    /// Because the parameter type is [`SignatureAlgorithm`], which has no variant
    /// outside the hard allowlist, this cannot extend the policy — only narrow it.
    ///
    /// # Errors
    /// Returns [`HeaderRejection::EmptyAllowlist`] when `selection` is empty: a
    /// deployment that accepts nothing would fail closed in a way that looks like
    /// a signature failure, which is a worse diagnostic than refusing to start.
    pub fn narrowed(
        selection: impl IntoIterator<Item = SignatureAlgorithm>,
    ) -> Result<Self, HeaderRejection> {
        let selected: BTreeSet<SignatureAlgorithm> = selection.into_iter().collect();
        if selected.is_empty() {
            return Err(HeaderRejection::EmptyAllowlist);
        }
        Ok(Self(selected))
    }

    /// Whether `algorithm` is selected.
    #[must_use]
    pub fn permits(&self, algorithm: SignatureAlgorithm) -> bool {
        self.0.contains(&algorithm)
    }

    /// The selected algorithms, in a stable order.
    #[must_use]
    pub fn selected(&self) -> Vec<SignatureAlgorithm> {
        self.0.iter().copied().collect()
    }
}

impl Default for AlgorithmAllowlist {
    fn default() -> Self {
        Self::hard()
    }
}

/// Why step 1 refused a token header.
///
/// These variants exist for tests and internal diagnostics. The HTTP boundary
/// collapses every one of them into a single indistinguishable challenge, so a
/// caller cannot use the response to learn which check fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderRejection {
    /// The token is not three dot-separated parts.
    Malformed,
    /// The header segment is not valid unpadded base64url, or not a JSON object.
    UndecodableHeader,
    /// `alg` is absent or is not a string.
    MissingAlgorithm,
    /// `alg` is `none`.
    NoneAlgorithm,
    /// `alg` names an HMAC algorithm.
    HmacAlgorithm,
    /// `alg` is outside the hard allowlist.
    NonAllowlistedAlgorithm,
    /// `alg` is allowlisted but not selected by this deployment.
    UnselectedAlgorithm,
    /// The header carries `crit`.
    CriticalHeader,
    /// The header carries `jku`.
    JkuHeader,
    /// The header carries `x5u`.
    X5uHeader,
    /// The header embeds a `jwk`.
    EmbeddedJwk,
    /// The header embeds an `x5c` chain.
    EmbeddedX5c,
    /// `kid` is absent, not a string, or empty.
    MissingKid,
    /// A deployment tried to select an empty algorithm set.
    EmptyAllowlist,
}

impl HeaderRejection {
    /// A stable snake_case label, used only in tests and internal counters.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "malformed_token",
            Self::UndecodableHeader => "undecodable_header",
            Self::MissingAlgorithm => "missing_alg",
            Self::NoneAlgorithm => "none_algorithm",
            Self::HmacAlgorithm => "hmac_algorithm",
            Self::NonAllowlistedAlgorithm => "non_allowlisted_algorithm",
            Self::UnselectedAlgorithm => "unselected_algorithm",
            Self::CriticalHeader => "crit_header",
            Self::JkuHeader => "jku_header",
            Self::X5uHeader => "x5u_header",
            Self::EmbeddedJwk => "embedded_jwk",
            Self::EmbeddedX5c => "embedded_x5c",
            Self::MissingKid => "missing_kid",
            Self::EmptyAllowlist => "empty_allowlist",
        }
    }
}

/// Header members that name material a token must never be able to point at.
const KEY_REDIRECTING_MEMBERS: [(&str, HeaderRejection); 4] = [
    ("jku", HeaderRejection::JkuHeader),
    ("x5u", HeaderRejection::X5uHeader),
    ("jwk", HeaderRejection::EmbeddedJwk),
    ("x5c", HeaderRejection::EmbeddedX5c),
];

/// HMAC `alg` spellings, refused before anything else can consider them.
const HMAC_ALGORITHMS: [&str; 3] = ["HS256", "HS384", "HS512"];

/// The outcome of step 1: an algorithm this deployment selected, plus the `kid`
/// lookup constraint. Nothing else from the header survives.
///
/// This type is the *only* way to reach step 2, it has no public constructor, and
/// its fields are private — so a caller cannot fabricate a "validated" header, and
/// cannot skip step 1 on the way to key selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnverifiedHeader {
    algorithm: SignatureAlgorithm,
    kid: String,
}

impl UnverifiedHeader {
    /// Step 1. Parse `token`'s header and refuse it before any further processing
    /// if the policy in this module's header is violated.
    ///
    /// # Errors
    /// Returns the [`HeaderRejection`] that stopped it. On any error no key has
    /// been looked at, no JWKS has been fetched, and no signature check has run.
    pub fn parse(token: &str, allowlist: &AlgorithmAllowlist) -> Result<Self, HeaderRejection> {
        let mut parts = token.split('.');
        let (Some(encoded), Some(_payload), Some(_signature), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(HeaderRejection::Malformed);
        };
        let decoded = b64::decode(encoded).ok_or(HeaderRejection::UndecodableHeader)?;
        let header: Value =
            serde_json::from_slice(&decoded).map_err(|_| HeaderRejection::UndecodableHeader)?;
        let header = header
            .as_object()
            .ok_or(HeaderRejection::UndecodableHeader)?;

        // `crit` first: a header declaring critical extensions must be refused
        // before any other member is interpreted, because an extension could
        // change what those members mean.
        if header.contains_key("crit") {
            return Err(HeaderRejection::CriticalHeader);
        }
        for (member, rejection) in KEY_REDIRECTING_MEMBERS {
            if header.contains_key(member) {
                return Err(rejection);
            }
        }

        let name = header
            .get("alg")
            .and_then(Value::as_str)
            .ok_or(HeaderRejection::MissingAlgorithm)?;
        if name.eq_ignore_ascii_case("none") {
            return Err(HeaderRejection::NoneAlgorithm);
        }
        if HMAC_ALGORITHMS.contains(&name) {
            return Err(HeaderRejection::HmacAlgorithm);
        }
        let algorithm = SignatureAlgorithm::parse_allowlisted(name)
            .ok_or(HeaderRejection::NonAllowlistedAlgorithm)?;
        if !allowlist.permits(algorithm) {
            return Err(HeaderRejection::UnselectedAlgorithm);
        }

        let kid = header
            .get("kid")
            .and_then(Value::as_str)
            .ok_or(HeaderRejection::MissingKid)?;
        if kid.is_empty() {
            return Err(HeaderRejection::MissingKid);
        }

        Ok(Self {
            algorithm,
            kid: kid.to_owned(),
        })
    }

    /// The validated algorithm. Steps 3 and 4 use exactly this value.
    #[must_use]
    pub const fn algorithm(&self) -> SignatureAlgorithm {
        self.algorithm
    }

    /// The `kid` lookup constraint, to be matched against server-controlled data.
    #[must_use]
    pub fn kid(&self) -> &str {
        &self.kid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_token(header: &Value) -> String {
        let encoded = b64::encode(serde_json::to_string(header).expect("serialize").as_bytes());
        format!("{encoded}.eyJzdWIiOiJhIn0.c2ln")
    }

    #[test]
    fn an_allowlisted_header_with_a_kid_passes() {
        let allowlist = AlgorithmAllowlist::hard();
        for algorithm in HARD_ALLOWLIST {
            let token = header_token(&serde_json::json!({
                "alg": algorithm.as_str(),
                "kid": "key-1",
                "typ": "JWT",
            }));
            let parsed = UnverifiedHeader::parse(&token, &allowlist).expect("accepted");
            assert_eq!(parsed.algorithm(), algorithm);
            assert_eq!(parsed.kid(), "key-1");
        }
    }

    #[test]
    fn none_and_every_hmac_algorithm_are_refused() {
        let allowlist = AlgorithmAllowlist::hard();
        for (name, expected) in [
            ("none", HeaderRejection::NoneAlgorithm),
            ("None", HeaderRejection::NoneAlgorithm),
            ("HS256", HeaderRejection::HmacAlgorithm),
            ("HS384", HeaderRejection::HmacAlgorithm),
            ("HS512", HeaderRejection::HmacAlgorithm),
        ] {
            let token = header_token(&serde_json::json!({"alg": name, "kid": "key-1"}));
            assert_eq!(
                UnverifiedHeader::parse(&token, &allowlist),
                Err(expected),
                "{name} must be refused"
            );
        }
    }

    #[test]
    fn algorithms_outside_the_allowlist_are_refused() {
        let allowlist = AlgorithmAllowlist::hard();
        for name in [
            "RS384", "RS512", "PS384", "PS512", "ES384", "EdDSA", "RSA1_5", "",
        ] {
            let token = header_token(&serde_json::json!({"alg": name, "kid": "key-1"}));
            assert_eq!(
                UnverifiedHeader::parse(&token, &allowlist),
                Err(HeaderRejection::NonAllowlistedAlgorithm),
                "{name} must be refused"
            );
        }
        let narrowed =
            AlgorithmAllowlist::narrowed([SignatureAlgorithm::Es256]).expect("nonempty subset");
        let token = header_token(&serde_json::json!({"alg": "RS256", "kid": "key-1"}));
        assert_eq!(
            UnverifiedHeader::parse(&token, &narrowed),
            Err(HeaderRejection::UnselectedAlgorithm)
        );
        assert_eq!(
            AlgorithmAllowlist::narrowed(std::iter::empty()),
            Err(HeaderRejection::EmptyAllowlist)
        );
    }

    #[test]
    fn key_redirecting_and_critical_headers_are_refused() {
        let allowlist = AlgorithmAllowlist::hard();
        let cases = [
            (
                serde_json::json!({"alg": "RS256", "kid": "k", "jku": "https://evil.example/jwks"}),
                HeaderRejection::JkuHeader,
            ),
            (
                serde_json::json!({"alg": "RS256", "kid": "k", "x5u": "https://evil.example/chain"}),
                HeaderRejection::X5uHeader,
            ),
            (
                serde_json::json!({"alg": "RS256", "kid": "k", "jwk": {"kty": "RSA", "n": "a", "e": "AQAB"}}),
                HeaderRejection::EmbeddedJwk,
            ),
            (
                serde_json::json!({"alg": "RS256", "kid": "k", "x5c": ["MIIB"]}),
                HeaderRejection::EmbeddedX5c,
            ),
            (
                serde_json::json!({"alg": "RS256", "kid": "k", "crit": ["exp"]}),
                HeaderRejection::CriticalHeader,
            ),
            (
                serde_json::json!({"alg": "RS256", "kid": "k", "crit": []}),
                HeaderRejection::CriticalHeader,
            ),
        ];
        for (header, expected) in cases {
            assert_eq!(
                UnverifiedHeader::parse(&header_token(&header), &allowlist),
                Err(expected)
            );
        }
    }

    #[test]
    fn crit_is_refused_even_alongside_a_banned_algorithm() {
        // Ordering proof: `crit` is observed before `alg`, so a token that is
        // wrong in both ways still never reaches algorithm interpretation.
        let allowlist = AlgorithmAllowlist::hard();
        let token = header_token(&serde_json::json!({
            "alg": "HS256",
            "kid": "k",
            "crit": ["b64"],
        }));
        assert_eq!(
            UnverifiedHeader::parse(&token, &allowlist),
            Err(HeaderRejection::CriticalHeader)
        );
    }

    #[test]
    fn a_missing_or_empty_kid_is_refused() {
        let allowlist = AlgorithmAllowlist::hard();
        for header in [
            serde_json::json!({"alg": "RS256"}),
            serde_json::json!({"alg": "RS256", "kid": ""}),
            serde_json::json!({"alg": "RS256", "kid": 7}),
        ] {
            assert_eq!(
                UnverifiedHeader::parse(&header_token(&header), &allowlist),
                Err(HeaderRejection::MissingKid)
            );
        }
    }

    #[test]
    fn malformed_tokens_are_refused_before_anything_else() {
        let allowlist = AlgorithmAllowlist::hard();
        assert_eq!(
            UnverifiedHeader::parse("only.two", &allowlist),
            Err(HeaderRejection::Malformed)
        );
        assert_eq!(
            UnverifiedHeader::parse("a.b.c.d", &allowlist),
            Err(HeaderRejection::Malformed)
        );
        assert_eq!(
            UnverifiedHeader::parse("!!!.b.c", &allowlist),
            Err(HeaderRejection::UndecodableHeader)
        );
        let not_json = b64::encode(b"not json");
        assert_eq!(
            UnverifiedHeader::parse(&format!("{not_json}.b.c"), &allowlist),
            Err(HeaderRejection::UndecodableHeader)
        );
        let array = b64::encode(b"[1,2]");
        assert_eq!(
            UnverifiedHeader::parse(&format!("{array}.b.c"), &allowlist),
            Err(HeaderRejection::UndecodableHeader)
        );
    }

    #[test]
    fn the_hard_allowlist_is_exactly_three_sha256_asymmetric_algorithms() {
        let names: Vec<&str> = HARD_ALLOWLIST.iter().map(|a| a.as_str()).collect();
        assert_eq!(names, ["RS256", "PS256", "ES256"]);
        assert_eq!(SignatureAlgorithm::Rs256.key_type(), "RSA");
        assert_eq!(SignatureAlgorithm::Ps256.key_type(), "RSA");
        assert_eq!(SignatureAlgorithm::Es256.key_type(), "EC");
        assert_eq!(SignatureAlgorithm::Es256.curve(), Some("P-256"));
        assert_eq!(SignatureAlgorithm::Rs256.curve(), None);
        assert_eq!(AlgorithmAllowlist::default(), AlgorithmAllowlist::hard());
        assert_eq!(AlgorithmAllowlist::hard().selected().len(), 3);
    }
}
