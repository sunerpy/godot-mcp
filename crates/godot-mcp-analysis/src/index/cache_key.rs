//! Cache identity: derived from the project's path-free held-object identity.

use std::fmt;
use std::path::{Path, PathBuf};

use godot_mcp_domain::contracts::ProjectIdentity;

/// Prefix of every serialized [`IndexCacheKey`] spelling.
pub const CACHE_KEY_PREFIX: &str = "godot-mcp-index-";

/// Where one project's index cache lives inside the per-user state directory.
///
/// The key is derived **only** from
/// [`ProjectIdentity::object_id`] — the one-way digest of Todo 2's held-object
/// lease identity (root directory object composed with `project.godot` object).
/// Two consequences are load-bearing:
///
/// * The opened path spelling is never a component of the key. A project moved
///   or aliased on disk keeps one cache, and two different projects can never
///   collide onto one cache just because they share a spelling.
/// * [`ProjectIdentity::generation`] is deliberately excluded. A generation bump
///   marks a new snapshot of the same project; it must reuse — and invalidate
///   inside — the same cache rather than orphan it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexCacheKey {
    directory_name: String,
}

impl IndexCacheKey {
    /// Derive the cache key for one resolved project identity.
    ///
    /// Only the opaque object id contributes; the generation is ignored on
    /// purpose so warm reads of a later generation find the same cache.
    #[must_use]
    pub fn from_identity(identity: &ProjectIdentity) -> Self {
        Self {
            directory_name: format!("{CACHE_KEY_PREFIX}{}", identity.object_id().as_str()),
        }
    }

    /// The cache directory name, safe to use as a single path component.
    #[must_use]
    pub fn directory_name(&self) -> &str {
        &self.directory_name
    }

    /// Absolute cache directory for this project under `state_dir`.
    #[must_use]
    pub fn cache_dir(&self, state_dir: &Path) -> PathBuf {
        state_dir.join(&self.directory_name)
    }
}

impl fmt::Display for IndexCacheKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.directory_name)
    }
}
