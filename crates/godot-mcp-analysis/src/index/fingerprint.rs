//! Deterministic file fingerprints and the caller-declared file inventory.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

use super::error::IndexError;

/// Content fingerprint of one project file.
///
/// The value is derived from the file's **bytes only** — never from its mtime,
/// its size alone, or its inode — so a same-size touch does not fabricate a
/// change and a restored copy re-uses its cached slice. It is a
/// non-cryptographic content digest: this store is per-user and lease-owned, so
/// the fingerprint is a coherence signal, never a security boundary.
///
/// Two files with identical bytes fingerprint identically by design: a renamed
/// file therefore needs no reparse, only a re-key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileFingerprint(String);

impl FileFingerprint {
    /// Fingerprint one already-read file's bytes.
    #[must_use]
    pub fn of_bytes(bytes: &[u8]) -> Self {
        // FNV-1a is sufficient here because this digest is only a local coherence
        // signal, never an authenticity or trust boundary. Binding the byte length
        // avoids treating a sequence and one of its zero-extended forms alike.
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in bytes.len().to_le_bytes().iter().chain(bytes) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self(format!("{hash:016x}"))
    }

    /// The stable hex spelling stored in the cache and compared for coherence.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Byte length of the fingerprint spelling.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the spelling is empty, which a derived fingerprint never is.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for FileFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The set of project-relative paths one scan is allowed to consider.
///
/// Todo 2 exposes no in-root directory listing primitive, so the caller declares
/// which project-relative paths exist. The inventory is a *claim*, not authority:
/// every declared path is still read through the held lease, and a path the lease
/// refuses (escaping, symlinked, `.godot/`) is rejected rather than indexed.
///
/// Paths are normalized to forward slashes and kept in a
/// [`BTreeMap`]-backed order, so a scan visits them deterministically and no hash
/// iteration order can reach the persisted bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileInventory {
    entries: BTreeMap<String, ()>,
}

impl FileInventory {
    /// An empty inventory.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Declare every project-relative path in `paths`.
    ///
    /// # Errors
    /// Returns [`IndexError::InvalidInput`] when a spelling is empty, absolute,
    /// or contains a `..` component: those are refused here so the caller learns
    /// about the mistake before any read is attempted.
    pub fn from_paths<I, S>(paths: I) -> Result<Self, IndexError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut inventory = Self::new();
        for path in paths {
            inventory.insert(path.as_ref())?;
        }
        Ok(inventory)
    }

    /// Declare one project-relative path.
    ///
    /// # Errors
    /// Returns [`IndexError::InvalidInput`] for an empty, absolute or escaping
    /// spelling.
    pub fn insert(&mut self, path: &str) -> Result<(), IndexError> {
        if path.trim().is_empty() {
            return Err(IndexError::InvalidInput {
                reason: "a declared path must not be empty".to_owned(),
            });
        }

        let forward = path.replace('\\', "/");
        let candidate = Path::new(&forward);
        if candidate.is_absolute() {
            return Err(IndexError::InvalidInput {
                reason: format!("a declared path must be relative: {path}"),
            });
        }

        let mut parts = Vec::new();
        for component in candidate.components() {
            match component {
                Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(IndexError::InvalidInput {
                        reason: format!("a declared path must stay inside the project: {path}"),
                    });
                }
            }
        }
        if parts.is_empty() {
            return Err(IndexError::InvalidInput {
                reason: "a declared path must name a file".to_owned(),
            });
        }

        let normalized = parts.join("/");
        if self.entries.insert(normalized.clone(), ()).is_some() {
            return Err(IndexError::InvalidInput {
                reason: format!("duplicate declared path: {normalized}"),
            });
        }
        Ok(())
    }

    /// Every declared path, in deterministic order.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Whether `path` is declared.
    #[must_use]
    pub fn contains(&self, path: &str) -> bool {
        self.entries.contains_key(path)
    }

    /// Number of declared paths.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
