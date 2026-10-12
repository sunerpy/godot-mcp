//! The project-asset installer: install, idempotent update, conflict refusal and
//! removal, all driven by a recorded ownership manifest.
//!
//! ## What this installs, and what it does not
//!
//! The installer's payload is the bridge protocol's project assets: the Todo 19
//! codec (`crates/godot-mcp-bridge/scripts/bridge/bridge_codec.gd`) together
//! with the bridge wire schema, under `godot_mcp/` inside the target project.
//! It registers no editor plugin. The `godot_mcp` editor add-on is installed
//! into `addons/godot_mcp/` by the server's own `editor.addon.*` tools, which
//! apply the same ownership rules through the journal.
//!
//! The ownership, conflict, backup and idempotency semantics below are exactly
//! the ones the plan requires of an installer, and they are independent of what
//! the payload happens to be, so they are verified in full.
//!
//! ## Ownership rules
//!
//! * A file the manifest does not record is **foreign**. The installer never
//!   creates, overwrites, backs up or deletes a foreign file; encountering one
//!   where a payload file belongs is a conflict.
//! * A recorded file whose live digest still matches the manifest is **owned and
//!   clean**: it may be overwritten in place.
//! * A recorded file whose live digest has drifted was edited by the user. An
//!   `update` backs it up before overwriting; an `install` refuses.
//! * Planning is complete before the first byte is written, so a conflict leaves
//!   the project byte-for-byte unchanged.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Install root, relative to the project root.
pub const INSTALL_ROOT: &str = "godot_mcp";

/// File name of the ownership manifest inside the install root.
pub const MANIFEST_FILE: &str = "install-manifest.json";

/// Suffix appended when an update backs up a user-modified owned file.
pub const BACKUP_SUFFIX: &str = ".godot-mcp-backup";

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// One payload file, addressed relative to the project root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadFile {
    /// Project-relative path, `/`-separated, always under [`INSTALL_ROOT`].
    pub path: String,
    /// Exact bytes to install.
    pub bytes: Vec<u8>,
    /// Unix mode to apply.
    pub mode: u32,
}

/// The complete set of files one install/update writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallPayload {
    /// Payload files, sorted by path.
    pub files: Vec<PayloadFile>,
}

impl InstallPayload {
    /// Sort the payload so a plan is deterministic regardless of input order.
    fn normalized(mut self) -> Self {
        self.files.sort_by(|left, right| left.path.cmp(&right.path));
        self
    }
}

/// A recorded owned file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OwnedFile {
    /// Project-relative path.
    pub path: String,
    /// Digest of the bytes the installer wrote.
    pub sha256: String,
}

/// A backup an update created for a user-modified owned file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupRecord {
    /// The owned file that had drifted.
    pub path: String,
    /// Where its previous contents were preserved.
    pub backup: String,
    /// Digest of the preserved bytes.
    pub sha256: String,
}

/// The ownership manifest the installer maintains inside the install root.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstallManifest {
    /// Manifest schema version.
    pub schema_version: String,
    /// Version of the package that produced the current contents.
    pub package_version: String,
    /// Every file the installer owns.
    pub files: Vec<OwnedFile>,
    /// Every backup an update has taken, oldest first.
    #[serde(default)]
    pub backups: Vec<BackupRecord>,
}

impl InstallManifest {
    fn empty(package_version: &str) -> Self {
        Self {
            schema_version: "1.0.0".to_string(),
            package_version: package_version.to_string(),
            files: Vec::new(),
            backups: Vec::new(),
        }
    }

    fn owned(&self) -> BTreeMap<&str, &str> {
        self.files
            .iter()
            .map(|file| (file.path.as_str(), file.sha256.as_str()))
            .collect()
    }

    /// Load the manifest at `<project>/godot_mcp/install-manifest.json`, or
    /// `None` when nothing is installed.
    ///
    /// # Errors
    /// Returns an error if the manifest exists but cannot be read or parsed.
    pub fn load(project: &Path) -> Result<Option<Self>, String> {
        let path = manifest_path(project);
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| format!("parse {}: {error}", path.display()))
    }
}

fn manifest_path(project: &Path) -> PathBuf {
    project.join(INSTALL_ROOT).join(MANIFEST_FILE)
}

/// What the installer will do to one payload file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    /// The path is free; the file will be created.
    Create,
    /// The file is owned, clean and already byte-identical: nothing to do.
    Unchanged,
    /// The file is owned and clean but its contents differ: overwrite in place.
    Overwrite,
    /// The file is owned but the user modified it: back it up, then overwrite.
    BackupThenOverwrite,
    /// The path is occupied by a file the manifest does not own.
    ForeignConflict,
    /// The file is owned and drifted, and this mode refuses to touch it.
    DriftConflict,
}

impl FileAction {
    /// Whether this action blocks the whole operation.
    #[must_use]
    pub const fn is_conflict(self) -> bool {
        matches!(self, Self::ForeignConflict | Self::DriftConflict)
    }
}

/// The planned action for one payload file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    /// Project-relative path.
    pub path: String,
    /// What will happen to it.
    pub action: FileAction,
}

/// Which of the two writing modes is being planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// First install: refuses any occupied path, owned-and-drifted included.
    Install,
    /// Idempotent update: overwrites owned files, backing up drifted ones.
    Update,
}

/// A complete plan, computed before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    /// Mode this plan was computed for.
    pub mode: WriteMode,
    /// One entry per payload file, in payload order.
    pub files: Vec<PlannedFile>,
}

impl InstallPlan {
    /// Every conflicting path, with the reason.
    #[must_use]
    pub fn conflicts(&self) -> Vec<&PlannedFile> {
        self.files
            .iter()
            .filter(|file| file.action.is_conflict())
            .collect()
    }

    /// Whether applying this plan would change any byte on disk.
    #[must_use]
    pub fn changes_anything(&self) -> bool {
        self.files
            .iter()
            .any(|file| !matches!(file.action, FileAction::Unchanged))
    }
}

/// Compute the plan for `payload` against the live project state.
///
/// # Errors
/// Returns an error if the existing manifest cannot be read, a payload path
/// escapes the install root, or an existing file cannot be read for hashing.
pub fn plan(
    project: &Path,
    payload: &InstallPayload,
    mode: WriteMode,
) -> Result<InstallPlan, String> {
    let manifest = InstallManifest::load(project)?.unwrap_or_else(|| InstallManifest::empty(""));
    let owned = manifest.owned();
    let mut files = Vec::with_capacity(payload.files.len());
    for file in &payload.files {
        let relative = validated_relative(&file.path)?;
        let absolute = project.join(&relative);
        let action = if !absolute.exists() {
            FileAction::Create
        } else {
            let live = std::fs::read(&absolute)
                .map_err(|error| format!("read {}: {error}", absolute.display()))?;
            let live_digest = sha256_hex(&live);
            match owned.get(file.path.as_str()) {
                None => FileAction::ForeignConflict,
                Some(recorded) if *recorded == live_digest => {
                    if live == file.bytes {
                        FileAction::Unchanged
                    } else {
                        FileAction::Overwrite
                    }
                }
                Some(_) => match mode {
                    WriteMode::Install => FileAction::DriftConflict,
                    WriteMode::Update => FileAction::BackupThenOverwrite,
                },
            }
        };
        files.push(PlannedFile {
            path: file.path.clone(),
            action,
        });
    }
    Ok(InstallPlan { mode, files })
}

/// Reject a payload path that is not a plain relative path under the install
/// root, so a payload can never reach a user file elsewhere in the project.
fn validated_relative(path: &str) -> Result<PathBuf, String> {
    let expected_prefix = format!("{INSTALL_ROOT}/");
    if !path.starts_with(&expected_prefix) {
        return Err(format!(
            "payload path {path} is not under the {INSTALL_ROOT}/ install root"
        ));
    }
    // Validate the raw segments, never `Path::components()`: that iterator silently
    // normalizes `.` away and would let `godot_mcp/./x.gd` through, installing a
    // file under a key that does not match the one recorded as owned. A backslash
    // is rejected outright so a Windows-style separator cannot smuggle a segment
    // past this check on a Unix host.
    if path.contains('\\') {
        return Err(format!("payload path {path} contains a backslash"));
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(format!("payload path {path} is not a plain relative path"));
        }
    }
    Ok(PathBuf::from(path))
}

/// What an apply/remove actually did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallOutcome {
    /// Files created.
    pub created: Vec<String>,
    /// Files overwritten in place (owned and clean).
    pub overwritten: Vec<String>,
    /// Files backed up before being overwritten (owned and drifted).
    pub backed_up: Vec<BackupRecord>,
    /// Files that were already byte-identical.
    pub unchanged: Vec<String>,
    /// Files removed.
    pub removed: Vec<String>,
    /// Owned files a removal deliberately left in place because they had
    /// drifted, so the user's edit survives.
    pub retained: Vec<String>,
}

impl InstallOutcome {
    /// Number of files whose bytes this operation changed.
    #[must_use]
    pub fn changed(&self) -> usize {
        self.created.len() + self.overwritten.len() + self.backed_up.len() + self.removed.len()
    }
}

/// Install or update `payload` into `project`.
///
/// Fail-closed: if the plan reports any conflict, nothing is written and the
/// error names every conflicting path with its reason.
///
/// # Errors
/// Returns an error on a conflict, an unwritable path, or an unreadable manifest.
pub fn apply(
    project: &Path,
    payload: &InstallPayload,
    package_version: &str,
    mode: WriteMode,
) -> Result<InstallOutcome, String> {
    let payload = payload.clone().normalized();
    let plan = plan(project, &payload, mode)?;
    let conflicts = plan.conflicts();
    if !conflicts.is_empty() {
        let detail = conflicts
            .iter()
            .map(|file| {
                let reason = match file.action {
                    FileAction::ForeignConflict => {
                        "exists and is not owned by this installer (no ownership record)"
                    }
                    FileAction::DriftConflict => {
                        "is owned but was modified since install; run update to back it up first"
                    }
                    _ => "conflicts",
                };
                format!("{} {reason}", file.path)
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!(
            "installer refused before writing anything: {detail}"
        ));
    }

    let mut manifest =
        InstallManifest::load(project)?.unwrap_or_else(|| InstallManifest::empty(package_version));
    manifest.package_version = package_version.to_string();
    let mut outcome = InstallOutcome::default();
    let planned: BTreeMap<&str, FileAction> = plan
        .files
        .iter()
        .map(|file| (file.path.as_str(), file.action))
        .collect();

    for file in &payload.files {
        let relative = validated_relative(&file.path)?;
        let absolute = project.join(&relative);
        let action = planned
            .get(file.path.as_str())
            .copied()
            .ok_or_else(|| format!("no plan entry for {}", file.path))?;
        match action {
            FileAction::Unchanged => {
                outcome.unchanged.push(file.path.clone());
                continue;
            }
            FileAction::BackupThenOverwrite => {
                let existing = std::fs::read(&absolute)
                    .map_err(|error| format!("read {}: {error}", absolute.display()))?;
                let digest = sha256_hex(&existing);
                let backup = format!("{}{BACKUP_SUFFIX}-{}", file.path, &digest[..16]);
                let backup_absolute = project.join(&backup);
                std::fs::write(&backup_absolute, &existing)
                    .map_err(|error| format!("write {}: {error}", backup_absolute.display()))?;
                let record = BackupRecord {
                    path: file.path.clone(),
                    backup,
                    sha256: digest,
                };
                manifest.backups.push(record.clone());
                outcome.backed_up.push(record);
            }
            FileAction::Create => outcome.created.push(file.path.clone()),
            FileAction::Overwrite => outcome.overwritten.push(file.path.clone()),
            FileAction::ForeignConflict | FileAction::DriftConflict => {
                return Err(format!("unreachable conflict for {}", file.path));
            }
        }
        if let Some(parent) = absolute.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        std::fs::write(&absolute, &file.bytes)
            .map_err(|error| format!("write {}: {error}", absolute.display()))?;
        set_mode(&absolute, file.mode)?;
    }

    manifest.files = payload
        .files
        .iter()
        .map(|file| OwnedFile {
            path: file.path.clone(),
            sha256: sha256_hex(&file.bytes),
        })
        .collect();
    write_manifest(project, &manifest)?;
    Ok(outcome)
}

fn write_manifest(project: &Path, manifest: &InstallManifest) -> Result<(), String> {
    let path = manifest_path(project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let mut bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| format!("serialize install manifest: {error}"))?;
    bytes.push(b'\n');
    std::fs::write(&path, bytes).map_err(|error| format!("write {}: {error}", path.display()))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|error| format!("set mode on {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), String> {
    Ok(())
}

/// Remove every owned file whose live digest still matches the manifest.
///
/// A drifted owned file is deliberately retained, an unrecorded file is never
/// looked at, and only directories that end up empty inside the install root are
/// pruned. Removing when nothing is installed is a no-op, not an error.
///
/// # Errors
/// Returns an error if the manifest cannot be read or a file cannot be removed.
pub fn remove(project: &Path) -> Result<InstallOutcome, String> {
    let Some(manifest) = InstallManifest::load(project)? else {
        return Ok(InstallOutcome::default());
    };
    let mut outcome = InstallOutcome::default();
    for file in &manifest.files {
        let relative = validated_relative(&file.path)?;
        let absolute = project.join(&relative);
        if !absolute.exists() {
            continue;
        }
        let live = std::fs::read(&absolute)
            .map_err(|error| format!("read {}: {error}", absolute.display()))?;
        if sha256_hex(&live) == file.sha256 {
            std::fs::remove_file(&absolute)
                .map_err(|error| format!("remove {}: {error}", absolute.display()))?;
            outcome.removed.push(file.path.clone());
        } else {
            outcome.retained.push(file.path.clone());
        }
    }
    let manifest_file = manifest_path(project);
    if manifest_file.exists() {
        std::fs::remove_file(&manifest_file)
            .map_err(|error| format!("remove {}: {error}", manifest_file.display()))?;
    }
    prune_empty_directories(&project.join(INSTALL_ROOT))?;
    Ok(outcome)
}

/// Delete directories that are empty after a removal, deepest first, never
/// touching a directory that still holds a file.
fn prune_empty_directories(root: &Path) -> Result<(), String> {
    if !root.is_dir() {
        return Ok(());
    }
    let mut directories = Vec::new();
    let mut frontier = vec![root.to_path_buf()];
    while let Some(directory) = frontier.pop() {
        let entries = std::fs::read_dir(&directory)
            .map_err(|error| format!("list {}: {error}", directory.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("list {}: {error}", directory.display()))?;
            let path = entry.path();
            if std::fs::symlink_metadata(&path)
                .map_err(|error| format!("inspect {}: {error}", path.display()))?
                .is_dir()
            {
                frontier.push(path.clone());
                directories.push(path);
            }
        }
    }
    directories.push(root.to_path_buf());
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        let empty = std::fs::read_dir(&directory)
            .map_err(|error| format!("list {}: {error}", directory.display()))?
            .next()
            .is_none();
        if empty {
            std::fs::remove_dir(&directory)
                .map_err(|error| format!("remove {}: {error}", directory.display()))?;
        }
    }
    Ok(())
}
