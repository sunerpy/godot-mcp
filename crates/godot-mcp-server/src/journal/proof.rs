//! Typed durable proof derivation.
//!
//! A [`ProofSource`] re-derives the *observed* state of touched paths through
//! Todo 2's descriptor/handle-relative `SafeRoot` primitives. It never accepts a
//! display path as authority and never follows a symlink; every observation is
//! relative to the held project root.
//!
//! `SafeRoot` content hashing is proof **only** for the `file` effect kind. The
//! `process_launch` and `editor_runtime` kinds are proven by their own typed
//! records (see [`super::record::TypedProof`]) and are deliberately unreachable
//! from this module.

use std::fmt;
use std::path::Path;

use godot_mcp_domain::safe_root::{SafeRoot, SafeRootError};

use super::record::{Existence, FileOperation, FileStateEntry, hex_sha256};

/// Re-derives observed file state for the `file` effect kind.
pub(crate) trait ProofSource: Send + Sync {
    /// Read the current bytes of one project-relative path. `Ok(None)` means the
    /// path is absent (an existence tombstone).
    fn read_bytes(&self, path: &str) -> Result<Option<Vec<u8>>, SafeRootError>;

    /// Observe the current existence and content hash of one project-relative
    /// path. `Ok(None)` means the path is absent (an existence tombstone).
    fn observe(&self, path: &str) -> Result<Option<String>, SafeRootError> {
        Ok(self.read_bytes(path)?.map(|bytes| hex_sha256(&bytes)))
    }

    /// Observe every entry, preserving the declared operation so a `create`
    /// tombstone is never confused with an `update` hash.
    fn observe_file_states(
        &self,
        entries: &[FileStateEntry],
    ) -> Result<Vec<FileStateEntry>, SafeRootError> {
        entries
            .iter()
            .map(|entry| {
                let observed = self.observe(&entry.path)?;
                Ok(FileStateEntry {
                    path: entry.path.clone(),
                    operation: entry.operation,
                    existence: if observed.is_some() {
                        Existence::Present
                    } else {
                        Existence::Absent
                    },
                    sha256: observed,
                })
            })
            .collect()
    }
}

/// The production proof source: descriptor-relative reads through a held lease.
pub(crate) struct SafeRootProofSource {
    root: SafeRoot,
}

impl SafeRootProofSource {
    pub(crate) const fn new(root: SafeRoot) -> Self {
        Self { root }
    }
}

impl ProofSource for SafeRootProofSource {
    fn read_bytes(&self, path: &str) -> Result<Option<Vec<u8>>, SafeRootError> {
        match self.root.read(Path::new(path)) {
            Ok(bytes) => Ok(Some(bytes)),
            // Todo 2 maps an absent in-root target to a stale-preimage outcome;
            // for proof purposes that is exactly the existence tombstone.
            Err(SafeRootError::StalePreimage { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// Whether an independently re-derived observation matches a recorded state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProofOutcome {
    /// Every entry matched exactly.
    Matches,
    /// At least one entry did not match.
    Mismatch,
    /// The state could not be observed at all.
    Unobservable,
    /// There was nothing recorded to compare against.
    NoRecordedState,
}

impl fmt::Display for ProofOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Matches => "matches",
            Self::Mismatch => "mismatch",
            Self::Unobservable => "unobservable",
            Self::NoRecordedState => "no recorded state",
        })
    }
}

/// Compare recorded `file` state against a freshly derived observation.
///
/// Per-operation rules (normative):
/// * `update` — the path must exist with exactly the recorded hash.
/// * `create` — a pre-state entry is proven by an existence tombstone (absent, no
///   hash); a post-state entry requires existence with the recorded hash.
/// * `delete` — a pre-state entry requires existence with the recorded hash; a
///   post-state entry is proven by an existence tombstone (absent, no hash).
///
/// The recorded entry already carries the expected existence/hash pair for its
/// side, so this comparison is a strict equality on `(existence, sha256)`.
pub(crate) fn verify_file_proof(
    source: &dyn ProofSource,
    recorded: &[FileStateEntry],
) -> ProofOutcome {
    if recorded.is_empty() {
        return ProofOutcome::NoRecordedState;
    }
    for entry in recorded {
        let observed = match source.observe(&entry.path) {
            Ok(observed) => observed,
            Err(_) => return ProofOutcome::Unobservable,
        };
        let expected_present = entry.existence == Existence::Present;
        if observed.is_some() != expected_present {
            return ProofOutcome::Mismatch;
        }
        match (&entry.sha256, &observed) {
            // An absent path has no hash to compare; existence alone is the proof.
            (None, None) => {}
            (Some(expected), Some(actual)) if expected == actual => {}
            _ => return ProofOutcome::Mismatch,
        }
        // A present entry with no recorded hash could never prove content, so it
        // is never accepted as a hash-bearing proof.
        if expected_present && entry.sha256.is_none() {
            return ProofOutcome::Mismatch;
        }
    }
    ProofOutcome::Matches
}

/// Build the recorded pre-state entry for one planned mutation.
pub(crate) fn pre_state_entry(
    source: &dyn ProofSource,
    path: &str,
    operation: FileOperation,
) -> Result<FileStateEntry, SafeRootError> {
    let observed = source.observe(path)?;
    Ok(FileStateEntry {
        path: path.to_owned(),
        operation,
        existence: if observed.is_some() {
            Existence::Present
        } else {
            Existence::Absent
        },
        sha256: observed,
    })
}

/// Build the intended post-state entry for one planned mutation.
pub(crate) fn intended_post_entry(
    path: &str,
    operation: FileOperation,
    intended_bytes: Option<&[u8]>,
) -> FileStateEntry {
    match operation {
        FileOperation::Delete => FileStateEntry {
            path: path.to_owned(),
            operation,
            existence: Existence::Absent,
            sha256: None,
        },
        FileOperation::Create | FileOperation::Update => FileStateEntry {
            path: path.to_owned(),
            operation,
            existence: Existence::Present,
            sha256: intended_bytes.map(hex_sha256),
        },
    }
}
