//! Steps 2 and 3 of the ordered pipeline: fetched-JWKS key selection, then JWK
//! metadata validation against the already-validated algorithm.
//!
//! # The token never says where a key comes from
//!
//! A [`JwkSet`] is built only from bytes the server fetched from its *own*
//! configured endpoint (see [`super::resolver`]). Step 1's `kid` is then used as a
//! pure lookup constraint against that server-controlled data. There is no code
//! path from a token-supplied location to a key, because step 1 refuses `jku`,
//! `x5u`, `jwk` and `x5c` outright and nothing here accepts a URL at all.
//!
//! # Ambiguity is a rejection, not a heuristic
//!
//! Zero matches, two matches, an entry with no `kid`, or two entries sharing a
//! `kid` all fail closed. "Try each candidate until one verifies" is exactly how a
//! rotation window becomes an oracle: an attacker who can add a key to a set they
//! partially control would get a second chance at verification. A bounded old/new
//! overlap therefore means *two entries with distinct `kid`s* — both present, each
//! selected only by its own exact `kid` — and never a fallback after a failure.
//!
//! # Metadata is checked before the key is used, not after
//!
//! [`KeySelected::check_metadata`] compares `kty`, `crv`, `alg`, `use` and
//! `key_ops` against the algorithm step 1 validated, and only its output
//! ([`MetadataChecked`]) can build a verifying key. That ordering is what stops
//! algorithm/key confusion: an RSA modulus can never be handed to the ES256
//! verifier, and an EC key on P-384 can never satisfy an ES256 token, because the
//! mismatch is refused before [`MetadataChecked::decoding_key`] exists.

use std::collections::BTreeMap;

use jsonwebtoken::DecodingKey;
use serde_json::Value;

use super::header::{SignatureAlgorithm, UnverifiedHeader};

/// One JWK from a fetched set, kept as its raw members so nothing is inferred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jwk {
    kid: String,
    kty: String,
    alg: Option<String>,
    use_: Option<String>,
    key_ops: Option<Vec<String>>,
    crv: Option<String>,
    n: Option<String>,
    e: Option<String>,
    x: Option<String>,
    y: Option<String>,
}

impl Jwk {
    /// This key's `kid`, which is non-empty by construction.
    #[must_use]
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// This key's `kty`.
    #[must_use]
    pub fn key_type(&self) -> &str {
        &self.kty
    }
}

/// Why a JWKS could not be parsed, or a key could not be selected or trusted.
///
/// The `Display` rendering is the internal label, not a message for a caller: the
/// wire form is the single indistinguishable challenge in [`super::challenge`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", self.as_str())]
pub enum KeyRejection {
    /// The fetched document is not a JSON object with a `keys` array.
    MalformedKeySet,
    /// A member of `keys` is not an object, or its `kty` is absent.
    MalformedKey,
    /// A member of `keys` has an absent or empty `kid`.
    KeyWithoutKid,
    /// Two members of `keys` share a `kid`.
    DuplicateKid {
        /// The ambiguous identifier.
        kid: String,
    },
    /// No member of `keys` carries the token's `kid`.
    UnknownKid,
    /// The selected key's `kty` contradicts the validated algorithm.
    KeyTypeMismatch,
    /// The selected key's `crv` contradicts the validated algorithm.
    CurveMismatch,
    /// The selected key declares an `alg` other than the token's.
    AlgorithmMismatch,
    /// The selected key's `use` is present and is not `sig`.
    NonSignatureUse,
    /// The selected key's `key_ops` omits `verify` or contradicts it.
    KeyOpsMismatch,
    /// The selected key's material is absent or not valid base64url.
    UnusableKeyMaterial,
}

impl KeyRejection {
    /// A stable snake_case label, used only in tests and internal counters.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::MalformedKeySet => "malformed_key_set",
            Self::MalformedKey => "malformed_key",
            Self::KeyWithoutKid => "key_without_kid",
            Self::DuplicateKid { .. } => "duplicate_kid",
            Self::UnknownKid => "unknown_kid",
            Self::KeyTypeMismatch => "key_type_mismatch",
            Self::CurveMismatch => "curve_mismatch",
            Self::AlgorithmMismatch => "algorithm_mismatch",
            Self::NonSignatureUse => "non_signature_use",
            Self::KeyOpsMismatch => "key_ops_mismatch",
            Self::UnusableKeyMaterial => "unusable_key_material",
        }
    }
}

/// A JWKS the server fetched from its own configured endpoint.
///
/// Every key in it has a non-empty, unique `kid`: the invariant is established at
/// parse time so no later step has to re-check it, and a set that violates it is
/// rejected in full rather than partially used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JwkSet {
    keys: BTreeMap<String, Jwk>,
}

impl JwkSet {
    /// Parse a fetched JWKS document.
    ///
    /// # Errors
    /// Returns [`KeyRejection`] when the document is malformed, when any entry has
    /// no usable `kid`, or when two entries share one.
    pub fn parse(document: &str) -> Result<Self, KeyRejection> {
        let value: Value =
            serde_json::from_str(document).map_err(|_| KeyRejection::MalformedKeySet)?;
        let entries = value
            .get("keys")
            .and_then(Value::as_array)
            .ok_or(KeyRejection::MalformedKeySet)?;
        let mut keys: BTreeMap<String, Jwk> = BTreeMap::new();
        for entry in entries {
            let object = entry.as_object().ok_or(KeyRejection::MalformedKey)?;
            let kid = object
                .get("kid")
                .and_then(Value::as_str)
                .ok_or(KeyRejection::KeyWithoutKid)?;
            if kid.is_empty() {
                return Err(KeyRejection::KeyWithoutKid);
            }
            let kty = object
                .get("kty")
                .and_then(Value::as_str)
                .ok_or(KeyRejection::MalformedKey)?
                .to_owned();
            let text = |name: &str| object.get(name).and_then(Value::as_str).map(str::to_owned);
            let key_ops = match object.get("key_ops") {
                None => None,
                Some(Value::Array(items)) => Some(
                    items
                        .iter()
                        .map(|item| {
                            item.as_str()
                                .map(str::to_owned)
                                .ok_or(KeyRejection::MalformedKey)
                        })
                        .collect::<Result<Vec<String>, KeyRejection>>()?,
                ),
                Some(_) => return Err(KeyRejection::MalformedKey),
            };
            let jwk = Jwk {
                kid: kid.to_owned(),
                kty,
                alg: text("alg"),
                use_: text("use"),
                key_ops,
                crv: text("crv"),
                n: text("n"),
                e: text("e"),
                x: text("x"),
                y: text("y"),
            };
            if keys.insert(kid.to_owned(), jwk).is_some() {
                return Err(KeyRejection::DuplicateKid {
                    kid: kid.to_owned(),
                });
            }
        }
        if keys.is_empty() {
            return Err(KeyRejection::MalformedKeySet);
        }
        Ok(Self { keys })
    }

    /// How many keys the set holds. A bounded rotation overlap is two.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the set holds no keys. Never true for a parsed set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Step 2. Select the single key whose `kid` exactly matches the header's.
    ///
    /// The header is consumed by value so the algorithm it validated travels with
    /// the selection: there is no way to select a key for one algorithm and then
    /// verify with another.
    ///
    /// # Errors
    /// Returns [`KeyRejection::UnknownKid`] when the fetched set has no key with
    /// that `kid`. Zero and "more than one" are both impossible-to-confuse cases
    /// here because the set's own invariant already made `kid` unique.
    pub fn select(&self, header: UnverifiedHeader) -> Result<KeySelected<'_>, KeyRejection> {
        let matches = self
            .keys
            .values()
            .filter(|key| key.kid == header.kid())
            .count();
        if matches != 1 {
            return Err(if matches == 0 {
                KeyRejection::UnknownKid
            } else {
                KeyRejection::DuplicateKid {
                    kid: header.kid().to_owned(),
                }
            });
        }
        let key = self
            .keys
            .get(header.kid())
            .ok_or(KeyRejection::UnknownKid)?;
        Ok(KeySelected {
            algorithm: header.algorithm(),
            key,
        })
    }
}

/// The output of step 2: exactly one JWK, plus the algorithm step 1 validated.
///
/// Private fields and no public constructor: the only way to hold one is to have
/// gone through [`JwkSet::select`], which in turn requires an
/// [`UnverifiedHeader`] that only step 1 can produce.
#[derive(Debug, Clone, Copy)]
pub struct KeySelected<'a> {
    algorithm: SignatureAlgorithm,
    key: &'a Jwk,
}

impl KeySelected<'_> {
    /// The algorithm step 1 validated.
    #[must_use]
    pub const fn algorithm(&self) -> SignatureAlgorithm {
        self.algorithm
    }

    /// The selected key's `kid`.
    #[must_use]
    pub fn kid(&self) -> &str {
        &self.key.kid
    }

    /// Step 3. Validate the selected JWK's metadata against the validated
    /// algorithm, before the key is used to verify anything.
    ///
    /// # Errors
    /// Returns the [`KeyRejection`] describing the contradiction.
    pub fn check_metadata(self) -> Result<MetadataChecked<'static>, KeyRejection> {
        if self.key.kty != self.algorithm.key_type() {
            return Err(KeyRejection::KeyTypeMismatch);
        }
        match (self.algorithm.curve(), self.key.crv.as_deref()) {
            (Some(required), Some(declared)) if required == declared => {}
            (Some(_), _) => return Err(KeyRejection::CurveMismatch),
            // An RSA key must not claim a curve: a key that describes itself as
            // two different things is not a key this server will use.
            (None, None) => {}
            (None, Some(_)) => return Err(KeyRejection::CurveMismatch),
        }
        if let Some(declared) = self.key.alg.as_deref()
            && declared != self.algorithm.as_str()
        {
            return Err(KeyRejection::AlgorithmMismatch);
        }
        if let Some(declared) = self.key.use_.as_deref()
            && declared != "sig"
        {
            return Err(KeyRejection::NonSignatureUse);
        }
        if let Some(operations) = self.key.key_ops.as_deref() {
            if !operations.iter().any(|op| op == "verify") {
                return Err(KeyRejection::KeyOpsMismatch);
            }
            // RFC 7517 §4.3 forbids combining operations whose purposes differ;
            // a verification key that also claims to decrypt is contradictory.
            if operations
                .iter()
                .any(|op| matches!(op.as_str(), "encrypt" | "decrypt" | "wrapKey" | "unwrapKey"))
            {
                return Err(KeyRejection::KeyOpsMismatch);
            }
        }
        let decoding = match self.algorithm {
            SignatureAlgorithm::Rs256 | SignatureAlgorithm::Ps256 => {
                let modulus = self
                    .key
                    .n
                    .as_deref()
                    .ok_or(KeyRejection::UnusableKeyMaterial)?;
                let exponent = self
                    .key
                    .e
                    .as_deref()
                    .ok_or(KeyRejection::UnusableKeyMaterial)?;
                DecodingKey::from_rsa_components(modulus, exponent)
                    .map_err(|_| KeyRejection::UnusableKeyMaterial)?
            }
            SignatureAlgorithm::Es256 => {
                let x = self
                    .key
                    .x
                    .as_deref()
                    .ok_or(KeyRejection::UnusableKeyMaterial)?;
                let y = self
                    .key
                    .y
                    .as_deref()
                    .ok_or(KeyRejection::UnusableKeyMaterial)?;
                DecodingKey::from_ec_components(x, y)
                    .map_err(|_| KeyRejection::UnusableKeyMaterial)?
            }
        };
        Ok(MetadataChecked {
            algorithm: self.algorithm,
            kid: self.key.kid.clone(),
            decoding,
            _borrow: std::marker::PhantomData,
        })
    }
}

/// The output of step 3: a key whose declared metadata agrees with the validated
/// algorithm, and which may therefore be used for verification in step 4.
#[derive(Debug)]
pub struct MetadataChecked<'a> {
    algorithm: SignatureAlgorithm,
    kid: String,
    decoding: DecodingKey,
    _borrow: std::marker::PhantomData<&'a ()>,
}

impl MetadataChecked<'_> {
    /// The algorithm steps 1 and 3 agreed on.
    #[must_use]
    pub const fn algorithm(&self) -> SignatureAlgorithm {
        self.algorithm
    }

    /// The `kid` this key was selected by.
    #[must_use]
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// The verifying key. Reachable only from a completed step 3.
    #[must_use]
    pub const fn decoding_key(&self) -> &DecodingKey {
        &self.decoding
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::header::AlgorithmAllowlist;
    use crate::auth::testkeys;

    fn header_for(algorithm: SignatureAlgorithm, kid: &str) -> UnverifiedHeader {
        let header = crate::auth::b64::encode(
            serde_json::to_string(&serde_json::json!({"alg": algorithm.as_str(), "kid": kid}))
                .expect("serialize")
                .as_bytes(),
        );
        UnverifiedHeader::parse(
            &format!("{header}.eyJhIjoxfQ.c2ln"),
            &AlgorithmAllowlist::hard(),
        )
        .expect("the fixture header is allowlisted")
    }

    #[test]
    fn an_exact_kid_selects_one_key_and_metadata_passes() {
        let set = JwkSet::parse(&testkeys::jwks_document(&[
            testkeys::RSA_OLD,
            testkeys::RSA_NEW,
        ]))
        .expect("parse");
        assert_eq!(set.len(), 2);
        assert!(!set.is_empty());
        let selected = set
            .select(header_for(SignatureAlgorithm::Rs256, testkeys::RSA_NEW.kid))
            .expect("select");
        assert_eq!(selected.kid(), testkeys::RSA_NEW.kid);
        assert_eq!(selected.algorithm(), SignatureAlgorithm::Rs256);
        let checked = selected.check_metadata().expect("metadata");
        assert_eq!(checked.kid(), testkeys::RSA_NEW.kid);
        assert_eq!(checked.algorithm(), SignatureAlgorithm::Rs256);
        assert_eq!(
            checked.decoding_key().family(),
            jsonwebtoken::AlgorithmFamily::Rsa
        );
    }

    #[test]
    fn an_unknown_kid_is_refused_even_against_a_single_key_set() {
        let set = JwkSet::parse(&testkeys::jwks_document(&[testkeys::RSA_OLD])).expect("parse");
        assert_eq!(set.len(), 1);
        assert_eq!(
            set.select(header_for(SignatureAlgorithm::Rs256, "not-present"))
                .err(),
            Some(KeyRejection::UnknownKid)
        );
    }

    #[test]
    fn a_duplicate_or_missing_kid_rejects_the_whole_set() {
        let duplicate = format!(
            "{{\"keys\":[{},{}]}}",
            testkeys::RSA_OLD.jwk,
            testkeys::RSA_OLD.jwk
        );
        assert_eq!(
            JwkSet::parse(&duplicate).err(),
            Some(KeyRejection::DuplicateKid {
                kid: testkeys::RSA_OLD.kid.to_owned()
            })
        );
        assert_eq!(
            JwkSet::parse("{\"keys\":[{\"kty\":\"RSA\",\"n\":\"a\",\"e\":\"AQAB\"}]}").err(),
            Some(KeyRejection::KeyWithoutKid)
        );
        assert_eq!(
            JwkSet::parse("{\"keys\":[{\"kid\":\"\",\"kty\":\"RSA\"}]}").err(),
            Some(KeyRejection::KeyWithoutKid)
        );
        assert_eq!(
            JwkSet::parse("{\"keys\":[]}").err(),
            Some(KeyRejection::MalformedKeySet)
        );
        assert_eq!(
            JwkSet::parse("not json").err(),
            Some(KeyRejection::MalformedKeySet)
        );
        assert_eq!(
            JwkSet::parse("{\"key\":[]}").err(),
            Some(KeyRejection::MalformedKeySet)
        );
        assert_eq!(
            JwkSet::parse("{\"keys\":[7]}").err(),
            Some(KeyRejection::MalformedKey)
        );
        assert_eq!(
            JwkSet::parse("{\"keys\":[{\"kid\":\"k\"}]}").err(),
            Some(KeyRejection::MalformedKey)
        );
    }

    #[test]
    fn key_and_algorithm_confusion_is_refused_before_the_key_can_be_built() {
        // An RSA key can never satisfy an ES256 token, and vice versa.
        let rsa = JwkSet::parse(&testkeys::jwks_document(&[testkeys::RSA_OLD])).expect("parse");
        assert_eq!(
            rsa.select(header_for(SignatureAlgorithm::Es256, testkeys::RSA_OLD.kid))
                .expect("select")
                .check_metadata()
                .err(),
            Some(KeyRejection::KeyTypeMismatch)
        );
        // An EC key on P-384 can never satisfy ES256.
        let wrong_curve =
            JwkSet::parse(&testkeys::jwks_document(&[testkeys::EC_P384])).expect("parse");
        let rejection = wrong_curve
            .select(header_for(SignatureAlgorithm::Es256, testkeys::EC_P384.kid))
            .expect("select")
            .check_metadata()
            .err();
        assert!(
            matches!(
                rejection,
                Some(KeyRejection::CurveMismatch | KeyRejection::AlgorithmMismatch)
            ),
            "a P-384 key must not satisfy ES256, got {rejection:?}"
        );
    }

    #[test]
    fn declared_metadata_that_contradicts_the_algorithm_is_refused() {
        let cases = [
            (
                serde_json::json!({"kid":"k","kty":"RSA","alg":"RS512","n":"AQAB","e":"AQAB"}),
                KeyRejection::AlgorithmMismatch,
            ),
            (
                serde_json::json!({"kid":"k","kty":"RSA","use":"enc","n":"AQAB","e":"AQAB"}),
                KeyRejection::NonSignatureUse,
            ),
            (
                serde_json::json!({"kid":"k","kty":"RSA","key_ops":["sign"],"n":"AQAB","e":"AQAB"}),
                KeyRejection::KeyOpsMismatch,
            ),
            (
                serde_json::json!({"kid":"k","kty":"RSA","key_ops":["verify","decrypt"],"n":"AQAB","e":"AQAB"}),
                KeyRejection::KeyOpsMismatch,
            ),
            (
                serde_json::json!({"kid":"k","kty":"RSA","crv":"P-256","n":"AQAB","e":"AQAB"}),
                KeyRejection::CurveMismatch,
            ),
            (
                serde_json::json!({"kid":"k","kty":"RSA"}),
                KeyRejection::UnusableKeyMaterial,
            ),
            (
                serde_json::json!({"kid":"k","kty":"RSA","n":"!!!","e":"AQAB"}),
                KeyRejection::UnusableKeyMaterial,
            ),
        ];
        for (key, expected) in cases {
            let document = serde_json::json!({"keys": [key]}).to_string();
            let set = JwkSet::parse(&document).expect("parse");
            assert_eq!(
                set.select(header_for(SignatureAlgorithm::Rs256, "k"))
                    .expect("select")
                    .check_metadata()
                    .err(),
                Some(expected)
            );
        }
        let ec_missing =
            serde_json::json!({"keys":[{"kid":"k","kty":"EC","crv":"P-256"}]}).to_string();
        let set = JwkSet::parse(&ec_missing).expect("parse");
        assert_eq!(
            set.select(header_for(SignatureAlgorithm::Es256, "k"))
                .expect("select")
                .check_metadata()
                .err(),
            Some(KeyRejection::UnusableKeyMaterial)
        );
    }

    #[test]
    fn rejection_labels_are_stable() {
        assert_eq!(KeyRejection::UnknownKid.as_str(), "unknown_kid");
        assert_eq!(
            KeyRejection::DuplicateKid {
                kid: "k".to_owned()
            }
            .as_str(),
            "duplicate_kid"
        );
        assert_eq!(KeyRejection::KeyTypeMismatch.as_str(), "key_type_mismatch");
        assert_eq!(KeyRejection::CurveMismatch.as_str(), "curve_mismatch");
        assert_eq!(
            KeyRejection::AlgorithmMismatch.as_str(),
            "algorithm_mismatch"
        );
        assert_eq!(KeyRejection::NonSignatureUse.as_str(), "non_signature_use");
        assert_eq!(KeyRejection::KeyOpsMismatch.as_str(), "key_ops_mismatch");
        assert_eq!(
            KeyRejection::UnusableKeyMaterial.as_str(),
            "unusable_key_material"
        );
        assert_eq!(KeyRejection::MalformedKeySet.as_str(), "malformed_key_set");
        assert_eq!(KeyRejection::MalformedKey.as_str(), "malformed_key");
        assert_eq!(KeyRejection::KeyWithoutKid.as_str(), "key_without_kid");
    }
}
