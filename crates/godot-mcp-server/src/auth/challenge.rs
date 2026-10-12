//! The single, indistinguishable refusal this boundary returns.
//!
//! # Why every failure looks the same
//!
//! A caller must not be able to tell *why* it was refused. If a missing hop
//! credential, a wrong hop credential, an `alg: none` token, an unknown `kid`, a
//! bad signature and a second subject each produced a distinguishable response,
//! the endpoint would be an oracle: an attacker could walk the checks one at a time
//! and learn the configuration. So the wire form collapses to exactly two shapes —
//! `401` with a `WWW-Authenticate` challenge for "not authenticated", and `403`
//! with the same body for "authenticated but not this principal or not this scope"
//! — and the body is a constant either way.
//!
//! The *internal* reason is kept as [`AuthRejection`] for tests and for the
//! diagnostic sink, which never reaches the client. Todo 27's lesson applies: a
//! cross-context mismatch must be uniform and byte-stable, while inside the process
//! it should stay diagnosable.
//!
//! # 401 versus 403
//!
//! RFC 6750 §3.1 draws the line at whether a valid credential was presented:
//! `invalid_token` (and no token at all) is a `401`, while a token that is valid
//! but lacks the required authority is `insufficient_scope`, a `403`. Both carry a
//! `WWW-Authenticate: Bearer` challenge naming the external `resource_metadata`
//! URL, which is what RFC 9728 §5.1 requires so a client can discover where to get
//! an acceptable token.

use std::fmt;

use super::header::HeaderRejection;
use super::hop::HopRejection;
use super::jwks::KeyRejection;
use super::pipeline::ClaimRejection;
use super::resolver::SsrfRejection;

/// The constant body every refusal returns, whatever actually failed.
pub const REFUSAL_BODY: &str = "Unauthorized";

/// The `error` code a `401` advertises.
pub const INVALID_TOKEN: &str = "invalid_token";

/// The `error` code a `403` advertises.
pub const INSUFFICIENT_SCOPE: &str = "insufficient_scope";

/// Why the boundary refused a request. Never visible to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthRejection {
    /// The proxy hop did not authenticate.
    Hop(HopRejection),
    /// Step 1 refused the token header.
    Header(HeaderRejection),
    /// Steps 2 or 3 refused the key.
    Key(KeyRejection),
    /// Step 4 could not verify the signature.
    Signature,
    /// Step 5 refused the claims.
    Claims(ClaimRejection),
    /// The JWKS could not be fetched or was refused by SSRF vetting.
    Discovery(SsrfRejection),
}

impl AuthRejection {
    /// The HTTP status this refusal returns.
    ///
    /// Only an authenticated token that merely lacks authority is a `403`;
    /// everything else — including a hop failure, which the end user cannot fix by
    /// getting a better token — is a `401`.
    #[must_use]
    pub const fn status_code(&self) -> u16 {
        match self {
            Self::Claims(claims) if claims.is_authorization_failure() => 403,
            _ => 401,
        }
    }

    /// The RFC 6750 error code this refusal advertises.
    #[must_use]
    pub const fn error_code(&self) -> &'static str {
        match self.status_code() {
            403 => INSUFFICIENT_SCOPE,
            _ => INVALID_TOKEN,
        }
    }

    /// A stable snake_case label, for tests and the internal diagnostic sink.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Hop(hop) => hop.as_str(),
            Self::Header(header) => header.as_str(),
            Self::Key(key) => key.as_str(),
            Self::Signature => "invalid_signature",
            Self::Claims(claims) => claims.as_str(),
            Self::Discovery(ssrf) => ssrf.as_str(),
        }
    }
}

impl fmt::Display for AuthRejection {
    /// Deliberately uniform: even a log line built from this cannot reveal which
    /// check fired, so a diagnostic that escapes to a client is still not an oracle.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REFUSAL_BODY)
    }
}

impl std::error::Error for AuthRejection {
    /// No source is exposed. A chained cause would defeat the uniform `Display`
    /// above by letting a caller read the specific check out of an error report.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

/// The `WWW-Authenticate` challenge and status one refusal produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthChallenge {
    status: u16,
    error: &'static str,
    resource_metadata: String,
}

impl AuthChallenge {
    /// Build the challenge for `rejection`, advertising `resource_metadata`.
    #[must_use]
    pub fn new(rejection: &AuthRejection, resource_metadata: &str) -> Self {
        Self {
            status: rejection.status_code(),
            error: rejection.error_code(),
            resource_metadata: resource_metadata.to_owned(),
        }
    }

    /// The challenge a request carrying no credential at all receives.
    #[must_use]
    pub fn unauthenticated(resource_metadata: &str) -> Self {
        Self {
            status: 401,
            error: INVALID_TOKEN,
            resource_metadata: resource_metadata.to_owned(),
        }
    }

    /// The HTTP status.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// The constant response body.
    #[must_use]
    pub const fn body(&self) -> &'static str {
        REFUSAL_BODY
    }

    /// The `WWW-Authenticate` header value, byte-stable for a given status.
    #[must_use]
    pub fn header_value(&self) -> String {
        format!(
            "Bearer error=\"{}\", resource_metadata=\"{}\"",
            self.error, self.resource_metadata
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const METADATA: &str = "https://mcp.example.com/.well-known/oauth-protected-resource";

    #[test]
    fn only_an_authorization_failure_is_forbidden_rather_than_unauthorized() {
        assert_eq!(
            AuthRejection::Claims(ClaimRejection::InsufficientScope).status_code(),
            403
        );
        assert_eq!(
            AuthRejection::Claims(ClaimRejection::UnknownCapabilityProfile).status_code(),
            403
        );
        for rejection in [
            AuthRejection::Hop(HopRejection::Missing),
            AuthRejection::Header(HeaderRejection::NoneAlgorithm),
            AuthRejection::Key(KeyRejection::UnknownKid),
            AuthRejection::Signature,
            AuthRejection::Claims(ClaimRejection::WrongSubject),
            AuthRejection::Claims(ClaimRejection::Expired),
            AuthRejection::Discovery(SsrfRejection::ProhibitedAddress),
        ] {
            assert_eq!(rejection.status_code(), 401, "{rejection:?}");
            assert_eq!(rejection.error_code(), INVALID_TOKEN);
        }
    }

    #[test]
    fn every_refusal_renders_identically_to_a_caller() {
        let rejections = [
            AuthRejection::Hop(HopRejection::Missing),
            AuthRejection::Hop(HopRejection::Mismatch),
            AuthRejection::Header(HeaderRejection::HmacAlgorithm),
            AuthRejection::Key(KeyRejection::UnknownKid),
            AuthRejection::Signature,
            AuthRejection::Claims(ClaimRejection::WrongIssuer),
        ];
        let bodies: std::collections::BTreeSet<String> = rejections
            .iter()
            .map(|rejection| {
                let challenge = AuthChallenge::new(rejection, METADATA);
                format!(
                    "{} {} {}",
                    challenge.status(),
                    challenge.body(),
                    challenge.header_value()
                )
            })
            .collect();
        assert_eq!(
            bodies.len(),
            1,
            "every unauthenticated refusal must be indistinguishable, got {bodies:?}"
        );
        for rejection in &rejections {
            assert_eq!(rejection.to_string(), REFUSAL_BODY);
            assert_eq!(format!("{rejection}"), REFUSAL_BODY);
        }
    }

    #[test]
    fn the_challenge_advertises_the_external_metadata_url() {
        let challenge = AuthChallenge::unauthenticated(METADATA);
        assert_eq!(challenge.status(), 401);
        assert_eq!(
            challenge.header_value(),
            format!("Bearer error=\"invalid_token\", resource_metadata=\"{METADATA}\"")
        );
        let forbidden = AuthChallenge::new(
            &AuthRejection::Claims(ClaimRejection::InsufficientScope),
            METADATA,
        );
        assert_eq!(forbidden.status(), 403);
        assert!(forbidden.header_value().contains(INSUFFICIENT_SCOPE));
        assert_eq!(forbidden.body(), REFUSAL_BODY);
    }

    #[test]
    fn internal_labels_stay_diagnosable() {
        assert_eq!(
            AuthRejection::Header(HeaderRejection::JkuHeader).as_str(),
            "jku_header"
        );
        assert_eq!(AuthRejection::Signature.as_str(), "invalid_signature");
        assert_eq!(
            AuthRejection::Hop(HopRejection::Missing).as_str(),
            "hop_credential_missing"
        );
        assert_eq!(
            AuthRejection::Discovery(SsrfRejection::EnvironmentProxy).as_str(),
            "environment_proxy"
        );
    }
}
