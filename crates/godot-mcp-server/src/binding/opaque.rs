//! Cryptographically strong opaque handles for sessions, tasks and artifacts.
//!
//! # Why these are minted rather than derived
//!
//! A handle must not be guessable and must not encode anything about what it
//! points at. Anything derived from a counter, a timestamp, a path, a principal or
//! a request hash fails one of those two tests: a counter is enumerable, and a
//! digest of a name leaks the name to anyone who can guess it. So every handle is
//! [`OPAQUE_HANDLE_BYTES`] bytes read straight from the OS CSPRNG through
//! `getrandom` — the same source Todo 28's launch secret and Todo 29's hop
//! credential use — and the only thing recoverable from a handle is which *kind*
//! of thing it is, because the kind is a fixed prefix rather than a decoded field.
//!
//! # Why the prefixes match the frozen domain identifiers
//!
//! Todo 3 froze `session_`/`artifact_` prefixed lowercase-hex identifiers, and the
//! kernel authorizes artifact reads against a [`SessionId`] and an [`ArtifactId`].
//! Minting in exactly that shape means a handle can *be* the domain identifier
//! rather than needing a second mapping table that could disagree with it.
//! `task_` has no domain counterpart because Todo 3 froze no task identifier; it
//! is defined here in the same shape so all three read alike on the wire.
//!
//! # Comparison is constant time
//!
//! [`OpaqueHandle::matches`] compares through Todo 28's
//! [`constant_time_eq`](crate::http::secret::constant_time_eq). A handle is a
//! bearer-shaped value in exactly the sense that matters here: presenting the right
//! bytes is what selects a record, so a comparison that returned early would leak
//! a prefix one byte at a time.
//!
//! # A handle is not a credential
//!
//! Presenting a handle proves nothing about *who* is presenting it. Every lookup in
//! [`registry`](super::registry) additionally requires the
//! [`BoundSubject`](super::BoundSubject) the request's credential independently
//! resolved to, and a handle whose subject does not match is indistinguishable from
//! one that never existed.

use std::fmt;

use godot_mcp_domain::contracts::{ArtifactId, SessionId};

use crate::http::secret::{compare_probe, constant_time_eq};

/// Bytes of OS entropy behind every handle.
///
/// Matches Todo 28's [`LAUNCH_SECRET_BYTES`](crate::http::LAUNCH_SECRET_BYTES) and
/// Todo 29's `HOP_CREDENTIAL_BYTES`: one entropy width for every unguessable value
/// this crate mints, so there is no weakest link to find.
pub const OPAQUE_HANDLE_BYTES: usize = 32;

/// Hexadecimal characters in a rendered handle payload, excluding its prefix.
pub const OPAQUE_HANDLE_HEX_CHARS: usize = OPAQUE_HANDLE_BYTES * 2;

/// Which kind of thing a handle names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HandleKind {
    /// An MCP session binding.
    Session,
    /// One admitted unit of work.
    Task,
    /// One registered artifact.
    Artifact,
}

impl HandleKind {
    /// Every kind, for exhaustive tests.
    pub const ALL: [Self; 3] = [Self::Session, Self::Task, Self::Artifact];

    /// The fixed prefix a rendered handle of this kind carries.
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Session => "session_",
            Self::Task => "task_",
            Self::Artifact => "artifact_",
        }
    }

    /// A stable snake_case label, for tests and the internal diagnostic sink.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Task => "task",
            Self::Artifact => "artifact",
        }
    }
}

/// The OS CSPRNG refused to produce entropy.
///
/// Carries no detail on purpose: there is nothing a caller can do about it except
/// fail closed, and the underlying message is not information a client should see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the operating system entropy source is unavailable")]
pub struct EntropyUnavailable;

/// One minted opaque handle.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpaqueHandle {
    kind: HandleKind,
    value: String,
}

impl OpaqueHandle {
    /// Mint a fresh handle of `kind` from [`OPAQUE_HANDLE_BYTES`] of OS entropy.
    ///
    /// # Errors
    /// Returns [`EntropyUnavailable`] when the OS CSPRNG refuses. There is no
    /// fallback generator: a weaker handle is worse than no handle.
    pub fn mint(kind: HandleKind) -> Result<Self, EntropyUnavailable> {
        let mut bytes = [0_u8; OPAQUE_HANDLE_BYTES];
        getrandom::fill(&mut bytes).map_err(|_| EntropyUnavailable)?;
        Ok(Self {
            kind,
            value: format!("{}{}", kind.prefix(), hex_encode(&bytes)),
        })
    }

    /// Accept a rendered handle of `kind`, or reject it.
    ///
    /// Rejection is a plain `None`: the shape of a handle is public, so there is
    /// nothing to disclose, and a caller that needs a refusal produces the single
    /// non-enumerating one from [`registry`](super::registry).
    #[must_use]
    pub fn parse(kind: HandleKind, candidate: &str) -> Option<Self> {
        let payload = candidate.strip_prefix(kind.prefix())?;
        if payload.len() != OPAQUE_HANDLE_HEX_CHARS {
            return None;
        }
        if !payload
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return None;
        }
        Some(Self {
            kind,
            value: candidate.to_owned(),
        })
    }

    /// Which kind of thing this handle names.
    #[must_use]
    pub const fn kind(&self) -> HandleKind {
        self.kind
    }

    /// The rendered handle, prefix included.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Whether `candidate` is this handle, compared in constant time.
    #[must_use]
    pub fn matches(&self, candidate: &str) -> bool {
        constant_time_eq(self.value.as_bytes(), candidate.as_bytes())
    }

    /// How many byte comparisons [`Self::matches`] performs for `candidate`.
    ///
    /// Exists so a test can prove the comparison does not short-circuit: two
    /// candidates differing at opposite ends must report the same count.
    #[must_use]
    pub fn compare_probe(&self, candidate: &str) -> usize {
        compare_probe(self.value.as_bytes(), candidate.as_bytes())
    }

    /// This handle as the frozen domain session identifier, when it names a session.
    #[must_use]
    pub fn to_session_id(&self) -> Option<SessionId> {
        if self.kind != HandleKind::Session {
            return None;
        }
        SessionId::parse(self.value.clone()).ok()
    }

    /// This handle as the frozen domain artifact identifier, when it names one.
    #[must_use]
    pub fn to_artifact_id(&self) -> Option<ArtifactId> {
        if self.kind != HandleKind::Artifact {
            return None;
        }
        ArtifactId::parse(self.value.clone()).ok()
    }
}

impl fmt::Display for OpaqueHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.value)
    }
}

/// Lowercase hexadecimal, so a handle renders in the shape Todo 3 froze.
fn hex_encode(bytes: &[u8]) -> String {
    let mut rendered = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut rendered, "{byte:02x}").expect("writing to a String cannot fail");
    }
    rendered
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn every_kind_mints_a_full_width_lowercase_hex_handle() {
        for kind in HandleKind::ALL {
            let handle = OpaqueHandle::mint(kind).expect("os entropy");
            assert_eq!(handle.kind(), kind);
            let payload = handle
                .as_str()
                .strip_prefix(kind.prefix())
                .expect("the rendered handle carries its kind prefix");
            assert_eq!(payload.len(), OPAQUE_HANDLE_HEX_CHARS);
            assert!(
                payload
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "a handle payload must be lowercase hexadecimal, got {payload}"
            );
        }
    }

    #[test]
    fn minting_never_repeats_across_a_large_batch() {
        for kind in HandleKind::ALL {
            let minted: BTreeSet<String> = (0..256)
                .map(|_| {
                    OpaqueHandle::mint(kind)
                        .expect("os entropy")
                        .as_str()
                        .to_owned()
                })
                .collect();
            assert_eq!(
                minted.len(),
                256,
                "256 mints of {} must all differ",
                kind.as_str()
            );
        }
    }

    #[test]
    fn a_handle_round_trips_through_its_rendered_form_but_not_another_kind() {
        let session = OpaqueHandle::mint(HandleKind::Session).expect("os entropy");
        assert_eq!(
            OpaqueHandle::parse(HandleKind::Session, session.as_str()).as_ref(),
            Some(&session)
        );
        assert!(OpaqueHandle::parse(HandleKind::Task, session.as_str()).is_none());
        assert!(OpaqueHandle::parse(HandleKind::Artifact, session.as_str()).is_none());
    }

    #[test]
    fn a_truncated_or_uppercase_or_unprefixed_candidate_is_refused() {
        let handle = OpaqueHandle::mint(HandleKind::Task).expect("os entropy");
        let rendered = handle.as_str();
        assert!(OpaqueHandle::parse(HandleKind::Task, &rendered[..rendered.len() - 1]).is_none());
        assert!(
            OpaqueHandle::parse(HandleKind::Task, &format!("{rendered}0")).is_none(),
            "an over-long payload must be refused"
        );
        assert!(
            OpaqueHandle::parse(HandleKind::Task, &rendered.to_ascii_uppercase()).is_none(),
            "uppercase hexadecimal is not the frozen shape"
        );
        let payload = rendered
            .strip_prefix(HandleKind::Task.prefix())
            .expect("prefix");
        assert!(OpaqueHandle::parse(HandleKind::Task, payload).is_none());
    }

    #[test]
    fn comparison_examines_every_byte_regardless_of_where_a_candidate_differs() {
        let handle = OpaqueHandle::mint(HandleKind::Session).expect("os entropy");
        let rendered = handle.as_str().to_owned();
        let flip = |index: usize| {
            let mut bytes = rendered.clone().into_bytes();
            bytes[index] = if bytes[index] == b'a' { b'b' } else { b'a' };
            String::from_utf8(bytes).expect("ascii stays utf-8")
        };
        let first = flip(HandleKind::Session.prefix().len());
        let last = flip(rendered.len() - 1);
        assert!(!handle.matches(&first));
        assert!(!handle.matches(&last));
        assert_eq!(handle.compare_probe(&first), handle.compare_probe(&last));
        assert_eq!(handle.compare_probe(&rendered), rendered.len());
        assert!(handle.matches(&rendered));
    }

    #[test]
    fn a_session_handle_is_a_domain_session_and_an_artifact_handle_a_domain_artifact() {
        let session = OpaqueHandle::mint(HandleKind::Session).expect("os entropy");
        assert_eq!(
            session
                .to_session_id()
                .expect("a session handle is a domain session identifier")
                .as_str(),
            session.as_str()
        );
        assert!(session.to_artifact_id().is_none());

        let artifact = OpaqueHandle::mint(HandleKind::Artifact).expect("os entropy");
        assert_eq!(
            artifact
                .to_artifact_id()
                .expect("an artifact handle is a domain artifact identifier")
                .as_str(),
            artifact.as_str()
        );
        assert!(artifact.to_session_id().is_none());

        let task = OpaqueHandle::mint(HandleKind::Task).expect("os entropy");
        assert!(task.to_session_id().is_none());
        assert!(task.to_artifact_id().is_none());
    }

    #[test]
    fn the_entropy_width_matches_every_other_unguessable_value_this_crate_mints() {
        assert_eq!(OPAQUE_HANDLE_BYTES, crate::http::LAUNCH_SECRET_BYTES);
        #[cfg(feature = "auth")]
        assert_eq!(OPAQUE_HANDLE_BYTES, crate::auth::hop::HOP_CREDENTIAL_BYTES);
        assert_eq!(OPAQUE_HANDLE_HEX_CHARS, 64);
    }
}
