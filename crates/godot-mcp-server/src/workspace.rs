//! The workspace coordinator: one live, coherent view of the held project.
//!
//! [`WorkspaceCoordinator`] owns the project's current [`WorkspaceSnapshot`] —
//! the file inventory, the parsed settings, the persisted semantic index and a
//! monotonic revision — behind a short lock. A request clones one snapshot and
//! answers entirely from it, so an answer never combines a live file read with an
//! older semantic graph.
//!
//! ## When the revision advances
//!
//! A new coherent snapshot, with the next revision, is published after:
//!
//! * a committed MCP mutation, through [`WorkspaceCoordinator::observe_commit`],
//!   before the mutation's response is returned (read-your-writes);
//! * a debounced filesystem watch batch for an external create, modify, rename
//!   or remove; and
//! * a watcher overflow, which forces one full rescan rather than a partial
//!   revision.
//!
//! A refresh that fails leaves the previous snapshot readable but marks it
//! [`WorkspaceCoherence::Stale`] with the reason, so it is never reported as
//! coherent. The failed batch was never applied, so the refresh after it is a
//! full rescan, whatever its own batch names; the next successful refresh
//! publishes a new revision.
//!
//! ## Authority
//!
//! The watcher only *hints*. Every byte a snapshot is built from is read through
//! the held lease's `SafeRoot`, so a symlinked, escaping or engine-cache path is
//! refused by the lease rather than followed because an event named it, and a
//! missed event is caught by the per-snapshot coherence check that fingerprints
//! every inventoried file.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use godot_mcp_analysis::index::{
    Coherence, FileInventory, IndexError, IndexSnapshot, ProjectIndex, RescanReason, StaleReason,
    WatchBatch, WatchDebouncer, WatchSignal,
};
use godot_mcp_analysis::project::{ENGINE_CACHE_DIR, ProjectSettings, ResolvedProject};
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_domain::lease::{Lease, PROJECT_STATE_DIR_NAME};
use godot_mcp_domain::sysfs::is_staging_name;
use godot_mcp_engine::canonical::helper::HELPER_ROOT;
use notify::Watcher as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::journal::hex_sha256;

const INVENTORY_MAX_ENTRIES: usize = 100_000;
const INVENTORY_MAX_DEPTH: usize = 64;
/// Test hook: with this variable set, a server starts as one whose platform
/// watcher could not start.
#[cfg(feature = "test-hooks")]
pub const WATCHER_UNAVAILABLE_ENV: &str = "GODOT_MCP_TEST_WATCHER_UNAVAILABLE";
/// Quiet window, in milliseconds, before a burst of watch signals is applied.
const WATCH_QUIET_MS: u64 = 75;
/// Distinct pending signals before a burst degrades to a full rescan.
const WATCH_CAPACITY: usize = 4_096;
const WATCH_POLL: Duration = Duration::from_millis(25);

/// Whether a snapshot may be presented as the project's current state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceCoherence {
    /// Every inventoried file matched its indexed bytes when the snapshot was
    /// published, and no later change has been observed and left unapplied.
    Coherent,
    /// The snapshot is readable but provably behind the project.
    Stale {
        /// Why the latest refresh could not produce a coherent snapshot.
        reason: String,
    },
}

impl WorkspaceCoherence {
    /// Whether this verdict may be presented as current.
    #[must_use]
    pub const fn is_coherent(&self) -> bool {
        matches!(self, Self::Coherent)
    }
}

/// One immutable, coherent view of the project.
#[derive(Debug, Clone)]
pub struct WorkspaceSnapshot {
    revision: u64,
    inventory: FileInventory,
    inventory_digest: String,
    settings: ProjectSettings,
    settings_digest: String,
    index: Arc<IndexSnapshot>,
    coherence: WorkspaceCoherence,
}

impl WorkspaceSnapshot {
    /// The monotonic workspace revision. Revision 1 is the startup scan.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Every project-relative regular file the snapshot was built from.
    #[must_use]
    pub const fn inventory(&self) -> &FileInventory {
        &self.inventory
    }

    /// Digest over every inventoried path and its content fingerprint.
    #[must_use]
    pub fn inventory_digest(&self) -> &str {
        &self.inventory_digest
    }

    /// The parsed `project.godot` this snapshot was built from.
    #[must_use]
    pub const fn settings(&self) -> &ProjectSettings {
        &self.settings
    }

    /// Digest of the `project.godot` bytes this snapshot was built from.
    #[must_use]
    pub fn settings_digest(&self) -> &str {
        &self.settings_digest
    }

    /// The semantic index snapshot.
    #[must_use]
    pub fn index(&self) -> &Arc<IndexSnapshot> {
        &self.index
    }

    /// Whether this snapshot may be presented as current.
    #[must_use]
    pub const fn coherence(&self) -> &WorkspaceCoherence {
        &self.coherence
    }

    /// The content fingerprint recorded for one inventoried path.
    #[must_use]
    pub fn fingerprint_of(&self, path: &str) -> Option<&str> {
        self.index
            .slice(path)
            .map(|slice| slice.fingerprint().as_str())
    }

    /// The public, path-free description every answer carries.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let (coherent, stale_reason) = match &self.coherence {
            WorkspaceCoherence::Coherent => (true, Value::Null),
            WorkspaceCoherence::Stale { reason } => (false, Value::String(reason.clone())),
        };
        json!({
            "revision": self.revision,
            "coherent": coherent,
            "stale_reason": stale_reason,
            "files": self.inventory.len(),
            "inventory_digest": self.inventory_digest,
            "settings_digest": self.settings_digest,
        })
    }
}

/// Where a coordinator's inventory comes from.
#[derive(Debug, Clone)]
enum InventorySource {
    /// Every regular file the held root lists, excluding the engine cache and
    /// the private state root. This is the production source.
    Listed,
    /// A fixed declared set, re-read on every refresh; files outside it are
    /// never inventoried. Used where a caller supplies its own inventory.
    Declared(FileInventory),
}

/// What a committed mutation touched, for the read-your-writes barrier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitScope {
    /// Exactly these project-relative paths changed.
    Paths(Vec<String>),
    /// The change set is not known path by path (an engine import, for example).
    Unknown,
}

struct State {
    current: Arc<WorkspaceSnapshot>,
}

/// The coordinator itself: one per held project, shared by every handler.
pub struct WorkspaceCoordinator {
    lease: Lease,
    generation: u64,
    index_dir: PathBuf,
    source: InventorySource,
    state: RwLock<State>,
    /// Serializes refreshes so revisions are published in order by one writer,
    /// and records whether the last one failed. A failed refresh applied
    /// nothing, and the stale snapshot it left still lists the old inventory,
    /// so the refresh after it rebuilds from a fresh listing instead of
    /// patching that inventory with whatever the next batch happens to name.
    refresh: Mutex<bool>,
    /// Watch signals not yet applied; present once watching has started.
    pending: Mutex<Option<WatchDebouncer>>,
    epoch: Instant,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    /// Set when the watcher could not start: nothing reports an external
    /// change, so every request rescans instead of trusting the last snapshot.
    unwatched: AtomicBool,
}

impl std::fmt::Debug for WorkspaceCoordinator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let revision = self.current().revision;
        formatter
            .debug_struct("WorkspaceCoordinator")
            .field("revision", &revision)
            .finish_non_exhaustive()
    }
}

impl WorkspaceCoordinator {
    /// Build the production coordinator over the held root's listing and
    /// publish revision 1 from a full scan.
    ///
    /// A startup scan that fails still yields a coordinator: its snapshot is
    /// empty and explicitly stale, so the server can start and report why
    /// instead of serving nothing.
    #[must_use]
    pub fn open(lease: Lease, generation: u64, index_dir: PathBuf) -> Arc<Self> {
        Self::open_with(lease, generation, index_dir, InventorySource::Listed)
    }

    /// Build a coordinator over a fixed declared inventory, without a watcher.
    #[must_use]
    pub fn open_declared(
        lease: Lease,
        generation: u64,
        index_dir: PathBuf,
        inventory: FileInventory,
    ) -> Arc<Self> {
        Self::open_with(
            lease,
            generation,
            index_dir,
            InventorySource::Declared(inventory),
        )
    }

    fn open_with(
        lease: Lease,
        generation: u64,
        index_dir: PathBuf,
        source: InventorySource,
    ) -> Arc<Self> {
        let (initial, rescan_due) = match build_full(&lease, generation, &index_dir, &source, 1) {
            Ok(snapshot) => (snapshot, false),
            Err(error) => (empty_stale_snapshot(&lease, generation, &error), true),
        };
        Arc::new(Self {
            lease,
            generation,
            index_dir,
            source,
            state: RwLock::new(State {
                current: Arc::new(initial),
            }),
            refresh: Mutex::new(rescan_due),
            pending: Mutex::new(None),
            epoch: Instant::now(),
            watcher: Mutex::new(None),
            unwatched: AtomicBool::new(false),
        })
    }

    /// Start the filesystem watcher. Events only hint; see the module docs.
    ///
    /// The startup scan ran before the watcher existed, so once it watches, one
    /// more scan closes that gap; it publishes a revision only when the project
    /// changed in between.
    ///
    /// # Errors
    /// Returns `unavailable` when the platform watcher cannot be started. The
    /// coordinator keeps working without it: every request then rescans the
    /// project, so an external change is still never answered from an older
    /// snapshot.
    pub fn start_watching(self: &Arc<Self>) -> Result<(), OutcomeError> {
        // Test hook: a server whose platform watcher cannot start.
        #[cfg(feature = "test-hooks")]
        if std::env::var_os(WATCHER_UNAVAILABLE_ENV).is_some() {
            return self
                .start_watching_root(&self.lease.display_root().join(".no-watch-test-hook"));
        }
        let root = self.lease.display_root().to_path_buf();
        self.start_watching_root(&root)
    }

    fn start_watching_root(self: &Arc<Self>, root: &Path) -> Result<(), OutcomeError> {
        let started = self.watch(root);
        if started.is_err() {
            self.unwatched.store(true, Ordering::SeqCst);
        }
        started?;
        let _ = self.rescan_if_changed();
        Ok(())
    }

    fn watch(self: &Arc<Self>, root: &Path) -> Result<(), OutcomeError> {
        let debouncer =
            WatchDebouncer::new(root, WATCH_QUIET_MS, WATCH_CAPACITY).map_err(index_error)?;
        *self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(debouncer);
        let (sender, receiver): (Sender<notify::Result<notify::Event>>, Receiver<_>) = channel();
        let mut watcher = notify::recommended_watcher(sender).map_err(watch_error)?;
        watcher
            .watch(root, notify::RecursiveMode::Recursive)
            .map_err(watch_error)?;
        *self
            .watcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(watcher);
        let coordinator = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("godot-mcp-workspace-watch".to_owned())
            .spawn(move || {
                loop {
                    let received = receiver.recv_timeout(WATCH_POLL);
                    let Some(coordinator) = coordinator.upgrade() else {
                        return;
                    };
                    match received {
                        Ok(Ok(event)) => coordinator.record_event(&event),
                        Ok(Err(_)) => coordinator.record_signal(WatchSignal::Overflow),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                    if coordinator.pending_ready() {
                        coordinator.apply_pending();
                    }
                }
            })
            .map_err(|error| {
                OutcomeError::new(
                    ErrorKind::Unavailable,
                    format!("the workspace watcher thread could not start: {error}"),
                    false,
                    std::collections::BTreeMap::new(),
                )
            })?;
        Ok(())
    }

    /// The current snapshot, without applying pending watch signals.
    #[must_use]
    pub fn current(&self) -> Arc<WorkspaceSnapshot> {
        Arc::clone(
            &self
                .state
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .current,
        )
    }

    /// The snapshot a request must answer from.
    ///
    /// Watch signals that arrived but have not been applied yet are applied
    /// first, so a change the coordinator already knows about is never answered
    /// from the snapshot it supersedes.
    #[must_use]
    pub fn snapshot(&self) -> Arc<WorkspaceSnapshot> {
        if self.unwatched.load(Ordering::SeqCst) {
            return self.rescan_if_changed();
        }
        if self.has_pending() {
            self.apply_pending();
        }
        self.current()
    }

    /// The read-your-writes barrier: observe a committed mutation's paths and
    /// publish the snapshot that contains it before the mutation answers.
    ///
    /// # Errors
    /// Returns the snapshot anyway when the refresh fails, marked stale, so the
    /// caller can report the committed effect without claiming coherence.
    pub fn observe_commit(&self, scope: &CommitScope) -> Arc<WorkspaceSnapshot> {
        // Without a watcher nothing reports an external change, so the
        // snapshot a commit answers with is a full rescan, never only the
        // committed paths on top of an inventory that may be behind.
        if self.unwatched.load(Ordering::SeqCst) {
            return self.apply(&WatchBatch::overflow());
        }
        let batch = match scope {
            CommitScope::Paths(paths) => self.batch_for_paths(paths),
            CommitScope::Unknown => WatchBatch::overflow(),
        };
        self.apply(&batch)
    }

    /// Force one full rescan and publish its snapshot.
    pub fn rescan(&self) -> Arc<WorkspaceSnapshot> {
        self.apply(&WatchBatch::overflow())
    }

    /// One full rescan, published only when the project's files or settings
    /// differ from the current coherent snapshot, so an unchanged project
    /// keeps its revision.
    fn rescan_if_changed(&self) -> Arc<WorkspaceSnapshot> {
        let mut rescan_due = self
            .refresh
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = self.current();
        let refreshed = build_full(
            &self.lease,
            self.generation,
            &self.index_dir,
            &self.source,
            previous.revision.saturating_add(1),
        );
        if let Ok(fresh) = &refreshed
            && previous.coherence.is_coherent()
            && !*rescan_due
            && fresh.inventory_digest == previous.inventory_digest
            && fresh.settings_digest == previous.settings_digest
        {
            return previous;
        }
        *rescan_due = refreshed.is_err();
        self.publish(&previous, refreshed, &WatchBatch::overflow())
    }

    fn tick(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn record_event(&self, event: &notify::Event) {
        let signals = normalize_event(event, self.lease.display_root());
        let tick = self.tick();
        if let Some(pending) = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_mut()
        {
            pending.extend(signals, tick);
        }
    }

    fn record_signal(&self, signal: WatchSignal) {
        let tick = self.tick();
        if let Some(pending) = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_mut()
        {
            pending.record(signal, tick);
        }
    }

    fn has_pending(&self) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .is_some_and(|pending| pending.pending() > 0 || pending.overflowed())
    }

    fn pending_ready(&self) -> bool {
        let tick = self.tick();
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .is_some_and(|pending| pending.ready(tick))
    }

    fn apply_pending(&self) {
        let batch = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_mut()
            .map(WatchDebouncer::flush);
        if let Some(batch) = batch
            && !batch.is_empty()
        {
            let _ = self.apply(&batch);
        }
    }

    /// Classify each committed path by what is on disk now: present paths are
    /// changed, absent paths are removed.
    fn batch_for_paths(&self, paths: &[String]) -> WatchBatch {
        let root = self.lease.safe_root();
        let mut debouncer = match WatchDebouncer::new(self.lease.display_root(), 1, WATCH_CAPACITY)
        {
            Ok(debouncer) => debouncer,
            Err(_) => return WatchBatch::overflow(),
        };
        for path in paths {
            if is_private_path(path) {
                continue;
            }
            let signal = if root.read(Path::new(path)).is_ok() {
                WatchSignal::Changed { file: path.clone() }
            } else {
                WatchSignal::Removed { file: path.clone() }
            };
            debouncer.record(signal, 0);
        }
        debouncer.flush()
    }

    /// Apply one batch and publish the next revision, or mark the current
    /// snapshot stale when the refresh fails.
    fn apply(&self, batch: &WatchBatch) -> Arc<WorkspaceSnapshot> {
        let mut rescan_due = self
            .refresh
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = self.current();
        let next_revision = previous.revision.saturating_add(1);
        let refreshed = if batch.overflowed() || *rescan_due {
            build_full(
                &self.lease,
                self.generation,
                &self.index_dir,
                &self.source,
                next_revision,
            )
        } else {
            // An incremental refresh reads only what its batch names, then
            // checks every file. It can find the index behind them: another
            // tool's file it indexed was renamed away, or a change no event has
            // reported yet. Such a refresh reads everything again rather than
            // publish a snapshot it cannot prove current.
            match build_incremental(
                &self.lease,
                self.generation,
                &self.index_dir,
                &self.source,
                &previous,
                batch,
                next_revision,
            ) {
                Ok(snapshot) if !snapshot.coherence.is_coherent() => build_full(
                    &self.lease,
                    self.generation,
                    &self.index_dir,
                    &self.source,
                    next_revision,
                ),
                other => other,
            }
        };
        *rescan_due = refreshed.is_err();
        self.publish(&previous, refreshed, batch)
    }

    /// Publish a refresh's result, or, when it failed, the previous snapshot
    /// marked stale. Called with the refresh lock held, so revisions are
    /// published in order.
    fn publish(
        &self,
        previous: &Arc<WorkspaceSnapshot>,
        refreshed: Result<WorkspaceSnapshot, IndexError>,
        batch: &WatchBatch,
    ) -> Arc<WorkspaceSnapshot> {
        let published = match refreshed {
            Ok(snapshot) => snapshot,
            Err(error) => {
                // The previous snapshot stays readable, but its index is replaced
                // by a copy that admits it is behind, so no answer rendered from
                // it can claim exact, current confidence.
                let index = previous
                    .index
                    .superseded(observed_coherence(batch))
                    .map_or_else(|_| Arc::clone(&previous.index), Arc::new);
                WorkspaceSnapshot {
                    index,
                    coherence: WorkspaceCoherence::Stale {
                        reason: refresh_failure(&error),
                    },
                    ..(**previous).clone()
                }
            }
        };
        let published = Arc::new(published);
        self.state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .current = Arc::clone(&published);
        published
    }
}

/// Every regular file the project holds, excluding the engine cache, the private
/// state root and the server-owned helper scratch.
fn list_inventory(lease: &Lease) -> Result<FileInventory, IndexError> {
    let listing = lease
        .safe_root()
        .list_regular_files(
            INVENTORY_MAX_ENTRIES,
            INVENTORY_MAX_DEPTH,
            [
                OsStr::new(ENGINE_CACHE_DIR),
                OsStr::new(PROJECT_STATE_DIR_NAME),
                OsStr::new(HELPER_ROOT),
            ]
            .as_slice(),
        )
        .map_err(|error| IndexError::InvalidInput {
            reason: error.to_string(),
        })?;
    if !listing.is_complete() {
        return Err(IndexError::InvalidInput {
            reason: format!(
                "the project inventory exceeds {INVENTORY_MAX_ENTRIES} entries or \
                 {INVENTORY_MAX_DEPTH} directory levels"
            ),
        });
    }
    let paths = listing
        .files()
        .iter()
        .map(|path| {
            path.to_str()
                .map(|path| path.replace('\\', "/"))
                .ok_or_else(|| IndexError::InvalidInput {
                    reason: "the project inventory contains a path that is not valid UTF-8"
                        .to_owned(),
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    FileInventory::from_paths(paths.into_iter().filter(|path| !is_private_path(path)))
}

fn build_full(
    lease: &Lease,
    generation: u64,
    index_dir: &Path,
    source: &InventorySource,
    revision: u64,
) -> Result<WorkspaceSnapshot, IndexError> {
    let inventory = match source {
        InventorySource::Listed => list_inventory(lease)?,
        InventorySource::Declared(declared) => declared.clone(),
    };
    let project = ResolvedProject::from_lease(lease.clone(), generation)?;
    let mut index = ProjectIndex::open(&project, index_dir)?;
    let snapshot = index.scan(&inventory)?;
    finish(
        lease,
        project.settings().clone(),
        inventory,
        snapshot,
        revision,
    )
}

fn build_incremental(
    lease: &Lease,
    generation: u64,
    index_dir: &Path,
    source: &InventorySource,
    previous: &WorkspaceSnapshot,
    batch: &WatchBatch,
    revision: u64,
) -> Result<WorkspaceSnapshot, IndexError> {
    if let InventorySource::Declared(declared) = source {
        // A declared inventory never grows or shrinks; its files are re-read.
        let project = ResolvedProject::from_lease(lease.clone(), generation)?;
        let mut index = ProjectIndex::open(&project, index_dir)?;
        let snapshot = index.apply(batch, declared)?;
        return finish(
            lease,
            project.settings().clone(),
            declared.clone(),
            snapshot,
            revision,
        );
    }
    let mut paths: BTreeSet<String> = previous.inventory.paths().map(str::to_owned).collect();
    for removed in batch.removed() {
        paths.remove(removed);
    }
    for (from, to) in batch.renamed() {
        paths.remove(from);
        paths.insert(to.to_owned());
    }
    let root = lease.safe_root();
    for changed in batch.changed() {
        paths.insert(changed.to_owned());
    }
    // A path is inventoried only while the lease can read it as a regular file.
    let paths: Vec<String> = paths
        .into_iter()
        .filter(|path| !is_private_path(path) && root.read(Path::new(path)).is_ok())
        .collect();
    let inventory = FileInventory::from_paths(paths)?;
    let project = ResolvedProject::from_lease(lease.clone(), generation)?;
    let mut index = ProjectIndex::open(&project, index_dir)?;
    let snapshot = index.apply(batch, &inventory)?;
    finish(
        lease,
        project.settings().clone(),
        inventory,
        snapshot,
        revision,
    )
}

fn finish(
    lease: &Lease,
    settings: ProjectSettings,
    inventory: FileInventory,
    index: Arc<IndexSnapshot>,
    revision: u64,
) -> Result<WorkspaceSnapshot, IndexError> {
    let coherence = if index.is_current() {
        WorkspaceCoherence::Coherent
    } else {
        WorkspaceCoherence::Stale {
            reason: "the semantic index could not be proven current".to_owned(),
        }
    };
    let settings_bytes = lease
        .safe_root()
        .read(Path::new("project.godot"))
        .map_err(|error| IndexError::InvalidInput {
            reason: error.to_string(),
        })?;
    Ok(WorkspaceSnapshot {
        revision,
        inventory_digest: inventory_digest(&index),
        settings_digest: hex_sha256(&settings_bytes),
        inventory,
        settings,
        index,
        coherence,
    })
}

fn inventory_digest(index: &IndexSnapshot) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"godot-mcp-workspace-inventory-v1");
    for (file, slice) in index.slices() {
        hasher.update((file.len() as u64).to_le_bytes());
        hasher.update(file.as_bytes());
        hasher.update(slice.fingerprint().as_str().as_bytes());
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

/// The verdict a failed refresh leaves behind: the observed paths when the
/// batch named them, a rescan requirement when it overflowed.
fn observed_coherence(batch: &WatchBatch) -> Coherence {
    if batch.overflowed() {
        return Coherence::RescanRequired {
            reason: RescanReason::WatchOverflow,
            subjects: Vec::new(),
        };
    }
    let mut subjects: Vec<String> = batch
        .changed()
        .chain(batch.removed())
        .map(str::to_owned)
        .chain(
            batch
                .renamed()
                .flat_map(|(from, to)| [from.to_owned(), to.to_owned()]),
        )
        .collect();
    subjects.sort();
    subjects.dedup();
    Coherence::Stale {
        reason: StaleReason::FileContentChanged,
        subjects,
    }
}

fn empty_stale_snapshot(lease: &Lease, generation: u64, error: &IndexError) -> WorkspaceSnapshot {
    let identity =
        godot_mcp_domain::contracts::ProjectIdentity::from_lease_key(lease.lease_key(), generation);
    let settings_bytes = lease
        .safe_root()
        .read(Path::new("project.godot"))
        .unwrap_or_default();
    // The settings parser is lenient: it only fails on its own provenance bug,
    // so an unreadable manifest degrades to an empty, diagnosed parse.
    let settings = ProjectSettings::parse(
        std::str::from_utf8(&settings_bytes).unwrap_or_default(),
        generation,
    )
    .or_else(|_| ProjectSettings::parse("", generation))
    .expect("the lenient settings parser accepts an empty manifest");
    let index = IndexSnapshot::unavailable(identity, RescanReason::InventoryUnavailable)
        .expect("an empty graph over a valid identity is always constructible");
    WorkspaceSnapshot {
        revision: 1,
        inventory: FileInventory::new(),
        inventory_digest: inventory_digest(&index),
        settings,
        settings_digest: hex_sha256(&settings_bytes),
        index: Arc::new(index),
        coherence: WorkspaceCoherence::Stale {
            reason: refresh_failure(error),
        },
    }
}

/// A caller-safe reason: the store location and SQL detail are never quoted.
fn refresh_failure(error: &IndexError) -> String {
    match error {
        IndexError::Source { reason, .. } => {
            format!("a project document could not be modelled: {reason}")
        }
        IndexError::Refused(_) => "the project refused a declared path while indexing".to_owned(),
        IndexError::InvalidInput { reason } => reason.clone(),
        IndexError::Graph(inner) => format!("the semantic graph was rejected: {inner}"),
        IndexError::Conflict => "another owner holds this project's analysis index".to_owned(),
        IndexError::InsecureState { .. } => {
            "the private analysis cache location is not usable".to_owned()
        }
        IndexError::StoreUnreadable { .. } | IndexError::Io(_) => {
            "the analysis index could not be updated".to_owned()
        }
    }
}

/// Paths inside the engine cache, the private state root or the server-owned
/// helper scratch, and the files a compare-and-swap stages or parks beside its
/// target, are never project source, whatever an event or a mutation names.
fn is_private_path(path: &str) -> bool {
    let first = path.split('/').next().unwrap_or_default();
    first == ENGINE_CACHE_DIR
        || first == PROJECT_STATE_DIR_NAME
        || first == HELPER_ROOT
        || path.rsplit('/').next().is_some_and(is_staging_name)
}

/// Normalize one platform event. Directory-level changes, an event the backend
/// flags for rescan and an unpaired rename all become an overflow, because the
/// files they affect cannot be enumerated from the event alone.
fn normalize_event(event: &notify::Event, root: &Path) -> Vec<WatchSignal> {
    use notify::EventKind;
    use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};

    if event.need_rescan() {
        return vec![WatchSignal::Overflow];
    }
    // Each event path, `None` when it is private: a rename between a private
    // name and a project path is a change or a removal of the project path.
    let sides: Vec<Option<String>> = event
        .paths
        .iter()
        .map(|path| relative_event_path(path, root))
        .collect();
    let paths: Vec<String> = sides.iter().flatten().cloned().collect();
    let all_private = !event.paths.is_empty() && paths.is_empty();
    if all_private {
        return Vec::new();
    }
    let changed = |paths: Vec<String>| {
        paths
            .into_iter()
            .map(|file| WatchSignal::Changed { file })
            .collect::<Vec<_>>()
    };
    match event.kind {
        EventKind::Access(_) => match event.kind {
            EventKind::Access(notify::event::AccessKind::Close(
                notify::event::AccessMode::Write,
            )) => changed(paths),
            _ => Vec::new(),
        },
        EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder) => {
            vec![WatchSignal::Overflow]
        }
        EventKind::Create(_) => changed(paths),
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => match sides.as_slice() {
            [Some(from), Some(to)] => vec![WatchSignal::Renamed {
                from: from.clone(),
                to: to.clone(),
            }],
            // Staged bytes published under a project name.
            [None, Some(to)] => vec![WatchSignal::Changed { file: to.clone() }],
            // A project file parked under a private name.
            [Some(from), None] => vec![WatchSignal::Removed { file: from.clone() }],
            _ => vec![WatchSignal::Overflow],
        },
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => paths
            .into_iter()
            .map(|file| WatchSignal::Removed { file })
            .collect(),
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => changed(paths),
        EventKind::Modify(ModifyKind::Name(_)) => vec![WatchSignal::Overflow],
        EventKind::Modify(ModifyKind::Metadata(_)) => Vec::new(),
        EventKind::Modify(_) => changed(paths),
        EventKind::Remove(_) => paths
            .into_iter()
            .map(|file| WatchSignal::Removed { file })
            .collect(),
        EventKind::Any | EventKind::Other => vec![WatchSignal::Overflow],
    }
}

fn relative_event_path(path: &Path, root: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(value) => components.push(value.to_str()?.to_owned()),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    let joined = components.join("/");
    (!joined.is_empty() && !is_private_path(&joined)).then_some(joined)
}

fn index_error(error: IndexError) -> OutcomeError {
    OutcomeError::new(
        error.error_kind(),
        refresh_failure(&error),
        false,
        std::collections::BTreeMap::new(),
    )
}

fn watch_error(error: notify::Error) -> OutcomeError {
    OutcomeError::new(
        ErrorKind::Unavailable,
        format!("the workspace watcher is unavailable: {error}"),
        false,
        std::collections::BTreeMap::new(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use godot_mcp_domain::lease::LeaseManager;

    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        coordinator: Arc<WorkspaceCoordinator>,
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().canonicalize().expect("canonical").join("game");
        std::fs::create_dir_all(root.join("scripts")).expect("project dirs");
        std::fs::write(root.join("project.godot"), "config_version=5\n").expect("manifest");
        std::fs::write(
            root.join("scripts/a.gd"),
            "extends Node\n\nfunc a() -> void:\n\tpass\n",
        )
        .expect("script");
        let lease = LeaseManager::project_local()
            .acquire(&root)
            .expect("lease the fixture");
        let index_dir = lease.state_root().join("index");
        let coordinator = WorkspaceCoordinator::open(lease, 0, index_dir);
        Fixture {
            _temp: temp,
            root,
            coordinator,
        }
    }

    #[test]
    fn the_startup_scan_is_revision_one_and_coherent() {
        let fixture = fixture();
        let snapshot = fixture.coordinator.current();
        assert_eq!(snapshot.revision(), 1);
        assert!(snapshot.coherence().is_coherent());
        assert!(snapshot.inventory().contains("scripts/a.gd"));
        assert!(snapshot.inventory().contains("project.godot"));
        assert!(
            !snapshot
                .inventory()
                .paths()
                .any(|path| path.starts_with(".godot-mcp")),
            "the private state root is never inventoried"
        );
    }

    #[test]
    fn an_overflow_publishes_exactly_one_full_rescan_revision() {
        let fixture = fixture();
        std::fs::write(fixture.root.join("scripts/b.gd"), "extends Node\n").expect("b");
        std::fs::write(fixture.root.join("scripts/c.gd"), "extends Node\n").expect("c");
        let before = fixture.coordinator.current().revision();
        let after = fixture.coordinator.rescan();
        assert_eq!(after.revision(), before + 1, "one overflow, one revision");
        assert!(after.coherence().is_coherent());
        assert!(after.inventory().contains("scripts/b.gd"));
        assert!(after.inventory().contains("scripts/c.gd"));
    }

    #[test]
    fn a_committed_removal_leaves_the_inventory_in_the_next_revision() {
        let fixture = fixture();
        std::fs::remove_file(fixture.root.join("scripts/a.gd")).expect("remove");
        let after = fixture
            .coordinator
            .observe_commit(&CommitScope::Paths(vec!["scripts/a.gd".to_owned()]));
        assert_eq!(after.revision(), 2);
        assert!(!after.inventory().contains("scripts/a.gd"));
        assert!(after.coherence().is_coherent());
    }

    #[test]
    fn a_failed_refresh_keeps_the_revision_and_is_never_coherent() {
        let fixture = fixture();
        std::fs::write(fixture.root.join("scripts/bad.gd"), [0xff, 0xfe]).expect("bad bytes");
        let stale = fixture
            .coordinator
            .observe_commit(&CommitScope::Paths(vec!["scripts/bad.gd".to_owned()]));
        assert_eq!(
            stale.revision(),
            1,
            "a failed refresh publishes no new revision"
        );
        assert!(!stale.coherence().is_coherent());
        assert!(
            !stale.index().is_current(),
            "the retained index must admit it is behind"
        );
        std::fs::remove_file(fixture.root.join("scripts/bad.gd")).expect("repair");
        let repaired = fixture
            .coordinator
            .observe_commit(&CommitScope::Paths(vec!["scripts/bad.gd".to_owned()]));
        assert_eq!(repaired.revision(), 2);
        assert!(repaired.coherence().is_coherent());
    }

    /// A failed refresh applies nothing, so the change it carried must not be
    /// forgotten by the next one. Windows reports a file's creation and its
    /// directory's modification as separate events: a later batch that names
    /// only the directory, or any other file, must still see the script that
    /// could not be indexed instead of publishing a coherent view without it.
    #[test]
    fn a_change_a_failed_refresh_could_not_apply_is_not_forgotten_by_the_next_one() {
        use godot_mcp_analysis::index::FileFingerprint;
        use notify::event::{CreateKind, DataChange, EventKind, ModifyKind, RemoveKind};
        let fixture = fixture();
        *fixture.coordinator.pending.lock().expect("pending signals") = Some(
            WatchDebouncer::new(&fixture.root, WATCH_QUIET_MS, WATCH_CAPACITY).expect("debouncer"),
        );
        // The watcher names paths under the root it watches, the lease's own
        // spelling of it (on Windows not the `\\?\` form `canonicalize` gives).
        let watched = fixture.coordinator.lease.display_root().to_path_buf();
        let event = |kind: EventKind, path: &str| {
            fixture
                .coordinator
                .record_event(&notify::Event::new(kind).add_path(watched.join(path)));
            fixture.coordinator.snapshot()
        };
        std::fs::write(
            fixture.root.join("scripts/bad.gd"),
            [0xff, 0xfe, 0x00, 0x80],
        )
        .expect("bad bytes");
        let stale = event(EventKind::Create(CreateKind::File), "scripts/bad.gd");
        assert!(!stale.coherence().is_coherent(), "{}", stale.to_value());

        let directory = event(EventKind::Modify(ModifyKind::Any), "scripts");
        assert!(
            !directory.coherence().is_coherent(),
            "the script that cannot be indexed is still there: {}",
            directory.to_value()
        );
        std::fs::write(fixture.root.join("scripts/a.gd"), "extends Node\n").expect("an edit");
        let unrelated = event(
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            "scripts/a.gd",
        );
        assert!(
            !unrelated.coherence().is_coherent(),
            "an unrelated edit does not make it go away: {}",
            unrelated.to_value()
        );
        assert_eq!(unrelated.revision(), 1, "no refresh has succeeded yet");

        std::fs::remove_file(fixture.root.join("scripts/bad.gd")).expect("repair");
        let repaired = event(EventKind::Remove(RemoveKind::File), "scripts/bad.gd");
        assert!(
            repaired.coherence().is_coherent(),
            "{}",
            repaired.to_value()
        );
        assert_eq!(repaired.revision(), 2);
        assert!(!repaired.inventory().contains("scripts/bad.gd"));
        assert_eq!(
            repaired.fingerprint_of("scripts/a.gd"),
            Some(FileFingerprint::of_bytes(b"extends Node\n").as_str()),
            "the edit made while the workspace was stale is in the repaired view"
        );
    }

    /// A coordinator whose startup scan failed starts with an empty inventory;
    /// a later change to one file must not publish a view of that file alone.
    #[test]
    fn a_failed_startup_scan_is_never_completed_by_one_file_change() {
        let fixture = fixture();
        std::fs::write(fixture.root.join("scripts/bad.gd"), [0xff, 0xfe]).expect("bad bytes");
        drop(fixture.coordinator);
        let lease = LeaseManager::project_local()
            .acquire(&fixture.root)
            .expect("lease the fixture again");
        let index_dir = lease.state_root().join("index-restart");
        let coordinator = WorkspaceCoordinator::open(lease, 0, index_dir);
        assert!(!coordinator.current().coherence().is_coherent());

        std::fs::write(fixture.root.join("scripts/a.gd"), "extends Node\n").expect("an edit");
        let edited =
            coordinator.observe_commit(&CommitScope::Paths(vec!["scripts/a.gd".to_owned()]));
        assert!(
            !edited.coherence().is_coherent(),
            "the unindexable script is still there: {}",
            edited.to_value()
        );

        std::fs::remove_file(fixture.root.join("scripts/bad.gd")).expect("repair");
        let repaired =
            coordinator.observe_commit(&CommitScope::Paths(vec!["scripts/bad.gd".to_owned()]));
        assert!(
            repaired.coherence().is_coherent(),
            "{}",
            repaired.to_value()
        );
        assert!(repaired.inventory().contains("project.godot"));
        assert!(repaired.inventory().contains("scripts/a.gd"));
        assert_eq!(repaired.inventory().len(), 2, "{}", repaired.to_value());
    }

    /// Godot writes `run/main_scene=""` for a project whose main scene was
    /// cleared; that is a project without a main scene, not one that cannot be
    /// modelled.
    #[test]
    fn an_empty_main_scene_setting_is_a_project_without_one() {
        let fixture = fixture();
        std::fs::write(
            fixture.root.join("project.godot"),
            "config_version=5\n\n[application]\n\nrun/main_scene=\"\"\n",
        )
        .expect("manifest");
        let snapshot = fixture.coordinator.rescan();
        assert!(
            snapshot.coherence().is_coherent(),
            "{}",
            snapshot.to_value()
        );
        assert!(snapshot.settings().main_scene().is_none());
    }

    /// The startup scan runs before the watcher exists, so nothing reports a
    /// change made in between; starting the watcher must still bring it in.
    #[test]
    fn a_file_created_before_the_watcher_started_is_never_missed() {
        let fixture = fixture();
        std::fs::write(fixture.root.join("scripts/early.gd"), "extends Node\n").expect("early");
        fixture
            .coordinator
            .start_watching()
            .expect("the watcher starts");
        let snapshot = fixture.coordinator.snapshot();
        assert!(
            snapshot.inventory().contains("scripts/early.gd"),
            "{}",
            snapshot.to_value()
        );
        assert_eq!(snapshot.revision(), 2);
        assert!(snapshot.coherence().is_coherent());
    }

    /// Closing that gap publishes nothing for a project that did not change.
    #[test]
    fn starting_the_watcher_on_an_unchanged_project_keeps_revision_one() {
        let fixture = fixture();
        fixture
            .coordinator
            .start_watching()
            .expect("the watcher starts");
        let snapshot = fixture.coordinator.snapshot();
        assert_eq!(snapshot.revision(), 1, "{}", snapshot.to_value());
        assert!(snapshot.coherence().is_coherent());
    }

    /// Without a watcher nothing reports an external change, so every request
    /// looks at the project itself rather than trusting the last snapshot.
    #[test]
    fn a_workspace_without_a_watcher_still_sees_external_changes() {
        let fixture = fixture();
        assert!(
            fixture
                .coordinator
                .start_watching_root(&fixture.root.join("missing"))
                .is_err(),
            "a watcher over a missing directory cannot start"
        );
        std::fs::write(fixture.root.join("scripts/late.gd"), "extends Node\n").expect("late");
        let snapshot = fixture.coordinator.snapshot();
        assert!(
            snapshot.inventory().contains("scripts/late.gd"),
            "{}",
            snapshot.to_value()
        );
        assert!(snapshot.coherence().is_coherent());
        let again = fixture.coordinator.snapshot();
        assert_eq!(
            again.revision(),
            snapshot.revision(),
            "an unchanged project publishes no new revision"
        );
    }

    /// Without a watcher nothing reports an external change, so a commit's
    /// snapshot is a full rescan: it holds every file on disk, not only the
    /// committed paths and the inventory before them.
    #[test]
    fn a_commit_without_a_watcher_still_publishes_every_external_change() {
        let fixture = fixture();
        assert!(
            fixture
                .coordinator
                .start_watching_root(&fixture.root.join("missing"))
                .is_err()
        );
        std::fs::write(fixture.root.join("scripts/external.gd"), "extends Node\n")
            .expect("an external file");
        std::fs::write(
            fixture.root.join("scripts/a.gd"),
            "extends Node\n# committed\n",
        )
        .expect("the committed change");
        let committed = fixture
            .coordinator
            .observe_commit(&CommitScope::Paths(vec!["scripts/a.gd".to_owned()]));
        assert!(
            committed.coherence().is_coherent(),
            "{:?}",
            committed.coherence()
        );
        assert!(
            committed.inventory().contains("scripts/external.gd"),
            "a commit's snapshot without a watcher holds the external change"
        );
    }

    #[test]
    fn the_server_owned_helper_scratch_is_never_project_source() {
        let fixture = fixture();
        let staging = fixture.root.join("godot_mcp_helpers/2/staging");
        std::fs::create_dir_all(&staging).expect("helper scratch");
        std::fs::write(
            fixture.root.join("godot_mcp_helpers/2/canonical_write.gd"),
            "extends SceneTree\n",
        )
        .expect("installed helper");
        std::fs::write(staging.join("x.tres"), "[gd_resource format=3]\n").expect("staged");
        let rescanned = fixture.coordinator.rescan();
        assert!(
            !rescanned
                .inventory()
                .paths()
                .any(|path| path.starts_with("godot_mcp_helpers/")),
            "the helper's install and scratch area is never inventoried"
        );
        let committed = fixture.coordinator.observe_commit(&CommitScope::Paths(vec![
            "godot_mcp_helpers/2/staging/x.tres".to_owned(),
        ]));
        assert!(
            !committed
                .inventory()
                .contains("godot_mcp_helpers/2/staging/x.tres")
        );
        let event = notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
            .add_path(staging.join("x.tres"));
        assert!(normalize_event(&event, &fixture.root).is_empty());
    }

    /// Every compare-and-swap stages its bytes, and parks what it replaces or
    /// deletes, under a private `.cas-<pid>-<n>.tmp` name beside the target.
    /// Such a file is the server's own write in flight: a refresh that indexed
    /// it would find it gone a moment later, and the next commit's snapshot
    /// could never be proven current.
    #[test]
    fn the_servers_own_staging_files_are_never_project_source() {
        use notify::event::{CreateKind, EventKind, ModifyKind, RenameMode};
        let fixture = fixture();
        let staged = fixture.root.join("scripts/.cas-4242-7.tmp");
        std::fs::write(&staged, "extends Node\n").expect("a write in flight");
        let rescanned = fixture.coordinator.rescan();
        assert!(
            !rescanned.inventory().contains("scripts/.cas-4242-7.tmp"),
            "a full rescan never inventories it"
        );
        let indexed = fixture.coordinator.observe_commit(&CommitScope::Paths(vec![
            "scripts/.cas-4242-7.tmp".to_owned(),
        ]));
        assert!(!indexed.inventory().contains("scripts/.cas-4242-7.tmp"));
        // Published: the staged file is gone, and the next commit's snapshot
        // is proven current.
        std::fs::remove_file(&staged).expect("published");
        let next = fixture
            .coordinator
            .observe_commit(&CommitScope::Paths(vec!["scripts/a.gd".to_owned()]));
        assert!(next.coherence().is_coherent(), "{:?}", next.coherence());

        let root = &fixture.root;
        let created =
            notify::Event::new(EventKind::Create(CreateKind::File)).add_path(staged.clone());
        assert!(normalize_event(&created, root).is_empty());
        let published = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(staged.clone())
            .add_path(root.join("scripts/a.gd"));
        assert_eq!(
            normalize_event(&published, root),
            vec![WatchSignal::Changed {
                file: "scripts/a.gd".to_owned()
            }],
            "publishing the staged bytes changes the target"
        );
        let parked = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(root.join("scripts/a.gd"))
            .add_path(staged);
        assert_eq!(
            normalize_event(&parked, root),
            vec![WatchSignal::Removed {
                file: "scripts/a.gd".to_owned()
            }],
            "parking the target removes it"
        );
    }

    /// Another tool's file can be in a refresh's inventory while it exists and
    /// be gone a moment later: an editor's safe save writes `a.gd.tmp` and
    /// renames it over `a.gd`. The next commit's snapshot is still proven
    /// current, because what a refresh finds behind the files is read again
    /// rather than published stale.
    #[test]
    fn a_file_another_tool_renamed_away_never_leaves_a_commit_stale() {
        let fixture = fixture();
        let saved = fixture.root.join("scripts/a.gd.tmp");
        std::fs::write(&saved, "extends Node\n\nfunc saved() -> void:\n\tpass\n")
            .expect("an editor's safe save in flight");
        let indexed = fixture
            .coordinator
            .observe_commit(&CommitScope::Paths(vec!["scripts/a.gd.tmp".to_owned()]));
        assert!(
            indexed.inventory().contains("scripts/a.gd.tmp"),
            "another tool's file is project source while it exists"
        );
        std::fs::rename(&saved, fixture.root.join("scripts/a.gd")).expect("renamed over a.gd");
        std::fs::write(fixture.root.join("scripts/b.gd"), "extends Node\n").expect("b");
        let next = fixture
            .coordinator
            .observe_commit(&CommitScope::Paths(vec!["scripts/b.gd".to_owned()]));
        assert!(next.coherence().is_coherent(), "{:?}", next.coherence());
        assert!(!next.inventory().contains("scripts/a.gd.tmp"));
        assert!(next.inventory().contains("scripts/a.gd"));
        assert!(next.inventory().contains("scripts/b.gd"));
        let bytes = std::fs::read(fixture.root.join("scripts/a.gd")).expect("a.gd");
        assert_eq!(
            next.fingerprint_of("scripts/a.gd"),
            Some(godot_mcp_analysis::index::FileFingerprint::of_bytes(&bytes).as_str()),
            "the saved bytes are indexed"
        );
    }

    #[test]
    fn events_for_private_or_directory_paths_never_become_partial_batches() {
        use notify::event::{CreateKind, EventKind};
        let root = Path::new("/srv/game");
        let private = notify::Event::new(EventKind::Create(CreateKind::File))
            .add_path(root.join(".godot-mcp/journal/x"));
        assert!(normalize_event(&private, root).is_empty());
        let folder =
            notify::Event::new(EventKind::Create(CreateKind::Folder)).add_path(root.join("scenes"));
        assert_eq!(normalize_event(&folder, root), vec![WatchSignal::Overflow]);
        let content = notify::Event::new(EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        )))
        .add_path(root.join("scripts/a.gd"));
        assert_eq!(
            normalize_event(&content, root),
            vec![WatchSignal::Changed {
                file: "scripts/a.gd".to_owned()
            }]
        );
    }
}
