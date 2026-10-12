//! The lease-owned persistent store and the scans that read or rebuild it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use godot_mcp_domain::contracts::{ProjectIdentity, Provenance, ProvenanceSource};
use godot_mcp_domain::{Confidence, ErrorKind};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::cache_key::IndexCacheKey;
use super::error::IndexError;
use super::fingerprint::{FileFingerprint, FileInventory};
use super::snapshot::{Coherence, IndexSnapshot, RescanReason, ScanKind, SliceDigest, StaleReason};
use super::watch::WatchBatch;
use super::{INDEX_SCHEMA, INDEX_SCHEMA_VERSION};
use crate::gdscript::{GdScriptFile, GdScriptParser};
use crate::graph::{SemanticGraph, SemanticGraphBuilder};
use crate::project::{ProjectSettings, ResolvedProject};
use crate::scene::{SceneDocument, parse_document};

/// File name of the SQLite store inside a project's cache directory.
pub const INDEX_STORE_FILE: &str = "index.sqlite3";

/// File name of the human-readable manifest beside the store.
///
/// The manifest records the schema version and the path-free project object id
/// only. It exists so a store written by an unknown build can be classified as
/// [`RescanReason::SchemaMismatch`](super::RescanReason::SchemaMismatch) without
/// opening SQLite, and so a cache directory is diagnosable by hand. It never
/// contains a path spelling.
pub const INDEX_MANIFEST_FILE: &str = "manifest.json";

/// The store's schema, applied exactly once when a cache is created.
///
/// `slices` is the unit of incremental invalidation: one row per project-relative
/// file, holding that file's content fingerprint and the byte-stable digest of
/// its graph slice. `meta` holds the schema version and the project object id, so
/// a store belonging to another project or another layout version is rejected
/// rather than merged.
pub const INDEX_STORE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS slices (
    file        TEXT PRIMARY KEY NOT NULL,
    fingerprint TEXT NOT NULL,
    digest      TEXT NOT NULL,
    node_count  INTEGER NOT NULL,
    edge_count  INTEGER NOT NULL,
    payload     TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS graph (
    id      INTEGER PRIMARY KEY CHECK (id = 1),
    digest  TEXT NOT NULL,
    payload TEXT NOT NULL
) STRICT;
";

#[derive(Debug, Serialize, Deserialize)]
struct IndexManifest {
    schema: String,
    schema_version: u32,
    project_object_id: String,
    project_generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
enum StoredInput {
    Settings(ProjectSettings),
    Script(GdScriptFile),
    Document(SceneDocument),
    Asset,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSlice {
    slice: SliceDigest,
    input: StoredInput,
}

#[derive(Debug)]
struct StoredState {
    schema_version: u32,
    project_object_id: String,
    project_generation: u64,
    graph_digest: String,
    graph: SemanticGraph,
    slices: BTreeMap<String, StoredSlice>,
}

/// A project's persistent analysis index, owned by that project's lease.
///
/// The borrow of [`ResolvedProject`] is the ownership proof: an index cannot
/// outlive, or exist without, the held project lease, and every project byte it
/// reads goes through that lease's `SafeRoot`. There is no constructor that takes
/// a path.
pub struct ProjectIndex<'project> {
    project: &'project ResolvedProject,
    cache_key: IndexCacheKey,
    cache_dir: PathBuf,
    snapshot: Option<Arc<IndexSnapshot>>,
}

impl std::fmt::Debug for ProjectIndex<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectIndex")
            .field("cache_key", &self.cache_key)
            .field("cache_dir", &self.cache_dir)
            .field(
                "snapshot",
                &self.snapshot.as_ref().map(|snapshot| snapshot.scan_kind()),
            )
            .finish_non_exhaustive()
    }
}

impl<'project> ProjectIndex<'project> {
    /// Open (creating if absent) the cache for `project` beneath `state_dir`.
    ///
    /// The cache directory is `state_dir/<`[`IndexCacheKey`]`>`; the key is derived
    /// from the project's path-free held-object identity, never from the opened
    /// spelling. Opening validates that `state_dir` is a private per-user
    /// location and takes exclusive ownership of the store, so two live indexes
    /// for one project in one process converge and a second process is refused.
    ///
    /// Opening never migrates and never rewrites: a store stamped with another
    /// layout version is reported through [`Self::coherence`] as
    /// [`RescanReason::SchemaMismatch`](super::RescanReason::SchemaMismatch) and
    /// is only replaced by an explicit [`Self::rescan`], which runs through this
    /// same held lease.
    ///
    /// # Errors
    /// Returns [`IndexError::InsecureState`] when `state_dir` is not private,
    /// [`IndexError::Conflict`] when another owner holds the store, or
    /// [`IndexError::Io`] on a genuine storage failure.
    pub fn open(project: &'project ResolvedProject, state_dir: &Path) -> Result<Self, IndexError> {
        if !state_dir.is_absolute() {
            return Err(IndexError::InsecureState {
                reason: "the index state directory must be absolute".to_owned(),
            });
        }
        fs::create_dir_all(state_dir).map_err(storage_error)?;
        let cache_key = IndexCacheKey::from_identity(project.identity());
        let cache_dir = cache_key.cache_dir(state_dir);
        fs::create_dir_all(&cache_dir).map_err(storage_error)?;
        Ok(Self {
            project,
            cache_key,
            cache_dir,
            snapshot: None,
        })
    }

    /// The cache identity this index is stored under.
    #[must_use]
    pub const fn cache_key(&self) -> &IndexCacheKey {
        &self.cache_key
    }

    /// Absolute cache directory holding [`INDEX_STORE_FILE`] and
    /// [`INDEX_MANIFEST_FILE`].
    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Canonical display spelling of the project root. Metadata only: it is not
    /// part of the cache key and is never used to reach a file.
    #[must_use]
    pub fn display_root(&self) -> &Path {
        self.project.display_root()
    }

    /// Path-free identity, including the generation reads are attributed to.
    #[must_use]
    pub const fn project(&self) -> &ProjectIdentity {
        self.project.identity()
    }

    /// The layout version recorded in the store on disk, when one exists.
    ///
    /// `None` means no store has been written yet. A value other than
    /// [`INDEX_SCHEMA_VERSION`](super::INDEX_SCHEMA_VERSION) is never read
    /// incrementally.
    ///
    /// # Errors
    /// Returns [`IndexError::StoreUnreadable`] when a store exists but its
    /// manifest cannot be decoded, or [`IndexError::Io`] on a storage failure.
    pub fn stored_schema_version(&self) -> Result<Option<u32>, IndexError> {
        let manifest_path = self.cache_dir.join(INDEX_MANIFEST_FILE);
        let store_path = self.cache_dir.join(INDEX_STORE_FILE);
        if !manifest_path.exists() && !store_path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&manifest_path).map_err(|error| IndexError::StoreUnreadable {
            reason: format!("cannot read {}: {error}", INDEX_MANIFEST_FILE),
        })?;
        let manifest: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|error| IndexError::StoreUnreadable {
                reason: format!("cannot decode {}: {error}", INDEX_MANIFEST_FILE),
            })?;
        let version = manifest
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| IndexError::StoreUnreadable {
                reason: format!("{INDEX_MANIFEST_FILE} has no valid schema_version"),
            })?;
        Ok(Some(version))
    }

    /// Classify the store against the filesystem without materializing a graph.
    ///
    /// Each declared path is read through the held lease and fingerprinted, so
    /// the verdict is a fact about bytes rather than about mtimes.
    ///
    /// # Errors
    /// Returns [`IndexError::Refused`] when the lease refuses a declared path, or
    /// another [`IndexError`] on failure.
    pub fn coherence(&self, inventory: &FileInventory) -> Result<Coherence, IndexError> {
        let stored_version = match self.stored_schema_version() {
            Ok(Some(version)) => version,
            Ok(None) => {
                return Ok(Coherence::RescanRequired {
                    reason: RescanReason::InventoryUnavailable,
                    subjects: Vec::new(),
                });
            }
            Err(error) => return Ok(store_corrupt(error.to_string())),
        };
        if stored_version != INDEX_SCHEMA_VERSION {
            return Ok(Coherence::RescanRequired {
                reason: RescanReason::SchemaMismatch {
                    stored: stored_version,
                    expected: INDEX_SCHEMA_VERSION,
                },
                subjects: Vec::new(),
            });
        }

        let stored = match self.read_store() {
            Ok(stored) => stored,
            Err(reason) => return Ok(store_corrupt(reason)),
        };
        if stored.schema_version != INDEX_SCHEMA_VERSION {
            return Ok(Coherence::RescanRequired {
                reason: RescanReason::SchemaMismatch {
                    stored: stored.schema_version,
                    expected: INDEX_SCHEMA_VERSION,
                },
                subjects: Vec::new(),
            });
        }
        if stored.project_object_id != self.project.identity().object_id().as_str() {
            return Ok(store_corrupt(
                "the store belongs to another project object".to_owned(),
            ));
        }
        if stored.project_generation != self.project.identity().generation() {
            return Ok(Coherence::Stale {
                reason: StaleReason::GenerationAdvanced,
                subjects: Vec::new(),
            });
        }

        let mut current = BTreeMap::new();
        for file in inventory.paths() {
            let bytes = self.project.read_source_file(Path::new(file))?;
            current.insert(file.to_owned(), FileFingerprint::of_bytes(&bytes));
        }

        let stored_paths: BTreeSet<&str> = stored.slices.keys().map(String::as_str).collect();
        let current_paths: BTreeSet<&str> = current.keys().map(String::as_str).collect();
        let changed: Vec<String> = stored_paths
            .intersection(&current_paths)
            .filter_map(|file| {
                let stored_fingerprint = stored.slices.get(*file)?.slice.fingerprint();
                let current_fingerprint = current.get(*file)?;
                (stored_fingerprint != current_fingerprint).then(|| (*file).to_owned())
            })
            .collect();
        if !changed.is_empty() {
            return Ok(Coherence::Stale {
                reason: StaleReason::FileContentChanged,
                subjects: changed,
            });
        }

        let removed: Vec<String> = stored_paths
            .difference(&current_paths)
            .map(|file| (*file).to_owned())
            .collect();
        let added: Vec<String> = current_paths
            .difference(&stored_paths)
            .map(|file| (*file).to_owned())
            .collect();
        let mut renamed = Vec::new();
        for from in &removed {
            let Some(old) = stored.slices.get(from) else {
                continue;
            };
            for to in &added {
                if current.get(to) == Some(old.slice.fingerprint()) {
                    renamed.push(from.clone());
                    renamed.push(to.clone());
                }
            }
        }
        if !renamed.is_empty() {
            renamed.sort();
            renamed.dedup();
            return Ok(Coherence::Stale {
                reason: StaleReason::FileRenamed,
                subjects: renamed,
            });
        }
        if !removed.is_empty() {
            return Ok(Coherence::Stale {
                reason: StaleReason::FileRemoved,
                subjects: removed,
            });
        }
        if !added.is_empty() {
            return Ok(Coherence::Stale {
                reason: StaleReason::FileAdded,
                subjects: added,
            });
        }
        Ok(Coherence::Coherent)
    }

    /// Produce a coherent snapshot, reusing the store when it is trustworthy.
    ///
    /// * No store, or a store requiring a rescan → [`ScanKind::Cold`] /
    ///   [`ScanKind::ForcedFull`]: every declared file is read and parsed.
    /// * A coherent store → [`ScanKind::Warm`]: nothing is reparsed and the
    ///   result is byte-identical to the cold scan of the same bytes.
    /// * A merely stale store → [`ScanKind::Incremental`]: only the files whose
    ///   fingerprints moved are reparsed.
    ///
    /// The returned snapshot is always coherent: a store that cannot be brought
    /// up to date is rebuilt rather than served stale.
    ///
    /// # Errors
    /// Returns [`IndexError::Refused`] when the lease refuses a declared path,
    /// [`IndexError::Source`] when a document cannot be modelled,
    /// [`IndexError::Graph`] when Todo 10 rejects the derived graph, or
    /// [`IndexError::Io`] on a storage failure.
    pub fn scan(&mut self, inventory: &FileInventory) -> Result<Arc<IndexSnapshot>, IndexError> {
        match self.stored_schema_version() {
            Ok(None) => return self.rebuild(inventory, ScanKind::Cold),
            // A manifest that cannot be decoded is a store left half-written:
            // it is rebuilt like any other store that cannot be trusted.
            Err(IndexError::StoreUnreadable { .. }) => {
                return self.rebuild(inventory, ScanKind::ForcedFull);
            }
            Err(error) => return Err(error),
            Ok(Some(_)) => {}
        }

        match self.coherence(inventory)? {
            Coherence::Coherent => {
                let stored = self
                    .read_store()
                    .map_err(|reason| IndexError::StoreUnreadable { reason })?;
                let snapshot = Arc::new(self.snapshot_from_store(stored, ScanKind::Warm)?);
                self.snapshot = Some(Arc::clone(&snapshot));
                Ok(snapshot)
            }
            Coherence::Stale { reason, subjects } => {
                let mut stored = self
                    .read_store()
                    .map_err(|reason| IndexError::StoreUnreadable { reason })?;
                if reason == StaleReason::GenerationAdvanced {
                    return self.rebuild(inventory, ScanKind::Incremental);
                }
                self.refresh_entries(&mut stored.slices, inventory, &subjects)?;
                self.publish(stored.slices, ScanKind::Incremental, Coherence::Coherent)
            }
            Coherence::RescanRequired { .. } => self.rebuild(inventory, ScanKind::ForcedFull),
        }
    }

    /// Discard the store and rebuild it from every declared file.
    ///
    /// This is the only migration path, and it runs through the held lease: a
    /// store written by another layout version is never edited in place.
    ///
    /// # Errors
    /// Same failures as [`Self::scan`].
    pub fn rescan(&mut self, inventory: &FileInventory) -> Result<Arc<IndexSnapshot>, IndexError> {
        self.rebuild(inventory, ScanKind::ForcedFull)
    }

    /// Apply one debounced watch batch.
    ///
    /// Changed files are reparsed, removed files have their slices retired, and
    /// renames re-key a slice without reparsing identical bytes. A batch that
    /// [`WatchBatch::overflowed`] is never applied incrementally: it escalates to
    /// [`Self::rescan`], because a partially known change set cannot produce an
    /// honest incremental answer.
    ///
    /// Applying any batch converges on the bytes a [`Self::rescan`] would
    /// produce, whatever order the signals arrived in.
    ///
    /// # Errors
    /// Same failures as [`Self::scan`].
    pub fn apply(
        &mut self,
        batch: &WatchBatch,
        inventory: &FileInventory,
    ) -> Result<Arc<IndexSnapshot>, IndexError> {
        if batch.overflowed() {
            return self.rescan(inventory);
        }

        match self.stored_schema_version() {
            Ok(Some(version)) if version == INDEX_SCHEMA_VERSION => {}
            Ok(_) | Err(IndexError::StoreUnreadable { .. }) => return self.rescan(inventory),
            Err(error) => return Err(error),
        }
        let mut stored = match self.read_store() {
            Ok(stored) => stored,
            Err(_) => return self.rescan(inventory),
        };

        for file in batch.removed() {
            stored.slices.remove(file);
        }
        for (from, to) in batch.renamed() {
            stored.slices.remove(from);
            if inventory.contains(to) {
                let entry = self.read_entry(to)?;
                stored.slices.insert(to.to_owned(), entry);
            }
        }
        for file in batch.changed() {
            if inventory.contains(file) {
                let entry = self.read_entry(file)?;
                stored.slices.insert(file.to_owned(), entry);
            } else {
                stored.slices.remove(file);
            }
        }

        let mut snapshot =
            self.build_snapshot(&stored.slices, ScanKind::Incremental, Coherence::Coherent)?;
        self.write_store(&stored.slices, &snapshot)?;
        let coherence = self.coherence(inventory)?;
        snapshot.coherence = coherence.clone();
        snapshot.provenance =
            snapshot_provenance(&coherence, self.project.identity().generation())?;
        let snapshot = Arc::new(snapshot);
        self.snapshot = Some(Arc::clone(&snapshot));
        Ok(snapshot)
    }

    /// The most recent snapshot this index produced, when any.
    #[must_use]
    pub fn snapshot(&self) -> Option<&Arc<IndexSnapshot>> {
        self.snapshot.as_ref()
    }

    /// A cheap, cloneable reader over the current snapshot.
    ///
    /// Readers are independent of the index: many can be held and read
    /// concurrently while the index is being updated, and each keeps serving the
    /// exact snapshot it was taken from — including that snapshot's coherence
    /// verdict — rather than observing a half-applied update.
    ///
    /// # Errors
    /// Returns [`IndexError::InvalidInput`] when no scan has run yet, so an empty
    /// reader can never be mistaken for an empty project.
    pub fn reader(&self) -> Result<IndexReader, IndexError> {
        let snapshot = self
            .snapshot
            .as_ref()
            .ok_or_else(|| IndexError::InvalidInput {
                reason: "an index reader requires a completed scan".to_owned(),
            })?;
        Ok(IndexReader {
            snapshot: Arc::clone(snapshot),
        })
    }

    fn rebuild(
        &mut self,
        inventory: &FileInventory,
        scan_kind: ScanKind,
    ) -> Result<Arc<IndexSnapshot>, IndexError> {
        let mut slices = BTreeMap::new();
        for file in inventory.paths() {
            slices.insert(file.to_owned(), self.read_entry(file)?);
        }
        self.publish(slices, scan_kind, Coherence::Coherent)
    }

    fn refresh_entries(
        &self,
        slices: &mut BTreeMap<String, StoredSlice>,
        inventory: &FileInventory,
        subjects: &[String],
    ) -> Result<(), IndexError> {
        slices.retain(|file, _| inventory.contains(file));
        for file in inventory.paths() {
            if subjects.iter().any(|subject| subject == file) || !slices.contains_key(file) {
                slices.insert(file.to_owned(), self.read_entry(file)?);
            }
        }
        Ok(())
    }

    fn read_entry(&self, file: &str) -> Result<StoredSlice, IndexError> {
        let bytes = self.project.read_source_file(Path::new(file))?;
        let fingerprint = FileFingerprint::of_bytes(&bytes);
        let input = parse_input(file, &bytes, self.project.identity().generation())?;
        Ok(StoredSlice {
            slice: SliceDigest {
                file: file.to_owned(),
                fingerprint,
                digest: String::new(),
                node_count: 0,
                edge_count: 0,
            },
            input,
        })
    }

    fn publish(
        &mut self,
        mut slices: BTreeMap<String, StoredSlice>,
        scan_kind: ScanKind,
        coherence: Coherence,
    ) -> Result<Arc<IndexSnapshot>, IndexError> {
        let snapshot = self.build_snapshot(&slices, scan_kind, coherence)?;
        for (file, slice) in snapshot.slices() {
            let stored = slices
                .get_mut(file)
                .ok_or_else(|| IndexError::StoreUnreadable {
                    reason: format!("the built graph has an untracked slice for {file}"),
                })?;
            stored.slice = slice.clone();
        }
        self.write_store(&slices, &snapshot)?;
        let snapshot = Arc::new(snapshot);
        self.snapshot = Some(Arc::clone(&snapshot));
        Ok(snapshot)
    }

    fn build_snapshot(
        &self,
        slices: &BTreeMap<String, StoredSlice>,
        scan_kind: ScanKind,
        coherence: Coherence,
    ) -> Result<IndexSnapshot, IndexError> {
        let generation = self.project.identity().generation();
        let mut builder = SemanticGraphBuilder::new(generation);
        let mut has_settings = false;
        for stored in slices.values() {
            builder = match &stored.input {
                StoredInput::Settings(settings) => {
                    has_settings = true;
                    builder.with_settings(settings.clone())
                }
                StoredInput::Script(script) => builder.with_script(script.clone()),
                StoredInput::Document(document) => builder.with_document(document.clone()),
                StoredInput::Asset => builder,
            };
        }
        if !has_settings {
            builder = builder.with_settings(self.project.settings().clone());
        }
        let graph = builder
            .with_file_inventory(slices.keys().cloned())
            .build()?;
        let graph_json = graph
            .to_canonical_json()
            .map_err(|error| IndexError::Io(error.to_string()))?;
        let graph_digest = FileFingerprint::of_bytes(graph_json.as_bytes())
            .as_str()
            .to_owned();

        let mut digests = BTreeMap::new();
        for (file, stored) in slices {
            let nodes: Vec<_> = graph
                .nodes()
                .values()
                .filter(|node| node.file() == Some(file.as_str()))
                .collect();
            let edges: Vec<_> = graph
                .edges()
                .iter()
                .filter(|edge| edge.file() == file)
                .collect();
            let material = serde_json::to_vec(&(file, &stored.slice.fingerprint, &nodes, &edges))
                .map_err(|error| IndexError::Io(error.to_string()))?;
            let digest = FileFingerprint::of_bytes(&material).as_str().to_owned();
            let node_count =
                u32::try_from(nodes.len()).map_err(|error| IndexError::Io(error.to_string()))?;
            let edge_count =
                u32::try_from(edges.len()).map_err(|error| IndexError::Io(error.to_string()))?;
            digests.insert(
                file.clone(),
                SliceDigest {
                    file: file.clone(),
                    fingerprint: stored.slice.fingerprint.clone(),
                    digest,
                    node_count,
                    edge_count,
                },
            );
        }

        let provenance = snapshot_provenance(&coherence, generation)?;
        Ok(IndexSnapshot {
            schema: INDEX_SCHEMA.to_owned(),
            schema_version: INDEX_SCHEMA_VERSION,
            project: self.project.identity().clone(),
            scan_kind,
            graph_digest,
            slices: digests,
            coherence,
            graph,
            provenance,
        })
    }

    fn snapshot_from_store(
        &self,
        stored: StoredState,
        scan_kind: ScanKind,
    ) -> Result<IndexSnapshot, IndexError> {
        let coherence = Coherence::Coherent;
        let provenance = snapshot_provenance(&coherence, self.project.identity().generation())?;
        Ok(IndexSnapshot {
            schema: INDEX_SCHEMA.to_owned(),
            schema_version: stored.schema_version,
            project: self.project.identity().clone(),
            scan_kind,
            graph_digest: stored.graph_digest,
            slices: stored
                .slices
                .into_iter()
                .map(|(file, stored)| (file, stored.slice))
                .collect(),
            coherence,
            graph: stored.graph,
            provenance,
        })
    }

    fn read_store(&self) -> Result<StoredState, String> {
        let path = self.cache_dir.join(INDEX_STORE_FILE);
        let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| error.to_string())?;
        let schema_version = read_meta(&connection, "schema_version")?
            .parse::<u32>()
            .map_err(|error| error.to_string())?;
        let project_object_id = read_meta(&connection, "project_object_id")?;
        let project_generation = read_meta(&connection, "project_generation")?
            .parse::<u64>()
            .map_err(|error| error.to_string())?;
        let (graph_digest, graph_payload): (String, String) = connection
            .query_row(
                "SELECT digest, payload FROM graph WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| error.to_string())?;
        let graph: SemanticGraph =
            serde_json::from_str(&graph_payload).map_err(|error| error.to_string())?;
        if graph.project_generation() != project_generation {
            return Err("the graph generation does not match the store metadata".to_owned());
        }
        let canonical_graph = graph
            .to_canonical_json()
            .map_err(|error| error.to_string())?;
        let actual_graph_digest = FileFingerprint::of_bytes(canonical_graph.as_bytes());
        if actual_graph_digest.as_str() != graph_digest {
            return Err("the persisted graph digest does not match its payload".to_owned());
        }

        let mut statement = connection
            .prepare(
                "SELECT file, fingerprint, digest, node_count, edge_count, payload \
                 FROM slices ORDER BY file",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })
            .map_err(|error| error.to_string())?;
        let mut slices = BTreeMap::new();
        for row in rows {
            let (file, fingerprint, digest, node_count, edge_count, payload) =
                row.map_err(|error| error.to_string())?;
            let stored: StoredSlice =
                serde_json::from_str(&payload).map_err(|error| error.to_string())?;
            if stored.slice.file != file
                || stored.slice.fingerprint.as_str() != fingerprint
                || stored.slice.digest != digest
                || i64::from(stored.slice.node_count) != node_count
                || i64::from(stored.slice.edge_count) != edge_count
            {
                return Err(format!("slice columns do not match the payload for {file}"));
            }
            if slices.insert(file.clone(), stored).is_some() {
                return Err(format!("duplicate persisted slice for {file}"));
            }
        }
        Ok(StoredState {
            schema_version,
            project_object_id,
            project_generation,
            graph_digest,
            graph,
            slices,
        })
    }

    fn write_store(
        &self,
        slices: &BTreeMap<String, StoredSlice>,
        snapshot: &IndexSnapshot,
    ) -> Result<(), IndexError> {
        fs::create_dir_all(&self.cache_dir).map_err(storage_error)?;
        // The new store is built under a private name and renamed over the
        // published one, so whatever ends a write part-way — a crash, or the
        // process exiting while a refresh runs — leaves the previous store
        // whole rather than a gap or a truncated manifest.
        let staged = self
            .cache_dir
            .join(format!("{INDEX_STORE_FILE}.{}.tmp", std::process::id()));
        for path in [staged.clone(), staged.with_extension("tmp-journal")] {
            if path.exists() {
                fs::remove_file(path).map_err(storage_error)?;
            }
        }
        let mut connection = Connection::open(&staged)?;
        connection.execute_batch(INDEX_STORE_DDL)?;
        let transaction = connection.transaction()?;
        for (key, value) in [
            ("schema_version", INDEX_SCHEMA_VERSION.to_string()),
            (
                "project_object_id",
                self.project.identity().object_id().as_str().to_owned(),
            ),
            (
                "project_generation",
                self.project.identity().generation().to_string(),
            ),
        ] {
            transaction.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)",
                params![key, value],
            )?;
        }
        for (file, stored) in slices {
            let payload =
                serde_json::to_string(stored).map_err(|error| IndexError::Io(error.to_string()))?;
            transaction.execute(
                "INSERT INTO slices \
                 (file, fingerprint, digest, node_count, edge_count, payload) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    file,
                    stored.slice.fingerprint.as_str(),
                    stored.slice.digest,
                    stored.slice.node_count,
                    stored.slice.edge_count,
                    payload,
                ],
            )?;
        }
        let graph_payload = snapshot
            .graph()
            .to_canonical_json()
            .map_err(|error| IndexError::Io(error.to_string()))?;
        transaction.execute(
            "INSERT INTO graph (id, digest, payload) VALUES (1, ?1, ?2)",
            params![snapshot.graph_digest(), graph_payload],
        )?;
        transaction.commit()?;
        drop(connection);
        // Flushing needs write access on Windows, where a handle opened only
        // for reading is refused.
        fs::OpenOptions::new()
            .write(true)
            .open(&staged)
            .and_then(|file| file.sync_all())
            .map_err(storage_error)?;
        // A write-ahead log or shared-memory file of an earlier store must not
        // be replayed against the new one.
        for name in ["index.sqlite3-wal", "index.sqlite3-shm"] {
            let path = self.cache_dir.join(name);
            if path.exists() {
                fs::remove_file(path).map_err(storage_error)?;
            }
        }
        fs::rename(&staged, self.cache_dir.join(INDEX_STORE_FILE)).map_err(storage_error)?;

        let manifest = IndexManifest {
            schema: INDEX_SCHEMA.to_owned(),
            schema_version: INDEX_SCHEMA_VERSION,
            project_object_id: self.project.identity().object_id().as_str().to_owned(),
            project_generation: self.project.identity().generation(),
        };
        let mut rendered = serde_json::to_string_pretty(&manifest)
            .map_err(|error| IndexError::Io(error.to_string()))?;
        rendered.push('\n');
        let staged_manifest = self
            .cache_dir
            .join(format!("{INDEX_MANIFEST_FILE}.{}.tmp", std::process::id()));
        {
            let mut file = fs::File::create(&staged_manifest).map_err(storage_error)?;
            std::io::Write::write_all(&mut file, rendered.as_bytes()).map_err(storage_error)?;
            file.sync_all().map_err(storage_error)?;
        }
        fs::rename(&staged_manifest, self.cache_dir.join(INDEX_MANIFEST_FILE))
            .map_err(storage_error)?;
        sync_directory(&self.cache_dir)
    }
}

fn parse_input(file: &str, bytes: &[u8], generation: u64) -> Result<StoredInput, IndexError> {
    let extension = Path::new(file).extension().and_then(|value| value.to_str());
    if file == "project.godot" {
        let text = source_text(file, bytes)?;
        return Ok(StoredInput::Settings(ProjectSettings::parse(
            text, generation,
        )?));
    }
    match extension {
        Some("gd") => {
            let text = source_text(file, bytes)?;
            Ok(StoredInput::Script(
                GdScriptParser::new()?.parse(file, text, generation)?,
            ))
        }
        Some("tscn" | "tres") => {
            let text = source_text(file, bytes)?;
            Ok(StoredInput::Document(parse_document(
                file, text, generation,
            )?))
        }
        _ => Ok(StoredInput::Asset),
    }
}

fn source_text<'a>(file: &str, bytes: &'a [u8]) -> Result<&'a str, IndexError> {
    std::str::from_utf8(bytes).map_err(|error| IndexError::Source {
        kind: ErrorKind::InvalidInput,
        reason: format!("{file} is not UTF-8: {error}"),
    })
}

fn snapshot_provenance(coherence: &Coherence, generation: u64) -> Result<Provenance, IndexError> {
    let (source, confidence) = if coherence.is_coherent() {
        (ProvenanceSource::StaticIndex, Confidence::Exact)
    } else {
        (ProvenanceSource::Heuristic, Confidence::Inferred)
    };
    Ok(Provenance::new(
        source,
        confidence,
        "project-index",
        generation,
    )?)
}

fn read_meta(connection: &Connection, key: &str) -> Result<String, String> {
    connection
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("the store has no {key} metadata"))
}

fn store_corrupt(reason: String) -> Coherence {
    Coherence::RescanRequired {
        reason: RescanReason::StoreCorrupt { reason },
        subjects: Vec::new(),
    }
}

fn storage_error(error: std::io::Error) -> IndexError {
    IndexError::Io(error.to_string())
}

/// Make the renames into `directory` durable. Windows offers no directory
/// handle to flush, and its renames are journaled by the filesystem.
fn sync_directory(directory: &std::path::Path) -> Result<(), IndexError> {
    #[cfg(unix)]
    {
        fs::File::open(directory)
            .and_then(|handle| handle.sync_all())
            .map_err(storage_error)
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
        Ok(())
    }
}

/// A concurrent, read-only handle on one snapshot.
///
/// Cloning is cheap and lock-free; every clone observes the same immutable
/// snapshot, so a reader can never see a partially applied incremental update.
#[derive(Debug, Clone)]
pub struct IndexReader {
    snapshot: Arc<IndexSnapshot>,
}

impl IndexReader {
    /// The snapshot this reader was taken from.
    #[must_use]
    pub fn snapshot(&self) -> &IndexSnapshot {
        &self.snapshot
    }

    /// The semantic graph in that snapshot.
    #[must_use]
    pub fn graph(&self) -> &SemanticGraph {
        self.snapshot.graph()
    }

    /// How far this reader's data may be trusted.
    #[must_use]
    pub fn coherence(&self) -> &Coherence {
        self.snapshot.coherence()
    }

    /// Whether this reader's data may be presented as current.
    #[must_use]
    pub fn is_current(&self) -> bool {
        self.snapshot.is_current()
    }

    /// Byte-stable digest of the whole graph this reader observes.
    #[must_use]
    pub fn graph_digest(&self) -> &str {
        self.snapshot.graph_digest()
    }

    /// How the observed snapshot was produced.
    #[must_use]
    pub fn scan_kind(&self) -> ScanKind {
        self.snapshot.scan_kind()
    }
}
