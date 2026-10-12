//! Task-34 host-package manifest: the deterministic packaged-binary contract.
//!
//! Task 34's repair commands must never invoke an ambient-PATH `godot-mcp`
//! binary. Instead every repair command resolves
//! `target/qa-task-34/host-package/manifest.json`, which records the absolute
//! path and SHA-256 of the exact packaged `godot-mcp` executable produced by the
//! Task-34 packaging step, verifies that recorded hash against the live file,
//! and only then invokes that exact binary.
//!
//! This module is the Todo-1 **scaffold** for that contract: it defines the
//! manifest schema and a fail-closed loader/validator
//! ([`HostPackageManifest::load_and_verify`]) that proves the manifest is
//! well-formed, its `executable_path` is an absolute symlink-free regular file,
//! and the opened executable's SHA-256 matches the recorded value. The verified
//! file remains open through launch so path substitution cannot change what is
//! executed. Todo 1 deliberately does NOT implement the repair behavior itself
//! (that is Todo 34); it only provides the deterministic resolver so Task 34's
//! descriptor can be frozen against the packaged binary rather than the ambient
//! PATH.

use std::fs::File;
use std::io::{Read, Seek};
use std::path::{Component, Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::Command;
use std::process::ExitStatus;

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The frozen, workspace-relative location of the Task-34 host-package
/// manifest. Every Task-34 repair command must resolve exactly this path.
pub const HOST_PACKAGE_MANIFEST_PATH: &str = "target/qa-task-34/host-package/manifest.json";

/// A Task-34 host-package manifest describing the exact packaged `godot-mcp`
/// executable a repair command must invoke.
#[derive(Debug, Clone, Deserialize)]
pub struct HostPackageManifest {
    /// Schema version for forward compatibility.
    pub schema_version: String,
    /// Absolute path of the packaged `godot-mcp` executable.
    pub executable_path: String,
    /// SHA-256 (lowercase hex) of the packaged executable's bytes.
    pub executable_sha256: String,
    // The four archive-provenance fields below are optional only so a minimal
    // hand-written manifest stays loadable; Todo 34's packaging step always
    // writes them, and `load_and_verify` re-checks every one that is present
    // rather than trusting it.
    #[serde(default)]
    pub archive_path: Option<String>,
    #[serde(default)]
    pub archive_sha256: Option<String>,
    #[serde(default)]
    pub extracted_root: Option<String>,
    #[serde(default)]
    pub executable_relative_path: Option<String>,
}

fn is_hex_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// An executable whose exact opened file object has passed path and digest
/// verification. Keeping the file open binds later launch to these verified
/// bytes instead of reopening a mutable pathname.
#[derive(Debug)]
pub struct VerifiedExecutable {
    #[cfg_attr(
        not(target_os = "linux"),
        allow(
            dead_code,
            reason = "held open so the verified bytes stay bound; only Linux launches through it"
        )
    )]
    file: File,
    display_path: PathBuf,
}

impl VerifiedExecutable {
    /// Launch the verified open executable with `args`.
    ///
    /// Linux supplies a race-free handle path through `/proc/self/fd`. Other
    /// platforms fail closed until they have an equivalent handle-based spawn
    /// implementation; reopening `display_path` would reintroduce the path
    /// substitution race this type exists to prevent.
    pub fn status<I, S>(&self, args: I) -> std::io::Result<ExitStatus>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;

            let handle_path = format!("/proc/self/fd/{}", self.file.as_raw_fd());
            Command::new(handle_path).args(args).status()
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = args;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!(
                    "handle-based packaged executable launch is unsupported on this platform: {}",
                    self.display_path.display()
                ),
            ))
        }
    }

    /// Human-readable manifest path used only for diagnostics, never launch.
    #[must_use]
    pub fn display_path(&self) -> &Path {
        &self.display_path
    }
}

fn reject_symlink_or_non_normal_components(path: &Path) -> Result<(), String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                current.push(component.as_os_str());
            }
            Component::CurDir | Component::ParentDir => {
                return Err(format!(
                    "host-package executable_path contains a non-normal component: {}",
                    path.display()
                ));
            }
        }
        let metadata = std::fs::symlink_metadata(&current).map_err(|e| {
            format!(
                "inspect host-package executable path component {}: {e}",
                current.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "host-package executable_path contains a symlink component: {}",
                current.display()
            ));
        }
    }
    Ok(())
}

fn sha256_hex_open_file(file: &mut File) -> Result<String, String> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|e| format!("read opened packaged executable: {e}"))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    file.rewind()
        .map_err(|e| format!("rewind opened packaged executable: {e}"))?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

impl HostPackageManifest {
    /// Load, schema-validate, and hash-verify the manifest at `path`, returning
    /// an open handle to the verified packaged executable ready to invoke.
    ///
    /// Fail-closed order: parse -> schema (non-empty version, absolute
    /// executable path, hex SHA-256) -> every path component is non-symlink ->
    /// open the executable once -> prove the opened object is a regular file and
    /// its SHA-256 equals the recorded value. Any failure returns an error and no
    /// spawn must occur.
    ///
    /// # Errors
    /// Returns a descriptive error if the manifest is missing/malformed, the
    /// executable path is not an absolute regular file, or the live hash does
    /// not match the recorded value.
    pub fn load_and_verify(path: &Path) -> Result<VerifiedExecutable, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("read host-package manifest {}: {e}", path.display()))?;
        let manifest: Self = serde_json::from_str(&text)
            .map_err(|e| format!("parse host-package manifest {}: {e}", path.display()))?;

        if manifest.schema_version.trim().is_empty() {
            return Err("host-package manifest schema_version is empty".to_string());
        }
        if manifest.executable_path.trim().is_empty() {
            return Err("host-package manifest executable_path is empty".to_string());
        }
        let exe = PathBuf::from(&manifest.executable_path);
        if !exe.is_absolute() {
            return Err(format!(
                "host-package manifest executable_path is not absolute: {}",
                manifest.executable_path
            ));
        }
        if !is_hex_sha256(&manifest.executable_sha256) {
            return Err(
                "host-package manifest executable_sha256 is not a lowercase hex SHA-256"
                    .to_string(),
            );
        }
        reject_symlink_or_non_normal_components(&exe)?;
        let mut file = File::open(&exe)
            .map_err(|e| format!("open packaged executable {}: {e}", exe.display()))?;
        let metadata = file
            .metadata()
            .map_err(|e| format!("inspect opened packaged executable {}: {e}", exe.display()))?;
        if !metadata.is_file() {
            return Err(format!(
                "host-package manifest executable_path is not a regular file: {}",
                manifest.executable_path
            ));
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;

            let opened_path = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
                .map_err(|e| format!("resolve opened packaged executable handle: {e}"))?;
            if opened_path != exe {
                return Err(format!(
                    "opened packaged executable object does not match manifest path: {} resolved to {}",
                    exe.display(),
                    opened_path.display()
                ));
            }
        }
        let live = sha256_hex_open_file(&mut file)?;
        if live != manifest.executable_sha256 {
            return Err(format!(
                "packaged executable live SHA-256 {live} does not match recorded {}",
                manifest.executable_sha256
            ));
        }
        manifest.verify_archive_provenance(&exe)?;
        Ok(VerifiedExecutable {
            file,
            display_path: exe,
        })
    }

    /// Re-check every archive-provenance field the manifest actually carries.
    ///
    /// # Errors
    /// Returns an error if the recorded relative path does not join the recorded
    /// extraction root to the verified executable, or if the recorded archive is
    /// missing or its digest does not match.
    fn verify_archive_provenance(&self, verified: &Path) -> Result<(), String> {
        if let (Some(root), Some(relative)) = (&self.extracted_root, &self.executable_relative_path)
        {
            let joined = PathBuf::from(root).join(relative);
            if joined != verified {
                return Err(format!(
                    "host-package extracted_root/executable_relative_path resolves to {} but the verified executable is {}",
                    joined.display(),
                    verified.display()
                ));
            }
        }
        if let Some(archive) = &self.archive_path {
            let path = PathBuf::from(archive);
            let bytes = std::fs::read(&path)
                .map_err(|e| format!("read recorded archive {}: {e}", path.display()))?;
            if let Some(expected) = &self.archive_sha256 {
                if !is_hex_sha256(expected) {
                    return Err(
                        "host-package manifest archive_sha256 is not a lowercase hex SHA-256"
                            .to_string(),
                    );
                }
                let mut hasher = Sha256::new();
                hasher.update(&bytes);
                let live: String = hasher
                    .finalize()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                if &live != expected {
                    return Err(format!(
                        "recorded archive live SHA-256 {live} does not match recorded {expected}"
                    ));
                }
            }
        }
        Ok(())
    }
}
