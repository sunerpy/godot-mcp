//! `tooling.lock.json` loading and validation.
//!
//! Records, for the Rust toolchain and for each pinned cargo subcommand
//! (`cargo-deny`, `cargo-cyclonedx`), the exact version, the registry/source URL
//! it was installed from, the source-published package checksum, and the
//! executable identity appropriate to the source's reproducibility class.
//!
//! Three validation layers, each fail-closed:
//!
//! * [`ToolingLock::validate_populated`] — offline structural validation: every
//!   required tool present and every field well-formed.
//! * [`ToolingLock::validate_live`] — resolves the executable selected by the
//!   pinned toolchain / deterministic cargo-subcommand location at runtime,
//!   then proves that it is a real regular file, that its live SHA-256 matches
//!   the source-kind-specific anchor, and that invoking it reports the recorded
//!   exact version. Cargo-installed binaries use a same-run digest record rather
//!   than committing one machine's non-reproducible compiler output.
//! * [`ToolingLock::validate_selected_executables`] — the primitive a gate uses
//!   to prove that the executable it is about to invoke *is* the runtime-resolved
//!   selected file (path identity) and still hashes to the same local runtime
//!   record after invocation (live-hash identity).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The exact pinned Rust toolchain channel. Never the floating `stable` alias.
pub const TOOLCHAIN_CHANNEL: &str = "1.96.0";

/// How the executable bytes for one tool are produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    /// An upstream-published executable artifact whose bytes are reproducible.
    PrebuiltArtifact,
    /// A crates.io package compiled locally by `cargo install`.
    CargoInstall,
}

/// One pinned tool entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolEntry {
    /// Exact semantic version (e.g. `1.96.0`, `0.18.5`).
    pub version: String,
    /// Registry / distribution source URL the artifact was installed from.
    pub source_url: String,
    /// Explicit executable-source reproducibility class.
    pub source_kind: SourceKind,
    /// Source-published package/artifact checksum (hex SHA-256).
    pub package_checksum: String,
    /// Upstream prebuilt executable SHA-256; absent for locally compiled tools.
    #[serde(default)]
    pub executable_sha256: Option<String>,
}

/// The whole `tooling.lock.json` document: a schema version plus a map of tool
/// name to entry. The three required tools are `rust-toolchain`, `cargo-deny`
/// and `cargo-cyclonedx`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolingLock {
    /// Schema version for forward compatibility.
    pub schema_version: String,
    /// Tool name -> pinned entry.
    pub tools: BTreeMap<String, ToolEntry>,
}

/// The three tool keys every valid `tooling.lock.json` must record.
pub const REQUIRED_TOOLS: &[&str] = &["rust-toolchain", "cargo-deny", "cargo-cyclonedx"];

fn is_hex_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn expected_source_kind(tool: &str) -> Result<SourceKind, String> {
    match tool {
        "rust-toolchain" => Ok(SourceKind::PrebuiltArtifact),
        "cargo-deny" | "cargo-cyclonedx" => Ok(SourceKind::CargoInstall),
        other => Err(format!("unknown tool: {other}")),
    }
}

fn source_url_matches(tool: &str, entry: &ToolEntry) -> bool {
    match entry.source_kind {
        SourceKind::PrebuiltArtifact => {
            entry
                .source_url
                .starts_with("https://static.rust-lang.org/dist/")
                && entry.source_url.contains(&entry.version)
                && entry.source_url.ends_with(".tar.xz")
        }
        SourceKind::CargoInstall => {
            entry.source_url
                == format!(
                    "https://crates.io/api/v1/crates/{tool}/{}/download",
                    entry.version
                )
        }
    }
}

/// Compute the lowercase-hex SHA-256 of a file's bytes.
///
/// # Errors
/// Returns an error if the file cannot be read.
pub fn sha256_hex_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(sha256_hex_bytes(&bytes))
}

/// Compute the lowercase-hex SHA-256 of a byte slice.
#[must_use]
pub fn sha256_hex_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Compare two paths for identity, canonicalizing both when possible so that
/// equivalent spellings of the same file compare equal, while genuinely
/// different files never do.
fn paths_identical(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// The directory pinned cargo subcommands are installed into
/// (`$CARGO_HOME/bin`, else `$HOME/.cargo/bin`).
fn cargo_bin_dir() -> Result<PathBuf, String> {
    if let Ok(cargo_home) = std::env::var("CARGO_HOME")
        && !cargo_home.trim().is_empty()
    {
        return Ok(PathBuf::from(cargo_home).join("bin"));
    }
    let home =
        std::env::var("HOME").map_err(|_| "neither CARGO_HOME nor HOME is set".to_string())?;
    Ok(PathBuf::from(home).join(".cargo").join("bin"))
}

/// Deterministically resolve the executable a gate would select for `tool`.
///
/// * `rust-toolchain` resolves `rustc` strictly through the pinned
///   [`TOOLCHAIN_CHANNEL`] via `rustup which --toolchain <channel> rustc`
///   (never the floating `stable` alias).
/// * `cargo-deny` / `cargo-cyclonedx` resolve deterministically to
///   `$CARGO_HOME/bin/<tool>`.
///
/// # Errors
/// Returns an error if the tool is unknown or resolution fails.
pub fn resolve_selected_executable(tool: &str) -> Result<PathBuf, String> {
    match tool {
        "rust-toolchain" => {
            let out = Command::new("rustup")
                .args(["which", "--toolchain", TOOLCHAIN_CHANNEL, "rustc"])
                .output()
                .map_err(|e| format!("invoke rustup which: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "rustup which --toolchain {TOOLCHAIN_CHANNEL} rustc failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if path.is_empty() {
                return Err("rustup which returned an empty path".to_string());
            }
            Ok(PathBuf::from(path))
        }
        "cargo-deny" | "cargo-cyclonedx" => Ok(cargo_bin_dir()?.join(tool)),
        other => Err(format!("unknown tool: {other}")),
    }
}

/// The argument vector that makes `tool`'s executable print its version.
fn version_query_args(tool: &str) -> &'static [&'static str] {
    match tool {
        // cargo-cyclonedx is a cargo subcommand; it only reports its version
        // through the `cyclonedx --version` form.
        "cargo-cyclonedx" => &["cyclonedx", "--version"],
        _ => &["--version"],
    }
}

fn validate_live_executable(tool: &str, entry: &ToolEntry, selected: &Path) -> Result<(), String> {
    if !selected.is_file() {
        return Err(format!(
            "{tool}: selected executable is not a regular file: {}",
            selected.display()
        ));
    }

    let runtime_sha256 = sha256_hex_file(selected)?;
    match entry.source_kind {
        SourceKind::PrebuiltArtifact => {
            let expected = entry.executable_sha256.as_deref().ok_or_else(|| {
                format!("{tool}.executable_sha256 is required for a prebuilt artifact")
            })?;
            if runtime_sha256 != expected {
                return Err(format!(
                    "{tool}: live executable SHA-256 {runtime_sha256} does not match recorded {expected}"
                ));
            }
        }
        SourceKind::CargoInstall => {}
    }

    let out = Command::new(selected)
        .args(version_query_args(tool))
        .output()
        .map_err(|e| format!("{tool}: invoke {}: {e}", selected.display()))?;
    if !out.status.success() {
        return Err(format!(
            "{tool}: version query on {} failed: {}",
            selected.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let reported = String::from_utf8_lossy(&out.stdout);
    if reported.split_whitespace().nth(1) != Some(entry.version.as_str()) {
        return Err(format!(
            "{tool}: reported version {:?} does not equal recorded {}",
            reported.trim(),
            entry.version
        ));
    }

    let after_invocation_sha256 = sha256_hex_file(selected)?;
    if after_invocation_sha256 != runtime_sha256 {
        return Err(format!(
            "{tool}: executable SHA-256 changed during validation from {runtime_sha256} to {after_invocation_sha256}"
        ));
    }
    Ok(())
}

impl ToolingLock {
    /// Load and parse `tooling.lock.json` from `path`.
    ///
    /// # Errors
    /// Returns an error if the file cannot be read or is not valid JSON.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
    }

    /// Validate that every required tool is present and every field is a
    /// non-empty, well-formed value. Source kinds determine whether a committed
    /// executable digest is required or forbidden.
    ///
    /// # Errors
    /// Returns a descriptive error naming the first offending field. This makes
    /// a corrupted/unpopulated entry fail closed.
    pub fn validate_populated(&self) -> Result<(), String> {
        if self.schema_version.trim().is_empty() {
            return Err("schema_version is empty".to_string());
        }
        for tool in REQUIRED_TOOLS {
            let entry = self
                .tools
                .get(*tool)
                .ok_or_else(|| format!("missing required tool entry: {tool}"))?;
            if entry.version.trim().is_empty() {
                return Err(format!("{tool}.version is empty"));
            }
            if entry.source_url.trim().is_empty() {
                return Err(format!("{tool}.source_url is empty"));
            }
            if !entry.source_url.starts_with("https://") {
                return Err(format!("{tool}.source_url is not an https URL"));
            }
            let expected_kind = expected_source_kind(tool)?;
            if entry.source_kind != expected_kind {
                return Err(format!(
                    "{tool}.source_kind {:?} does not match required {:?}",
                    entry.source_kind, expected_kind
                ));
            }
            if !source_url_matches(tool, entry) {
                return Err(format!(
                    "{tool}.source_url does not match its source kind and exact version"
                ));
            }
            if !is_hex_sha256(&entry.package_checksum) {
                return Err(format!(
                    "{tool}.package_checksum is not a lowercase hex SHA-256"
                ));
            }
            match entry.source_kind {
                SourceKind::PrebuiltArtifact => {
                    let executable_sha256 =
                        entry.executable_sha256.as_deref().ok_or_else(|| {
                            format!("{tool}.executable_sha256 is required for a prebuilt artifact")
                        })?;
                    if !is_hex_sha256(executable_sha256) {
                        return Err(format!(
                            "{tool}.executable_sha256 is not a lowercase hex SHA-256"
                        ));
                    }
                }
                SourceKind::CargoInstall => {
                    if entry.executable_sha256.is_some() {
                        return Err(format!(
                            "{tool} must not record executable_sha256 for a cargo-install source"
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Resolve every recorded tool through its deterministic pinned selection,
    /// then prove live and fail-closed that it is a real regular file whose
    /// SHA-256 matches the recorded value and whose self-report matches the
    /// recorded exact version.
    ///
    /// # Errors
    /// Returns a descriptive error for the first tool whose selected path,
    /// existence, live hash or reported version does not match the lock.
    pub fn validate_live(&self) -> Result<(), String> {
        // Structural gate first so a malformed lock fails before any process is
        // spawned.
        self.validate_populated()?;
        for tool in REQUIRED_TOOLS {
            let entry = &self.tools[*tool];
            let selected = resolve_selected_executable(tool)?;

            validate_live_executable(tool, entry, &selected)?;
        }
        Ok(())
    }

    /// Prove that each requested tool's *selected* executable — the one a gate
    /// is about to invoke — is exactly the deterministically runtime-resolved
    /// file and still hashes to the recorded value.
    ///
    /// This is the primitive underpinning the selected-executable contract: a
    /// gate resolves the executable it will run, then calls this to prove that
    /// executable is the pinned one before use. Two independent checks fail
    /// closed:
    ///
    /// * **path identity** — the caller-selected path must be the same file as
    ///   the runtime resolver selects (so a *different* binary is rejected even
    ///   if its bytes happen to hash identically), and
    /// * **live-hash identity** — prebuilt bytes must match their committed
    ///   upstream anchor; locally compiled bytes are recorded before invocation
    ///   and must remain identical afterward.
    ///
    /// # Errors
    /// Returns a descriptive error for the first requested tool whose entry is
    /// missing, whose selected path is absent from `selected`, whose selected
    /// path is not the runtime-resolved file, or whose live hash does not match.
    pub fn validate_selected_executables(
        &self,
        selected: &BTreeMap<String, PathBuf>,
        tools: &[&str],
    ) -> Result<(), String> {
        self.validate_populated()?;
        for tool in tools {
            let entry = self
                .tools
                .get(*tool)
                .ok_or_else(|| format!("missing required tool entry: {tool}"))?;
            let selected_path = selected
                .get(*tool)
                .ok_or_else(|| format!("{tool}: no selected executable supplied"))?;

            let resolved = resolve_selected_executable(tool)?;
            if !paths_identical(selected_path, &resolved) {
                return Err(format!(
                    "{tool}: selected executable {} is not the runtime-resolved {}",
                    selected_path.display(),
                    resolved.display()
                ));
            }
            validate_live_executable(tool, entry, selected_path)?;
        }
        Ok(())
    }
}
