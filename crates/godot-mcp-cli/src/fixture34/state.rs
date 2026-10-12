//! The fixture's own cleanup manifest.
//!
//! Cleanup must terminate exactly the processes this scenario started and
//! nothing else. Matching by program name or command-line pattern would kill
//! unrelated processes on a shared host, so the fixture records, for every
//! process it starts, the identity Todo 14 already treats as authoritative: the
//! PID **together with** the platform-native start time. A PID whose start time
//! no longer matches is a different process after PID reuse, and is left alone.
//!
//! The manifest is written incrementally — after every spawn, before the next
//! step — so a setup that fails midway still leaves a complete record of what it
//! had already started, and its own rollback can tear that down.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::FixtureError;

/// Version marker carried by the manifest.
pub const STATE_SCHEMA_VERSION: &str = "1";

/// One process this scenario started, with the identity cleanup matches on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurvivorRecord {
    /// Why this process exists, for diagnostics.
    pub label: String,
    /// The transaction this process belongs to.
    pub transaction_id: String,
    /// Observed PID.
    pub pid: u32,
    /// Observed platform-native start time. `None` makes every identity
    /// comparison fail closed, so such a process is never killed.
    pub start_time: Option<String>,
    /// Whether setup deliberately left this process alive. A process recorded as
    /// already terminated is not expected to be live at cleanup time.
    pub expect_live: bool,
}

/// The fake bridge listener this scenario started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeRecord {
    /// Observed PID of the bridge process.
    pub pid: u32,
    /// Observed platform-native start time.
    pub start_time: Option<String>,
    /// The loopback endpoint it bound.
    pub endpoint: String,
    /// The inode of the listening socket it holds, when the platform reports one.
    /// Cleanup uses this to prove the recorded PID is the process that owns this
    /// exact socket before terminating it.
    pub socket_inode: Option<u64>,
    /// Where its rendezvous file lives, inside the fixture tree.
    pub rendezvous_path: String,
}

/// Everything cleanup needs, and nothing it could use to guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixtureState {
    /// Manifest version.
    pub schema_version: String,
    /// The scenario name, so a manifest from another scenario is never consumed.
    pub scenario: String,
    /// The project root this manifest describes.
    pub project_root: String,
    /// Every process started, in the order it was started.
    pub survivors: Vec<SurvivorRecord>,
    /// The fake bridge, once it is listening.
    pub bridge: Option<BridgeRecord>,
}

impl FixtureState {
    /// An empty manifest for one scenario and project root.
    #[must_use]
    pub fn new(scenario: &str, project_root: &Path) -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION.to_owned(),
            scenario: scenario.to_owned(),
            project_root: project_root.display().to_string(),
            survivors: Vec::new(),
            bridge: None,
        }
    }

    /// Read the manifest. `Ok(None)` means there is no manifest at all, which is
    /// the normal state of an already-clean fixture tree.
    ///
    /// # Errors
    /// Returns [`FixtureError::Io`] when the file exists but cannot be read, and
    /// [`FixtureError::State`] when it is not a manifest for `scenario`.
    pub fn load(path: &Path, scenario: &str) -> Result<Option<Self>, FixtureError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(FixtureError::Io {
                    path: path.display().to_string(),
                    detail: error.to_string(),
                });
            }
        };
        let state: Self = serde_json::from_str(&text).map_err(|error| FixtureError::State {
            detail: format!("{} is not a fixture manifest: {error}", path.display()),
        })?;
        if state.schema_version != STATE_SCHEMA_VERSION {
            return Err(FixtureError::State {
                detail: format!(
                    "{} carries unsupported manifest version {}",
                    path.display(),
                    state.schema_version
                ),
            });
        }
        if state.scenario != scenario {
            return Err(FixtureError::State {
                detail: format!(
                    "{} describes scenario {}, not {scenario}",
                    path.display(),
                    state.scenario
                ),
            });
        }
        Ok(Some(state))
    }

    /// Write the manifest, replacing any existing one.
    ///
    /// # Errors
    /// Returns [`FixtureError::Io`] when the write fails.
    pub fn save(&self, path: &Path) -> Result<(), FixtureError> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| FixtureError::State {
            detail: format!("the fixture manifest could not be serialized: {error}"),
        })?;
        super::tree::write_file(path, &bytes)
    }
}
