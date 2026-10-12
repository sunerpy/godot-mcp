//! The release-gates CI harness.
//!
//! Godot matrix runs, GPU/display jobs, the strong Docker isolation check and
//! network-dependent package/deployment smokes cannot execute inside this
//! process: they need a real display, a real container runtime, a real macOS or
//! Windows runner and a real network. The harness therefore does not pretend to
//! run them. It **binds** them:
//!
//! * [`required_checks`] derives the complete required-check catalogue — every
//!   name, and every exact command — from the committed
//!   `tests/fixtures/godot-releases.lock.json` plus a fixed set of
//!   workspace-level gates. No version label, URL or digest is written here.
//! * Each CI job writes an [`ObservationRecord`] (its captured exit code plus the
//!   runner identity it ran on) into `<candidate>/ci/observations/*.json`.
//! * [`run`] joins catalogue to observations. A required check whose observation
//!   is absent, `skipped` or `unavailable` becomes an explicit **failed** entry
//!   with a distinct nonzero sentinel exit code — never a pass, never a silent
//!   skip. So does a command mismatch, a source-identity mismatch, or a recorded
//!   nonzero exit.
//!
//! ## Qualifying emission
//!
//! `QUALIFYING=0` is an in-progress observation: it may only write a
//! caller-supplied temporary output, and is structurally forbidden from writing
//! anywhere under `.omo/evidence/`. `QUALIFYING=1` is the single final emission
//! and is rejected unless the caller proves it holds Todo 37's producer-owned
//! orchestration context: an environment token equal to
//! [`producer_context_token`] of the frozen candidate's commit and tree. That
//! token cannot be a copied literal — it is derived from the candidate identity
//! that only exists after candidate freeze — and the canonical output path is
//! additionally pinned. This module never invokes itself with `QUALIFYING=1`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::lock_matrix::{
    ALL_PLATFORMS, EDITOR_KIND, GodotReleasesLock, LINUX_PLATFORM, MACOS_PLATFORM, MatrixEntry,
    WINDOWS_PLATFORM,
};
use super::schema;

/// The exact variables `make release-gates-harness` requires, in order.
pub const REQUIRED_VARIABLES: [&str; 7] = [
    "CANDIDATE_WORKTREE",
    "SOURCE_COMMIT",
    "SOURCE_TREE",
    "ARCHIVES_DIR",
    "OUTPUT",
    "OUTPUT_SHA",
    "QUALIFYING",
];

/// The one canonical final-gate path. Todo 37 alone writes it.
pub const CANONICAL_FINAL_OUTPUT: &str =
    ".omo/evidence/godot-mcp-server-architecture/external/release-gates.json";

/// The sidecar of the one canonical final-gate path.
pub const CANONICAL_FINAL_OUTPUT_SHA: &str =
    ".omo/evidence/godot-mcp-server-architecture/external/release-gates.sha256";

/// Evidence tree a non-qualifying run may never write into.
pub const EVIDENCE_PREFIX: &str = ".omo/evidence/";

/// Environment variable carrying Todo 37's producer-owned orchestration context.
pub const PRODUCER_CONTEXT_ENV: &str = "RELEASE_GATES_PRODUCER_CONTEXT";

/// Domain separator of the producer-context token.
pub const PRODUCER_CONTEXT_DOMAIN: &str = "godot-mcp release-gates producer context v1";

/// Optional environment override for the runner's OS version.
pub const RUNNER_OS_VERSION_ENV: &str = "RELEASE_GATES_RUNNER_OS_VERSION";

/// Optional environment override for the runner's container image digest.
pub const RUNNER_IMAGE_DIGEST_ENV: &str = "RELEASE_GATES_RUNNER_IMAGE_DIGEST";

/// Directory, relative to the candidate worktree, CI jobs deposit observations in.
pub const OBSERVATIONS_DIR: &str = "ci/observations";

/// The OIDC/JWKS fixture declaration, relative to the candidate worktree.
pub const OIDC_FIXTURE: &str = "ci/oidc/openid-configuration.json";

/// Published checksum manifest expected inside `ARCHIVES_DIR`.
pub const ARCHIVE_CHECKSUMS: &str = "SHA256SUMS.txt";

/// Sentinel exit code recorded for a required check with no observation at all.
pub const EXIT_MISSING: i64 = 64;

/// Sentinel exit code recorded for a check its runner reported as skipped.
pub const EXIT_SKIPPED: i64 = 65;

/// Sentinel exit code recorded for a check whose environment was unavailable.
pub const EXIT_UNAVAILABLE: i64 = 66;

/// Sentinel exit code recorded when an observation contradicts its binding
/// (command, source commit/tree or archive digest).
pub const EXIT_BINDING_MISMATCH: i64 = 67;

/// How a check was observed on its runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Observation {
    /// The command actually ran and its exit code was captured.
    Executed,
    /// No observation was produced for this check.
    Missing,
    /// The runner deliberately did not run it.
    Skipped,
    /// The runner could not run it (no GPU, no display, no credential, no
    /// container runtime, no network).
    Unavailable,
}

impl Observation {
    /// The wire label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Executed => "executed",
            Self::Missing => "missing",
            Self::Skipped => "skipped",
            Self::Unavailable => "unavailable",
        }
    }

    /// The nonzero sentinel a non-executed observation is recorded with, so it
    /// can never present itself as a pass.
    #[must_use]
    pub const fn sentinel_exit(self) -> Option<i64> {
        match self {
            Self::Executed => None,
            Self::Missing => Some(EXIT_MISSING),
            Self::Skipped => Some(EXIT_SKIPPED),
            Self::Unavailable => Some(EXIT_UNAVAILABLE),
        }
    }
}

/// The environment a check was observed on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerIdentity {
    /// Operating system name.
    pub os: String,
    /// Operating system version.
    pub os_version: String,
    /// CPU architecture.
    pub arch: String,
    /// Container image digest, when the job ran in a pinned image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_digest: Option<String>,
    /// Digest of the workflow definitions that produced the job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_hash: Option<String>,
}

impl RunnerIdentity {
    /// The identity of the machine this harness is running on.
    #[must_use]
    pub fn host() -> Self {
        Self {
            os: std::env::consts::OS.to_owned(),
            os_version: host_os_version(),
            arch: std::env::consts::ARCH.to_owned(),
            image_digest: std::env::var(RUNNER_IMAGE_DIGEST_ENV)
                .ok()
                .filter(|value| !value.trim().is_empty()),
            workflow_hash: None,
        }
    }

    fn is_well_formed(&self) -> bool {
        !self.os.trim().is_empty()
            && !self.os_version.trim().is_empty()
            && !self.arch.trim().is_empty()
    }
}

fn host_os_version() -> String {
    if let Ok(value) = std::env::var(RUNNER_OS_VERSION_ENV)
        && !value.trim().is_empty()
    {
        return value;
    }
    for probe in ["/proc/sys/kernel/osrelease", "/etc/os-release"] {
        if let Ok(text) = std::fs::read_to_string(probe)
            && let Some(first) = text.lines().next()
            && !first.trim().is_empty()
        {
            return first.trim().to_owned();
        }
    }
    "unknown".to_owned()
}

/// What kind of external environment a required check needs. Recorded so a
/// reader can see at a glance why the harness cannot run it in-process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckKind {
    /// A workspace gate runnable on any runner.
    Workspace,
    /// An exact-locked-Godot-binary matrix run.
    GodotMatrix,
    /// A job needing a real GPU and display.
    GpuDisplay,
    /// The strong Docker isolation check.
    Isolation,
    /// A network-dependent packaged smoke, bound to a published archive.
    PackagedSmoke,
    /// A network-dependent deployment/auth smoke.
    DeploymentSmoke,
}

impl CheckKind {
    /// Stable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::GodotMatrix => "godot-matrix",
            Self::GpuDisplay => "gpu-display",
            Self::Isolation => "isolation",
            Self::PackagedSmoke => "packaged-smoke",
            Self::DeploymentSmoke => "deployment-smoke",
        }
    }
}

/// One required external check, bound to its exact command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckSpec {
    /// Stable check name; also the observation file's `name`.
    pub name: String,
    /// Whether the release may proceed without it. Every check the harness
    /// derives is required; the field exists so the emitted document states it
    /// explicitly rather than implying it.
    pub required: bool,
    /// The exact command the runner must execute.
    pub command: String,
    /// Which external environment it needs.
    pub kind: CheckKind,
    /// For a packaged smoke, the release archive file name the check is bound to.
    pub archive_file_name: Option<String>,
}

/// The pinned toolchain every workspace gate runs under. Mirrors the Makefile.
pub const TOOLCHAIN: &str = "1.96.0";

/// Workspace-level gates, in a fixed order. `advisories` is separated from
/// `bans licenses sources` deliberately: the advisory database is an external,
/// moving input, so it gets its own required entry whose failure is visible
/// instead of being folded into the license gate.
fn workspace_checks() -> Vec<CheckSpec> {
    let cargo = format!("cargo +{TOOLCHAIN}");
    let gates: [(&str, String); 11] = [
        ("format", format!("{cargo} fmt --all -- --check")),
        (
            "lint",
            format!("{cargo} clippy --workspace --all-targets --all-features -- -D warnings"),
        ),
        ("unit", format!("{cargo} test --workspace --all-features")),
        (
            "contract",
            format!("{cargo} test -p godot-mcp-domain --all-features --test contract_snapshots"),
        ),
        (
            "golden",
            format!("{cargo} test -p godot-mcp-bridge --all-features --test bridge_protocol"),
        ),
        (
            "build-locked",
            format!("{cargo} build --workspace --all-features --locked"),
        ),
        (
            "license",
            format!("{cargo} deny check bans licenses sources"),
        ),
        ("advisory", format!("{cargo} deny check advisories")),
        ("sbom", format!("{cargo} cyclonedx --format json")),
        (
            "release-graph-audit",
            format!("{cargo} test -p godot-mcp-cli --all-features --test release_graph_audit"),
        ),
        (
            "reproducibility",
            format!("RUSTUP_TOOLCHAIN={TOOLCHAIN} make reproducibility"),
        ),
    ];
    gates
        .into_iter()
        .map(|(name, command)| CheckSpec {
            name: name.to_owned(),
            required: true,
            command,
            kind: CheckKind::Workspace,
            archive_file_name: None,
        })
        .collect()
}

/// The release-graph audit's own check name. It is a separate required entry and
/// is never satisfied by an isolated non-shipping gdext reference lane.
pub const RELEASE_GRAPH_AUDIT_CHECK: &str = "release-graph-audit";

/// The name an isolated non-shipping gdext reference lane would report under. It
/// is deliberately absent from the required catalogue: a passing reference lane
/// contributes nothing to the release-graph audit's required entry.
pub const GDEXT_REFERENCE_LANE_CHECK: &str = "gdext-reference-lane";

/// The check name of the deterministic local OIDC/JWKS auth smoke.
pub const AUTH_SMOKE_CHECK: &str = "deployment-smoke-oidc-local-jwks";

/// The command the exact-locked-Godot-binary matrix runs. Every value it needs
/// comes from the matrix row, which came from the lock.
fn godot_matrix_command(entry: &MatrixEntry) -> String {
    format!(
        "RUSTUP_TOOLCHAIN={TOOLCHAIN} make godot-test \
         GODOT_ARTIFACT_URL={} GODOT_ARTIFACT_SHA256={} GODOT_ARTIFACT_NAME={}",
        entry.download_url, entry.sha256, entry.artifact_name
    )
}

/// Derive the complete required-check catalogue.
///
/// # Errors
/// Returns an error if the lock is invalid or a matrix row cannot be derived.
pub fn required_checks(lock: &GodotReleasesLock, version: &str) -> Result<Vec<CheckSpec>, String> {
    let mut checks = workspace_checks();

    for entry in lock.engine_editor_matrix()? {
        let scope = match entry.platform.as_str() {
            LINUX_PLATFORM => "linux-editor",
            ALL_PLATFORMS => "export-templates",
            other => return Err(format!("unexpected engine/editor platform `{other}`")),
        };
        checks.push(CheckSpec {
            name: format!("godot-matrix-{scope}-{}", entry.version),
            required: true,
            command: godot_matrix_command(&entry),
            kind: CheckKind::GodotMatrix,
            archive_file_name: None,
        });
    }

    // The GPU/display job runs the newest locked editor windowed (never
    // `--headless`), because that is the only way a real display and driver are
    // exercised. Its version comes from the lock, not from this source.
    let newest = lock.newest()?;
    let newest_linux_editor = lock
        .engine_editor_matrix()?
        .into_iter()
        .find(|entry| {
            entry.version == newest.version
                && entry.platform == LINUX_PLATFORM
                && entry.artifact_kind == EDITOR_KIND
        })
        .ok_or_else(|| "the newest release records no Linux editor artifact".to_owned())?;
    checks.push(CheckSpec {
        name: format!("gpu-display-editor-{}", newest_linux_editor.version),
        required: true,
        command: format!(
            "RUSTUP_TOOLCHAIN={TOOLCHAIN} make godot-test \
             GODOT_ARTIFACT_URL={} GODOT_ARTIFACT_SHA256={} GODOT_DISPLAY_REQUIRED=1",
            newest_linux_editor.download_url, newest_linux_editor.sha256
        ),
        kind: CheckKind::GpuDisplay,
        archive_file_name: None,
    });

    checks.push(CheckSpec {
        name: "isolation-docker-clean-room".to_owned(),
        required: true,
        command: "docker run --rm --network=none --read-only --cap-drop=ALL \
                  --security-opt=no-new-privileges \
                  --mount type=bind,source=$PWD/target/release-dist,target=/dist,readonly \
                  $RELEASE_GATES_ISOLATION_IMAGE /dist/smoke.sh"
            .to_owned(),
        kind: CheckKind::Isolation,
        archive_file_name: None,
    });

    for entry in lock.packaged_smoke_matrix()? {
        let archive = packaged_archive_file_name(&entry.platform, version)?;
        checks.push(CheckSpec {
            name: format!("packaged-smoke-{}-{}", entry.platform, entry.version),
            required: true,
            command: format!(
                "RUSTUP_TOOLCHAIN={TOOLCHAIN} ./scripts/packaged-smoke.sh --archive {archive} \
                 --godot-url {} --godot-sha256 {}",
                entry.download_url, entry.sha256
            ),
            kind: CheckKind::PackagedSmoke,
            archive_file_name: Some(archive),
        });
    }

    checks.push(CheckSpec {
        name: AUTH_SMOKE_CHECK.to_owned(),
        required: true,
        command: format!(
            "{} test -p godot-mcp-server --all-features --test oidc_resource_server",
            format_args!("cargo +{TOOLCHAIN}")
        ),
        kind: CheckKind::DeploymentSmoke,
        archive_file_name: None,
    });

    Ok(checks)
}

/// The release archive file name a Godot platform's packaged smoke consumes.
///
/// The mapping is explicit rather than string-derived so a new Godot platform
/// cannot silently acquire an archive name nobody publishes.
///
/// # Errors
/// Returns an error for a platform with no published release target.
pub fn packaged_archive_file_name(platform: &str, version: &str) -> Result<String, String> {
    let target = match platform {
        LINUX_PLATFORM => crate::package::ReleaseTarget::LinuxX86_64Musl,
        MACOS_PLATFORM => crate::package::ReleaseTarget::MacOsAarch64,
        WINDOWS_PLATFORM => crate::package::ReleaseTarget::WindowsX86_64Msvc,
        other => {
            return Err(format!(
                "no published release target for platform `{other}`"
            ));
        }
    };
    Ok(format!(
        "{}.{}",
        crate::package::payload_root(version, target),
        target.archive_extension()
    ))
}

/// What one CI job deposits for one check.
#[derive(Debug, Clone)]
pub struct ObservationRecord {
    /// Which check this observes.
    pub name: String,
    /// The non-empty command the job actually ran. It must equal the bound
    /// command exactly.
    pub command: String,
    /// How it was observed.
    pub observation: Observation,
    /// The captured exit code, when the command ran.
    pub exit_code: Option<i64>,
    /// The runner it ran on.
    pub runner_identity: RunnerIdentity,
    /// Candidate commit the job tested.
    pub source_commit: String,
    /// Candidate tree the job tested.
    pub source_tree: String,
    /// Digest of the archive the job actually consumed, required for an
    /// archive-bound check.
    pub archive_sha256: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservationRecordInput {
    name: String,
    #[serde(default)]
    command: Option<String>,
    observation: Observation,
    #[serde(default)]
    exit_code: Option<i64>,
    runner_identity: RunnerIdentity,
    source_commit: String,
    source_tree: String,
    #[serde(default)]
    archive_sha256: Option<String>,
}

impl<'de> Deserialize<'de> for ObservationRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let input = ObservationRecordInput::deserialize(deserializer)?;
        Ok(Self {
            name: input.name,
            command: input.command.unwrap_or_default(),
            observation: input.observation,
            exit_code: input.exit_code,
            runner_identity: input.runner_identity,
            source_commit: input.source_commit,
            source_tree: input.source_tree,
            archive_sha256: input.archive_sha256,
        })
    }
}

/// Load every observation under `<candidate>/ci/observations`.
///
/// A file may hold one record or an array of records, so a job may report one
/// check or a whole matrix leg.
///
/// # Errors
/// Returns an error if the directory cannot be read, a file cannot be parsed, or
/// two files claim the same check name.
pub fn load_observations(
    candidate_worktree: &Path,
) -> Result<BTreeMap<String, ObservationRecord>, String> {
    let directory = candidate_worktree.join(OBSERVATIONS_DIR);
    if !directory.is_dir() {
        return Err(format!(
            "{} is not a directory; no CI observation was deposited",
            directory.display()
        ));
    }
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&directory)
        .map_err(|error| format!("read {}: {error}", directory.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();

    let mut out: BTreeMap<String, ObservationRecord> = BTreeMap::new();
    for path in paths {
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|error| format!("parse {}: {error}", path.display()))?;
        let records: Vec<ObservationRecord> = if value.is_array() {
            serde_json::from_value(value)
        } else {
            serde_json::from_value(value).map(|one: ObservationRecord| vec![one])
        }
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
        for record in records {
            if let Some(previous) = out.insert(record.name.clone(), record) {
                return Err(format!(
                    "check `{}` is observed more than once (second sighting in {})",
                    previous.name,
                    path.display()
                ));
            }
        }
    }
    Ok(out)
}

/// One emitted check entry plus the harness's verdict on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckEntry {
    /// Check name.
    pub name: String,
    /// Whether the release requires it.
    pub required: bool,
    /// The exact bound command.
    pub command: String,
    /// How it was observed.
    pub observation: Observation,
    /// Exit code recorded in the document. Non-executed observations always
    /// carry a nonzero sentinel.
    pub exit_code: i64,
    /// The runner it was observed on.
    pub runner_identity: RunnerIdentity,
    /// Candidate commit.
    pub source_commit: String,
    /// Candidate tree.
    pub source_tree: String,
    /// Archive digest, for an archive-bound check.
    pub archive_sha256: Option<String>,
    /// Why this entry failed, when it did.
    pub failure: Option<String>,
}

impl CheckEntry {
    /// Whether this entry passed. Only an `executed` observation with exit code
    /// `0` and no binding failure passes.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.failure.is_none() && self.observation == Observation::Executed && self.exit_code == 0
    }

    fn to_json(&self) -> Value {
        let mut object = serde_json::Map::new();
        object.insert("name".to_owned(), Value::String(self.name.clone()));
        object.insert("required".to_owned(), Value::Bool(self.required));
        object.insert("command".to_owned(), Value::String(self.command.clone()));
        object.insert(
            "observation".to_owned(),
            Value::String(self.observation.label().to_owned()),
        );
        object.insert("exit_code".to_owned(), Value::from(self.exit_code));
        object.insert(
            "runner_identity".to_owned(),
            runner_identity_json(&self.runner_identity),
        );
        object.insert(
            "source_commit".to_owned(),
            Value::String(self.source_commit.clone()),
        );
        object.insert(
            "source_tree".to_owned(),
            Value::String(self.source_tree.clone()),
        );
        if let Some(digest) = &self.archive_sha256 {
            object.insert("archive_sha256".to_owned(), Value::String(digest.clone()));
        }
        Value::Object(object)
    }
}

fn runner_identity_json(identity: &RunnerIdentity) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("os".to_owned(), Value::String(identity.os.clone()));
    object.insert(
        "os_version".to_owned(),
        Value::String(identity.os_version.clone()),
    );
    object.insert("arch".to_owned(), Value::String(identity.arch.clone()));
    if let Some(digest) = &identity.image_digest {
        object.insert("image_digest".to_owned(), Value::String(digest.clone()));
    }
    if let Some(hash) = &identity.workflow_hash {
        object.insert("workflow_hash".to_owned(), Value::String(hash.clone()));
    }
    Value::Object(object)
}

/// Whether a run is a Todo-35 observation or Todo 37's single final emission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qualifying {
    /// `QUALIFYING=0`.
    No,
    /// `QUALIFYING=1`.
    Yes,
}

impl Qualifying {
    /// Parse the `QUALIFYING` variable. Only `0` and `1` are accepted; anything
    /// else is a hard error rather than a default.
    ///
    /// # Errors
    /// Returns an error for any other value.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "0" => Ok(Self::No),
            "1" => Ok(Self::Yes),
            other => Err(format!(
                "QUALIFYING must be `0` or `1`, got `{other}`; the harness never guesses"
            )),
        }
    }

    /// Whether this is the qualifying emission.
    #[must_use]
    pub const fn is_qualifying(self) -> bool {
        matches!(self, Self::Yes)
    }
}

/// Everything `make release-gates-harness` supplies.
#[derive(Debug, Clone)]
pub struct HarnessRequest {
    /// The candidate worktree whose CI observations are being joined.
    pub candidate_worktree: PathBuf,
    /// The candidate commit under test.
    pub source_commit: String,
    /// The candidate tree under test.
    pub source_tree: String,
    /// Directory holding the published release archives and their checksums.
    pub archives_dir: PathBuf,
    /// Where to write the gates document.
    pub output: PathBuf,
    /// Where to write its strict sidecar.
    pub output_sha: PathBuf,
    /// Observation or final emission.
    pub qualifying: Qualifying,
}

/// Compute the producer-context token that authorises a qualifying emission of
/// the candidate identified by `source_commit` / `source_tree`.
///
/// The token is derived from the frozen candidate identity, so it cannot be a
/// literal copied ahead of freeze: whoever emits final gates must already know
/// the exact commit and tree they froze.
#[must_use]
pub fn producer_context_token(source_commit: &str, source_tree: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(PRODUCER_CONTEXT_DOMAIN.as_bytes());
    hasher.update(b"\n");
    hasher.update(source_commit.as_bytes());
    hasher.update(b"\n");
    hasher.update(source_tree.as_bytes());
    hasher.update(b"\n");
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

/// Lowercase-hex SHA-256 of a byte slice.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn is_lower_hex_64(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn normalize(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Decide whether this invocation is allowed to write where it says it will.
///
/// Two structural rules, neither of them a documentation convention:
///
/// * a non-qualifying run may not write anywhere under `.omo/evidence/`, so it
///   cannot produce the canonical final gate or a look-alike beside it;
/// * a qualifying run must present Todo 37's producer-owned orchestration context
///   for the exact candidate it names, and must write the canonical paths.
///
/// # Errors
/// Returns an error describing the refusal. On any error the caller must write
/// nothing at all.
pub fn authorize(request: &HarnessRequest, producer_context: Option<&str>) -> Result<(), String> {
    let output = normalize(&request.output);
    let output_sha = normalize(&request.output_sha);
    match request.qualifying {
        Qualifying::No => {
            for (label, path) in [("OUTPUT", &output), ("OUTPUT_SHA", &output_sha)] {
                if path.contains(EVIDENCE_PREFIX) {
                    return Err(format!(
                        "refusing a non-qualifying run that writes {label} under \
                         `{EVIDENCE_PREFIX}`: only Todo 37's qualifying emission may write there \
                         ({path})"
                    ));
                }
            }
            Ok(())
        }
        Qualifying::Yes => {
            let expected = producer_context_token(&request.source_commit, &request.source_tree);
            let Some(presented) = producer_context.map(str::trim).filter(|it| !it.is_empty())
            else {
                return Err(format!(
                    "QUALIFYING=1 is rejected: no {PRODUCER_CONTEXT_ENV} was presented. The \
                     qualifying emission is Todo 37's alone and requires its producer-owned \
                     orchestration context for the frozen candidate"
                ));
            };
            if presented != expected {
                return Err(format!(
                    "QUALIFYING=1 is rejected: the presented {PRODUCER_CONTEXT_ENV} does not \
                     authorise the candidate named by SOURCE_COMMIT/SOURCE_TREE"
                ));
            }
            if output != CANONICAL_FINAL_OUTPUT || output_sha != CANONICAL_FINAL_OUTPUT_SHA {
                return Err(format!(
                    "QUALIFYING=1 is rejected: the qualifying emission writes exactly \
                     `{CANONICAL_FINAL_OUTPUT}` and `{CANONICAL_FINAL_OUTPUT_SHA}`"
                ));
            }
            Ok(())
        }
    }
}

/// Published archive digests, read from `ARCHIVES_DIR/SHA256SUMS.txt` and
/// re-verified against the archive bytes on disk.
///
/// # Errors
/// Returns an error if the manifest is absent, malformed, names an absent
/// archive, or records a digest the bytes do not produce.
pub fn verified_archive_digests(archives_dir: &Path) -> Result<BTreeMap<String, String>, String> {
    let manifest = archives_dir.join(ARCHIVE_CHECKSUMS);
    let text = std::fs::read_to_string(&manifest)
        .map_err(|error| format!("read {}: {error}", manifest.display()))?;
    let mut out = BTreeMap::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let digest = parts
            .next()
            .ok_or_else(|| format!("{}:{}: no digest", manifest.display(), number + 1))?;
        let name = parts
            .next()
            .map(|name| name.trim_start_matches('*'))
            .ok_or_else(|| format!("{}:{}: no file name", manifest.display(), number + 1))?;
        if parts.next().is_some() {
            return Err(format!(
                "{}:{}: expected exactly `<sha256>  <name>`",
                manifest.display(),
                number + 1
            ));
        }
        if !is_lower_hex_64(digest) {
            return Err(format!(
                "{}:{}: digest is not 64 lowercase hex characters",
                manifest.display(),
                number + 1
            ));
        }
        let archive = archives_dir.join(name);
        let bytes = std::fs::read(&archive)
            .map_err(|error| format!("read {}: {error}", archive.display()))?;
        let live = sha256_hex(&bytes);
        if live != digest {
            return Err(format!(
                "{} does not hash to the digest {} records",
                archive.display(),
                manifest.display()
            ));
        }
        out.insert(name.to_owned(), live);
    }
    if out.is_empty() {
        return Err(format!("{} records no archive", manifest.display()));
    }
    Ok(out)
}

/// Verify the deterministic local OIDC/JWKS fixture that satisfies the
/// network-dependent auth smoke without a real IdP.
///
/// No key material is created here. The fixture declares, relative to the
/// workspace root, the directory holding the JWKs Todo 29 already committed; this
/// function proves those exact files are present, parse, carry distinct `kid`s
/// and include the algorithm-confusion probe key that must always be rejected.
///
/// # Errors
/// Returns an error if the fixture or any declared key is absent or malformed.
pub fn verify_oidc_fixture(
    candidate_worktree: &Path,
    workspace_root: &Path,
) -> Result<Vec<String>, String> {
    let path = candidate_worktree.join(OIDC_FIXTURE);
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let document: Value = serde_json::from_str(&text)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    for key in ["issuer", "jwks_source", "keys"] {
        if document.get(key).is_none() {
            return Err(format!("{}: missing `{key}`", path.display()));
        }
    }
    let source = document["jwks_source"]
        .as_str()
        .ok_or_else(|| format!("{}: `jwks_source` is not a string", path.display()))?;
    if source.starts_with('/') || source.contains("..") {
        return Err(format!(
            "{}: `jwks_source` must be a workspace-relative path without `..`",
            path.display()
        ));
    }
    let names = document["keys"]
        .as_array()
        .ok_or_else(|| format!("{}: `keys` is not an array", path.display()))?;
    if names.is_empty() {
        return Err(format!("{}: `keys` is empty", path.display()));
    }
    let directory = workspace_root.join(source);
    let mut kids: Vec<String> = Vec::new();
    for name in names {
        let file = name
            .as_str()
            .ok_or_else(|| format!("{}: a `keys` entry is not a string", path.display()))?;
        let jwk_path = directory.join(file);
        let jwk_text = std::fs::read_to_string(&jwk_path)
            .map_err(|error| format!("read {}: {error}", jwk_path.display()))?;
        let jwk: Value = serde_json::from_str(&jwk_text)
            .map_err(|error| format!("parse {}: {error}", jwk_path.display()))?;
        for member in ["kty", "alg", "kid"] {
            if !jwk.get(member).is_some_and(Value::is_string) {
                return Err(format!("{}: missing `{member}`", jwk_path.display()));
            }
        }
        for private in ["d", "p", "q", "dp", "dq", "qi"] {
            if jwk.get(private).is_some() {
                return Err(format!(
                    "{}: a published JWK must carry no private member (`{private}` present)",
                    jwk_path.display()
                ));
            }
        }
        kids.push(jwk["kid"].as_str().unwrap_or_default().to_owned());
    }
    let mut unique = kids.clone();
    unique.sort();
    unique.dedup();
    if unique.len() != kids.len() {
        return Err(format!(
            "{}: two declared keys share a `kid`",
            path.display()
        ));
    }
    Ok(kids)
}

/// The verdict of one harness run.
#[derive(Debug, Clone)]
pub struct HarnessOutcome {
    /// Every emitted entry, catalogue order.
    pub entries: Vec<CheckEntry>,
    /// The emitted document.
    pub document: Value,
    /// Its canonical bytes.
    pub bytes: Vec<u8>,
    /// Its digest.
    pub digest: String,
    /// Whether the document was written.
    pub written: bool,
}

impl HarnessOutcome {
    /// Every failed entry.
    #[must_use]
    pub fn failures(&self) -> Vec<&CheckEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.required && !entry.passed())
            .collect()
    }

    /// Whether every required entry passed.
    #[must_use]
    pub fn all_required_passed(&self) -> bool {
        self.failures().is_empty()
    }
}

/// Join the required-check catalogue to the deposited observations and build the
/// gates document, without writing anything.
///
/// # Errors
/// Returns an error only for inputs that make evaluation impossible (an invalid
/// lock, unreadable observations, an unusable archive manifest, a broken OIDC
/// fixture). A check that simply did not pass is an entry, not an error.
pub fn evaluate(
    request: &HarnessRequest,
    workspace_root: &Path,
    version: &str,
) -> Result<HarnessOutcome, String> {
    let lock = GodotReleasesLock::load_from_workspace(workspace_root)?;
    lock.validate()?;
    let specs = required_checks(&lock, version)?;
    let observations = load_observations(&request.candidate_worktree)?;
    let archives = verified_archive_digests(&request.archives_dir)?;
    let oidc_kids = verify_oidc_fixture(&request.candidate_worktree, workspace_root)?;
    let host = RunnerIdentity::host();
    let workflow_hash = workflow_digest(workspace_root)?;

    let mut entries = Vec::with_capacity(specs.len());
    for spec in &specs {
        entries.push(entry_for(
            spec,
            observations.get(&spec.name),
            request,
            &archives,
            &host,
            &oidc_kids,
        ));
    }

    let mut top = host.clone();
    top.workflow_hash = Some(workflow_hash);
    let document = serde_json::json!({
        "schema_version": schema::SCHEMA_VERSION,
        "qualifying": request.qualifying.is_qualifying(),
        "runner_identity": runner_identity_json(&top),
        "checks": entries.iter().map(CheckEntry::to_json).collect::<Vec<_>>(),
    });
    schema::validate(&document)?;
    let bytes = canonical_bytes(&document)?;
    let digest = sha256_hex(&bytes);
    Ok(HarnessOutcome {
        entries,
        document,
        bytes,
        digest,
        written: false,
    })
}

fn entry_for(
    spec: &CheckSpec,
    record: Option<&ObservationRecord>,
    request: &HarnessRequest,
    archives: &BTreeMap<String, String>,
    host: &RunnerIdentity,
    oidc_kids: &[String],
) -> CheckEntry {
    let Some(record) = record else {
        return CheckEntry {
            name: spec.name.clone(),
            required: spec.required,
            command: spec.command.clone(),
            observation: Observation::Missing,
            exit_code: EXIT_MISSING,
            runner_identity: host.clone(),
            source_commit: request.source_commit.clone(),
            source_tree: request.source_tree.clone(),
            archive_sha256: None,
            failure: Some(format!(
                "no observation was deposited for required check `{}` ({})",
                spec.name,
                spec.kind.label()
            )),
        };
    };

    let mut failure: Option<String> = None;
    let mut fail = |reason: String| {
        if failure.is_none() {
            failure = Some(reason);
        }
    };

    if record.command.trim().is_empty() {
        fail(format!(
            "check `{}` reported no command evidence for the bound command",
            spec.name
        ));
    } else if record.command != spec.command {
        fail(format!(
            "check `{}` reported a command that is not the bound command",
            spec.name
        ));
    }
    if record.source_commit != request.source_commit {
        fail(format!(
            "check `{}` observed a different SOURCE_COMMIT",
            spec.name
        ));
    }
    if record.source_tree != request.source_tree {
        fail(format!(
            "check `{}` observed a different SOURCE_TREE",
            spec.name
        ));
    }
    // An incomplete runner identity is a failed entry, not an unemittable
    // document: the harness records the host it evaluated on so the output stays
    // schema-valid, and the entry still fails because the check is not bound to a
    // real environment.
    let runner_identity = if record.runner_identity.is_well_formed() {
        record.runner_identity.clone()
    } else {
        fail(format!(
            "check `{}` reported an incomplete runner identity, so it is not bound to an \
             environment",
            spec.name
        ));
        host.clone()
    };
    let archive_sha256 = record.archive_sha256.clone();
    match (spec.archive_file_name.as_deref(), archive_sha256.as_deref()) {
        (Some(_), None) => fail(format!(
            "check `{}` is archive-bound but its receipt records no archive_sha256",
            spec.name
        )),
        (Some(_), Some(receipt_digest)) if !is_lower_hex_64(receipt_digest) => fail(format!(
            "check `{}` reported an archive_sha256 that is not 64 lowercase hex characters",
            spec.name
        )),
        (Some(archive_name), Some(receipt_digest)) => match archives.get(archive_name) {
            Some(qualification_digest) if receipt_digest == qualification_digest => {}
            Some(_) => fail(format!(
                "check `{}` reported an archive_sha256 that does not match the qualification \
                 archive",
                spec.name
            )),
            None => fail(format!(
                "check `{}` is archive-bound but its archive is absent from the checksum manifest",
                spec.name
            )),
        },
        (None, Some(_)) => fail(format!(
            "check `{}` reported archive_sha256 but the catalogue binds no archive",
            spec.name
        )),
        (None, None) => {}
    }
    if spec.name == AUTH_SMOKE_CHECK && oidc_kids.len() < 2 {
        fail(format!(
            "check `{}` needs the deterministic local OIDC/JWKS fixture to declare more than one \
             key so key/algorithm confusion is actually probed",
            spec.name
        ));
    }

    // A non-executed observation can never present itself as a pass: the
    // recorded exit code is replaced by a distinct nonzero sentinel even if the
    // job wrote `0`.
    let exit_code = match spec_sentinel(record) {
        Some(sentinel) => {
            fail(format!(
                "required check `{}` ({}) was {} on its runner, which is a failed entry, never a \
                 skipped non-failure",
                spec.name,
                spec.kind.label(),
                record.observation.label()
            ));
            sentinel
        }
        None => {
            let captured = record.exit_code.unwrap_or(EXIT_BINDING_MISMATCH);
            if record.exit_code.is_none() {
                fail(format!(
                    "check `{}` claims it executed but captured no exit code",
                    spec.name
                ));
            } else if captured != 0 {
                fail(format!(
                    "required check `{}` ({}) exited {captured}",
                    spec.name,
                    spec.kind.label()
                ));
            }
            captured
        }
    };

    // A binding mismatch on an otherwise-zero exit must still be nonzero in the
    // document, or a reader could mistake it for a pass.
    let exit_code = if failure.is_some() && exit_code == 0 {
        EXIT_BINDING_MISMATCH
    } else {
        exit_code
    };

    CheckEntry {
        name: spec.name.clone(),
        required: spec.required,
        command: spec.command.clone(),
        observation: record.observation,
        exit_code,
        runner_identity,
        source_commit: request.source_commit.clone(),
        source_tree: request.source_tree.clone(),
        archive_sha256,
        failure,
    }
}

fn spec_sentinel(record: &ObservationRecord) -> Option<i64> {
    record.observation.sentinel_exit()
}

/// Canonical (compact, key-sorted by construction) document bytes.
fn canonical_bytes(document: &Value) -> Result<Vec<u8>, String> {
    serde_json::to_vec(document).map_err(|error| format!("serialize gates document: {error}"))
}

/// Digest binding the emitted document to the workflow definitions that produced
/// the observations.
///
/// # Errors
/// Returns an error if the workflow directory is absent or unreadable: a gates
/// document that could not name its own CI definition is not evidence.
pub fn workflow_digest(workspace_root: &Path) -> Result<String, String> {
    let directory = workspace_root.join(".github/workflows");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&directory)
        .map_err(|error| format!("read {}: {error}", directory.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext == "yml" || ext == "yaml")
        })
        .collect();
    paths.sort();
    if paths.is_empty() {
        return Err(format!("{} defines no workflow", directory.display()));
    }
    let mut hasher = Sha256::new();
    for path in paths {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let bytes =
            std::fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
        hasher.update(name.as_bytes());
        hasher.update(b"\n");
        hasher.update(&bytes);
        hasher.update(b"\n");
    }
    Ok(hex(&hasher.finalize()))
}

/// Evaluate, authorise and — only when every required check passed — write the
/// gates document plus its strict sidecar.
///
/// Fail-closed in both directions: an unauthorised invocation writes nothing, and
/// a run with any failed required entry writes nothing while reporting every
/// failure.
///
/// # Errors
/// Returns an error for an unauthorised invocation, an unevaluable input, or any
/// failed required check.
pub fn run(
    request: &HarnessRequest,
    workspace_root: &Path,
    version: &str,
) -> Result<HarnessOutcome, String> {
    let producer_context = std::env::var(PRODUCER_CONTEXT_ENV).ok();
    authorize(request, producer_context.as_deref())?;
    let mut outcome = evaluate(request, workspace_root, version)?;
    if !outcome.all_required_passed() {
        let detail = outcome
            .failures()
            .iter()
            .filter_map(|entry| entry.failure.clone())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!(
            "{} required release-gate check(s) failed, so no gates document was written: {detail}",
            outcome.failures().len()
        ));
    }
    if let Some(parent) = request.output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    if let Some(parent) = request.output_sha.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    std::fs::write(&request.output, &outcome.bytes)
        .map_err(|error| format!("write {}: {error}", request.output.display()))?;
    std::fs::write(&request.output_sha, format!("{}\n", outcome.digest))
        .map_err(|error| format!("write {}: {error}", request.output_sha.display()))?;
    outcome.written = true;
    Ok(outcome)
}
