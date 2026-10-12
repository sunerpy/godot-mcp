//! Lock-driven CI matrix derivation.
//!
//! `tests/fixtures/godot-releases.lock.json` is the sole compatibility source
//! for every Godot version this project claims to support. This module is the
//! only place a CI job or the release-gates harness learns a version label, a
//! download URL or an expected digest: nothing here (and nothing in the workflow
//! definitions) may restate one of those values.
//!
//! Three responsibilities, all deterministic and offline:
//!
//! 1. [`GodotReleasesLock::load`] parses the committed lock.
//! 2. [`GodotReleasesLock::validate`] proves the lock is usable as a matrix
//!    source: exact patch versions only (never a `major.minor` alias), tags and
//!    artifact names/URLs that spell the exact version, one artifact per required
//!    platform per release, well-formed digests, and no duplicated URL or digest
//!    anywhere in the file.
//! 3. [`GodotReleasesLock::engine_editor_matrix`] and
//!    [`GodotReleasesLock::packaged_smoke_matrix`] derive the two CI matrices the
//!    workflows consume through `fromJSON`.
//!
//! [`scan_for_restated_lock_values`] closes the loop from the other side: it
//! reads a CI/harness source file and reports any version label, alias, URL or
//! digest that was copied into it instead of being derived here. Only code
//! regions are scanned — a comment that legitimately names a value is not a
//! finding (a scanner that flagged its own documentation would be useless).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Workspace-relative path of the sole compatibility source.
pub const LOCK_PATH: &str = "tests/fixtures/godot-releases.lock.json";

/// The Godot platform key of the Linux engine/editor matrix.
pub const LINUX_PLATFORM: &str = "linux-x86_64";

/// The Godot platform key of the macOS packaged smoke.
pub const MACOS_PLATFORM: &str = "macos-universal";

/// The Godot platform key of the Windows packaged smoke.
pub const WINDOWS_PLATFORM: &str = "windows-x86_64";

/// The Godot platform key export templates are published under.
pub const ALL_PLATFORMS: &str = "all-platforms";

/// The artifact kind of an editor build.
pub const EDITOR_KIND: &str = "editor";

/// The artifact kind of an export-template bundle.
pub const TEMPLATES_KIND: &str = "export_templates";

/// The three platforms the packaged smoke covers, in a fixed order.
pub const PACKAGED_SMOKE_PLATFORMS: [&str; 3] = [LINUX_PLATFORM, MACOS_PLATFORM, WINDOWS_PLATFORM];

/// One per-platform artifact of one release.
#[derive(Debug, Clone, Deserialize)]
pub struct LockedArtifact {
    /// Godot platform key (for example `linux-x86_64`).
    pub platform: String,
    /// `editor` or `export_templates`.
    pub artifact_kind: String,
    /// Published asset file name.
    pub artifact_name: String,
    /// Published download URL.
    pub download_url: String,
    /// Published asset size in bytes.
    pub size_bytes: u64,
    /// Official lowercase-hex SHA-256 of the asset bytes.
    pub sha256: String,
    /// Official lowercase-hex SHA-512 of the asset bytes.
    pub sha512: String,
    /// How the recorded SHA-256 was established.
    pub sha256_provenance: String,
}

/// One exact Godot release.
#[derive(Debug, Clone, Deserialize)]
pub struct LockedRelease {
    /// Exact patch version (`MAJOR.MINOR.PATCH`). Never an alias.
    pub version: String,
    /// Release channel.
    pub channel: String,
    /// Upstream git tag.
    pub git_tag: String,
    /// Publication timestamp.
    pub published_at: String,
    /// Release page URL.
    pub release_page_url: String,
    /// Editor flags this release must advertise.
    pub required_editor_capabilities: Vec<String>,
    /// Export-template flags this release must advertise.
    pub required_template_capabilities: Vec<String>,
    /// Per-platform artifacts.
    pub artifacts: Vec<LockedArtifact>,
}

/// The whole committed lock document.
#[derive(Debug, Clone, Deserialize)]
pub struct GodotReleasesLock {
    /// Lock schema version.
    pub schema_version: String,
    /// Human-readable purpose statement.
    pub description: String,
    /// Oldest version the project claims to support.
    pub minimum_supported_version: String,
    /// Platform keys every release must carry.
    pub required_platforms: Vec<String>,
    /// Accepted `sha256_provenance` values, keyed by kind.
    pub sha256_provenance_kinds: BTreeMap<String, String>,
    /// Every exact release, oldest first.
    pub releases: Vec<LockedRelease>,
}

/// One derived matrix row. Every field is copied straight out of the lock, which
/// is why the workflows never need to spell one out.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct MatrixEntry {
    /// Stable job identity, derived from version, platform and artifact kind.
    pub job: String,
    /// Exact patch version.
    pub version: String,
    /// Upstream git tag.
    pub git_tag: String,
    /// Godot platform key.
    pub platform: String,
    /// `editor` or `export_templates`.
    pub artifact_kind: String,
    /// Published asset file name.
    pub artifact_name: String,
    /// Published download URL.
    pub download_url: String,
    /// Published asset size in bytes.
    pub size_bytes: u64,
    /// Official lowercase-hex SHA-256 the download must hash to before it may be
    /// cached or executed.
    pub sha256: String,
}

/// A value a CI/harness source restated instead of deriving from the lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestatedValue {
    /// Which file the copy was found in.
    pub location: String,
    /// What kind of value it is (`version`, `alias`, `url`, `sha256`, `sha512`).
    pub kind: String,
    /// The offending literal.
    pub value: String,
}

fn is_lower_hex(text: &str, width: usize) -> bool {
    text.len() == width
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Split `version` into its numeric components, rejecting anything that is not
/// exactly three non-empty numeric parts.
fn exact_patch_parts(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    let patch = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let parse = |part: &str| -> Option<u64> {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    Some((parse(major)?, parse(minor)?, parse(patch)?))
}

/// The `major.minor` alias of an exact patch version. Aliases are never lock
/// entries and must never appear in CI/harness code; this helper exists so the
/// scanner can look for them without restating one.
///
/// # Errors
/// Returns an error if `version` is not an exact `MAJOR.MINOR.PATCH` version.
pub fn alias_of(version: &str) -> Result<String, String> {
    let (major, minor, _) = exact_patch_parts(version)
        .ok_or_else(|| format!("`{version}` is not an exact MAJOR.MINOR.PATCH version"))?;
    Ok(format!("{major}.{minor}"))
}

impl GodotReleasesLock {
    /// Load the lock from an explicit path.
    ///
    /// # Errors
    /// Returns an error if the file cannot be read or parsed.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        serde_json::from_str(&text).map_err(|error| format!("parse {}: {error}", path.display()))
    }

    /// Load the committed lock relative to a workspace root.
    ///
    /// # Errors
    /// Returns an error if the file cannot be read or parsed.
    pub fn load_from_workspace(workspace_root: &Path) -> Result<Self, String> {
        Self::load(&workspace_root.join(LOCK_PATH))
    }

    /// The lock path relative to `workspace_root`.
    #[must_use]
    pub fn path_in(workspace_root: &Path) -> PathBuf {
        workspace_root.join(LOCK_PATH)
    }

    /// Prove the lock is usable as the sole matrix source.
    ///
    /// Every rule here is a fail-closed precondition of matrix derivation: a lock
    /// that carried an alias, a shortened URL, a malformed digest, a missing
    /// platform or a duplicated URL/digest would silently produce a matrix that
    /// tests the wrong bytes.
    ///
    /// # Errors
    /// Returns every violation found, joined into one message.
    pub fn validate(&self) -> Result<(), String> {
        let mut problems: Vec<String> = Vec::new();
        if self.schema_version.trim().is_empty() {
            problems.push("schema_version is empty".to_owned());
        }
        if self.required_platforms.is_empty() {
            problems.push("required_platforms is empty".to_owned());
        }
        if exact_patch_parts(&self.minimum_supported_version).is_none() {
            problems.push(format!(
                "minimum_supported_version `{}` is not an exact MAJOR.MINOR.PATCH version",
                self.minimum_supported_version
            ));
        }
        if self.releases.is_empty() {
            problems.push("releases is empty".to_owned());
        }

        let mut seen_versions: BTreeSet<&str> = BTreeSet::new();
        let mut seen_urls: BTreeMap<&str, String> = BTreeMap::new();
        let mut seen_digests: BTreeMap<&str, String> = BTreeMap::new();
        let mut previous: Option<(u64, u64, u64)> = None;

        for release in &self.releases {
            let where_ = format!("release {}", release.version);
            let Some(parts) = exact_patch_parts(&release.version) else {
                problems.push(format!(
                    "{where_}: version is a `major.minor` alias or otherwise not an exact \
                     MAJOR.MINOR.PATCH version"
                ));
                continue;
            };
            if !seen_versions.insert(release.version.as_str()) {
                problems.push(format!("{where_}: duplicate version"));
            }
            if let Some(before) = previous
                && before >= parts
            {
                problems.push(format!("{where_}: releases are not in ascending order"));
            }
            previous = Some(parts);

            let expected_tag = format!("{}-{}", release.version, release.channel);
            if release.git_tag != expected_tag {
                problems.push(format!(
                    "{where_}: git_tag `{}` is not `{expected_tag}`",
                    release.git_tag
                ));
            }
            if !release.release_page_url.ends_with(&release.git_tag) {
                problems.push(format!(
                    "{where_}: release_page_url does not end with the exact git tag"
                ));
            }
            if release.published_at.trim().is_empty() {
                problems.push(format!("{where_}: published_at is empty"));
            }
            if release.required_editor_capabilities.is_empty() {
                problems.push(format!("{where_}: required_editor_capabilities is empty"));
            }
            if release.required_template_capabilities.is_empty() {
                problems.push(format!("{where_}: required_template_capabilities is empty"));
            }

            for platform in &self.required_platforms {
                let count = release
                    .artifacts
                    .iter()
                    .filter(|artifact| &artifact.platform == platform)
                    .count();
                if count != 1 {
                    problems.push(format!(
                        "{where_}: expected exactly one `{platform}` artifact, found {count}"
                    ));
                }
            }

            for artifact in &release.artifacts {
                let at = format!("{where_} artifact {}", artifact.platform);
                if !self.required_platforms.contains(&artifact.platform) {
                    problems.push(format!("{at}: platform is not a required platform"));
                }
                if !matches!(
                    artifact.artifact_kind.as_str(),
                    EDITOR_KIND | TEMPLATES_KIND
                ) {
                    problems.push(format!(
                        "{at}: artifact_kind `{}` is neither `{EDITOR_KIND}` nor `{TEMPLATES_KIND}`",
                        artifact.artifact_kind
                    ));
                }
                let exact_marker = format!("v{}-{}", release.version, release.channel);
                if !artifact.artifact_name.contains(&exact_marker) {
                    problems.push(format!(
                        "{at}: artifact_name does not spell the exact version marker"
                    ));
                }
                if !artifact.download_url.contains(&release.git_tag)
                    || !artifact.download_url.ends_with(&artifact.artifact_name)
                {
                    problems.push(format!(
                        "{at}: download_url does not spell the exact tag and asset name"
                    ));
                }
                if artifact.size_bytes == 0 {
                    problems.push(format!("{at}: size_bytes is zero"));
                }
                if !is_lower_hex(&artifact.sha256, 64) {
                    problems.push(format!("{at}: sha256 is not 64 lowercase hex characters"));
                }
                if !is_lower_hex(&artifact.sha512, 128) {
                    problems.push(format!("{at}: sha512 is not 128 lowercase hex characters"));
                }
                if !self
                    .sha256_provenance_kinds
                    .contains_key(&artifact.sha256_provenance)
                {
                    problems.push(format!(
                        "{at}: sha256_provenance `{}` is not a declared provenance kind",
                        artifact.sha256_provenance
                    ));
                }
                if let Some(first) = seen_urls.insert(artifact.download_url.as_str(), at.clone()) {
                    problems.push(format!("{at}: download_url duplicates {first}"));
                }
                if let Some(first) = seen_digests.insert(artifact.sha256.as_str(), at.clone()) {
                    problems.push(format!("{at}: sha256 duplicates {first}"));
                }
            }
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    /// Every exact version in the lock, oldest first.
    #[must_use]
    pub fn versions(&self) -> Vec<String> {
        self.releases
            .iter()
            .map(|release| release.version.clone())
            .collect()
    }

    /// The newest release, derived from lock order (which [`Self::validate`]
    /// proves ascending). The packaged smoke targets this release; no code may
    /// name it.
    ///
    /// # Errors
    /// Returns an error if the lock records no release.
    pub fn newest(&self) -> Result<&LockedRelease, String> {
        self.releases
            .last()
            .ok_or_else(|| "the lock records no release".to_owned())
    }

    fn artifact(&self, release: &LockedRelease, platform: &str) -> Result<LockedArtifact, String> {
        release
            .artifacts
            .iter()
            .find(|artifact| artifact.platform == platform)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "release {} records no `{platform}` artifact",
                    release.version
                )
            })
    }

    fn entry(release: &LockedRelease, artifact: &LockedArtifact) -> MatrixEntry {
        MatrixEntry {
            job: format!(
                "godot-{}-{}-{}",
                release.version, artifact.platform, artifact.artifact_kind
            ),
            version: release.version.clone(),
            git_tag: release.git_tag.clone(),
            platform: artifact.platform.clone(),
            artifact_kind: artifact.artifact_kind.clone(),
            artifact_name: artifact.artifact_name.clone(),
            download_url: artifact.download_url.clone(),
            size_bytes: artifact.size_bytes,
            sha256: artifact.sha256.clone(),
        }
    }

    /// The Linux engine/editor matrix: for every exact release in the lock, the
    /// Linux editor build plus the export-template bundle that release's engine
    /// runs need.
    ///
    /// # Errors
    /// Returns an error if the lock is invalid or an expected artifact is absent.
    pub fn engine_editor_matrix(&self) -> Result<Vec<MatrixEntry>, String> {
        self.validate()?;
        let mut rows = Vec::new();
        for release in &self.releases {
            for platform in [LINUX_PLATFORM, ALL_PLATFORMS] {
                let artifact = self.artifact(release, platform)?;
                rows.push(Self::entry(release, &artifact));
            }
        }
        Ok(rows)
    }

    /// The packaged Linux/macOS/Windows smoke matrix, pinned to the newest exact
    /// release in the lock.
    ///
    /// # Errors
    /// Returns an error if the lock is invalid or an expected artifact is absent.
    pub fn packaged_smoke_matrix(&self) -> Result<Vec<MatrixEntry>, String> {
        self.validate()?;
        let release = self.newest()?;
        let mut rows = Vec::new();
        for platform in PACKAGED_SMOKE_PLATFORMS {
            let artifact = self.artifact(release, platform)?;
            rows.push(Self::entry(release, &artifact));
        }
        Ok(rows)
    }

    /// Both matrices as the JSON object the workflows read through `fromJSON`.
    ///
    /// # Errors
    /// Returns an error if either matrix cannot be derived.
    pub fn matrix_document(&self) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({
            "lock_path": LOCK_PATH,
            "engine_editor": self.engine_editor_matrix()?,
            "packaged_smoke": self.packaged_smoke_matrix()?,
        }))
    }

    /// Every literal a CI/harness source is forbidden to restate, as
    /// `(kind, value)` pairs.
    ///
    /// # Errors
    /// Returns an error if a release version is not an exact patch version.
    pub fn forbidden_literals(&self) -> Result<Vec<(String, String)>, String> {
        let mut out: Vec<(String, String)> = Vec::new();
        for release in &self.releases {
            out.push(("version".to_owned(), release.version.clone()));
            out.push(("alias".to_owned(), alias_of(&release.version)?));
            out.push(("tag".to_owned(), release.git_tag.clone()));
            for artifact in &release.artifacts {
                out.push(("url".to_owned(), artifact.download_url.clone()));
                out.push(("asset".to_owned(), artifact.artifact_name.clone()));
                out.push(("sha256".to_owned(), artifact.sha256.clone()));
                out.push(("sha512".to_owned(), artifact.sha512.clone()));
            }
        }
        Ok(out)
    }
}

/// Strip comment regions from `text`, leaving code regions in place with their
/// byte offsets preserved (comment bytes become spaces).
///
/// Both `//`-style Rust line comments and `#`-style YAML comments are removed,
/// as are `/* … */` Rust block comments. String literals are deliberately NOT
/// treated as comments: a restated URL inside a string is exactly the finding
/// this scanner exists for.
///
/// Only code regions are scanned because a scanner that flagged a value named in
/// its own documentation would report its own doc comments forever.
#[must_use]
pub fn code_regions_only(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = vec![b' '; bytes.len()];
    let mut index = 0usize;
    let mut in_block = false;
    let mut in_line = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_block {
            if byte == b'*' && bytes.get(index + 1) == Some(&b'/') {
                in_block = false;
                index += 2;
                continue;
            }
            if byte == b'\n' {
                out[index] = b'\n';
            }
            index += 1;
            continue;
        }
        if in_line {
            if byte == b'\n' {
                in_line = false;
                out[index] = b'\n';
            }
            index += 1;
            continue;
        }
        // `//` opens a Rust line comment — unless it follows a `:`, which makes it
        // the authority separator of a URL. Treating `https://…` as a comment
        // would blind the scanner to exactly the restatement it exists to catch.
        if byte == b'/'
            && bytes.get(index + 1) == Some(&b'/')
            && (index == 0 || bytes[index - 1] != b':')
        {
            in_line = true;
            index += 2;
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            in_block = true;
            index += 2;
            continue;
        }
        // `#` opens a YAML/shell/Make comment only at the start of a line or after
        // whitespace, which is the real comment rule. A `#` in the middle of a
        // token is data.
        if byte == b'#' && (index == 0 || bytes[index - 1].is_ascii_whitespace()) {
            in_line = true;
            index += 1;
            continue;
        }
        out[index] = byte;
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether the alias `alias` occurs in `code` as a standalone version token
/// rather than as part of a longer number (`4.4.1` contains its own alias and is
/// never an alias finding).
fn contains_standalone_alias(code: &str, alias: &str) -> bool {
    let bytes = code.as_bytes();
    let needle = alias.as_bytes();
    let mut start = 0usize;
    while let Some(found) = code[start..].find(alias) {
        let at = start + found;
        let before_ok = at == 0 || !matches!(bytes[at - 1], b'0'..=b'9' | b'.');
        let after = at + needle.len();
        let after_ok = after >= bytes.len() || !matches!(bytes[after], b'0'..=b'9' | b'.');
        if before_ok && after_ok {
            return true;
        }
        start = at + 1;
    }
    false
}

/// Report every lock value restated in `text`'s code regions.
///
/// `location` is only used to label findings.
///
/// # Errors
/// Returns an error if the lock cannot supply its forbidden-literal set.
pub fn scan_for_restated_lock_values(
    location: &str,
    text: &str,
    lock: &GodotReleasesLock,
) -> Result<Vec<RestatedValue>, String> {
    let code = code_regions_only(text);
    let mut found = Vec::new();
    for (kind, value) in lock.forbidden_literals()? {
        let hit = if kind == "alias" {
            contains_standalone_alias(&code, &value)
        } else {
            code.contains(&value)
        };
        if hit {
            found.push(RestatedValue {
                location: location.to_owned(),
                kind,
                value,
            });
        }
    }
    found.sort_by(|left, right| (&left.kind, &left.value).cmp(&(&right.kind, &right.value)));
    found.dedup();
    Ok(found)
}
