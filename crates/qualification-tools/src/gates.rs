//! Shared B3/B4 dual-review gate machinery for the two Todo-6-owned binaries.
//!
//! Everything here is deterministic and fail-closed. There is exactly one
//! canonical byte serialization and exactly one sidecar format, shared by
//! `batch-review-bundle` (which writes them) and `finalize-batch-review` (which
//! verifies them), so a producer/consumer disagreement is impossible.
//!
//! ## Canonical byte serialization
//!
//! An artifact's canonical bytes are `serde_json`'s compact encoding of the
//! document: UTF-8, object keys in sorted order (`serde_json::Map` is a
//! `BTreeMap` in this workspace — the `preserve_order` feature is not enabled),
//! no insignificant whitespace, and **no trailing whitespace of any kind**
//! (including no trailing newline). An artifact's SHA-256 is computed over
//! exactly those bytes.
//!
//! ## Sidecar format
//!
//! A `.sha256` sidecar contains exactly 64 lowercase hexadecimal characters
//! followed by exactly one `\n` byte and nothing else — no uppercase, no
//! filename, no `*` marker, no `\r`, no surrounding whitespace, no second line.
//! Every consumer rejects any deviation, a wrong length, and a digest mismatch.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The version marker carried by every gate artifact this package emits.
pub const GATE_SCHEMA_VERSION: &str = "1";

/// Exit code for a fail-closed rejection: bad input, failed gate, bad sidecar.
pub const REJECTED_EXIT: u8 = 1;

/// Exit code for a usage error (missing/unknown argument).
pub const USAGE_EXIT: u8 = 2;

/// The default plan path, relative to the workspace root the gate commands run
/// from. `finalize-batch-review` compares the bundle's recorded whole-plan
/// SHA-256 against the live plan so a plan-byte change cannot be approved with a
/// stale bundle.
pub const DEFAULT_PLAN_PATH: &str = ".omo/plans/godot-mcp-server-architecture.md";

/// Which dual-review gate is being produced or finalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Batch {
    /// Wave 3 — frozen Todo 19 bridge protocol plus cancelled Todos 20–25 editor plane.
    B3,
    /// Wave 4 — policy and optional HTTP, Todos 26–31.
    B4,
}

impl Batch {
    /// Parse the committed `--batch` spelling. Only lowercase `b3`/`b4` exist.
    pub fn parse(value: &str) -> Result<Self, GateError> {
        match value {
            "b3" => Ok(Self::B3),
            "b4" => Ok(Self::B4),
            other => Err(GateError::Usage(format!(
                "--batch must be exactly `b3` or `b4`, got {other:?}"
            ))),
        }
    }

    /// Canonical lowercase name used in every artifact.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::B3 => "b3",
            Self::B4 => "b4",
        }
    }

    /// Inclusive todo range this batch's reviewers evaluate.
    #[must_use]
    pub const fn todo_range(self) -> (u32, u32) {
        match self {
            Self::B3 => (19, 25),
            Self::B4 => (26, 31),
        }
    }

    /// The first todo number *after* this batch, which bounds the plan slice.
    const fn slice_end_exclusive(self) -> u32 {
        self.todo_range().1 + 1
    }
}

/// Frozen source identity carried by a batch review bundle.
///
/// The two ids are **independent** git object ids, never derived from one
/// another: `commit` is the SHA-1 of the frozen commit object and `tree` is the
/// SHA-1 of the tree that commit points at. A commit object's own hash is
/// computed over a header that already records its tree id, so the two hashes
/// always name different objects and can never be equal. A bundle whose `commit`
/// equals its `tree` therefore proves one value was copied into both fields, and
/// cannot identify the source the reviewers actually read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseSourceIdentity {
    /// Frozen post-Todo-6 commit object id: exactly 40 lowercase hex digits.
    pub commit: String,
    /// Frozen post-Todo-6 tree object id: exactly 40 lowercase hex digits, never
    /// equal to `commit`.
    pub tree: String,
}

impl BaseSourceIdentity {
    /// Build the frozen source identity from two independently supplied git
    /// object ids. Neither value is ever copied into the other field.
    ///
    /// # Errors
    /// Returns [`GateError::Usage`] when either id is empty, is not exactly 40
    /// lowercase hexadecimal digits, or when the two ids are equal — which can
    /// only happen when one hash was supplied for both, never for a real commit
    /// and the tree it points at.
    pub fn new(commit: &str, tree: &str) -> Result<Self, GateError> {
        for (flag, role, value) in [
            ("--base-commit", "commit object id", commit),
            ("--base-tree", "tree object id", tree),
        ] {
            if value.trim().is_empty() {
                return Err(GateError::Usage(format!(
                    "{flag} must name the frozen post-Todo-6 {role}; it is never empty or initial"
                )));
            }
            if !is_lower_hex_object_id(value) {
                return Err(GateError::Usage(format!(
                    "{flag} must be exactly 40 lowercase hexadecimal digits naming the frozen \
                     post-Todo-6 {role}, got {value:?} ({} character(s))",
                    value.chars().count()
                )));
            }
        }
        if commit == tree {
            return Err(GateError::Usage(format!(
                "--base-commit and --base-tree were both given {commit:?}, but a commit id can \
                 never equal its own tree id: a commit object's SHA-1 is computed over a header \
                 that already records that tree id, so the two hashes always name different \
                 objects. Two equal values mean one hash was copied into both fields, which makes \
                 the bundle's recorded source identity unprovable. Pass `git rev-parse <ref>` for \
                 --base-commit and `git rev-parse <ref>^{{tree}}` for --base-tree"
            )));
        }
        Ok(Self {
            commit: commit.to_owned(),
            tree: tree.to_owned(),
        })
    }
}

/// Canonical B3/B4 review input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchBundle {
    /// Gate schema version.
    pub schema_version: String,
    /// `b3` or `b4`.
    pub batch: String,
    /// SHA-256 of the complete plan bytes.
    pub plan_sha256: String,
    /// Canonicalized plan substring for this batch's Todo range.
    pub target_plan_slice: String,
    /// Frozen source identity reviewed by this batch.
    pub base_source_identity: BaseSourceIdentity,
    /// Exact dependency-matrix rows for the batch's Todo range.
    pub dependency_matrix_rows: Vec<String>,
}

/// One finding recorded by a batch reviewer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchReviewFinding {
    /// `Blocker`, `Major`, or `Follow-up`.
    pub severity: String,
    /// `open` or `closed`.
    pub status: String,
    /// Nonempty reviewer summary.
    pub summary: String,
}

/// Canonical independent-review receipt consumed by the gate finalizer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchReviewReceipt {
    /// Gate schema version.
    pub schema_version: String,
    /// `b3` or `b4`.
    pub batch: String,
    /// Independent review round identity.
    pub round_id: String,
    /// Independent reviewer identity.
    pub reviewer_identity: String,
    /// Exact reviewed bundle digest.
    pub bundle_sha256: String,
    /// `APPROVE` or `REJECT`.
    pub verdict: String,
    /// Review findings.
    pub findings: Vec<BatchReviewFinding>,
    /// Bundle digest before review.
    pub before_bundle_sha256: String,
    /// Bundle digest after review.
    pub after_bundle_sha256: String,
}

/// One receipt identity embedded in an approval manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalReceiptBinding {
    /// Independent reviewer identity.
    pub reviewer_identity: String,
    /// SHA-256 of that reviewer's canonical receipt bytes.
    pub receipt_sha256: String,
}

/// Canonical approval manifest emitted only for a passing dual review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchApprovalManifest {
    /// Gate schema version.
    pub schema_version: String,
    /// `b3` or `b4`.
    pub batch: String,
    /// SHA-256 of the complete current plan bytes.
    pub plan_sha256: String,
    /// SHA-256 of the canonical review bundle bytes.
    pub bundle_sha256: String,
    /// Exactly two independent receipt identities.
    pub receipts: Vec<ApprovalReceiptBinding>,
    /// Always true; a rejected gate emits no manifest.
    pub approved: bool,
}

/// A fail-closed gate failure. No variant is recoverable and none produces a
/// partial artifact.
#[derive(Debug, thiserror::Error)]
pub enum GateError {
    /// The command line is not usable.
    #[error("usage: {0}")]
    Usage(String),
    /// A file could not be read or written.
    #[error("{path}: {reason}")]
    Io { path: String, reason: String },
    /// A document is not valid JSON.
    #[error("{path}: not valid JSON: {reason}")]
    Json { path: String, reason: String },
    /// JSON bytes parse but are not the compact sorted-key canonical form.
    #[error(
        "{path}: artifact is not encoded as canonical JSON bytes (UTF-8, sorted keys, compact, no trailing whitespace)"
    )]
    NonCanonical { path: String },
    /// A document does not satisfy its canonical schema.
    #[error("{path}: canonical schema violation: {reason}")]
    Schema { path: String, reason: String },
    /// The sidecar file is absent.
    #[error("{path}: required .sha256 sidecar is missing")]
    SidecarMissing { path: String },
    /// The sidecar is not exactly 64 lowercase hex characters plus one LF.
    #[error(
        "{path}: .sha256 sidecar is malformed ({reason}); \
         it must contain exactly 64 lowercase hex digits followed by one LF and nothing else"
    )]
    SidecarMalformed { path: String, reason: String },
    /// The sidecar digest does not match the artifact bytes.
    #[error("{artifact}: bytes do not match the digest recorded in {sidecar}")]
    SidecarMismatch { artifact: String, sidecar: String },
    /// The plan text does not contain the requested slice or matrix rows.
    #[error("{path}: {reason}")]
    PlanShape { path: String, reason: String },
    /// The bundle's recorded whole-plan SHA-256 is stale.
    #[error(
        "stale whole-plan SHA-256: bundle records {recorded} but {plan} currently hashes to \
         {actual}; both bundles, both receipt pairs and both approval manifests are invalidated \
         by any plan byte change and must be regenerated and re-approved"
    )]
    StalePlanSha {
        plan: String,
        recorded: String,
        actual: String,
    },
    /// A receipt does not authorize the gate.
    #[error("{path}: {reason}")]
    ReceiptRejected { path: String, reason: String },
}

fn io_error(path: &Path, error: &std::io::Error) -> GateError {
    GateError::Io {
        path: path.display().to_string(),
        reason: error.to_string(),
    }
}

/// Lowercase hexadecimal SHA-256 of `bytes`.
#[must_use]
pub fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

fn is_lower_hex_of_length(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Whether `value` is exactly 64 lowercase hexadecimal characters.
#[must_use]
pub fn is_lower_hex_sha256(value: &str) -> bool {
    is_lower_hex_of_length(value, 64)
}

/// Whether `value` is exactly 40 lowercase hexadecimal characters — the shape of
/// a git SHA-1 object id such as a commit id or a tree id.
#[must_use]
pub fn is_lower_hex_object_id(value: &str) -> bool {
    is_lower_hex_of_length(value, 40)
}

/// Serialize a document to canonical artifact bytes.
///
/// # Errors
/// Returns [`GateError::Json`] if the document cannot be serialized.
pub fn canonical_bytes(value: &Value) -> Result<Vec<u8>, GateError> {
    serde_json::to_vec(value).map_err(|error| GateError::Json {
        path: "<in-memory>".to_owned(),
        reason: error.to_string(),
    })
}

/// Read a file's raw bytes.
///
/// # Errors
/// Returns [`GateError::Io`] when the file cannot be read.
pub fn read_bytes(path: &Path) -> Result<Vec<u8>, GateError> {
    std::fs::read(path).map_err(|error| io_error(path, &error))
}

/// Read a strict `.sha256` sidecar and return the recorded digest.
///
/// # Errors
/// Returns [`GateError::SidecarMissing`] when absent, or
/// [`GateError::SidecarMalformed`] for any deviation from the exact format:
/// uppercase, a `\r`, leading/trailing spaces, a missing or duplicated newline,
/// a wrong length, an embedded filename or `*` marker, a second line, or any
/// non-hexadecimal byte.
pub fn read_sidecar_digest(sidecar: &Path) -> Result<String, GateError> {
    let raw = match std::fs::read(sidecar) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(GateError::SidecarMissing {
                path: sidecar.display().to_string(),
            });
        }
        Err(error) => return Err(io_error(sidecar, &error)),
    };
    let malformed = |reason: &str| GateError::SidecarMalformed {
        path: sidecar.display().to_string(),
        reason: reason.to_owned(),
    };
    if raw.len() != 65 {
        return Err(malformed(&format!(
            "expected exactly 65 bytes, found {}",
            raw.len()
        )));
    }
    if raw[64] != b'\n' {
        return Err(malformed("the 65th byte is not a single LF"));
    }
    let digest = std::str::from_utf8(&raw[..64]).map_err(|_| malformed("not valid UTF-8"))?;
    if !is_lower_hex_sha256(digest) {
        return Err(malformed(
            "the first 64 bytes are not all lowercase hexadecimal digits",
        ));
    }
    Ok(digest.to_owned())
}

/// Verify one artifact against its explicit sidecar and return the digest plus
/// the artifact's parsed JSON.
///
/// # Errors
/// Returns the first [`GateError`] explaining why the pair is unusable. The
/// sidecar is verified *before* the artifact is parsed, so a tampered artifact
/// is rejected on identity rather than on content.
pub fn verify_artifact(artifact: &Path, sidecar: &Path) -> Result<(String, Value), GateError> {
    let (actual, bytes) = verify_artifact_bytes(artifact, sidecar)?;
    let value = parse_canonical_json(artifact, &bytes)?;
    Ok((actual, value))
}

/// Verify only an artifact's strict sidecar and byte digest, without parsing it.
///
/// # Errors
/// Returns the first [`GateError`] explaining why the artifact/sidecar identity
/// is unusable.
pub fn verify_artifact_bytes(
    artifact: &Path,
    sidecar: &Path,
) -> Result<(String, Vec<u8>), GateError> {
    let recorded = read_sidecar_digest(sidecar)?;
    let bytes = read_bytes(artifact)?;
    let actual = hex_sha256(&bytes);
    if recorded != actual {
        return Err(GateError::SidecarMismatch {
            artifact: artifact.display().to_string(),
            sidecar: sidecar.display().to_string(),
        });
    }
    Ok((actual, bytes))
}

/// Parse JSON and require its bytes to equal the canonical compact encoding.
///
/// # Errors
/// Returns [`GateError::Json`] for malformed JSON or
/// [`GateError::NonCanonical`] for a noncanonical byte representation.
pub fn parse_canonical_json(path: &Path, bytes: &[u8]) -> Result<Value, GateError> {
    let value = serde_json::from_slice(bytes).map_err(|error| GateError::Json {
        path: path.display().to_string(),
        reason: error.to_string(),
    })?;
    if canonical_bytes(&value)? != bytes {
        return Err(GateError::NonCanonical {
            path: path.display().to_string(),
        });
    }
    Ok(value)
}

/// Validate a document against a canonical schema file using the in-repo
/// restrictive-subset validator. No external JSON Schema crate is used, so no
/// unpinned dependency enters the graph.
///
/// # Errors
/// Returns [`GateError::Schema`] when the document is rejected, or
/// [`GateError::Io`]/[`GateError::Json`] when the schema itself is unusable.
pub fn validate_against_schema(
    label: &str,
    schema_path: &Path,
    value: &Value,
) -> Result<(), GateError> {
    let schema_text =
        std::fs::read_to_string(schema_path).map_err(|error| io_error(schema_path, &error))?;
    let schema: Value = serde_json::from_str(&schema_text).map_err(|error| GateError::Json {
        path: schema_path.display().to_string(),
        reason: error.to_string(),
    })?;
    godot_mcp_domain::jsonschema::validate(&schema, value).map_err(|error| GateError::Schema {
        path: label.to_owned(),
        reason: error.to_string(),
    })
}

/// The `.sha256` sidecar path implied by an artifact path, used only when the
/// caller did not pass an explicit sidecar path.
///
/// The plan's committed sidecar names *replace* the artifact extension
/// (`b3-bundle.json` pairs with `b3-bundle.sha256`), so this follows that
/// spelling rather than appending a second extension.
#[must_use]
pub fn implied_sidecar(artifact: &Path) -> PathBuf {
    artifact.with_extension("sha256")
}

/// Write one canonical artifact and its strict sidecar as one fail-closed pair.
/// Both files are prepared as private sibling temporaries before publication.
/// Existing regular-file outputs are moved aside and restored if either publish
/// rename fails, so an error never leaves one newly written output without the
/// other. Returns the artifact digest.
///
/// # Errors
/// Returns [`GateError::Io`] on any write failure.
pub fn write_artifact(artifact: &Path, sidecar: &Path, bytes: &[u8]) -> Result<String, GateError> {
    if artifact == sidecar {
        return Err(GateError::Io {
            path: artifact.display().to_string(),
            reason: "artifact and sidecar paths must be different".to_owned(),
        });
    }
    for path in [artifact, sidecar] {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|error| io_error(parent, &error))?;
        }
        reject_non_file_target(path)?;
    }

    let digest = hex_sha256(bytes);
    let sidecar_bytes = format!("{digest}\n");
    let artifact_temp = temporary_sibling(artifact, "tmp");
    let sidecar_temp = temporary_sibling(sidecar, "tmp");
    write_private_temp(&artifact_temp, bytes)?;
    if let Err(error) = write_private_temp(&sidecar_temp, sidecar_bytes.as_bytes()) {
        let _ = std::fs::remove_file(&artifact_temp);
        return Err(error);
    }

    let result = publish_pair(&artifact_temp, artifact, &sidecar_temp, sidecar);
    let _ = std::fs::remove_file(&artifact_temp);
    let _ = std::fs::remove_file(&sidecar_temp);
    result?;
    Ok(digest)
}

static TEMPORARY_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temporary_sibling(path: &Path, role: &str) -> PathBuf {
    let counter = TEMPORARY_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "artifact".into());
    path.with_file_name(format!(
        ".{file_name}.{}.{}.{}",
        std::process::id(),
        counter,
        role
    ))
}

fn reject_non_file_target(path: &Path) -> Result<(), GateError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => Err(GateError::Io {
            path: path.display().to_string(),
            reason: "output exists but is not a regular file".to_owned(),
        }),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(path, &error)),
    }
}

fn write_private_temp(path: &Path, bytes: &[u8]) -> Result<(), GateError> {
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(path).map_err(|error| io_error(path, &error))?;
        file.write_all(bytes)
            .map_err(|error| io_error(path, &error))?;
        file.sync_all().map_err(|error| io_error(path, &error))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

fn move_existing_to_backup(path: &Path) -> Result<Option<PathBuf>, GateError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            let backup = temporary_sibling(path, "backup");
            std::fs::rename(path, &backup).map_err(|error| io_error(path, &error))?;
            Ok(Some(backup))
        }
        Ok(_) => Err(GateError::Io {
            path: path.display().to_string(),
            reason: "output exists but is not a regular file".to_owned(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(path, &error)),
    }
}

fn restore_backup(path: &Path, backup: &Option<PathBuf>) {
    if let Some(backup) = backup {
        let _ = std::fs::rename(backup, path);
    }
}

fn publish_pair(
    artifact_temp: &Path,
    artifact: &Path,
    sidecar_temp: &Path,
    sidecar: &Path,
) -> Result<(), GateError> {
    let artifact_backup = move_existing_to_backup(artifact)?;
    let sidecar_backup = match move_existing_to_backup(sidecar) {
        Ok(backup) => backup,
        Err(error) => {
            restore_backup(artifact, &artifact_backup);
            return Err(error);
        }
    };

    if let Err(error) = std::fs::rename(artifact_temp, artifact) {
        restore_backup(artifact, &artifact_backup);
        restore_backup(sidecar, &sidecar_backup);
        return Err(io_error(artifact, &error));
    }
    if let Err(error) = std::fs::rename(sidecar_temp, sidecar) {
        let _ = std::fs::remove_file(artifact);
        restore_backup(artifact, &artifact_backup);
        restore_backup(sidecar, &sidecar_backup);
        return Err(io_error(sidecar, &error));
    }

    if let Some(backup) = artifact_backup {
        let _ = std::fs::remove_file(backup);
    }
    if let Some(backup) = sidecar_backup {
        let _ = std::fs::remove_file(backup);
    }
    Ok(())
}

/// The plan text plus its whole-file SHA-256.
pub struct PlanDocument {
    path: PathBuf,
    text: String,
    sha256: String,
}

impl PlanDocument {
    /// Read the plan and hash its exact bytes.
    ///
    /// # Errors
    /// Returns [`GateError::Io`] when unreadable, or [`GateError::PlanShape`]
    /// when the bytes are not UTF-8.
    pub fn load(path: &Path) -> Result<Self, GateError> {
        let bytes = read_bytes(path)?;
        let sha256 = hex_sha256(&bytes);
        let text = String::from_utf8(bytes).map_err(|_| GateError::PlanShape {
            path: path.display().to_string(),
            reason: "plan is not valid UTF-8".to_owned(),
        })?;
        Ok(Self {
            path: path.to_path_buf(),
            text,
            sha256,
        })
    }

    /// SHA-256 of the whole plan file.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Displayable plan path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The canonicalized plan text for this batch's todo range.
    ///
    /// The slice runs from the batch's first `- [ ] N.` (or `- [x] N.`) heading
    /// up to, but excluding, the heading immediately after the batch's last todo.
    /// Canonicalization strips per-line trailing whitespace and normalizes line
    /// endings to `\n`, so an editor's trailing-space churn cannot change the
    /// batch SHA while any real text change still does.
    ///
    /// # Errors
    /// Returns [`GateError::PlanShape`] when a boundary heading is absent or the
    /// slice does not contain every todo in the range.
    pub fn target_slice(&self, batch: Batch) -> Result<String, GateError> {
        let (first, last) = batch.todo_range();
        let start = self
            .todo_heading_offset(first)
            .ok_or_else(|| self.shape(&format!("no `- [ ] {first}.` todo heading found")))?;
        let end = self
            .todo_heading_offset(batch.slice_end_exclusive())
            .unwrap_or(self.text.len());
        if end <= start {
            return Err(self.shape(&format!(
                "todo {} heading precedes todo {first}",
                batch.slice_end_exclusive()
            )));
        }
        let slice = canonicalize_plan_text(&self.text[start..end]);
        for todo in first..=last {
            if !slice.contains(&format!("] {todo}. ")) {
                return Err(self.shape(&format!(
                    "canonicalized slice for batch {} is missing todo {todo}",
                    batch.as_str()
                )));
            }
        }
        if slice.is_empty() {
            return Err(self.shape("canonicalized slice is empty"));
        }
        Ok(slice)
    }

    /// The dependency-matrix rows for this batch's todo range, verbatim from the
    /// plan's matrix table with per-line trailing whitespace stripped.
    ///
    /// # Errors
    /// Returns [`GateError::PlanShape`] when a row in the range is absent.
    pub fn dependency_matrix_rows(&self, batch: Batch) -> Result<Vec<String>, GateError> {
        let (first, last) = batch.todo_range();
        let mut rows = Vec::new();
        for todo in first..=last {
            let prefix = format!("| {todo} |");
            let row = self
                .text
                .lines()
                .map(str::trim_end)
                .find(|line| line.starts_with(&prefix))
                .ok_or_else(|| {
                    self.shape(&format!("no dependency-matrix row found for todo {todo}"))
                })?;
            rows.push(row.to_owned());
        }
        Ok(rows)
    }

    /// The plan's heading line for `todo`, trailing whitespace stripped.
    #[must_use]
    pub fn todo_heading(&self, todo: u32) -> Option<&str> {
        let unchecked = format!("- [ ] {todo}. ");
        let checked = format!("- [x] {todo}. ");
        self.text
            .lines()
            .map(str::trim_end)
            .find(|line| line.starts_with(&unchecked) || line.starts_with(&checked))
    }

    /// Whether the plan declares `todo` at all.
    #[must_use]
    pub fn has_todo_heading(&self, todo: u32) -> bool {
        self.todo_heading(todo).is_some()
    }

    /// Whether the plan marks `todo` accepted, i.e. `- [x] N.`.
    #[must_use]
    pub fn has_accepted_todo(&self, todo: u32) -> bool {
        self.todo_heading(todo)
            .is_some_and(|heading| heading.starts_with(&format!("- [x] {todo}. ")))
    }

    fn todo_heading_offset(&self, todo: u32) -> Option<usize> {
        let mut offset = 0_usize;
        for line in self.text.split_inclusive('\n') {
            let trimmed = line.trim_end();
            if trimmed.starts_with(&format!("- [ ] {todo}. "))
                || trimmed.starts_with(&format!("- [x] {todo}. "))
            {
                return Some(offset);
            }
            offset += line.len();
        }
        None
    }

    fn shape(&self, reason: &str) -> GateError {
        GateError::PlanShape {
            path: self.path.display().to_string(),
            reason: reason.to_owned(),
        }
    }
}

/// Strip per-line trailing whitespace, normalize CRLF to LF, and remove leading
/// and trailing blank lines.
fn canonicalize_plan_text(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().map(str::trim_end).collect();
    let first = lines.iter().position(|line| !line.is_empty());
    let last = lines.iter().rposition(|line| !line.is_empty());
    match (first, last) {
        (Some(first), Some(last)) => lines[first..=last].join("\n"),
        _ => String::new(),
    }
}

/// One reviewer's verified receipt.
pub struct VerifiedReceipt {
    /// Reviewer identity recorded in the receipt.
    pub reviewer_identity: String,
    /// SHA-256 of the receipt file's exact bytes, equal to its sidecar digest.
    pub receipt_sha256: String,
    /// Round ID recorded in the receipt.
    pub round_id: String,
}

/// Validate one already sidecar- and schema-verified receipt against a gate.
///
/// # Errors
/// Returns the first [`GateError`] explaining why the receipt does not authorize
/// the gate. Nothing is written by this function, so a rejection can never leave
/// a partial approval behind.
pub fn validate_receipt(
    receipt_path: &Path,
    receipt_sha256: String,
    value: &Value,
    batch: Batch,
    bundle_sha256: &str,
) -> Result<VerifiedReceipt, GateError> {
    let receipt: BatchReviewReceipt =
        serde_json::from_value(value.clone()).map_err(|error| GateError::Json {
            path: receipt_path.display().to_string(),
            reason: error.to_string(),
        })?;
    let reject = |reason: String| GateError::ReceiptRejected {
        path: receipt_path.display().to_string(),
        reason,
    };
    if receipt.batch != batch.as_str() {
        return Err(reject(format!(
            "receipt is for batch {:?}, not {:?}",
            receipt.batch,
            batch.as_str()
        )));
    }
    if receipt.bundle_sha256 != bundle_sha256 {
        return Err(reject(format!(
            "receipt binds bundle SHA-256 {} but the bundle hashes to \
             {bundle_sha256}; the receipt is stale and must be re-issued against the current bundle",
            receipt.bundle_sha256
        )));
    }
    if receipt.before_bundle_sha256 != bundle_sha256 {
        return Err(reject(format!(
            "receipt's before_bundle_sha256 {} does not equal the reviewed bundle SHA-256 \
             {bundle_sha256}",
            receipt.before_bundle_sha256
        )));
    }
    if receipt.after_bundle_sha256 != bundle_sha256 {
        return Err(reject(format!(
            "receipt's after_bundle_sha256 {} does not equal the reviewed bundle SHA-256 \
             {bundle_sha256}",
            receipt.after_bundle_sha256
        )));
    }
    if receipt.verdict != "APPROVE" {
        return Err(reject(format!(
            "verdict is {:?}; the gate requires an unambiguous APPROVE",
            receipt.verdict
        )));
    }
    let mut open = Vec::new();
    for finding in &receipt.findings {
        if finding.status == "open" && matches!(finding.severity.as_str(), "Blocker" | "Major") {
            open.push(format!("{}: {}", finding.severity, finding.summary));
        }
    }
    if !open.is_empty() {
        return Err(reject(format!(
            "receipt records {} open Blocker/Major finding(s): {open:?}",
            open.len()
        )));
    }
    Ok(VerifiedReceipt {
        reviewer_identity: receipt.reviewer_identity,
        receipt_sha256,
        round_id: receipt.round_id,
    })
}

/// Report a gate failure to stderr and map it to an exit code. stdout is never
/// written by a failing gate command, so a machine consumer never sees partial
/// output.
#[must_use]
pub fn report(error: &GateError) -> std::process::ExitCode {
    eprintln!("{error}");
    match error {
        GateError::Usage(_) => std::process::ExitCode::from(USAGE_EXIT),
        _ => std::process::ExitCode::from(REJECTED_EXIT),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_bytes_are_sorted_and_have_no_trailing_whitespace() {
        let value = serde_json::json!({ "b": 1, "a": { "d": 2, "c": 3 } });
        let bytes = canonical_bytes(&value).expect("canonical bytes");
        assert_eq!(
            String::from_utf8(bytes).expect("utf-8"),
            "{\"a\":{\"c\":3,\"d\":2},\"b\":1}"
        );
    }

    #[test]
    fn sidecar_format_is_exact() {
        let temp = tempfile::tempdir().expect("temp dir");
        let digest = "a".repeat(64);
        let cases: [(&str, Vec<u8>); 8] = [
            ("uppercase", format!("{}\n", "A".repeat(64)).into_bytes()),
            ("no newline", digest.clone().into_bytes()),
            ("crlf", format!("{digest}\r\n").into_bytes()),
            ("two newlines", format!("{digest}\n\n").into_bytes()),
            ("leading space", format!(" {digest}\n").into_bytes()),
            (
                "filename suffix",
                format!("{digest}  bundle.json\n").into_bytes(),
            ),
            ("short", format!("{}\n", "a".repeat(63)).into_bytes()),
            ("long", format!("{}\n", "a".repeat(65)).into_bytes()),
        ];
        for (label, bytes) in cases {
            let path = temp.path().join(format!("{label}.sha256"));
            std::fs::write(&path, &bytes).expect("write sidecar");
            assert!(
                matches!(
                    read_sidecar_digest(&path),
                    Err(GateError::SidecarMalformed { .. })
                ),
                "sidecar variant {label:?} must be rejected"
            );
        }
        let good = temp.path().join("good.sha256");
        std::fs::write(&good, format!("{digest}\n")).expect("write sidecar");
        assert_eq!(read_sidecar_digest(&good).expect("accepted"), digest);
        assert!(matches!(
            read_sidecar_digest(&temp.path().join("absent.sha256")),
            Err(GateError::SidecarMissing { .. })
        ));
    }

    #[test]
    fn implied_sidecar_replaces_the_extension_like_the_plan_paths() {
        assert_eq!(
            implied_sidecar(Path::new("gates/b3-bundle.json")),
            PathBuf::from("gates/b3-bundle.sha256")
        );
        assert_eq!(
            implied_sidecar(Path::new("gates/b4-momus-receipt.json")),
            PathBuf::from("gates/b4-momus-receipt.sha256")
        );
    }

    #[test]
    fn base_source_identity_keeps_the_commit_and_tree_ids_distinct() {
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let tree = "fedcba9876543210fedcba9876543210fedcba98";
        let identity = BaseSourceIdentity::new(commit, tree).expect("two distinct object ids");
        assert_eq!(identity.commit, commit);
        assert_eq!(identity.tree, tree);

        // A commit id can never equal the tree id its own object records, so equal
        // values are only reachable by copying one hash into both fields.
        let error = BaseSourceIdentity::new(commit, commit).expect_err("equal ids are rejected");
        assert!(matches!(error, GateError::Usage(_)));
        assert!(
            error
                .to_string()
                .contains("can never equal its own tree id"),
            "the rejection explains why the two ids cannot be equal: {error}"
        );

        for bad in [
            "",
            "   ",
            "0123456789abcdef0123456789abcdef0123456",
            "0123456789abcdef0123456789abcdef012345678",
            "0123456789ABCDEF0123456789ABCDEF01234567",
            "0123456789abcdef0123456789abcdef0123456g",
        ] {
            assert!(
                matches!(BaseSourceIdentity::new(bad, tree), Err(GateError::Usage(_))),
                "--base-commit {bad:?} must be rejected"
            );
            assert!(
                matches!(
                    BaseSourceIdentity::new(commit, bad),
                    Err(GateError::Usage(_))
                ),
                "--base-tree {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn object_id_and_digest_shapes_are_distinct_lengths() {
        assert!(is_lower_hex_object_id(&"a".repeat(40)));
        assert!(!is_lower_hex_object_id(&"a".repeat(64)));
        assert!(is_lower_hex_sha256(&"a".repeat(64)));
        assert!(!is_lower_hex_sha256(&"a".repeat(40)));
    }

    #[test]
    fn batch_ranges_match_the_plan_waves() {
        assert_eq!(Batch::parse("b3").expect("b3").todo_range(), (19, 25));
        assert_eq!(Batch::parse("b4").expect("b4").todo_range(), (26, 31));
        assert!(Batch::parse("B3").is_err());
        assert!(Batch::parse("b5").is_err());
    }

    #[test]
    #[cfg_attr(
        not(maintainer_workspace),
        ignore = "needs the maintainer's .omo workspace"
    )]
    fn plan_slice_and_matrix_rows_are_extracted_for_both_batches() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root is two levels above the crate manifest");
        let plan = PlanDocument::load(workspace.join(DEFAULT_PLAN_PATH).as_path())
            .expect("the committed plan is readable from the crate directory");
        assert!(is_lower_hex_sha256(plan.sha256()));
        for batch in [Batch::B3, Batch::B4] {
            let slice = plan.target_slice(batch).expect("slice");
            let (first, last) = batch.todo_range();
            assert!(
                slice.starts_with(&format!("- [ ] {first}."))
                    || slice.contains(&format!("] {first}. "))
            );
            assert!(slice.contains(&format!("] {last}. ")));
            assert!(
                !slice.contains(&format!("] {}. ", last + 1)),
                "the slice must stop before the next batch's first todo"
            );
            let rows = plan.dependency_matrix_rows(batch).expect("rows");
            assert_eq!(rows.len() as u32, last - first + 1);
            for (index, row) in rows.iter().enumerate() {
                assert!(row.starts_with(&format!("| {} |", first + index as u32)));
                assert_eq!(row.trim_end(), row, "rows carry no trailing whitespace");
            }
        }
    }
}
