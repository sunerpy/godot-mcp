//! Deterministic, committed OIDC key fixtures.
//!
//! # Why the keys live in the repository
//!
//! Every acceptance case for this boundary needs a *real* signature over a *real*
//! key, and a rotation case needs two distinct keys present at once. Generating
//! keys at test time would make the suite nondeterministic and slow, and reaching
//! for a live identity provider would make it a network test. So four keypairs are
//! generated once and committed: two RSA (with an additional PS256 metadata view
//! of the old key), one EC P-256 (the ES256 happy path) and one EC P-384 (an
//! algorithm/key-confusion probe that must always be refused).
//!
//! These are **test fixtures with published private keys**. They are compiled only
//! under `cfg(test)` or the `test-hooks` feature, never into a release build, and
//! nothing in production reads this module.
//!
//! The `.jwk.json` documents hold only public members, derived from the same PEM
//! files by the generator, so a fixture cannot silently drift from its key.

/// One committed keypair: its `kid`, its public JWK text, and its private PEM.
#[derive(Debug, Clone, Copy)]
pub struct TestKey {
    /// The `kid` this key publishes, and the only way to select it.
    pub kid: &'static str,
    /// The public JWK, exactly as it appears in a served JWKS document.
    pub jwk: &'static str,
    /// The PKCS#8 private key, for signing fixture tokens.
    pub private_pem: &'static [u8],
    /// Whether this key is RSA (as opposed to EC).
    pub rsa: bool,
}

/// The "old" RSA key: valid before and during the rotation overlap.
pub const RSA_OLD: TestKey = TestKey {
    kid: "key-old",
    jwk: include_str!("../../tests/fixtures/auth/rsa-old.jwk.json"),
    private_pem: include_bytes!("../../tests/fixtures/auth/rsa-old.pem"),
    rsa: true,
};

/// The "new" RSA key: introduced during the overlap and valid after it.
pub const RSA_NEW: TestKey = TestKey {
    kid: "key-new",
    jwk: include_str!("../../tests/fixtures/auth/rsa-new.jwk.json"),
    private_pem: include_bytes!("../../tests/fixtures/auth/rsa-new.pem"),
    rsa: true,
};

/// The EC P-256 key, for the ES256 path.
pub const EC_P256: TestKey = TestKey {
    kid: "key-ec",
    jwk: include_str!("../../tests/fixtures/auth/ec-p256.jwk.json"),
    private_pem: include_bytes!("../../tests/fixtures/auth/ec-p256.pem"),
    rsa: false,
};

/// The EC P-384 key. Present only so a test can prove it is *never* accepted.
pub const EC_P384: TestKey = TestKey {
    kid: "key-ec384",
    jwk: include_str!("../../tests/fixtures/auth/ec-p384.jwk.json"),
    private_pem: include_bytes!("../../tests/fixtures/auth/ec-p384.pem"),
    rsa: false,
};

/// Build a JWKS document publishing exactly `keys`.
#[must_use]
pub fn jwks_document(keys: &[TestKey]) -> String {
    let entries: Vec<&str> = keys.iter().map(|key| key.jwk.trim()).collect();
    format!("{{\"keys\":[{}]}}", entries.join(","))
}

/// Sign `claims` with `key` under `algorithm`, emitting `kid` in the header.
///
/// `kid` is a parameter rather than taken from `key` so a test can produce a token
/// whose `kid` is absent, wrong, or ambiguous.
///
/// # Panics
/// Panics if the fixture key cannot be loaded or the token cannot be signed —
/// both are bugs in the fixture, not conditions under test.
#[must_use]
pub fn sign(
    key: &TestKey,
    algorithm: super::header::SignatureAlgorithm,
    kid: Option<&str>,
    claims: &serde_json::Value,
) -> String {
    let mut header = jsonwebtoken::Header::new(algorithm.jwt_algorithm());
    header.kid = kid.map(str::to_owned);
    let encoding = if key.rsa {
        jsonwebtoken::EncodingKey::from_rsa_pem(key.private_pem).expect("fixture RSA key loads")
    } else {
        jsonwebtoken::EncodingKey::from_ec_pem(key.private_pem).expect("fixture EC key loads")
    };
    jsonwebtoken::encode(&header, claims, &encoding).expect("the fixture token signs")
}

/// Assemble a token from a *literal* header object, signing the result with `key`.
///
/// Used for the header-policy probes: `jsonwebtoken::Header` cannot express
/// `alg: none`, an unknown algorithm name, or a bare `crit` entry, and the point of
/// those cases is that the header is refused *before* any signature matters. The
/// signature bytes here are real for an allowlisted `alg` and arbitrary otherwise.
///
/// # Panics
/// Panics if the header cannot be serialized, which would be a fixture bug.
#[must_use]
pub fn sign_with_literal_header(
    key: &TestKey,
    header: &serde_json::Value,
    claims: &serde_json::Value,
) -> String {
    let header_text = serde_json::to_vec(header).expect("the fixture header serializes");
    let claims_text = serde_json::to_vec(claims).expect("the fixture claims serialize");
    let message = format!(
        "{}.{}",
        super::b64::encode(&header_text),
        super::b64::encode(&claims_text)
    );
    let algorithm = header
        .get("alg")
        .and_then(serde_json::Value::as_str)
        .and_then(super::header::SignatureAlgorithm::parse_allowlisted);
    let signature = match algorithm {
        Some(algorithm) => {
            let encoding = if key.rsa {
                jsonwebtoken::EncodingKey::from_rsa_pem(key.private_pem)
                    .expect("fixture RSA key loads")
            } else {
                jsonwebtoken::EncodingKey::from_ec_pem(key.private_pem)
                    .expect("fixture EC key loads")
            };
            jsonwebtoken::crypto::sign(message.as_bytes(), &encoding, algorithm.jwt_algorithm())
                .expect("the fixture signature computes")
        }
        // A header this deployment refuses in step 1 never reaches verification,
        // so any signature-shaped bytes are enough to make the token well-formed.
        None => super::b64::encode(b"not-a-signature"),
    };
    format!("{message}.{signature}")
}

/// The old RSA key published under PS256 metadata for the RSA-PSS path.
pub const RSA_PSS: TestKey = TestKey {
    kid: "key-pss",
    jwk: include_str!("../../tests/fixtures/auth/rsa-pss.jwk.json"),
    private_pem: include_bytes!("../../tests/fixtures/auth/rsa-old.pem"),
    rsa: true,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fixture_jwk_declares_the_kid_it_is_selected_by() {
        for key in [RSA_OLD, RSA_NEW, RSA_PSS, EC_P256, EC_P384] {
            let value: serde_json::Value = serde_json::from_str(key.jwk).expect("valid jwk");
            assert_eq!(
                value.get("kid").and_then(serde_json::Value::as_str),
                Some(key.kid)
            );
            assert_eq!(
                value.get("use").and_then(serde_json::Value::as_str),
                Some("sig")
            );
            assert!(!key.private_pem.is_empty());
        }
    }

    #[test]
    fn a_jwks_document_publishes_exactly_the_requested_keys() {
        let document = jwks_document(&[RSA_OLD, EC_P256]);
        let value: serde_json::Value = serde_json::from_str(&document).expect("valid json");
        let keys = value
            .get("keys")
            .and_then(serde_json::Value::as_array)
            .expect("keys");
        assert_eq!(keys.len(), 2);
    }
}
