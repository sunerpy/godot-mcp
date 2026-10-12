//! Incremental index persistence and filesystem coherence.
//!
//! This module is the first layer that *remembers* an analysis result across
//! process lifetimes. It stores per-file slices of Todo 10's semantic graph in a
//! project-lock-owned cache, fingerprints the bytes each slice was derived from,
//! and refuses to present a remembered answer as current once those bytes stop
//! matching.
//!
//! ## Ownership and cache identity
//!
//! * A store is only reachable through [`ProjectIndex::open`], which takes a
//!   [`ResolvedProject`](crate::project::ResolvedProject). Opening an index
//!   without holding the project lease is therefore not expressible: the type
//!   system, not a runtime check, carries that invariant.
//! * The cache location is keyed by [`IndexCacheKey`], derived from Todo 8's
//!   path-free [`ProjectIdentity`](godot_mcp_domain::contracts::ProjectIdentity)
//!   — the held-object lease identity of the root directory composed with that of
//!   its `project.godot`. The opened *path spelling* is never a component of the
//!   key, so two aliases of the same objects share one cache and a directory
//!   renamed in place keeps its cache. The project generation is deliberately
//!   excluded: a generation bump is a new snapshot of the same project, not a new
//!   cache.
//! * The canonical display path remains metadata only
//!   ([`ProjectIndex::display_root`]); it is never authority and never a key.
//!
//! ## Reads
//!
//! Every project byte is read through the held lease's `SafeRoot` by way of
//! [`ResolvedProject::read_source_file`](crate::project::ResolvedProject::read_source_file),
//! so `..`, absolute paths, in-root symlinks and the `.godot/` engine cache are
//! refused by Todo 2/8 rather than re-checked here. This module owns no path
//! authority, opens no project file itself and never follows a symlink out of
//! the root. Because Todo 2 exposes no in-root directory listing primitive, the
//! set of project-relative paths to index is supplied by the caller as a
//! [`FileInventory`]; the index still verifies each declared path by reading it
//! through the lease.
//!
//! ## Coherence, never a stale success
//!
//! [`Coherence`] is a structured verdict, not a boolean:
//!
//! | Verdict | Meaning |
//! | --- | --- |
//! | [`Coherence::Coherent`] | every stored slice's fingerprint still matches the bytes on disk |
//! | [`Coherence::Stale`] | the store is readable but provably behind the filesystem, with the reason and the exact subjects |
//! | [`Coherence::RescanRequired`] | the store cannot be trusted at all (schema mismatch, corruption, dropped watch events) and must be rebuilt before any answer is served |
//!
//! A snapshot always carries its verdict, so a caller cannot accidentally read a
//! remembered exact answer as a current one: see [`IndexSnapshot::is_current`].
//!
//! ## Incremental invalidation
//!
//! A slice is the part of the semantic graph attributable to one file. Editing
//! one file changes that file's [`SliceDigest`] and the whole-graph
//! [`IndexSnapshot::graph_digest`], and leaves every untouched slice digest
//! byte-identical. Deletions retire a slice; renames retire the old key and
//! register the new one. Any batch of [`WatchSignal`]s — in any order, and an
//! overflow included — converges on the same bytes a [`ScanKind::ForcedFull`]
//! scan would produce.
//!
//! ## Boundaries
//!
//! * No transport, no UI, no Godot process, and no MCP tool: exposing this as
//!   tools is Todo 12's work.
//! * Parsing is delegated verbatim to Todos 7–9 and graph derivation to Todo 10;
//!   nothing here re-implements a grammar or a relationship rule.

mod cache_key;
mod error;
mod fingerprint;
mod snapshot;
mod store;
mod watch;

pub use cache_key::{CACHE_KEY_PREFIX, IndexCacheKey};
pub use error::IndexError;
pub use fingerprint::{FileFingerprint, FileInventory};
pub use snapshot::{Coherence, IndexSnapshot, RescanReason, ScanKind, SliceDigest, StaleReason};
pub use store::{
    INDEX_MANIFEST_FILE, INDEX_STORE_DDL, INDEX_STORE_FILE, IndexReader, ProjectIndex,
};
pub use watch::{WatchBatch, WatchDebouncer, WatchSignal};

/// Stable schema identity of a serialized [`IndexSnapshot`].
pub const INDEX_SCHEMA: &str = "godot-mcp.index.v1";

/// Persisted store layout version understood by this build.
///
/// A store stamped with any other value is never read incrementally: it is
/// [`RescanReason::SchemaMismatch`] and must be rebuilt through the held lease.
pub const INDEX_SCHEMA_VERSION: u32 = 1;
