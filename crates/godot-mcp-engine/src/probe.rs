//! Probing a resolved Godot executable and negotiating its capabilities.
//!
//! A probe answers three questions about one candidate executable, in this
//! order, and refuses to guess any of them:
//!
//! 1. **What bytes is this?** The file is opened handle-relative through
//!    [`godot_mcp_domain::sysfs`] with symlink components forbidden and proven to
//!    be a regular file, then digested. The digest — not the path, not the
//!    filename — is the executable's identity.
//! 2. **What version is this?** `--version` is executed under a deadline and its
//!    output parsed into an [`ExactVersion`]. An unparsable line is never
//!    tolerated, and a version below the matrix floor is a clear `unsupported`.
//! 3. **What can this build actually do?** `--help` is executed and the option
//!    column of its output parsed into the set of advertised command-line
//!    capabilities. The build is classified **from that set** against the
//!    capability requirements the pinned release declares — never from the
//!    filename, never from a version string, and never from the exit status of a
//!    probe invocation.
//!
//! The third point is load-bearing. A Godot export-template build accepts
//! `--editor` with a success status and silently ignores it, so an exit code
//! proves nothing about a capability. Only advertisement does.
//!
//! Snapshots are cached by [`SnapshotKey`] — the binary's own content digest
//! plus its probed version — so replacing the executable's bytes in place always
//! invalidates its snapshot: the stale entry is keyed by bytes that no longer
//! exist and can never be served.
//!
//! Nothing here downloads, installs or mutates a Godot build.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::sysfs::{self, SysError};
use sha2::{Digest, Sha256};

use crate::releases::{ExactVersion, PinnedRelease, ReleaseLock};

/// Render a 32-byte digest as lowercase hex.
#[must_use]
pub fn hex_digest(digest: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(char::from_digit(u32::from(byte >> 4), 16).expect("nibble is hex"));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("nibble is hex"));
    }
    out
}

/// One probe invocation's inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeRequest {
    /// Executable to probe. Never followed through a symlink.
    pub binary: PathBuf,
    /// Wall-clock budget for a single probe invocation.
    pub deadline: Duration,
}

/// What a probed build is, established from advertised capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuildKind {
    /// An editor build: advertises the editor-only command surface.
    Editor,
    /// An export-template build: advertises only the runtime command surface.
    ExportTemplate,
}

impl BuildKind {
    /// Stable wire name.
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Editor => "editor",
            Self::ExportTemplate => "export_template",
        }
    }
}

/// Cache key of a capability snapshot: the binary's content digest plus the
/// version that content reported.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapshotKey {
    /// Lowercase-hex SHA-256 of the executable's bytes.
    pub binary_sha256: String,
    /// Exact version the executable reported.
    pub version: ExactVersion,
}

/// What one probed executable can do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilitySnapshot {
    /// Content+version cache key.
    pub key: SnapshotKey,
    /// Display path the snapshot was taken through. Never an authority.
    pub binary: PathBuf,
    /// Exact probed version.
    pub version: ExactVersion,
    /// Raw `--version` line, retained verbatim for provenance.
    pub version_line: String,
    /// Classification derived from `capabilities`.
    pub build_kind: BuildKind,
    /// Command-line flags this build advertises in `--help`.
    pub capabilities: BTreeSet<String>,
    /// Whether the probed version is an exact entry of the pinned matrix.
    pub pinned: bool,
}

impl CapabilitySnapshot {
    /// Whether this build advertises `flag`.
    #[must_use]
    pub fn advertises(&self, flag: &str) -> bool {
        self.capabilities.contains(flag)
    }
}

/// Whether a snapshot was newly probed or served from cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheOutcome {
    /// The binary was probed during this call.
    Fresh,
    /// A cached snapshot for these exact bytes was reused.
    Hit,
}

/// Why a probe failed.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// The candidate is not a regular file.
    #[error("`{path}` is not a regular file and cannot be probed")]
    NotRegularFile {
        /// Rejected display path.
        path: String,
    },
    /// A component of the candidate path is a symlink or unsupported reparse
    /// point. Never followed.
    #[error("`{path}` is reached through a symlink, which a probe never follows")]
    SymlinkRejected {
        /// Rejected display path.
        path: String,
    },
    /// The candidate could not be read or executed.
    #[error("probing `{path}` failed: {reason}")]
    Unreachable {
        /// Rejected display path.
        path: String,
        /// Sanitised failure reason.
        reason: String,
    },
    /// A probe invocation exceeded its deadline.
    #[error("`{path} {argument}` did not finish within {millis} ms")]
    ProbeTimeout {
        /// Probed display path.
        path: String,
        /// The probe argument that hung.
        argument: String,
        /// The deadline that elapsed.
        millis: u128,
    },
    /// `--version` printed something that is not a Godot version.
    #[error("`{path} --version` printed `{found}`, which is not a Godot version string")]
    UnparsableVersion {
        /// Probed display path.
        path: String,
        /// The rejected output, truncated for display.
        found: String,
    },
    /// The build reported a version outside the supported matrix.
    #[error(
        "Godot {found} is not supported: the minimum supported version is {floor} (pinned matrix: {supported})"
    )]
    UnsupportedVersion {
        /// The version the build reported.
        found: String,
        /// The matrix floor.
        floor: String,
        /// The pinned matrix, for orientation.
        supported: String,
    },
    /// `--help` was rejected, so capabilities cannot be negotiated.
    #[error("`{path} --help` exited with status {status}: capabilities cannot be negotiated")]
    HelpRejected {
        /// Probed display path.
        path: String,
        /// Observed exit status.
        status: String,
    },
    /// The build does not advertise a capability the caller requires.
    #[error(
        "the build at `{path}` is {observed} and does not advertise the required `{capability}` capability"
    )]
    MissingCapability {
        /// Probed display path.
        path: String,
        /// What the build actually is.
        observed: String,
        /// The capability that is missing.
        capability: String,
    },
}

impl PartialEq for ProbeError {
    fn eq(&self, other: &Self) -> bool {
        self.to_string() == other.to_string()
    }
}

impl Eq for ProbeError {}

impl ProbeError {
    /// Stable error classification.
    #[must_use]
    pub fn error_kind(&self) -> ErrorKind {
        match self {
            Self::NotRegularFile { .. } => ErrorKind::InvalidInput,
            Self::SymlinkRejected { .. } => ErrorKind::PermissionDenied,
            Self::Unreachable { .. } => ErrorKind::Unavailable,
            Self::ProbeTimeout { .. } => ErrorKind::Timeout,
            Self::UnparsableVersion { .. }
            | Self::UnsupportedVersion { .. }
            | Self::HelpRejected { .. }
            | Self::MissingCapability { .. } => ErrorKind::Unsupported,
        }
    }

    /// Operator-facing remediation. Never offers to fetch a Godot build.
    #[must_use]
    pub fn remediation(&self) -> &'static str {
        match self {
            Self::NotRegularFile { .. } => {
                "Configure godot_binary with the path of the Godot executable file itself, not a \
                 directory or an application bundle."
            }
            Self::SymlinkRejected { .. } => {
                "Configure godot_binary with the real path of the executable; a probe never \
                 follows a symlinked final component."
            }
            Self::Unreachable { .. } => {
                "Check that the configured Godot executable is readable and executable by this \
                 user, then retry."
            }
            Self::ProbeTimeout { .. } => {
                "The configured binary did not answer a probe in time. Verify it is a real Godot \
                 build and not a wrapper that waits for input, then retry."
            }
            Self::UnparsableVersion { .. } => {
                "Configure godot_binary with a real Godot build: a probe never trusts a filename \
                 or a version claim it cannot parse."
            }
            Self::UnsupportedVersion { .. } => {
                "Install one of the exact Godot versions pinned by \
                 tests/fixtures/godot-releases.lock.json and configure godot_binary to point at \
                 it. godot-mcp never fetches an engine build on your behalf."
            }
            Self::HelpRejected { .. } => {
                "Configure godot_binary with a Godot build whose --help lists its command-line \
                 options; capabilities are negotiated from that list."
            }
            Self::MissingCapability { .. } => {
                "Configure godot_binary with a Godot editor build. An export-template (runtime) \
                 build silently ignores the editor flags and can never satisfy an editor \
                 requirement."
            }
        }
    }
}

fn map_sys_error(path: &Path, error: &SysError) -> ProbeError {
    let display = path.display().to_string();
    match error {
        SysError::SymlinkComponent => ProbeError::SymlinkRejected { path: display },
        SysError::NotRegularFile => ProbeError::NotRegularFile { path: display },
        other => ProbeError::Unreachable {
            path: display,
            reason: other.to_string(),
        },
    }
}

/// Digest an executable's bytes through a handle-safe read.
///
/// The final component is opened nofollow and proven to be a regular file before
/// a single byte is read, so a symlinked or non-regular candidate is refused
/// rather than followed.
///
/// # Errors
/// Returns [`ProbeError::SymlinkRejected`] for a symlinked component,
/// [`ProbeError::NotRegularFile`] for a directory or other non-regular object,
/// or [`ProbeError::Unreachable`] on an I/O failure.
pub fn executable_digest(binary: &Path) -> Result<[u8; 32], ProbeError> {
    let absolute = if binary.is_absolute() {
        binary.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| ProbeError::Unreachable {
                path: binary.display().to_string(),
                reason: error.to_string(),
            })?
            .join(binary)
    };
    let parent = absolute
        .parent()
        .ok_or_else(|| ProbeError::NotRegularFile {
            path: absolute.display().to_string(),
        })?;
    let name = absolute
        .file_name()
        .ok_or_else(|| ProbeError::NotRegularFile {
            path: absolute.display().to_string(),
        })?;
    let walked =
        sysfs::walk_dir_no_symlinks(parent).map_err(|error| map_sys_error(&absolute, &error))?;
    let bytes = sysfs::read_child_file(walked.dir(), name)
        .map_err(|error| map_sys_error(&absolute, &error))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hasher.finalize().into())
}

/// Extract the flags a Godot `--help` screen advertises.
///
/// Only the *option column* of an option line is scanned, so a flag mentioned
/// inside a description (`--display-driver` inside the `--headless` blurb) is
/// never mistaken for an advertised capability.
#[must_use]
pub fn parse_advertised_capabilities(help: &str) -> BTreeSet<String> {
    let help = strip_ansi_csi(help);
    let mut flags: BTreeSet<String> = BTreeSet::new();
    for line in help.lines() {
        let trimmed = line.trim_start();
        if trimmed.len() == line.len() || !trimmed.starts_with('-') {
            // Option lines are always indented; anything else is prose.
            continue;
        }
        let spec = match trimmed.find("  ") {
            Some(end) => &trimmed[..end],
            None => trimmed,
        };
        for token in spec.split_whitespace() {
            let candidate = token.trim_end_matches(',');
            if candidate.starts_with("--") && candidate.len() > 2 {
                flags.insert(candidate.to_string());
            }
        }
    }
    flags
}

fn strip_ansi_csi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' || chars.next_if_eq(&'[').is_none() {
            output.push(character);
            continue;
        }
        for sequence_character in chars.by_ref() {
            if ('@'..='~').contains(&sequence_character) {
                break;
            }
        }
    }
    output
}

/// Parse an exact version out of a Godot `--version` line.
///
/// Godot prints `<major>.<minor>.<patch>.<status>.<build>...`; only a fully
/// specified numeric triple is accepted.
fn parse_version_line(line: &str) -> Option<ExactVersion> {
    let parts: Vec<&str> = line.trim().split('.').collect();
    if parts.len() < 3 {
        return None;
    }
    ExactVersion::parse(&format!("{}.{}.{}", parts[0], parts[1], parts[2])).ok()
}

/// A probe cache bound to one pinned release matrix.
///
/// The matrix is the sole compatibility source: the floor a probed version must
/// clear and the capability requirements a build is classified against both come
/// from it.
#[derive(Debug)]
pub struct ProbeCache {
    lock: ReleaseLock,
    by_key: BTreeMap<SnapshotKey, CapabilitySnapshot>,
    digest_by_path: BTreeMap<PathBuf, String>,
    spawns: usize,
}

impl ProbeCache {
    /// Build a cache over a validated release matrix.
    #[must_use]
    pub fn new(lock: ReleaseLock) -> Self {
        Self {
            lock,
            by_key: BTreeMap::new(),
            digest_by_path: BTreeMap::new(),
            spawns: 0,
        }
    }

    /// The pinned matrix this cache negotiates against.
    #[must_use]
    pub fn lock(&self) -> &ReleaseLock {
        &self.lock
    }

    /// How many child processes this cache has spawned. A cache hit never
    /// increments it.
    #[must_use]
    pub fn spawn_count(&self) -> usize {
        self.spawns
    }

    /// How many snapshots are currently retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    /// Whether the cache holds no snapshot.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    /// Probe `request`, reusing a cached snapshot when the binary's bytes are
    /// unchanged.
    ///
    /// # Errors
    /// Returns [`ProbeError`] when the candidate cannot be digested, does not
    /// answer a probe in time, reports an unparsable or unsupported version,
    /// rejects `--help`, or advertises no recognisable capability set.
    pub async fn snapshot(
        &mut self,
        request: &ProbeRequest,
    ) -> Result<(CapabilitySnapshot, CacheOutcome), ProbeError> {
        let digest = hex_digest(&executable_digest(&request.binary)?);

        // The key is derived from content, so changed bytes can never be served
        // from the stale entry. Evict it eagerly so the cache cannot grow one
        // dead entry per rebuild of the same path.
        if let Some(previous) = self.digest_by_path.get(&request.binary)
            && previous != &digest
        {
            let stale = previous.clone();
            self.by_key.retain(|key, _| key.binary_sha256 != stale);
        }

        if let Some(hit) = self
            .by_key
            .values()
            .find(|snapshot| snapshot.key.binary_sha256 == digest)
        {
            return Ok((hit.clone(), CacheOutcome::Hit));
        }

        let snapshot = self.probe_with_digest(request, digest.clone()).await?;
        self.digest_by_path
            .insert(request.binary.clone(), digest.clone());
        self.by_key.insert(snapshot.key.clone(), snapshot.clone());
        Ok((snapshot, CacheOutcome::Fresh))
    }

    /// Probe `request` unconditionally, bypassing and not populating the cache.
    ///
    /// # Errors
    /// Returns the same errors as [`ProbeCache::snapshot`].
    pub async fn probe_uncached(
        &mut self,
        request: &ProbeRequest,
    ) -> Result<CapabilitySnapshot, ProbeError> {
        let digest = hex_digest(&executable_digest(&request.binary)?);
        self.probe_with_digest(request, digest).await
    }

    /// Probe `request` and require an editor build.
    ///
    /// # Errors
    /// Returns [`ProbeError::MissingCapability`] with [`ErrorKind::Unsupported`]
    /// when the build is not an editor, plus the same errors as
    /// [`ProbeCache::snapshot`].
    pub async fn require_editor(
        &mut self,
        request: &ProbeRequest,
    ) -> Result<CapabilitySnapshot, ProbeError> {
        let (snapshot, _) = self.snapshot(request).await?;
        if snapshot.build_kind == BuildKind::Editor {
            return Ok(snapshot);
        }
        let requirements = self.requirements_for(snapshot.version);
        let missing = requirements
            .and_then(|release| {
                release
                    .required_editor_capabilities
                    .iter()
                    .find(|capability| !snapshot.advertises(capability))
                    .cloned()
            })
            .unwrap_or_else(|| "--editor".to_string());
        Err(ProbeError::MissingCapability {
            path: request.binary.display().to_string(),
            observed: snapshot.build_kind.wire_name().to_string(),
            capability: missing,
        })
    }

    /// The pinned release whose capability requirements apply to `version`.
    ///
    /// An exactly pinned version uses its own entry. A version above the floor
    /// that the matrix does not pin exactly is negotiated against the newest
    /// pinned entry, so capability requirements never come from nowhere.
    fn requirements_for(&self, version: ExactVersion) -> Option<&PinnedRelease> {
        self.lock
            .releases()
            .iter()
            .find(|release| release.version == version)
            .or_else(|| self.lock.releases().last())
    }

    async fn probe_with_digest(
        &mut self,
        request: &ProbeRequest,
        digest: String,
    ) -> Result<CapabilitySnapshot, ProbeError> {
        let version_output = self.invoke(request, "--version").await?;
        let version_line = String::from_utf8_lossy(&version_output.stdout)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or_default()
            .to_string();
        let version = parse_version_line(&version_line).ok_or_else(|| {
            let mut found = version_line.clone();
            found.truncate(120);
            ProbeError::UnparsableVersion {
                path: request.binary.display().to_string(),
                found,
            }
        })?;

        let floor = self.lock.minimum_supported_version();
        if !version.at_least(floor) {
            return Err(ProbeError::UnsupportedVersion {
                found: version.to_string(),
                floor: floor.to_string(),
                supported: self.lock.supported_versions_display(),
            });
        }

        let help_output = self.invoke(request, "--help").await?;
        if !help_output.status.success() {
            return Err(ProbeError::HelpRejected {
                path: request.binary.display().to_string(),
                status: help_output.status.to_string(),
            });
        }
        let capabilities =
            parse_advertised_capabilities(&String::from_utf8_lossy(&help_output.stdout));

        let requirements =
            self.requirements_for(version)
                .ok_or_else(|| ProbeError::UnsupportedVersion {
                    found: version.to_string(),
                    floor: floor.to_string(),
                    supported: self.lock.supported_versions_display(),
                })?;
        let missing_editor = requirements
            .required_editor_capabilities
            .iter()
            .find(|capability| !capabilities.contains(capability.as_str()))
            .cloned();
        let missing_template = requirements
            .required_template_capabilities
            .iter()
            .find(|capability| !capabilities.contains(capability.as_str()))
            .cloned();
        let build_kind = match (&missing_editor, &missing_template) {
            (None, _) => BuildKind::Editor,
            (Some(_), None) => BuildKind::ExportTemplate,
            (Some(missing), Some(_)) => {
                return Err(ProbeError::MissingCapability {
                    path: request.binary.display().to_string(),
                    observed: "an unrecognised build".to_string(),
                    capability: missing.clone(),
                });
            }
        };

        Ok(CapabilitySnapshot {
            key: SnapshotKey {
                binary_sha256: digest,
                version,
            },
            binary: request.binary.clone(),
            version,
            version_line,
            build_kind,
            capabilities,
            pinned: self.lock.supports(&version),
        })
    }

    /// Run one probe invocation under the request deadline.
    async fn invoke(
        &mut self,
        request: &ProbeRequest,
        argument: &str,
    ) -> Result<std::process::Output, ProbeError> {
        let mut command = tokio::process::Command::new(&request.binary);
        command
            .arg(argument)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let child = command.spawn().map_err(|error| ProbeError::Unreachable {
            path: request.binary.display().to_string(),
            reason: error.to_string(),
        })?;
        self.spawns += 1;
        match tokio::time::timeout(request.deadline, child.wait_with_output()).await {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(error)) => Err(ProbeError::Unreachable {
                path: request.binary.display().to_string(),
                reason: error.to_string(),
            }),
            // `kill_on_drop` reaps the child when the timed-out future is
            // dropped, so a hung probe never leaks a process.
            Err(_) => Err(ProbeError::ProbeTimeout {
                path: request.binary.display().to_string(),
                argument: argument.to_string(),
                millis: request.deadline.as_millis(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_digest_is_lowercase_and_64_wide() {
        let digest = [0xab_u8; 32];
        let hex = hex_digest(&digest);
        assert_eq!(hex.len(), 64);
        assert_eq!(hex, "ab".repeat(32));
    }

    #[test]
    fn only_the_option_column_advertises_a_capability() {
        let help = "Godot Engine - https://godotengine.org\n\
                    \n\
                    General options:\n\
                    \x20 -h, --help                       Display this help message.\n\
                    \x20 --headless                       Enable headless mode (--display-driver \
                    headless --audio-driver Dummy).\n\
                    \x20 --export-release <preset> <path>  Export the project.\n";
        let flags = parse_advertised_capabilities(help);
        assert!(flags.contains("--help"));
        assert!(flags.contains("--headless"));
        assert!(flags.contains("--export-release"));
        assert!(
            !flags.contains("--display-driver"),
            "a flag named inside a description is not advertised: {flags:?}"
        );
        assert!(!flags.contains("--audio-driver"));
    }

    #[test]
    fn ansi_colours_do_not_hide_advertised_capabilities() {
        let help = "\x1b[1;93mGeneral options:\x1b[0m\n  \x1b[92m-h, --help  \x1b[1;92mR\x1b[0m  Display this help message.\n  \x1b[92m--headless  \x1b[1;92mR\x1b[0m  Enable headless mode.\n";
        let flags = parse_advertised_capabilities(help);

        assert!(flags.contains("--help"), "{flags:?}");
        assert!(flags.contains("--headless"), "{flags:?}");
    }

    #[test]
    fn version_lines_parse_only_as_exact_triples() {
        assert_eq!(
            parse_version_line("4.7.1.stable.official.b09f793f5"),
            Some(ExactVersion::new(4, 7, 1))
        );
        assert_eq!(
            parse_version_line("4.4.0.beta1"),
            Some(ExactVersion::new(4, 4, 0))
        );
        assert_eq!(parse_version_line("this build has no version"), None);
        assert_eq!(parse_version_line("4.7"), None);
        assert_eq!(parse_version_line(""), None);
    }

    #[test]
    fn a_directory_is_never_probed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = executable_digest(dir.path()).expect_err("a directory is not an executable");
        assert!(
            matches!(error, ProbeError::NotRegularFile { .. }),
            "{error:?}"
        );
        assert_eq!(error.error_kind(), ErrorKind::InvalidInput);
    }
}
