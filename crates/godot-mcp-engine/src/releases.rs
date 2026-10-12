//! The pinned Godot release matrix: the sole compatibility source.
//!
//! Every compatibility fact about Godot — which patch versions are supported,
//! which platform artifact belongs to which version, and the official SHA-256 of
//! each artifact — comes from the committed
//! `tests/fixtures/godot-releases.lock.json` file and from nowhere else. No
//! filename, no `--version` string and no operator claim is trusted in its
//! place.
//!
//! Three properties are enforced structurally rather than by convention:
//!
//! * Every entry is an **exact** patch version. A `major.minor` alias such as
//!   `4.7` is rejected at parse time ([`ReleaseLockError::AliasVersion`]), so a
//!   shortened entry can never silently widen the matrix.
//! * Every required platform artifact appears **exactly once** per release, with
//!   a unique download URL and a 64-hex lowercase digest.
//! * A download is never promoted into something executable until its bytes hash
//!   to the pinned digest ([`ReleaseLock::plan_execution`]).
//!
//! This module performs no I/O of its own: it parses bytes the caller supplies
//! and verifies bytes the caller supplies. Nothing here downloads anything.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use godot_mcp_domain::ErrorKind;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// An exact `major.minor.patch` Godot version.
///
/// Constructed only from a fully specified triple: a `major.minor` alias, a
/// channel suffix (`4.4.1-stable`) or a `v` prefix are all refused, because a
/// lock entry that is not exact cannot pin anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExactVersion {
    /// Major component.
    pub major: u16,
    /// Minor component.
    pub minor: u16,
    /// Patch component.
    pub patch: u16,
}

/// Why a version string is not an exact patch version.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VersionParseError {
    /// The string had fewer or more than three dot-separated components.
    #[error("`{found}` is not an exact major.minor.patch version")]
    NotExact {
        /// The rejected spelling.
        found: String,
    },
    /// A component was empty, non-numeric, or carried a suffix/prefix.
    #[error("`{found}` contains a non-numeric version component")]
    NonNumeric {
        /// The rejected spelling.
        found: String,
    },
}

impl ExactVersion {
    /// Build an exact version from its three components.
    #[must_use]
    pub const fn new(major: u16, minor: u16, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parse an exact `major.minor.patch` version.
    ///
    /// # Errors
    /// Returns [`VersionParseError`] for an alias (`4.4`), an empty string, a
    /// channel-suffixed spelling (`4.4.1-stable`), a `v` prefix or any
    /// non-numeric component.
    pub fn parse(text: &str) -> Result<Self, VersionParseError> {
        let parts: Vec<&str> = text.split('.').collect();
        if parts.len() != 3 {
            return Err(VersionParseError::NotExact {
                found: text.to_string(),
            });
        }
        let mut numbers = [0_u16; 3];
        for (slot, part) in numbers.iter_mut().zip(parts) {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(VersionParseError::NonNumeric {
                    found: text.to_string(),
                });
            }
            *slot = part.parse().map_err(|_| VersionParseError::NonNumeric {
                found: text.to_string(),
            })?;
        }
        Ok(Self::new(numbers[0], numbers[1], numbers[2]))
    }

    /// Whether this version is at least `floor`.
    #[must_use]
    pub fn at_least(self, floor: Self) -> bool {
        self >= floor
    }
}

impl fmt::Display for ExactVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A platform an artifact is pinned for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
pub enum Platform {
    /// 64-bit Linux editor build.
    #[serde(rename = "linux-x86_64")]
    LinuxX86_64,
    /// Universal macOS editor build.
    #[serde(rename = "macos-universal")]
    MacOsUniversal,
    /// 64-bit Windows editor build.
    #[serde(rename = "windows-x86_64")]
    WindowsX86_64,
    /// Platform-independent export-template archive.
    #[serde(rename = "all-platforms")]
    AllPlatforms,
}

impl Platform {
    /// Stable wire name used by the lock file.
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "linux-x86_64",
            Self::MacOsUniversal => "macos-universal",
            Self::WindowsX86_64 => "windows-x86_64",
            Self::AllPlatforms => "all-platforms",
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_name())
    }
}

/// What an artifact contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// An editor build.
    Editor,
    /// An export-template archive.
    ExportTemplates,
}

/// How the recorded SHA-256 was obtained from official material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sha256Provenance {
    /// Published directly by the official GitHub release as the asset digest.
    GithubReleaseAssetDigest,
    /// Derived from bytes first verified against the release's official
    /// `SHA512-SUMS.txt` entry (used for releases published before per-asset
    /// digests existed).
    Sha512SumsTxtChain,
}

/// One pinned platform artifact of one exact release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedArtifact {
    /// Platform this artifact targets.
    pub platform: Platform,
    /// What the artifact contains.
    pub kind: ArtifactKind,
    /// Official artifact filename.
    pub name: String,
    /// Official download URL.
    pub url: String,
    /// Official artifact size in bytes.
    pub size_bytes: u64,
    /// Official lowercase-hex SHA-256 of the artifact bytes.
    pub sha256: String,
    /// Official lowercase-hex SHA-512 of the artifact bytes.
    pub sha512: String,
    /// How `sha256` was obtained.
    pub sha256_provenance: Sha256Provenance,
}

/// One exact pinned release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedRelease {
    /// Exact version.
    pub version: ExactVersion,
    /// Release channel (always `stable` for a pinned entry).
    pub channel: String,
    /// Upstream git tag.
    pub git_tag: String,
    /// Publication timestamp as recorded upstream.
    pub published_at: String,
    /// Official release page.
    pub release_page_url: String,
    /// Command-line capabilities an editor build of this release must advertise.
    pub required_editor_capabilities: Vec<String>,
    /// Command-line capabilities an export-template build must advertise.
    pub required_template_capabilities: Vec<String>,
    /// Pinned platform artifacts.
    pub artifacts: Vec<PinnedArtifact>,
}

impl PinnedRelease {
    /// The pinned artifact for `platform`, if this release pins one.
    #[must_use]
    pub fn artifact(&self, platform: Platform) -> Option<&PinnedArtifact> {
        self.artifacts
            .iter()
            .find(|artifact| artifact.platform == platform)
    }
}

/// A verified, executable-ready matrix download.
///
/// Only [`ReleaseLock::plan_execution`] mints one, and only after the supplied
/// bytes hash to the pinned digest, so a corrupted download can never reach an
/// execution path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedDownload {
    /// The version whose artifact was verified.
    pub version: ExactVersion,
    /// The platform whose artifact was verified.
    pub platform: Platform,
    /// The pinned (and now observed) digest.
    pub sha256: String,
    /// Observed byte length.
    pub size_bytes: u64,
}

/// Why the release lock rejected an input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReleaseLockError {
    /// The lock file is not parsable JSON of the expected shape.
    #[error("the release lock file is malformed: {reason}")]
    Malformed {
        /// Sanitised parse reason.
        reason: String,
    },
    /// The lock file declares an unexpected schema version.
    #[error("unsupported release lock schema version `{found}` (expected `{expected}`)")]
    UnsupportedSchema {
        /// Declared version.
        found: String,
        /// Version this build understands.
        expected: String,
    },
    /// An entry used a `major.minor` alias instead of an exact patch version.
    #[error(
        "`{found}` is a major.minor alias, not an exact release: every lock entry must pin an exact major.minor.patch version"
    )]
    AliasVersion {
        /// The rejected spelling.
        found: String,
    },
    /// The same exact version appears more than once.
    #[error("release `{version}` appears more than once in the lock file")]
    DuplicateVersion {
        /// The duplicated version.
        version: String,
    },
    /// Two artifacts share a download URL.
    #[error("download URL `{url}` is pinned by more than one lock entry")]
    DuplicateArtifactUrl {
        /// The duplicated URL.
        url: String,
    },
    /// The same platform appears twice inside one release.
    #[error("release `{version}` pins platform `{platform}` more than once")]
    DuplicatePlatform {
        /// Owning release.
        version: String,
        /// The duplicated platform.
        platform: String,
    },
    /// A release is missing a required platform artifact.
    #[error("release `{version}` is missing the required `{platform}` artifact")]
    MissingPlatform {
        /// Owning release.
        version: String,
        /// The missing platform.
        platform: String,
    },
    /// A digest is not exactly 64 lowercase hex characters (or 128 for SHA-512).
    #[error("artifact `{artifact}` has a malformed {algorithm} digest")]
    MalformedDigest {
        /// Owning artifact name.
        artifact: String,
        /// Which digest was malformed.
        algorithm: String,
    },
    /// An artifact URL does not belong to the release that pins it.
    #[error("artifact `{artifact}` URL does not name its own release tag `{git_tag}`")]
    ForeignArtifactUrl {
        /// Owning artifact name.
        artifact: String,
        /// The release's own tag.
        git_tag: String,
    },
    /// The requested version is not pinned by the matrix.
    #[error("Godot `{version}` is not a pinned release: the supported matrix is {supported}")]
    UnknownVersion {
        /// The requested spelling.
        version: String,
        /// The supported versions.
        supported: String,
    },
    /// The requested platform is not pinned for that version.
    #[error("release `{version}` pins no `{platform}` artifact")]
    UnknownPlatform {
        /// Owning release.
        version: String,
        /// The requested platform.
        platform: String,
    },
    /// The supplied bytes do not hash to the pinned digest.
    #[error(
        "the downloaded `{artifact}` bytes do not match the pinned digest (expected {expected}, computed {actual}); refusing to execute"
    )]
    CorruptDownload {
        /// Owning artifact name.
        artifact: String,
        /// Pinned digest.
        expected: String,
        /// Digest of the supplied bytes.
        actual: String,
    },
}

impl ReleaseLockError {
    /// Stable error classification.
    #[must_use]
    pub fn error_kind(&self) -> ErrorKind {
        match self {
            Self::Malformed { .. }
            | Self::UnsupportedSchema { .. }
            | Self::AliasVersion { .. }
            | Self::DuplicateVersion { .. }
            | Self::DuplicateArtifactUrl { .. }
            | Self::DuplicatePlatform { .. }
            | Self::MissingPlatform { .. }
            | Self::MalformedDigest { .. }
            | Self::ForeignArtifactUrl { .. } => ErrorKind::InvalidInput,
            Self::UnknownVersion { .. } | Self::UnknownPlatform { .. } => ErrorKind::Unsupported,
            Self::CorruptDownload { .. } => ErrorKind::Conflict,
        }
    }

    /// Operator-facing remediation. Never offers to download anything.
    #[must_use]
    pub fn remediation(&self) -> &'static str {
        match self {
            Self::CorruptDownload { .. } => {
                "Discard the downloaded archive and obtain the artifact again from the official \
                 release page recorded in the lock file, then re-verify its SHA-256 before use."
            }
            Self::UnknownVersion { .. } => {
                "Install one of the exact Godot versions pinned by \
                 tests/fixtures/godot-releases.lock.json and configure godot_binary to point at it."
            }
            Self::UnknownPlatform { .. } => {
                "Use a platform pinned by tests/fixtures/godot-releases.lock.json for this release."
            }
            _ => {
                "Repair tests/fixtures/godot-releases.lock.json so every entry pins an exact \
                 major.minor.patch version with one uniquely-named, uniquely-addressed artifact \
                 per required platform and a full official SHA-256."
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Wire shapes (deny-unknown-fields so lock drift fails closed)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireLock {
    schema_version: String,
    #[allow(dead_code)]
    description: String,
    minimum_supported_version: String,
    required_platforms: Vec<Platform>,
    #[allow(dead_code)]
    sha256_provenance_kinds: BTreeMap<String, String>,
    releases: Vec<WireRelease>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRelease {
    version: String,
    channel: String,
    git_tag: String,
    published_at: String,
    release_page_url: String,
    required_editor_capabilities: Vec<String>,
    required_template_capabilities: Vec<String>,
    artifacts: Vec<WireArtifact>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireArtifact {
    platform: Platform,
    artifact_kind: ArtifactKind,
    artifact_name: String,
    download_url: String,
    size_bytes: u64,
    sha256: String,
    sha512: String,
    sha256_provenance: Sha256Provenance,
}

/// The parsed, validated release matrix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseLock {
    minimum_supported_version: ExactVersion,
    required_platforms: Vec<Platform>,
    releases: Vec<PinnedRelease>,
}

impl ReleaseLock {
    /// Repository-relative path of the committed lock file.
    pub const COMMITTED_PATH: &'static str = "tests/fixtures/godot-releases.lock.json";

    /// Schema version this build understands.
    pub const SCHEMA_VERSION: &'static str = "1";

    /// Parse and validate a lock file.
    ///
    /// # Errors
    /// Returns [`ReleaseLockError`] for malformed JSON, an unexpected schema
    /// version, a `major.minor` alias entry, a duplicated version/platform/URL,
    /// a missing required platform, a malformed digest, or an artifact URL that
    /// does not belong to its own release.
    pub fn from_json_slice(bytes: &[u8]) -> Result<Self, ReleaseLockError> {
        let wire: WireLock =
            serde_json::from_slice(bytes).map_err(|error| ReleaseLockError::Malformed {
                reason: error.to_string(),
            })?;
        if wire.schema_version != Self::SCHEMA_VERSION {
            return Err(ReleaseLockError::UnsupportedSchema {
                found: wire.schema_version,
                expected: Self::SCHEMA_VERSION.to_string(),
            });
        }
        let minimum_supported_version = parse_exact_or_alias(&wire.minimum_supported_version)?;

        // Settled in its own pass on purpose: inlining this check would instead
        // report whichever narrower defect the reused URL trips first.
        let mut seen_urls: BTreeSet<&str> = BTreeSet::new();
        for release in &wire.releases {
            for artifact in &release.artifacts {
                if !seen_urls.insert(artifact.download_url.as_str()) {
                    return Err(ReleaseLockError::DuplicateArtifactUrl {
                        url: artifact.download_url.clone(),
                    });
                }
            }
        }

        let mut releases: Vec<PinnedRelease> = Vec::with_capacity(wire.releases.len());
        let mut seen_versions: BTreeSet<ExactVersion> = BTreeSet::new();

        for release in wire.releases {
            let version = parse_exact_or_alias(&release.version)?;
            if !seen_versions.insert(version) {
                return Err(ReleaseLockError::DuplicateVersion {
                    version: version.to_string(),
                });
            }
            let mut seen_platforms: BTreeSet<Platform> = BTreeSet::new();
            let mut artifacts = Vec::with_capacity(release.artifacts.len());
            for artifact in release.artifacts {
                if !seen_platforms.insert(artifact.platform) {
                    return Err(ReleaseLockError::DuplicatePlatform {
                        version: version.to_string(),
                        platform: artifact.platform.to_string(),
                    });
                }
                check_digest(&artifact.artifact_name, "sha256", &artifact.sha256, 64)?;
                check_digest(&artifact.artifact_name, "sha512", &artifact.sha512, 128)?;
                if !artifact.download_url.contains(&release.git_tag) {
                    return Err(ReleaseLockError::ForeignArtifactUrl {
                        artifact: artifact.artifact_name.clone(),
                        git_tag: release.git_tag.clone(),
                    });
                }
                artifacts.push(PinnedArtifact {
                    platform: artifact.platform,
                    kind: artifact.artifact_kind,
                    name: artifact.artifact_name,
                    url: artifact.download_url,
                    size_bytes: artifact.size_bytes,
                    sha256: artifact.sha256,
                    sha512: artifact.sha512,
                    sha256_provenance: artifact.sha256_provenance,
                });
            }
            for platform in &wire.required_platforms {
                if !seen_platforms.contains(platform) {
                    return Err(ReleaseLockError::MissingPlatform {
                        version: version.to_string(),
                        platform: platform.to_string(),
                    });
                }
            }
            artifacts.sort_by_key(|artifact| artifact.platform);
            releases.push(PinnedRelease {
                version,
                channel: release.channel,
                git_tag: release.git_tag,
                published_at: release.published_at,
                release_page_url: release.release_page_url,
                required_editor_capabilities: release.required_editor_capabilities,
                required_template_capabilities: release.required_template_capabilities,
                artifacts,
            });
        }
        releases.sort_by_key(|release| release.version);

        Ok(Self {
            minimum_supported_version,
            required_platforms: wire.required_platforms,
            releases,
        })
    }

    /// The oldest Godot version this matrix supports at all.
    #[must_use]
    pub fn minimum_supported_version(&self) -> ExactVersion {
        self.minimum_supported_version
    }

    /// Platforms every release must pin.
    #[must_use]
    pub fn required_platforms(&self) -> &[Platform] {
        &self.required_platforms
    }

    /// Every pinned release, ascending by version.
    #[must_use]
    pub fn releases(&self) -> &[PinnedRelease] {
        &self.releases
    }

    /// The release pinned for an exact version spelling. An alias never matches.
    #[must_use]
    pub fn release(&self, version: &str) -> Option<&PinnedRelease> {
        let exact = ExactVersion::parse(version).ok()?;
        self.releases
            .iter()
            .find(|release| release.version == exact)
    }

    /// The pinned artifact for an exact version and platform.
    #[must_use]
    pub fn artifact(&self, version: &str, platform: Platform) -> Option<&PinnedArtifact> {
        self.release(version)?.artifact(platform)
    }

    /// Whether a probed version is inside the pinned matrix.
    #[must_use]
    pub fn supports(&self, version: &ExactVersion) -> bool {
        self.releases
            .iter()
            .any(|release| release.version == *version)
    }

    /// Comma-separated supported versions, for operator-facing messages.
    #[must_use]
    pub fn supported_versions_display(&self) -> String {
        self.releases
            .iter()
            .map(|release| release.version.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Verify downloaded artifact bytes against the pinned digest.
    ///
    /// # Errors
    /// Returns [`ReleaseLockError::UnknownVersion`] / [`ReleaseLockError::UnknownPlatform`]
    /// when nothing is pinned for the request, or
    /// [`ReleaseLockError::CorruptDownload`] when the bytes do not match.
    pub fn verify_download(
        &self,
        version: &str,
        platform: Platform,
        bytes: &[u8],
    ) -> Result<VerifiedDownload, ReleaseLockError> {
        let release = self
            .release(version)
            .ok_or_else(|| ReleaseLockError::UnknownVersion {
                version: version.to_string(),
                supported: self.supported_versions_display(),
            })?;
        let artifact =
            release
                .artifact(platform)
                .ok_or_else(|| ReleaseLockError::UnknownPlatform {
                    version: release.version.to_string(),
                    platform: platform.to_string(),
                })?;
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let actual = crate::probe::hex_digest(&hasher.finalize().into());
        if actual != artifact.sha256 {
            return Err(ReleaseLockError::CorruptDownload {
                artifact: artifact.name.clone(),
                expected: artifact.sha256.clone(),
                actual,
            });
        }
        Ok(VerifiedDownload {
            version: release.version,
            platform,
            sha256: actual,
            size_bytes: bytes.len() as u64,
        })
    }

    /// Promote verified download bytes into an execution-ready record.
    ///
    /// This is the only way a matrix download becomes eligible for execution: a
    /// corrupted archive fails here, before anything runs.
    ///
    /// # Errors
    /// Returns the same errors as [`ReleaseLock::verify_download`].
    pub fn plan_execution(
        &self,
        version: &str,
        platform: Platform,
        bytes: &[u8],
    ) -> Result<VerifiedDownload, ReleaseLockError> {
        self.verify_download(version, platform, bytes)
    }
}

fn parse_exact_or_alias(text: &str) -> Result<ExactVersion, ReleaseLockError> {
    ExactVersion::parse(text).map_err(|_| ReleaseLockError::AliasVersion {
        found: text.to_string(),
    })
}

fn check_digest(
    artifact: &str,
    algorithm: &str,
    digest: &str,
    expected_length: usize,
) -> Result<(), ReleaseLockError> {
    let well_formed = digest.len() == expected_length
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if well_formed {
        Ok(())
    } else {
        Err(ReleaseLockError::MalformedDigest {
            artifact: artifact.to_string(),
            algorithm: algorithm.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_version_rejects_aliases_and_suffixes() {
        assert_eq!(
            ExactVersion::parse("4.7.1").expect("exact"),
            ExactVersion::new(4, 7, 1)
        );
        for bad in ["4.7", "4", "", "4.7.1-stable", "v4.7.1", "4..1", "4.7.x"] {
            assert!(ExactVersion::parse(bad).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn exact_version_orders_by_component() {
        assert!(ExactVersion::new(4, 7, 1) > ExactVersion::new(4, 6, 9));
        assert!(ExactVersion::new(4, 4, 0).at_least(ExactVersion::new(4, 4, 0)));
        assert!(!ExactVersion::new(4, 3, 9).at_least(ExactVersion::new(4, 4, 0)));
    }

    #[test]
    fn malformed_lock_bytes_are_rejected() {
        let error = ReleaseLock::from_json_slice(b"not json").expect_err("must reject");
        assert_eq!(error.error_kind(), ErrorKind::InvalidInput);
        assert!(matches!(error, ReleaseLockError::Malformed { .. }));
    }

    #[test]
    fn digest_shape_is_enforced() {
        assert!(check_digest("a", "sha256", &"a".repeat(64), 64).is_ok());
        assert!(check_digest("a", "sha256", &"A".repeat(64), 64).is_err());
        assert!(check_digest("a", "sha256", &"a".repeat(63), 64).is_err());
    }
}
