//! The Task-34 fixture tree: where it lives and exactly what it contains.
//!
//! `qa-fixture-34` is the sole owner of `tests/fixtures/quarantined` and
//! `tests/fixtures/repair/*.json`. Every path below is derived from the located
//! workspace root, never from an ambient environment variable, so the fixture
//! cannot be pointed at an unrelated directory.
//!
//! Every byte written here is a fixed literal. The fixture must rebuild to the
//! same state on every run, so no content may depend on the clock, the PID or
//! the host.

use std::path::{Path, PathBuf};

use super::FixtureError;

/// The project root the fixture owns, relative to the workspace root.
pub const QUARANTINED_RELATIVE: &str = "tests/fixtures/quarantined";
/// The proof-file directory the fixture owns, relative to the workspace root.
pub const REPAIR_RELATIVE: &str = "tests/fixtures/repair";
/// The fixture's own bookkeeping directory, relative to the project root.
pub const BOOKKEEPING_RELATIVE: &str = "fixture34";

/// The `project.godot` bytes. Enough for Todo 2's project discovery, and nothing
/// a real editor would need.
const PROJECT_GODOT: &str = "; Task-34 quarantined repair fixture. Owned by qa-fixture-34.\n\
                             config_version=5\n\
                             \n\
                             [application]\n\
                             \n\
                             config/name=\"task34-quarantined\"\n";

/// Every resolved path the fixture owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixturePaths {
    /// The located workspace root.
    pub workspace_root: PathBuf,
    /// `tests/fixtures/quarantined`.
    pub project_root: PathBuf,
    /// `tests/fixtures/repair`.
    pub repair_dir: PathBuf,
    /// `tests/fixtures/quarantined/fixture34`.
    pub bookkeeping: PathBuf,
}

impl FixturePaths {
    /// Resolve every owned path from `start`, which may be any directory inside
    /// the workspace.
    ///
    /// # Errors
    /// Returns [`FixtureError::Layout`] when no workspace root can be located.
    pub fn resolve(start: &Path) -> Result<Self, FixtureError> {
        let workspace_root =
            crate::audit::find_workspace_root(start).map_err(FixtureError::Layout)?;
        let project_root = workspace_root.join(QUARANTINED_RELATIVE);
        Ok(Self {
            repair_dir: workspace_root.join(REPAIR_RELATIVE),
            bookkeeping: project_root.join(BOOKKEEPING_RELATIVE),
            project_root,
            workspace_root,
        })
    }

    /// Resolve every owned path from the current directory.
    ///
    /// # Errors
    /// Returns [`FixtureError::Layout`] when the current directory is unreadable
    /// or lies outside a Cargo workspace.
    pub fn from_current_dir() -> Result<Self, FixtureError> {
        let cwd = std::env::current_dir().map_err(|error| {
            FixtureError::Layout(format!("the current directory is unreadable: {error}"))
        })?;
        Self::resolve(&cwd)
    }

    /// The fixture's cleanup manifest.
    #[must_use]
    pub fn state_file(&self) -> PathBuf {
        self.bookkeeping.join("state.json")
    }

    /// The fake bridge's own directory inside the fixture tree.
    #[must_use]
    pub fn bridge_dir(&self) -> PathBuf {
        self.bookkeeping.join("bridge")
    }

    /// The fake bridge's rendezvous file, written by the bridge process itself.
    #[must_use]
    pub fn bridge_rendezvous(&self) -> PathBuf {
        self.bridge_dir().join("rendezvous.json")
    }

    /// The configuration setup hands the bridge process before spawning it.
    #[must_use]
    pub fn bridge_config(&self) -> PathBuf {
        self.bridge_dir().join("config.json")
    }

    /// The readiness record the bridge process writes once it is listening.
    #[must_use]
    pub fn bridge_ready(&self) -> PathBuf {
        self.bridge_dir().join("ready.json")
    }

    /// The bridge process's diagnostics sink. Never stdout: the bridge inherits
    /// nothing and writes its own file.
    #[must_use]
    pub fn bridge_log(&self) -> PathBuf {
        self.bridge_dir().join("bridge.log")
    }

    /// The containment-boundary directory for one transaction.
    #[must_use]
    pub fn boundary_dir(&self, transaction_id: &str) -> PathBuf {
        self.bookkeeping.join("boundary").join(transaction_id)
    }

    /// The blocking FIFO the still-suspended survivor waits on.
    #[must_use]
    pub fn survivor_fifo(&self, transaction_id: &str) -> PathBuf {
        self.bookkeeping.join(format!("survivor-{transaction_id}"))
    }

    /// Whether any part of the fixture tree is currently present.
    #[must_use]
    pub fn any_present(&self) -> bool {
        self.project_root.exists() || self.repair_dir.exists()
    }
}

/// The bytes one fixture scene file holds after setup.
///
/// The content is a pure function of the transaction ID, so the observed hash a
/// proof file records is reproducible and the file can never accidentally match
/// another transaction's intended post-state.
#[must_use]
pub fn scene_bytes(transaction_id: &str) -> Vec<u8> {
    format!(
        "[gd_scene load_steps=1 format=3]\n\
         \n\
         ; task-34 fixture scene for transaction {transaction_id}\n\
         \n\
         [node name=\"Root\" type=\"Node\"]\n"
    )
    .into_bytes()
}

/// The bytes a fixture scene file is recorded as having held *before* the
/// transaction, for a transaction whose live bytes are its intended post-state.
///
/// These bytes are deliberately never written to disk: the recorded pre-state
/// must not match the live file, or a post-state fixture would also satisfy a
/// pre-state claim.
#[must_use]
pub fn superseded_scene_bytes(transaction_id: &str) -> Vec<u8> {
    format!(
        "[gd_scene load_steps=1 format=3]\n\
         \n\
         ; task-34 superseded pre-state for transaction {transaction_id}\n"
    )
    .into_bytes()
}

/// Create the project object: the root directory, `project.godot` and the
/// `scenes/` subtree, with every scene file's final bytes in place.
///
/// # Errors
/// Returns [`FixtureError::Io`] when any directory or file cannot be created.
pub fn create_project_object(
    paths: &FixturePaths,
    scenes: &[(&str, Vec<u8>)],
) -> Result<(), FixtureError> {
    create_dir(&paths.project_root)?;
    create_dir(&paths.bookkeeping)?;
    create_dir(&paths.bridge_dir())?;
    create_dir(&paths.project_root.join("scenes"))?;
    create_dir(&paths.repair_dir)?;
    write_file(
        &paths.project_root.join("project.godot"),
        PROJECT_GODOT.as_bytes(),
    )?;
    for (relative, bytes) in scenes {
        write_file(&paths.project_root.join(relative), bytes)?;
    }
    Ok(())
}

/// Create one directory and every missing parent.
///
/// # Errors
/// Returns [`FixtureError::Io`] when creation fails.
pub fn create_dir(path: &Path) -> Result<(), FixtureError> {
    std::fs::create_dir_all(path).map_err(|error| FixtureError::Io {
        path: path.display().to_string(),
        detail: error.to_string(),
    })
}

/// Write one file, replacing any existing content.
///
/// # Errors
/// Returns [`FixtureError::Io`] when the write fails.
pub fn write_file(path: &Path, bytes: &[u8]) -> Result<(), FixtureError> {
    std::fs::write(path, bytes).map_err(|error| FixtureError::Io {
        path: path.display().to_string(),
        detail: error.to_string(),
    })
}

/// Remove the whole fixture tree: the project object and every proof file.
///
/// Removing an already-absent tree succeeds, so cleanup is idempotent.
///
/// # Errors
/// Returns [`FixtureError::Io`] when a present path cannot be removed.
pub fn remove_tree(paths: &FixturePaths) -> Result<(), FixtureError> {
    remove_dir(&paths.project_root)?;
    remove_dir(&paths.repair_dir)
}

fn remove_dir(path: &Path) -> Result<(), FixtureError> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(FixtureError::Io {
            path: path.display().to_string(),
            detail: error.to_string(),
        }),
    }
}
