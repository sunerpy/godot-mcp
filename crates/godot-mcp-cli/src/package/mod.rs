//! Release packaging: per-target archives, checksums, provenance and the
//! deterministic host-package manifest the Task-34 QA lane resolves.
//!
//! ## What ships
//!
//! Every archive carries exactly one payload tree:
//!
//! ```text
//! bin/godot-mcp[.exe]            the Rust binary for that target
//! bin/godot-mcp-supervisor[.exe] the engine-plane supervisor that binary launches
//! bridge/bridge_codec.gd         the Todo 19 pure-GDScript bridge codec
//! schemas/**                     the committed wire/domain schemas
//! licenses/LICENSE-MIT           the project licence
//! licenses/THIRD-PARTY-NOTICES.md the dependency notices
//! sbom/godot-mcp.cdx.json        a CycloneDX 1.5 SBOM of the linked closure
//! examples/godot-mcp.toml        a valid, credential-free example config
//! smoke.sh                       the offline payload verifier, mode 0755
//! MANIFEST.json                  every payload path with its SHA-256 and mode
//! ```
//!
//! `smoke.sh` is also published at the top of the dist directory, because the
//! frozen `isolation-docker-clean-room` gate bind-mounts that directory read-only
//! at `/dist` and execs `/dist/smoke.sh`. The two copies come from the same
//! committed source, `scripts/dist-smoke.sh`, and the script itself proves at gate
//! time that the copy it is running from is byte-identical to the payload entry
//! `MANIFEST.json` records — so the convenience copy cannot drift from the
//! digest-recorded one.
//!
//! Godot itself is never bundled, no credential is ever embedded, and no
//! `test-hooks` binary can enter an archive: the only executable staged is the
//! one the caller supplies, and [`test_hook_binary_names`] reads the crate
//! manifest to prove which binaries are feature-gated out of a release build.
//!
//! ## Reproducibility
//!
//! Archive bytes are a pure function of the payload: entry order is sorted, the
//! tar/zip metadata is pinned (see [`archive`]), the SBOM is derived from the
//! committed lock with no timestamp, and `MANIFEST.json` is sorted. Two runs over
//! the same inputs therefore produce identical archive bytes, which is what makes
//! the two-clean-build check meaningful — any difference it reports comes from the
//! compiler, not from the packager.
//!
//! ## Host honesty
//!
//! [`ReleaseTarget::buildable_on_host`] reports whether this machine can actually
//! produce a given target's executable. A target the host cannot build is
//! recorded in the provenance document as `built: false`; the packager never
//! invents an executable, a digest or an archive for it.

pub mod archive;
pub mod installer;
pub mod lock;
pub mod sbom;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use archive::ArchiveEntry;

/// Schema version of the payload `MANIFEST.json` and the provenance document.
pub const PACKAGE_SCHEMA_VERSION: &str = "1.0.0";

/// Workspace-relative directory the release artifacts are written to.
pub const DIST_DIR: &str = "target/release-dist";

/// Name of the checksum manifest published alongside the archives.
pub const CHECKSUM_FILE: &str = "SHA256SUMS.txt";

/// Name of the provenance attestation document.
pub const PROVENANCE_FILE: &str = "provenance.json";

/// Name of the reproducibility result document.
pub const REPRODUCIBILITY_FILE: &str = "reproducibility.json";

/// Payload and dist-level name of the offline verifier the isolation gate execs.
pub const SMOKE_FILE: &str = "smoke.sh";

/// Workspace-relative source of [`SMOKE_FILE`]. One committed script backs both
/// the payload entry and the dist-level copy, so the two can never diverge.
pub const SMOKE_SOURCE: &str = "scripts/dist-smoke.sh";

/// The crate whose linked closure the SBOM describes: the shipping binary's
/// own package.
pub const RELEASE_ROOT_CRATE: &str = "godot-mcp-cli";

/// The shipping binary's name.
pub const RELEASE_BINARY: &str = "godot-mcp";

/// Credential markers, stored **reversed**.
///
/// The scan covers the shipping executable, and the executable is compiled from
/// this very crate — so a marker held as a plain literal would be embedded in the
/// binary and every build would flag itself. Storing each marker reversed and
/// un-reversing it at runtime means the pattern the scanner looks for provably
/// never appears in the scanner's own image, while a real credential embedded in
/// any payload file still matches. `credential_markers_are_the_expected_plaintext`
/// pins the round-trip so a mistyped reversal cannot silently disable a check.
const REVERSED_CREDENTIAL_MARKERS: &[&str] = &[
    "YEK ETAVIRP ASR NIGEB-----",
    "YEK ETAVIRP HSSNEPO NIGEB-----",
    "YEK ETAVIRP NIGEB-----",
    "yek_ssecca_terces_swa",
    "ELPMAXE7NNDOFSOIAIKA",
];

/// The credential markers a payload file must not contain.
#[must_use]
pub fn credential_markers() -> Vec<String> {
    REVERSED_CREDENTIAL_MARKERS
        .iter()
        .map(|marker| marker.chars().rev().collect())
        .collect()
}

/// A release target triple the packager knows how to lay out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReleaseTarget {
    /// Linux, x86-64, statically linked musl.
    LinuxX86_64Musl,
    /// macOS, Apple silicon.
    MacOsAarch64,
    /// Windows, x86-64, MSVC ABI.
    WindowsX86_64Msvc,
}

impl ReleaseTarget {
    /// Every target a release publishes.
    pub const ALL: &[Self] = &[
        Self::LinuxX86_64Musl,
        Self::MacOsAarch64,
        Self::WindowsX86_64Msvc,
    ];

    /// The Rust target triple.
    #[must_use]
    pub const fn triple(self) -> &'static str {
        match self {
            Self::LinuxX86_64Musl => "x86_64-unknown-linux-musl",
            Self::MacOsAarch64 => "aarch64-apple-darwin",
            Self::WindowsX86_64Msvc => "x86_64-pc-windows-msvc",
        }
    }

    /// The executable file name inside `bin/`.
    #[must_use]
    pub const fn executable_name(self) -> &'static str {
        match self {
            Self::WindowsX86_64Msvc => "godot-mcp.exe",
            _ => "godot-mcp",
        }
    }

    /// Supervisor executable file name inside `bin/`.
    #[must_use]
    pub const fn supervisor_name(self) -> &'static str {
        match self {
            Self::WindowsX86_64Msvc => "godot-mcp-supervisor.exe",
            _ => "godot-mcp-supervisor",
        }
    }

    /// The archive extension for this target's platform convention.
    #[must_use]
    pub const fn archive_extension(self) -> &'static str {
        match self {
            Self::WindowsX86_64Msvc => "zip",
            _ => "tar.gz",
        }
    }

    /// The published target assigned to this host platform.
    ///
    /// Platform identity and build capability are deliberately separate: an
    /// x86-64 Linux host publishes the self-contained musl target even when the
    /// required musl linker has not yet been installed. Call
    /// [`Self::require_buildable_on_host`] before starting a build.
    #[must_use]
    pub fn host() -> Option<Self> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => Some(Self::LinuxX86_64Musl),
            ("macos", "aarch64") => Some(Self::MacOsAarch64),
            ("windows", "x86_64") => Some(Self::WindowsX86_64Msvc),
            _ => None,
        }
    }

    /// Whether this machine can produce this target's executable.
    ///
    /// Only the host target is claimed. Cross-compiling to macOS or to the MSVC
    /// ABI needs a platform SDK/linker this packager will not pretend to have, so
    /// those targets are honestly reported as not buildable here and are left to
    /// a real runner of that platform.
    #[must_use]
    pub fn buildable_on_host(self) -> bool {
        self.require_buildable_on_host().is_ok()
    }

    /// Require the native SDK or compiler needed to build this target here.
    ///
    /// # Errors
    /// Returns an actionable error when this is not the host platform's release
    /// target or when Linux cannot find the musl linker used by bundled SQLite.
    pub fn require_buildable_on_host(self) -> Result<(), String> {
        let path = std::env::var_os("PATH");
        self.require_buildable_on_host_with_path(path.as_deref())
    }

    fn require_buildable_on_host_with_path(self, path: Option<&OsStr>) -> Result<(), String> {
        let host = Self::host().ok_or_else(|| {
            format!(
                "this host ({} {}) is not one of the published release platforms",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })?;
        if self != host {
            return Err(format!(
                "target {} is not buildable on this {} {} host; use a native runner for that platform",
                self.triple(),
                std::env::consts::OS,
                std::env::consts::ARCH
            ));
        }

        if self != Self::LinuxX86_64Musl || executable_in_path("x86_64-linux-musl-gcc", path) {
            return Ok(());
        }

        Err(
            "`x86_64-linux-musl-gcc` is not executable on PATH; it is the linker the `cc` crate invokes for the bundled SQLite source. On Debian/Ubuntu install `musl-tools` (which depends on `musl-dev`), then run `rustup target add x86_64-unknown-linux-musl --toolchain 1.96.0`"
                .to_owned(),
        )
    }

    /// Parse a triple.
    #[must_use]
    pub fn from_triple(triple: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|target| target.triple() == triple)
    }
}

fn executable_in_path(executable: &str, path: Option<&OsStr>) -> bool {
    let Some(path) = path else {
        return false;
    };
    std::env::split_paths(path).any(|directory| is_executable_file(&directory.join(executable)))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Everything the packager needs to lay out one archive.
#[derive(Debug, Clone)]
pub struct PackageInputs {
    /// Workspace root, used to read the committed payload sources.
    pub workspace_root: PathBuf,
    /// Package version that names the archive.
    pub version: String,
    /// Which target this archive is for.
    pub target: ReleaseTarget,
    /// The already-built executable's bytes.
    pub executable: Vec<u8>,
    /// The already-built supervisor executable's bytes.
    pub supervisor: Vec<u8>,
    /// Which cargo profile produced the executable, recorded in the payload
    /// manifest so a QA-lane archive can never be mistaken for a release one.
    pub profile: String,
}

/// One produced archive, in memory.
#[derive(Debug, Clone)]
pub struct ProducedArchive {
    /// Which target it is for.
    pub target: ReleaseTarget,
    /// Archive file name, e.g. `godot-mcp-0.1.0-x86_64-unknown-linux-musl.tar.gz`.
    pub file_name: String,
    /// The archive bytes.
    pub bytes: Vec<u8>,
    /// SHA-256 of [`Self::bytes`].
    pub sha256: String,
    /// The payload entries the archive carries.
    pub entries: Vec<ArchiveEntry>,
}

impl ProducedArchive {
    /// Payload paths, sorted.
    #[must_use]
    pub fn paths(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect()
    }
}

/// The archive-internal root directory name.
#[must_use]
pub fn payload_root(version: &str, target: ReleaseTarget) -> String {
    format!("godot-mcp-{version}-{}", target.triple())
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))
}

/// The example configuration that ships. It selects the default local stdio
/// transport, enables no network listener and contains no credential.
#[must_use]
pub fn example_config() -> Vec<u8> {
    let text = "\
# godot-mcp example configuration.
#
# The default transport is local stdio: no socket is opened, no port is bound
# and no credential is required. Public HTTP is never enabled by default and a
# non-loopback bind is rejected outright by `godot-mcp check-config`.
transport = \"stdio\"
";
    text.as_bytes().to_vec()
}

/// Read the committed source of the offline payload verifier.
///
/// # Errors
/// Returns an error if `scripts/dist-smoke.sh` is missing from the workspace.
pub fn smoke_script(workspace_root: &Path) -> Result<Vec<u8>, String> {
    read(&workspace_root.join(SMOKE_SOURCE))
}

/// Stage the complete payload for one target, including `MANIFEST.json`.
///
/// # Errors
/// Returns an error if a committed payload source is missing, the SBOM cannot be
/// derived from `Cargo.lock`, or a credential marker is found in the payload.
pub fn stage(inputs: &PackageInputs) -> Result<Vec<ArchiveEntry>, String> {
    let root = &inputs.workspace_root;
    let mut entries = vec![ArchiveEntry {
        path: format!("bin/{}", inputs.target.executable_name()),
        bytes: inputs.executable.clone(),
        mode: 0o755,
    }];
    entries.push(ArchiveEntry {
        path: format!("bin/{}", inputs.target.supervisor_name()),
        bytes: inputs.supervisor.clone(),
        mode: 0o755,
    });

    entries.push(ArchiveEntry {
        path: "bridge/bridge_codec.gd".to_string(),
        bytes: read(
            &root
                .join("crates")
                .join("godot-mcp-bridge")
                .join("scripts")
                .join("bridge")
                .join("bridge_codec.gd"),
        )?,
        mode: 0o644,
    });

    let schemas = root.join("schemas");
    entries.extend(
        archive::collect_tree(&schemas, "schemas")
            .map_err(|error| format!("collect {}: {error}", schemas.display()))?,
    );

    for (name, source) in [
        ("licenses/LICENSE-MIT", "LICENSE-MIT"),
        ("licenses/THIRD-PARTY-NOTICES.md", "THIRD-PARTY-NOTICES.md"),
    ] {
        entries.push(ArchiveEntry {
            path: name.to_string(),
            bytes: read(&root.join(source))?,
            mode: 0o644,
        });
    }

    let locked = lock::CargoLock::load(&root.join("Cargo.lock"))?;
    let document =
        sbom::from_cargo_lock(&locked, RELEASE_ROOT_CRATE, RELEASE_BINARY, &inputs.version)?;
    entries.push(ArchiveEntry {
        path: "sbom/godot-mcp.cdx.json".to_string(),
        bytes: sbom::to_bytes(&document)?,
        mode: 0o644,
    });

    entries.push(ArchiveEntry {
        path: "examples/godot-mcp.toml".to_string(),
        bytes: example_config(),
        mode: 0o644,
    });

    entries.push(ArchiveEntry {
        path: SMOKE_FILE.to_string(),
        bytes: smoke_script(root)?,
        mode: 0o755,
    });

    entries.sort_by(|left, right| left.path.cmp(&right.path));
    reject_credential_markers(&entries)?;

    let manifest = payload_manifest(inputs, &entries)?;
    entries.push(ArchiveEntry {
        path: "MANIFEST.json".to_string(),
        bytes: manifest,
        mode: 0o644,
    });
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

fn reject_credential_markers(entries: &[ArchiveEntry]) -> Result<(), String> {
    let markers = credential_markers();
    for entry in entries {
        if contains_credential_marker(&entry.bytes, &markers) {
            return Err(format!(
                "payload file {} contains a credential marker",
                entry.path
            ));
        }
    }
    Ok(())
}

/// Search for every marker in a single pass, gated on a first-byte bitmap.
///
/// The largest payload file is the shipping executable — over a hundred megabytes
/// with debug info — and this scan runs on every packaging and self-check. A naive
/// marker-at-a-time `windows()` search costs one pass per marker and is
/// measurably slow in an unoptimized test build, so the bitmap reduces the whole
/// scan to one pass with one byte comparison per position and a full compare only
/// at a plausible start.
fn contains_credential_marker(bytes: &[u8], markers: &[String]) -> bool {
    let mut first_bytes = [false; 256];
    let mut shortest = usize::MAX;
    for marker in markers {
        let Some(first) = marker.as_bytes().first() else {
            continue;
        };
        first_bytes[usize::from(*first)] = true;
        shortest = shortest.min(marker.len());
    }
    if shortest > bytes.len() {
        return false;
    }
    for index in 0..=bytes.len() - shortest {
        if !first_bytes[usize::from(bytes[index])] {
            continue;
        }
        let tail = &bytes[index..];
        if markers
            .iter()
            .any(|marker| tail.starts_with(marker.as_bytes()))
        {
            return true;
        }
    }
    false
}

fn payload_manifest(inputs: &PackageInputs, entries: &[ArchiveEntry]) -> Result<Vec<u8>, String> {
    let files: Vec<serde_json::Value> = entries
        .iter()
        .map(|entry| {
            serde_json::json!({
                "path": entry.path,
                "sha256": sha256_hex(&entry.bytes),
                "size": entry.bytes.len(),
                "mode": format!("{:o}", entry.mode & 0o7777),
            })
        })
        .collect();
    let document = serde_json::json!({
        "schema_version": PACKAGE_SCHEMA_VERSION,
        "package": RELEASE_BINARY,
        "version": inputs.version,
        "target": inputs.target.triple(),
        "profile": inputs.profile,
        "executable": format!("bin/{}", inputs.target.executable_name()),
        "supervisor": format!("bin/{}", inputs.target.supervisor_name()),
        "built_on_host": inputs.target.buildable_on_host(),
        "files": files,
    });
    let mut bytes = serde_json::to_vec_pretty(&document)
        .map_err(|error| format!("serialize payload manifest: {error}"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Build the archive bytes for one target from staged inputs.
///
/// # Errors
/// Returns an error if staging fails or the archive writer rejects an entry.
pub fn build_archive(inputs: &PackageInputs) -> Result<ProducedArchive, String> {
    let payload = stage(inputs)?;
    let root = payload_root(&inputs.version, inputs.target);
    let entries: Vec<ArchiveEntry> = payload
        .iter()
        .map(|entry| ArchiveEntry {
            path: format!("{root}/{}", entry.path),
            bytes: entry.bytes.clone(),
            mode: entry.mode,
        })
        .collect();
    let bytes = match inputs.target {
        ReleaseTarget::WindowsX86_64Msvc => archive::write_zip(&entries)?,
        _ => archive::write_tar_gz(&entries)?,
    };
    Ok(ProducedArchive {
        target: inputs.target,
        file_name: format!("{root}.{}", inputs.target.archive_extension()),
        sha256: sha256_hex(&bytes),
        bytes,
        entries,
    })
}

/// Render the `SHA256SUMS.txt` body for a set of produced archives, in the exact
/// `sha256sum -c` format.
#[must_use]
pub fn checksum_manifest(archives: &[ProducedArchive]) -> String {
    let mut lines: Vec<String> = archives
        .iter()
        .map(|produced| format!("{}  {}", produced.sha256, produced.file_name))
        .collect();
    lines.sort();
    let mut body = lines.join("\n");
    body.push('\n');
    body
}

/// The commit this workspace is checked out at, read straight from the git
/// metadata without invoking git.
///
/// Returns `None` when the workspace is not a git checkout, in which case the
/// provenance document records the source revision as unknown rather than
/// guessing one.
#[must_use]
pub fn source_revision(workspace_root: &Path) -> Option<String> {
    let git = workspace_root.join(".git");
    let head = std::fs::read_to_string(git.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref: ") {
        let direct = git.join(reference);
        if let Ok(text) = std::fs::read_to_string(&direct) {
            return Some(text.trim().to_string());
        }
        let packed = std::fs::read_to_string(git.join("packed-refs")).ok()?;
        for line in packed.lines() {
            if let Some((object, name)) = line.split_once(' ')
                && name.trim() == reference
            {
                return Some(object.trim().to_string());
            }
        }
        None
    } else if head.len() == 40 && head.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(head.to_string())
    } else {
        None
    }
}

/// Build the provenance attestation document for a release run.
///
/// The document is an in-toto-shaped statement: it names the builder, the source
/// revision and the exact subject digests. It deliberately carries **no**
/// signature — signing is the CI harness's job, and the document records which
/// targets were genuinely built on this machine and which were not, so an
/// unsigned local run can never be mistaken for a complete signed release.
#[must_use]
pub fn provenance(
    workspace_root: &Path,
    version: &str,
    archives: &[ProducedArchive],
) -> serde_json::Value {
    let subjects: Vec<serde_json::Value> = archives
        .iter()
        .map(|produced| {
            serde_json::json!({
                "name": produced.file_name,
                "digest": { "sha256": produced.sha256 },
                "target": produced.target.triple(),
                "built_on_this_host": produced.target.buildable_on_host(),
            })
        })
        .collect();
    let produced: Vec<&str> = archives
        .iter()
        .map(|produced| produced.target.triple())
        .collect();
    let missing: Vec<serde_json::Value> = ReleaseTarget::ALL
        .iter()
        .copied()
        .filter(|target| !produced.contains(&target.triple()))
        .map(|target| {
            serde_json::json!({
                "target": target.triple(),
                "built": false,
                "reason": "no toolchain for this target on the packaging host; a real runner of that platform must produce it",
            })
        })
        .collect();
    serde_json::json!({
        "schema_version": PACKAGE_SCHEMA_VERSION,
        "_type": "https://in-toto.io/Statement/v1",
        "predicateType": "https://slsa.dev/provenance/v1",
        "subject": subjects,
        "predicate": {
            "buildDefinition": {
                "buildType": "https://godot-mcp.invalid/release-package/v1",
                "externalParameters": {
                    "version": version,
                    "sourceRevision": source_revision(workspace_root),
                },
            },
            "runDetails": {
                "builder": { "id": "https://godot-mcp.invalid/local-packager" },
            },
        },
        "signature": serde_json::Value::Null,
        "signed": false,
        "not_built_here": missing,
    })
}

/// The binary names the release build excludes by construction, read from the
/// CLI crate manifest: any `[[bin]]` whose `required-features` names
/// `test-hooks` cannot exist in a release build, so it cannot enter an archive.
///
/// This is derived rather than hard-coded so adding a new test-only binary
/// cannot silently escape the exclusion.
///
/// # Errors
/// Returns an error if the CLI manifest cannot be read or parsed.
pub fn test_hook_binary_names(workspace_root: &Path) -> Result<Vec<String>, String> {
    let manifest = workspace_root
        .join("crates")
        .join("godot-mcp-cli")
        .join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .map_err(|error| format!("read {}: {error}", manifest.display()))?;
    let value: toml::Value =
        toml::from_str(&text).map_err(|error| format!("parse {}: {error}", manifest.display()))?;
    let mut names = Vec::new();
    if let Some(bins) = value.get("bin").and_then(toml::Value::as_array) {
        for bin in bins {
            let gated = bin
                .get("required-features")
                .and_then(toml::Value::as_array)
                .is_some_and(|features| {
                    features
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .any(|feature| feature == "test-hooks")
                });
            if gated && let Some(name) = bin.get("name").and_then(toml::Value::as_str) {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

/// Write one archive plus the checksum manifest, the provenance document and the
/// dist-level `smoke.sh` into `dist`.
///
/// `smoke.sh` is written mode-executable because the frozen
/// `isolation-docker-clean-room` gate execs `/dist/smoke.sh` directly inside a
/// container that has no shell interpreter interposed for it.
///
/// # Errors
/// Returns an error if any artifact cannot be written.
pub fn write_dist(
    workspace_root: &Path,
    dist: &Path,
    version: &str,
    archives: &[ProducedArchive],
) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(dist).map_err(|error| format!("create {}: {error}", dist.display()))?;

    let archive_prefix = format!("godot-mcp-{version}-");
    for entry in
        std::fs::read_dir(dist).map_err(|error| format!("read {}: {error}", dist.display()))?
    {
        let entry = entry.map_err(|error| format!("read entry in {}: {error}", dist.display()))?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if name.starts_with(&archive_prefix)
            && (name.ends_with(".tar.gz") || name.ends_with(".zip"))
            && path.is_file()
        {
            std::fs::remove_file(&path)
                .map_err(|error| format!("remove stale archive {}: {error}", path.display()))?;
        }
    }

    let mut written = Vec::new();
    for produced in archives {
        let path = dist.join(&produced.file_name);
        std::fs::write(&path, &produced.bytes)
            .map_err(|error| format!("write {}: {error}", path.display()))?;
        written.push(path);
    }
    let checksums = dist.join(CHECKSUM_FILE);
    std::fs::write(&checksums, checksum_manifest(archives))
        .map_err(|error| format!("write {}: {error}", checksums.display()))?;
    written.push(checksums);

    let smoke = dist.join(SMOKE_FILE);
    std::fs::write(&smoke, smoke_script(workspace_root)?)
        .map_err(|error| format!("write {}: {error}", smoke.display()))?;
    set_executable(&smoke)?;
    written.push(smoke);

    let attestation = dist.join(PROVENANCE_FILE);
    let mut bytes = serde_json::to_vec_pretty(&provenance(workspace_root, version, archives))
        .map_err(|error| format!("serialize provenance: {error}"))?;
    bytes.push(b'\n');
    std::fs::write(&attestation, bytes)
        .map_err(|error| format!("write {}: {error}", attestation.display()))?;
    written.push(attestation);
    Ok(written)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("chmod 0755 {}: {error}", path.display()))
}

/// Windows carries no POSIX mode bit, so there is nothing to set. The archive
/// entry's recorded 0755 is what the isolation gate's Linux runner sees, and
/// `smoke.sh` asserts that mode against the manifest at gate time.
#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// The extracted package plus the manifest that pins its executable.
#[derive(Debug, Clone)]
pub struct HostPackage {
    /// Where the archive was written.
    pub archive_path: PathBuf,
    /// Where the archive was extracted.
    pub extracted_root: PathBuf,
    /// Package-relative path of the executable.
    pub executable_relative_path: String,
    /// Absolute path of the extracted executable.
    pub executable_path: PathBuf,
    /// SHA-256 of the extracted executable.
    pub executable_sha256: String,
    /// Where the host-package manifest was written.
    pub manifest_path: PathBuf,
}

/// Produce the Task-34 host package: write the archive, extract it, and emit the
/// manifest that pins the extracted executable by absolute path and digest.
///
/// Everything under `base` is recreated from scratch so a stale extraction can
/// never be resolved.
///
/// # Errors
/// Returns an error if the archive is not an extractable `tar.gz`, extraction
/// fails, or the manifest cannot be written.
pub fn write_host_package(
    base: &Path,
    produced: &ProducedArchive,
    version: &str,
) -> Result<HostPackage, String> {
    if produced.target.archive_extension() != "tar.gz" {
        return Err(format!(
            "host package requires an extractable tar.gz archive, got {}",
            produced.file_name
        ));
    }
    if base.exists() {
        std::fs::remove_dir_all(base)
            .map_err(|error| format!("clear {}: {error}", base.display()))?;
    }
    std::fs::create_dir_all(base).map_err(|error| format!("create {}: {error}", base.display()))?;
    // Every path the manifest records must be absolute: `qa-host-invoke` rejects a
    // relative `executable_path` outright and cross-checks the recorded extraction
    // root against the verified executable, so a relative root would never match.
    let base = &base
        .canonicalize()
        .map_err(|error| format!("canonicalize {}: {error}", base.display()))?;

    let archive_path = base.join(&produced.file_name);
    std::fs::write(&archive_path, &produced.bytes)
        .map_err(|error| format!("write {}: {error}", archive_path.display()))?;

    let extracted = base.join("extracted");
    std::fs::create_dir_all(&extracted)
        .map_err(|error| format!("create {}: {error}", extracted.display()))?;
    archive::extract_tar_gz(&produced.bytes, &extracted)?;

    let relative = format!(
        "{}/bin/{}",
        payload_root(version, produced.target),
        produced.target.executable_name()
    );
    let executable = extracted.join(&relative);
    let bytes = std::fs::read(&executable)
        .map_err(|error| format!("read {}: {error}", executable.display()))?;
    let digest = sha256_hex(&bytes);
    let absolute = executable
        .canonicalize()
        .map_err(|error| format!("canonicalize {}: {error}", executable.display()))?;

    let manifest_path = base.join("manifest.json");
    let document = serde_json::json!({
        "schema_version": PACKAGE_SCHEMA_VERSION,
        "executable_path": absolute.to_string_lossy(),
        "executable_sha256": digest,
        "archive_path": archive_path.to_string_lossy(),
        "archive_sha256": produced.sha256,
        "extracted_root": extracted.to_string_lossy(),
        "executable_relative_path": relative,
    });
    let mut manifest_bytes = serde_json::to_vec_pretty(&document)
        .map_err(|error| format!("serialize host-package manifest: {error}"))?;
    manifest_bytes.push(b'\n');
    std::fs::write(&manifest_path, manifest_bytes)
        .map_err(|error| format!("write {}: {error}", manifest_path.display()))?;

    Ok(HostPackage {
        archive_path,
        extracted_root: extracted,
        executable_relative_path: relative,
        executable_path: absolute,
        executable_sha256: digest,
        manifest_path,
    })
}

/// Payload paths every archive must carry. A missing one is a self-check
/// failure, so an archive cannot ship without its notices or its SBOM.
pub const REQUIRED_PAYLOAD_PATHS: &[&str] = &[
    "bridge/bridge_codec.gd",
    "examples/godot-mcp.toml",
    "licenses/LICENSE-MIT",
    "licenses/THIRD-PARTY-NOTICES.md",
    "sbom/godot-mcp.cdx.json",
    "schemas/bridge/protocol-v1.json",
    SMOKE_FILE,
];

/// Locate the package root that `executable` was launched from: the directory
/// above `bin/` that carries a `MANIFEST.json`.
///
/// Returns `None` for a binary that is not running out of an extracted package
/// (a plain `cargo run`, for instance), so the caller can say so instead of
/// guessing a root.
#[must_use]
pub fn locate_package_root(executable: &Path) -> Option<PathBuf> {
    let root = executable.parent()?.parent()?;
    root.join("MANIFEST.json")
        .is_file()
        .then(|| root.to_path_buf())
}

#[derive(Debug, Clone)]
pub struct SelfCheckReport {
    pub package_root: PathBuf,
    pub version: String,
    pub target: String,
    pub files_verified: usize,
    pub problems: Vec<String>,
}

impl SelfCheckReport {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }

    #[must_use]
    pub fn to_text(&self) -> String {
        let mut text = format!(
            "package {}\nversion {}\ntarget {}\nfiles_verified {}\n",
            self.package_root.display(),
            self.version,
            self.target,
            self.files_verified
        );
        if self.problems.is_empty() {
            text.push_str("result clean\n");
        } else {
            text.push_str("result rejected\n");
            for problem in &self.problems {
                text.push_str(&format!("problem {problem}\n"));
            }
        }
        text
    }
}

/// Verify an extracted package against its own `MANIFEST.json`.
///
/// Every recorded file must exist with the recorded size and digest; every
/// [`REQUIRED_PAYLOAD_PATHS`] entry must be recorded; no payload file may contain
/// a credential marker; and `bin/` must contain **exactly** the one declared
/// executable, which is what structurally forbids any extra (test-hook or
/// otherwise) binary from riding along inside an archive.
///
/// # Errors
/// Returns an error only when the manifest itself cannot be read or parsed.
/// Content problems are collected into [`SelfCheckReport::problems`] so the caller
/// reports all of them at once instead of stopping at the first.
pub fn self_check(package_root: &Path) -> Result<SelfCheckReport, String> {
    let manifest_path = package_root.join("MANIFEST.json");
    let bytes = read(&manifest_path)?;
    let document: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", manifest_path.display()))?;
    let version = document
        .get("version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let target = document
        .get("target")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let declared_executable = document
        .get("executable")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let declared_supervisor = document
        .get("supervisor")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();

    let markers = credential_markers();
    let mut problems = Vec::new();
    let mut recorded = Vec::new();
    let mut verified = 0_usize;
    let files = document
        .get("files")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for file in files {
        let Some(path) = file.get("path").and_then(serde_json::Value::as_str) else {
            problems.push("manifest entry without a path".to_string());
            continue;
        };
        recorded.push(path.to_string());
        let absolute = package_root.join(path);
        let Ok(live) = std::fs::read(&absolute) else {
            problems.push(format!("{path} is recorded but missing from the package"));
            continue;
        };
        let expected_digest = file
            .get("sha256")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if sha256_hex(&live) != expected_digest {
            problems.push(format!("{path} does not match its recorded SHA-256"));
        }
        let expected_size = file.get("size").and_then(serde_json::Value::as_u64);
        if expected_size != Some(live.len() as u64) {
            problems.push(format!("{path} does not match its recorded size"));
        }
        if contains_credential_marker(&live, &markers) {
            problems.push(format!("{path} contains a credential marker"));
        }
        verified += 1;
    }

    for required in REQUIRED_PAYLOAD_PATHS {
        if !recorded.iter().any(|path| path == required) {
            problems.push(format!("{required} is missing from the package manifest"));
        }
    }

    let binaries: Vec<&String> = recorded
        .iter()
        .filter(|path| path.starts_with("bin/"))
        .collect();
    let declared_binaries = [declared_executable.as_str(), declared_supervisor.as_str()];
    if binaries.len() != 2
        || !declared_binaries
            .iter()
            .all(|declared| binaries.iter().any(|path| path.as_str() == *declared))
    {
        problems.push(format!(
            "bin/ must contain exactly the declared executable {declared_executable:?} and supervisor {declared_supervisor:?}, found {binaries:?}"
        ));
    }

    Ok(SelfCheckReport {
        package_root: package_root.to_path_buf(),
        version,
        target,
        files_verified: verified,
        problems,
    })
}

/// The installer payload carried by an extracted package: the bridge codec and
/// the bridge wire schema, mapped into the project install root.
///
/// # Errors
/// Returns an error if a required package file is missing.
pub fn install_payload_from_package(
    extracted_root: &Path,
    version: &str,
    target: ReleaseTarget,
) -> Result<installer::InstallPayload, String> {
    let root = extracted_root.join(payload_root(version, target));
    let mut files = vec![installer::PayloadFile {
        path: format!("{}/bridge/bridge_codec.gd", installer::INSTALL_ROOT),
        bytes: read(&root.join("bridge").join("bridge_codec.gd"))?,
        mode: 0o644,
    }];
    files.push(installer::PayloadFile {
        path: format!(
            "{}/schemas/bridge/protocol-v1.json",
            installer::INSTALL_ROOT
        ),
        bytes: read(&root.join("schemas").join("bridge").join("protocol-v1.json"))?,
        mode: 0o644,
    });
    files.push(installer::PayloadFile {
        path: format!("{}/examples/godot-mcp.toml", installer::INSTALL_ROOT),
        bytes: read(&root.join("examples").join("godot-mcp.toml"))?,
        mode: 0o644,
    });
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(installer::InstallPayload { files })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_musl_buildability_requires_the_expected_compiler_on_path() {
        use std::os::unix::fs::PermissionsExt;

        let path = tempfile::tempdir().expect("temporary PATH");
        let target = ReleaseTarget::LinuxX86_64Musl;
        let error = target
            .require_buildable_on_host_with_path(Some(path.path().as_os_str()))
            .expect_err("an empty PATH cannot build bundled SQLite for musl");
        assert!(error.contains("x86_64-linux-musl-gcc"), "{error}");
        assert!(error.contains("musl-tools"), "{error}");
        assert!(error.contains("musl-dev"), "{error}");
        assert!(
            error.contains("rustup target add x86_64-unknown-linux-musl"),
            "{error}"
        );

        let compiler = path.path().join("x86_64-linux-musl-gcc");
        std::fs::write(&compiler, b"#!/bin/sh\nexit 0\n").expect("write compiler stand-in");
        std::fs::set_permissions(&compiler, std::fs::Permissions::from_mode(0o755))
            .expect("make compiler stand-in executable");
        target
            .require_buildable_on_host_with_path(Some(path.path().as_os_str()))
            .expect("an executable compiler stand-in satisfies the PATH probe");
    }

    #[test]
    fn credential_markers_are_the_expected_plaintext() {
        assert_eq!(
            credential_markers(),
            vec![
                "-----BEGIN RSA PRIVATE KEY".to_string(),
                "-----BEGIN OPENSSH PRIVATE KEY".to_string(),
                "-----BEGIN PRIVATE KEY".to_string(),
                "aws_secret_access_key".to_string(),
                "AKIAIOSFODNN7EXAMPLE".to_string(),
            ],
            "a mistyped reversal must not silently disable a credential check"
        );
    }

    #[test]
    fn a_planted_credential_is_rejected_before_any_archive_is_written() {
        let planted = credential_markers()
            .into_iter()
            .next()
            .expect("at least one marker");
        let entries = vec![ArchiveEntry {
            path: "examples/leaked.pem".to_string(),
            bytes: format!("{planted}\nMIIBOgIBAAJB\n").into_bytes(),
            mode: 0o644,
        }];
        let refusal =
            reject_credential_markers(&entries).expect_err("a planted credential is refused");
        assert!(refusal.contains("examples/leaked.pem"), "{refusal}");
    }
}
