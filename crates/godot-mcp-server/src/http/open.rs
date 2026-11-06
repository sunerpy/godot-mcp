//! The `open` HTTP mode: a listener for a development network (ADR 0014).
//!
//! An open listener may bind any address, so a phone, a second machine or a
//! container on the same network can reach it. It asks for no credential
//! unless the operator configured one fixed token, which every request then
//! presents as `Authorization: Bearer <token>`, compared in constant time.
//!
//! The listener cannot tell its callers apart, so it serves every request as
//! one principal named after this launch: sessions, tasks and artifacts stay
//! bound to it exactly as they are to the launch secret's principal under
//! `trusted_local`, and the journal and audit record the mode as `open`.

use godot_mcp_domain::config::Secret;
use godot_mcp_domain::contracts::PrincipalId;
use sha2::{Digest, Sha256};

use super::secret::constant_time_eq;
use crate::idempotency::ModePrincipal;

/// Why an open listener could not be prepared.
#[derive(Debug, thiserror::Error)]
pub enum OpenAccessError {
    /// The operating system's random source refused.
    #[error("no entropy for the open listener's launch principal: {detail}")]
    NoEntropy {
        /// What the random source reported.
        detail: String,
    },
}

/// What an open listener accepts, and who it serves every request as.
pub struct OpenAccess {
    launch: [u8; 32],
    token: Option<Secret>,
}

impl std::fmt::Debug for OpenAccess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAccess")
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .finish_non_exhaustive()
    }
}

impl OpenAccess {
    /// Prepare one launch's open listener, requiring `token` when one is set.
    ///
    /// # Errors
    /// Returns [`OpenAccessError::NoEntropy`] when the random source refuses.
    pub fn new(token: Option<Secret>) -> Result<Self, OpenAccessError> {
        let mut launch = [0_u8; 32];
        getrandom::fill(&mut launch).map_err(|error| OpenAccessError::NoEntropy {
            detail: error.to_string(),
        })?;
        Ok(Self {
            launch,
            token: token.filter(|token| !token.is_empty()),
        })
    }

    /// Whether requests must present the configured token.
    #[must_use]
    pub const fn requires_token(&self) -> bool {
        self.token.is_some()
    }

    /// The principal every request of this launch is served as: stable for the
    /// launch, different across launches, and never derived from the token.
    #[must_use]
    pub fn principal(&self) -> PrincipalId {
        let mut hasher = Sha256::new();
        hasher.update(b"godot-mcp-http-open-principal-v1");
        hasher.update(self.launch);
        let digest = hasher.finalize();
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        PrincipalId::parse(format!("principal_{hex}"))
            .expect("a 64-character lowercase hex payload is a valid principal identifier")
    }

    /// The identity shape the `open` mode presents.
    #[must_use]
    pub fn mode_principal(&self) -> ModePrincipal {
        ModePrincipal::OpenLaunch {
            principal: self.principal().as_str().to_owned(),
        }
    }

    /// Whether a request presenting `bearer` (the `Authorization` header's
    /// bearer value, empty when absent) is admitted.
    #[must_use]
    pub fn admits(&self, bearer: &str) -> bool {
        match self.token.as_ref() {
            None => true,
            Some(token) => constant_time_eq(token.expose().as_bytes(), bearer.as_bytes()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_token_every_request_is_admitted_as_the_launch_principal() {
        let open = OpenAccess::new(None).expect("entropy");
        assert!(!open.requires_token());
        assert!(open.admits(""));
        assert!(open.admits("anything"));
        assert_eq!(open.principal(), open.principal());
        let ModePrincipal::OpenLaunch { principal } = open.mode_principal() else {
            panic!("the open mode presents its own principal shape");
        };
        assert!(principal.starts_with("principal_"));
        let other = OpenAccess::new(None).expect("entropy");
        assert_ne!(
            other.principal(),
            open.principal(),
            "a principal names one launch"
        );
    }

    #[test]
    fn with_a_token_only_that_token_is_admitted() {
        let open = OpenAccess::new(Some(Secret::new("lan-token-1"))).expect("entropy");
        assert!(open.requires_token());
        assert!(open.admits("lan-token-1"));
        for wrong in ["", "lan-token-2", "lan-token-1 ", "LAN-TOKEN-1"] {
            assert!(!open.admits(wrong), "{wrong:?}");
        }
        assert!(!open.principal().as_str().contains("lan-token-1"));
        assert!(!format!("{open:?}").contains("lan-token-1"));
        // An empty configured token is no token.
        assert!(
            !OpenAccess::new(Some(Secret::new("")))
                .expect("entropy")
                .requires_token()
        );
    }
}
