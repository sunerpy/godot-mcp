//! Snapshot records: what a scan produced, and how far it can be trusted.

use std::collections::BTreeMap;

use godot_mcp_domain::Confidence;
use godot_mcp_domain::contracts::{ProjectIdentity, Provenance, ProvenanceSource};
use serde::{Deserialize, Serialize};

use super::error::IndexError;
use super::fingerprint::FileFingerprint;
use crate::graph::SemanticGraph;

/// How the snapshot in hand was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanKind {
    /// No usable store existed: every file was read and parsed.
    Cold,
    /// A coherent store was reused without reparsing any file.
    Warm,
    /// A coherent store was reused and only the changed files were reparsed.
    Incremental,
    /// The store was discarded on purpose — a schema mismatch, corruption, or a
    /// dropped watch batch — and rebuilt from scratch.
    ForcedFull,
}

const fn default_scan_kind() -> ScanKind {
    ScanKind::Cold
}

impl ScanKind {
    /// Stable wire code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Warm => "warm",
            Self::Incremental => "incremental",
            Self::ForcedFull => "forced_full",
        }
    }

    /// Whether this scan read every project file rather than a subset.
    #[must_use]
    pub const fn is_full(self) -> bool {
        matches!(self, Self::Cold | Self::ForcedFull)
    }
}

/// Why a readable store is provably behind the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleReason {
    /// A stored slice's file no longer fingerprints to the stored value.
    FileContentChanged,
    /// A stored slice's file is gone from the inventory and unreadable.
    FileRemoved,
    /// The inventory declares a file the store has no slice for.
    FileAdded,
    /// A stored slice's bytes reappeared under a different project-relative path.
    FileRenamed,
    /// The store was written for an earlier project generation.
    GenerationAdvanced,
}

impl StaleReason {
    /// Stable wire code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::FileContentChanged => "file_content_changed",
            Self::FileRemoved => "file_removed",
            Self::FileAdded => "file_added",
            Self::FileRenamed => "file_renamed",
            Self::GenerationAdvanced => "generation_advanced",
        }
    }
}

/// Why the store cannot be trusted at all and must be rebuilt.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RescanReason {
    /// The persisted layout version is not the one this build understands.
    SchemaMismatch {
        /// Version found in the store.
        stored: u32,
        /// Version this build writes and reads.
        expected: u32,
    },
    /// The store decoded but violates its own invariants.
    StoreCorrupt {
        /// Explanation of the violated invariant.
        reason: String,
    },
    /// The watcher dropped events, so the change set is unknowable and no
    /// incremental result can be honest.
    WatchOverflow,
    /// The caller supplied no inventory, so absence cannot be distinguished from
    /// "not declared" and a partial store would read as complete.
    InventoryUnavailable,
}

impl RescanReason {
    /// Stable wire code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::SchemaMismatch { .. } => "schema_mismatch",
            Self::StoreCorrupt { .. } => "store_corrupt",
            Self::WatchOverflow => "watch_overflow",
            Self::InventoryUnavailable => "inventory_unavailable",
        }
    }
}

/// How far a snapshot may be trusted.
///
/// Deliberately **not** a boolean: "not coherent" splits into "behind the
/// filesystem in a way I can name and repair incrementally" and "unusable, throw
/// it away". Collapsing the two is exactly how a stale exact answer gets served
/// as current.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "verdict")]
pub enum Coherence {
    /// Every stored slice's fingerprint still matches the bytes on disk.
    Coherent,
    /// The store is readable but behind the filesystem. The reason names what
    /// changed and `subjects` names the exact project-relative paths.
    Stale {
        /// Why the store is behind.
        reason: StaleReason,
        /// Project-relative paths responsible, in deterministic order.
        subjects: Vec<String>,
    },
    /// The store cannot be used at all; a full rescan is required before any
    /// answer is served.
    RescanRequired {
        /// Why the store is untrustworthy.
        reason: RescanReason,
        /// Project-relative paths responsible, when any are known.
        subjects: Vec<String>,
    },
}

impl Coherence {
    /// Whether the snapshot may be presented as current.
    #[must_use]
    pub const fn is_coherent(&self) -> bool {
        matches!(self, Self::Coherent)
    }

    /// Whether a full rebuild is required before serving an answer.
    #[must_use]
    pub const fn requires_rescan(&self) -> bool {
        matches!(self, Self::RescanRequired { .. })
    }

    /// The stale reason, when the store is merely behind.
    #[must_use]
    pub const fn stale_reason(&self) -> Option<StaleReason> {
        match self {
            Self::Stale { reason, .. } => Some(*reason),
            _ => None,
        }
    }

    /// The rescan reason, when the store is untrustworthy.
    #[must_use]
    pub const fn rescan_reason(&self) -> Option<&RescanReason> {
        match self {
            Self::RescanRequired { reason, .. } => Some(reason),
            _ => None,
        }
    }

    /// The project-relative paths this verdict names.
    #[must_use]
    pub fn subjects(&self) -> &[String] {
        match self {
            Self::Coherent => &[],
            Self::Stale { subjects, .. } | Self::RescanRequired { subjects, .. } => subjects,
        }
    }

    /// Stable wire code of the verdict itself.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Coherent => "coherent",
            Self::Stale { .. } => "stale",
            Self::RescanRequired { .. } => "rescan_required",
        }
    }
}

/// The persisted per-file slice of the semantic graph.
///
/// One slice covers everything attributable to one project-relative file: the
/// nodes declared in it and the edges recorded against it. `digest` is byte
/// stable across builds over the same bytes, so an untouched file's slice digest
/// is proof that nothing about that file changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SliceDigest {
    pub(super) file: String,
    pub(super) fingerprint: FileFingerprint,
    pub(super) digest: String,
    pub(super) node_count: u32,
    pub(super) edge_count: u32,
}

impl SliceDigest {
    /// Project-relative path this slice covers.
    #[must_use]
    pub fn file(&self) -> &str {
        &self.file
    }

    /// Content fingerprint of the bytes the slice was derived from.
    #[must_use]
    pub const fn fingerprint(&self) -> &FileFingerprint {
        &self.fingerprint
    }

    /// Byte-stable digest of this file's graph slice.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Number of graph nodes attributed to this file.
    #[must_use]
    pub const fn node_count(&self) -> u32 {
        self.node_count
    }

    /// Number of graph edges attributed to this file.
    #[must_use]
    pub const fn edge_count(&self) -> u32 {
        self.edge_count
    }
}

/// One coherent (or explicitly non-coherent) view of a project's index.
///
/// A snapshot always carries its own [`Coherence`] verdict and its own
/// [`Provenance`], so a caller physically cannot read a remembered answer without
/// also reading how current it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexSnapshot {
    pub(super) schema: String,
    pub(super) schema_version: u32,
    pub(super) project: ProjectIdentity,
    #[serde(skip, default = "default_scan_kind")]
    pub(super) scan_kind: ScanKind,
    pub(super) graph_digest: String,
    pub(super) slices: BTreeMap<String, SliceDigest>,
    pub(super) coherence: Coherence,
    pub(super) graph: SemanticGraph,
    pub(super) provenance: Provenance,
}

impl IndexSnapshot {
    /// Stable schema identity of the serialized snapshot.
    #[must_use]
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// Persisted store layout version this snapshot was produced against.
    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Path-free project identity, including the generation the read is
    /// attributed to. This is the source snapshot identity every result retains.
    #[must_use]
    pub const fn project(&self) -> &ProjectIdentity {
        &self.project
    }

    /// How this snapshot was produced.
    #[must_use]
    pub const fn scan_kind(&self) -> ScanKind {
        self.scan_kind
    }

    /// Byte-stable digest of the whole semantic graph.
    #[must_use]
    pub fn graph_digest(&self) -> &str {
        &self.graph_digest
    }

    /// Every per-file slice, keyed by project-relative path.
    #[must_use]
    pub const fn slices(&self) -> &BTreeMap<String, SliceDigest> {
        &self.slices
    }

    /// The slice for one project-relative path, when the index has one.
    #[must_use]
    pub fn slice(&self, file: &str) -> Option<&SliceDigest> {
        self.slices.get(file)
    }

    /// How far this snapshot may be trusted.
    #[must_use]
    pub const fn coherence(&self) -> &Coherence {
        &self.coherence
    }

    /// Whether this snapshot may be presented as the project's current state.
    ///
    /// A stale or rescan-required snapshot answers `false` even though its data
    /// is readable, so an exact-looking answer is never served as current.
    #[must_use]
    pub const fn is_current(&self) -> bool {
        self.coherence.is_coherent()
    }

    /// The materialized semantic graph this snapshot describes.
    #[must_use]
    pub const fn graph(&self) -> &SemanticGraph {
        &self.graph
    }

    /// Evidence and honesty classification of the snapshot as a whole.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    /// A copy of this snapshot that admits it is behind the project.
    ///
    /// A workspace coordinator whose refresh failed keeps the previous snapshot
    /// readable, but must never present it as current: the copy carries the
    /// given non-coherent verdict and a heuristic, inferred provenance, so every
    /// answer rendered from it is weakened accordingly.
    ///
    /// # Errors
    /// Returns [`IndexError::InvalidInput`] for a coherent verdict, which would
    /// make the copy a lie, or a contract violation from the provenance model.
    pub fn superseded(&self, coherence: Coherence) -> Result<Self, IndexError> {
        if coherence.is_coherent() {
            return Err(IndexError::InvalidInput {
                reason: "a superseded snapshot cannot be coherent".to_owned(),
            });
        }
        let mut stale = self.clone();
        stale.provenance = Provenance::new(
            ProvenanceSource::Heuristic,
            Confidence::Inferred,
            "project-index",
            self.project.generation(),
        )?;
        stale.coherence = coherence;
        Ok(stale)
    }

    /// An empty snapshot for a project whose first scan could not complete.
    ///
    /// It holds no slice and an empty graph, and its verdict requires a rescan,
    /// so it can be served while saying exactly why nothing is indexed.
    ///
    /// # Errors
    /// Returns an [`IndexError`] when the empty graph or its provenance cannot be
    /// constructed.
    pub fn unavailable(project: ProjectIdentity, reason: RescanReason) -> Result<Self, IndexError> {
        let graph = crate::graph::SemanticGraphBuilder::new(project.generation()).build()?;
        let rendered = graph
            .to_canonical_json()
            .map_err(|error| IndexError::Io(error.to_string()))?;
        let graph_digest = FileFingerprint::of_bytes(rendered.as_bytes())
            .as_str()
            .to_owned();
        let provenance = Provenance::new(
            ProvenanceSource::Heuristic,
            Confidence::Inferred,
            "project-index",
            project.generation(),
        )?;
        Ok(Self {
            schema: super::INDEX_SCHEMA.to_owned(),
            schema_version: super::INDEX_SCHEMA_VERSION,
            project,
            scan_kind: ScanKind::Cold,
            graph_digest,
            slices: BTreeMap::new(),
            coherence: Coherence::RescanRequired {
                reason,
                subjects: Vec::new(),
            },
            graph,
            provenance,
        })
    }

    /// Render the snapshot as canonical, byte-stable JSON.
    ///
    /// # Errors
    /// Returns a [`serde_json::Error`] only if a record cannot be serialized,
    /// which would be a defect in this crate rather than a property of the input.
    pub fn to_canonical_json(&self) -> Result<String, serde_json::Error> {
        let mut rendered = serde_json::to_string_pretty(self)?;
        rendered.push('\n');
        Ok(rendered)
    }
}
