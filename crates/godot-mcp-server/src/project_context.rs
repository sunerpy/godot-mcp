//! The one canonical project context a server process owns.
//!
//! [`ProjectContext::acquire`] is the only function that turns a caller's
//! candidate spelling into project ownership, and it performs the whole
//! acquisition sequence in one place:
//!
//! 1. discover the nearest `project.godot` upward from the candidate and keep an
//!    absolute, normalized display root (a relative candidate is resolved
//!    against the working directory exactly once, here);
//! 2. hold the root and `project.godot` handles without following a symlink;
//! 3. derive the stable object key from those handles;
//! 4. open the private state root relative to the *held root handle*;
//! 5. take the cross-process lock inside that state root;
//! 6. re-prove that the discovered spellings still name the held objects; and
//! 7. inventory candidate-derived state roots a version 0.1 server may have
//!    created, before any journal is opened or any mutation is admitted.
//!
//! After this the candidate spelling is dropped. The journal, audit log, index,
//! scheduler, launch providers and every Godot `--path` argument are built only
//! from the context, so a root, subdirectory, relative or alias spelling of one
//! project object cannot produce a second owner, a second state root or a second
//! journal.
//!
//! ## Legacy state roots
//!
//! Before 0.2 the state directory was derived from the candidate spelling, so a
//! server launched from a subdirectory created `<subdirectory>/.godot-mcp` with
//! its own lock and journal (finding F-01). Such a directory may hold the only
//! record of an unresolved effect, so it is never deleted or merged here:
//!
//! * its lock is taken with this lease's key, which is the exact lock a 0.1
//!   owner of the same project object holds — a live 0.1 owner is therefore a
//!   [`LeaseError::Conflict`], and holding the lock keeps one from starting;
//! * its journal is inspected read-only; an open or quarantined record blocks
//!   every mutation until an operator resolves it; and
//! * the selection of the canonical root and the inventory are recorded durably
//!   in [`MIGRATION_RECORD_FILE`] inside the canonical state root.

use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use godot_mcp_analysis::project::ENGINE_CACHE_DIR;
use godot_mcp_domain::contracts::ProjectIdentity;
use godot_mcp_domain::lease::{
    Lease, LeaseError, LeaseManager, MarkerPolicy, PROJECT_STATE_DIR_NAME, SiblingStateLock,
};
use serde::Serialize;

use crate::journal::{
    HeldProject, LegacyStoreSummary, encode_object_identity, inspect_legacy_store,
};

/// The frozen initial project generation.
///
/// Generation *semantics* (when a project's generation advances) are owned by the
/// workspace coordinator. Startup reconciliation only needs it to be stable
/// across restarts, so that a journaled transaction is compared against the same
/// generation that recorded it.
pub const INITIAL_PROJECT_GENERATION: u64 = 0;

/// File, inside the canonical state root, recording the state-root selection.
pub const MIGRATION_RECORD_FILE: &str = "state-root-migration.json";

const MIGRATION_RECORD_SCHEMA: &str = "godot-mcp.state-root-migration.v1";
const LEGACY_SCAN_MAX_ENTRIES: usize = 100_000;
const LEGACY_SCAN_MAX_DEPTH: usize = 64;

/// Why a project context could not be established.
#[derive(Debug, thiserror::Error)]
pub enum ProjectContextError {
    /// Ownership could not be established.
    #[error("project ownership could not be established: {0}")]
    Lease(#[from] LeaseError),
    /// Candidate-derived state roots could not be inventoried completely.
    #[error("legacy state roots could not be inventoried: {0}")]
    Inventory(String),
}

/// One candidate-derived state root found under the project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LegacyStateRoot {
    /// Project-relative directory spelling, with `/` separators.
    pub path: String,
    /// Every journaled transaction in it.
    pub records: u64,
    /// Transactions whose phase is not terminal.
    pub open_records: u64,
    /// Transactions settled as write-blocking quarantine.
    pub quarantined_records: u64,
    /// Whether this startup reconciled the root's open records in place.
    pub reconciled: bool,
}

impl LegacyStateRoot {
    /// Whether the root holds evidence of a possibly unresolved effect.
    #[must_use]
    pub const fn unresolved(&self) -> bool {
        self.open_records > 0 || self.quarantined_records > 0
    }
}

/// What startup may do with the legacy inventory, decided before any journal
/// record is settled (plan section 14.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LegacyPlan {
    /// No legacy root has an open record.
    Nothing,
    /// Exactly one state root holds unresolved evidence and it is this legacy
    /// root, so its open records are reconciled in place with their own schema.
    Reconcile(String),
    /// More than one state root holds unresolved evidence; choosing one could
    /// discard the only record of an effect, so nothing is reconciled and writes
    /// stay blocked for operator resolution.
    Split(Vec<String>),
}

#[derive(Serialize)]
struct MigrationRecord<'a> {
    schema: &'static str,
    selected: &'static str,
    legacy: &'a [LegacyStateRoot],
    split: bool,
    write_blocked: bool,
}

/// One acquired project: the held lease, its canonical display root, its private
/// state root and its path-free identity.
pub struct ProjectContext {
    lease: Lease,
    identity: ProjectIdentity,
    held: HeldProject,
    legacy: Vec<LegacyStateRoot>,
    split: Vec<String>,
    _legacy_locks: Vec<SiblingStateLock>,
}

impl std::fmt::Debug for ProjectContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Path-free on purpose: a context rendering must never carry a root.
        formatter
            .debug_struct("ProjectContext")
            .field("identity", &self.identity)
            .field("legacy_state_roots", &self.legacy.len())
            .finish_non_exhaustive()
    }
}

impl ProjectContext {
    /// Acquire the project `candidate` belongs to.
    ///
    /// # Errors
    /// Returns [`ProjectContextError::Lease`] for a discovery, privacy or
    /// ownership failure — including [`LeaseError::Conflict`] when another
    /// process owns the project through *any* spelling or through a legacy state
    /// root — and [`ProjectContextError::Inventory`] when the legacy inventory
    /// could not be completed or recorded.
    pub fn acquire(candidate: &Path) -> Result<Self, ProjectContextError> {
        // A live Godot editor saves project.godot by renaming a new file over
        // it, on startup and on every settings change; the project stays the
        // same object, so the server follows the marker rather than refusing
        // every later effect, and keeps the key it was first bound with.
        let lease = LeaseManager::project_local()
            .with_marker_policy(MarkerPolicy::FollowReplacement)
            .acquire(candidate)?;
        lease.revalidate()?;
        let key = lease.lease_key();
        let identity = ProjectIdentity::from_lease_key(key, INITIAL_PROJECT_GENERATION);
        let held = HeldProject {
            object_identity: identity.object_id().to_string(),
            root_object: encode_object_identity(key.root),
            file_object: encode_object_identity(key.project_file),
            generation: INITIAL_PROJECT_GENERATION,
        };
        let (legacy, legacy_locks) = inventory_legacy_state(&lease)?;
        Ok(Self {
            lease,
            identity,
            held,
            legacy,
            split: Vec::new(),
            _legacy_locks: legacy_locks,
        })
    }

    /// Decide what startup may do with the legacy inventory, given how many
    /// open records the canonical journal held before its own reconciliation.
    pub(crate) fn legacy_plan(&mut self, canonical_open_records: usize) -> LegacyPlan {
        let mut unresolved: Vec<String> = self
            .legacy
            .iter()
            .filter(|root| root.unresolved())
            .map(|root| root.path.clone())
            .collect();
        let legacy_open = self
            .legacy
            .iter()
            .filter(|root| root.open_records > 0)
            .map(|root| root.path.clone())
            .collect::<Vec<_>>();
        if canonical_open_records > 0 {
            unresolved.push(PROJECT_STATE_DIR_NAME.to_owned());
        }
        if unresolved.len() > 1 {
            self.split.clone_from(&unresolved);
            return LegacyPlan::Split(unresolved);
        }
        match legacy_open.as_slice() {
            [only] => LegacyPlan::Reconcile(only.clone()),
            _ => LegacyPlan::Nothing,
        }
    }

    /// Absolute display spelling of one inventoried legacy root.
    pub(crate) fn legacy_root_path(&self, relative: &str) -> PathBuf {
        let mut path = self.display_root().to_path_buf();
        for component in relative.split('/') {
            path.push(component);
        }
        path
    }

    /// Record that a legacy root was reconciled in place, with its re-read
    /// summary.
    pub(crate) fn note_reconciled(&mut self, relative: &str, summary: LegacyStoreSummary) {
        if let Some(root) = self.legacy.iter_mut().find(|root| root.path == relative) {
            root.records = summary.records;
            root.open_records = summary.open;
            root.quarantined_records = summary.quarantined;
            root.reconciled = true;
        }
    }

    /// The held lease. Holding the context holds the advisory lock.
    #[must_use]
    pub const fn lease(&self) -> &Lease {
        &self.lease
    }

    /// Absolute, normalized display spelling of the discovered project root.
    /// Metadata only: effects remain relative to the held root handle.
    #[must_use]
    pub fn display_root(&self) -> &Path {
        self.lease.display_root()
    }

    /// Absolute display spelling of the private state root.
    #[must_use]
    pub fn state_root(&self) -> &Path {
        self.lease.state_root()
    }

    /// Path-free public identity of the held project.
    #[must_use]
    pub const fn identity(&self) -> &ProjectIdentity {
        &self.identity
    }

    pub(crate) const fn held(&self) -> &HeldProject {
        &self.held
    }

    /// Candidate-derived state roots found at startup.
    #[must_use]
    pub fn legacy_state_roots(&self) -> &[LegacyStateRoot] {
        &self.legacy
    }

    /// The reason mutations must stay blocked because a legacy state root holds
    /// evidence of a possibly unresolved effect, if any does.
    #[must_use]
    pub fn legacy_write_block(&self) -> Option<String> {
        if !self.split.is_empty() {
            return Some(format!(
                "state roots {} each hold unresolved transactions for this project; none was \
                 reconciled because choosing one could discard the only record of an effect, and \
                 writes stay blocked until an operator resolves them",
                self.split.join(", ")
            ));
        }
        let blocking: Vec<&str> = self
            .legacy
            .iter()
            .filter(|root| root.unresolved())
            .map(|root| root.path.as_str())
            .collect();
        (!blocking.is_empty()).then(|| {
            format!(
                "legacy state root(s) {} hold open or quarantined transactions recorded before \
                 the canonical state root existed; writes stay blocked until an operator \
                 resolves them",
                blocking.join(", ")
            )
        })
    }

    /// Durably record the state-root selection and the legacy inventory, when
    /// any legacy root exists.
    ///
    /// # Errors
    /// Returns [`ProjectContextError::Inventory`] when the record cannot be
    /// written durably.
    pub(crate) fn record_migration(&self) -> Result<(), ProjectContextError> {
        if self.legacy.is_empty() {
            return Ok(());
        }
        let record = MigrationRecord {
            schema: MIGRATION_RECORD_SCHEMA,
            selected: PROJECT_STATE_DIR_NAME,
            legacy: &self.legacy,
            split: !self.split.is_empty(),
            write_blocked: self.legacy_write_block().is_some(),
        };
        let mut bytes = serde_json::to_vec_pretty(&record)
            .map_err(|error| ProjectContextError::Inventory(error.to_string()))?;
        bytes.push(b'\n');
        write_durably(self.state_root(), MIGRATION_RECORD_FILE, &bytes)
            .map_err(|error| ProjectContextError::Inventory(error.to_string()))
    }
}

fn inventory_legacy_state(
    lease: &Lease,
) -> Result<(Vec<LegacyStateRoot>, Vec<SiblingStateLock>), ProjectContextError> {
    let listing = lease
        .safe_root()
        .find_directories_named(
            OsStr::new(PROJECT_STATE_DIR_NAME),
            LEGACY_SCAN_MAX_ENTRIES,
            LEGACY_SCAN_MAX_DEPTH,
            &[OsStr::new(ENGINE_CACHE_DIR)],
        )
        .map_err(|error| ProjectContextError::Inventory(error.to_string()))?;
    if !listing.is_complete() {
        // A partial inventory could miss the only record of an unresolved
        // effect, so it is a refusal rather than a best effort.
        return Err(ProjectContextError::Inventory(format!(
            "the project tree exceeds {LEGACY_SCAN_MAX_ENTRIES} entries or \
             {LEGACY_SCAN_MAX_DEPTH} directory levels"
        )));
    }
    let mut roots = Vec::new();
    let mut locks = Vec::new();
    for relative in listing.files() {
        if relative.as_os_str() == OsStr::new(PROJECT_STATE_DIR_NAME) {
            continue;
        }
        locks.push(lease.lock_sibling_state_dir(relative)?);
        let summary = inspect_legacy_store(&lease.display_root().join(relative).join("journal"))
            .map_err(|error| ProjectContextError::Inventory(error.to_string()))?;
        roots.push(legacy_root(relative, summary));
    }
    Ok((roots, locks))
}

fn legacy_root(relative: &Path, summary: LegacyStoreSummary) -> LegacyStateRoot {
    let path = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    LegacyStateRoot {
        path,
        records: summary.records,
        open_records: summary.open,
        quarantined_records: summary.quarantined,
        reconciled: false,
    }
}

/// Write `bytes` to `directory/name` through a sibling temporary file, fsync
/// it, rename it into place and fsync the directory.
fn write_durably(directory: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let temporary: PathBuf = directory.join(format!(".{name}.tmp"));
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, directory.join(name))?;
    #[cfg(unix)]
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}
