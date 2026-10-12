//! Shared machinery for the six Todo-37-owned qualification binaries.
//!
//! `produce-qualification` publishes the five qualification paths plus the final
//! release-gates pair; `f1-plan-compliance`, `f2-code-quality`, `f3-real-qa` and
//! `f4-scope-fidelity` each bind exactly the two qualification sidecar values and
//! write one hash-only receipt; `finalize-approval` binds all of it into one final
//! manifest.
//!
//! Everything here is deterministic and fail-closed, and it reuses
//! [`crate::gates`] for canonical bytes, the strict `.sha256` sidecar format,
//! schema validation and atomic publication rather than restating any of them.
//!
//! ## Canonical hash-only receipt contract
//!
//! An F1–F4 receipt binds exactly `input-identity.sha256` and
//! `qualification-report.sha256` — never the identity or report body — plus its
//! schema version, verifier identity, an independent round/launch id, the verdict,
//! findings with severity and status, and the before/after bound hash. The
//! before/after values are the [`bound_hash`] of the two sidecar digests measured
//! before and after the checker's work, so a checker that mutated its own bound
//! evidence cannot report APPROVE.
//!
//! ## Structural test seam
//!
//! Three environment overrides exist so the six binaries can be proven against
//! fixtures without a multi-hour real gate run:
//! [`EVIDENCE_PLAN_ENV`], [`GATE_PLAN_ENV`] and [`FAULT_ENV`]. Each is refused —
//! a hard error, never a silent ignore — as soon as any output path lies under
//! `.omo/evidence/`, mirroring the release-gate harness's own structural rule. The
//! real qualifying emission writes exactly there, so it can never consult a
//! substituted catalogue or an injected fault.

use std::collections::BTreeMap;
use std::path::Component;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::gates::{
    GateError, canonical_bytes, hex_sha256, implied_sidecar, is_lower_hex_object_id,
    is_lower_hex_sha256, parse_canonical_json, read_bytes, read_sidecar_digest,
    validate_against_schema, verify_artifact, verify_artifact_bytes, write_artifact,
};

/// Schema version carried by every Todo-37 qualification artifact.
pub const QUALIFICATION_SCHEMA_VERSION: &str = "1";

/// The one producer name recorded in the provenance of every qualification path.
pub const PRODUCER_NAME: &str = "produce-qualification";

/// The exact Rust toolchain channel the whole release is pinned to.
pub const PINNED_TOOLCHAIN: &str = "1.96.0";

/// Canonical schema of the F1–F4 hash-only receipt.
pub const RECEIPT_SCHEMA: &str = "schemas/qualification/reviewer-receipt.schema.json";

/// Canonical schema of `input-identity.json`.
pub const IDENTITY_SCHEMA: &str = "schemas/qualification/input-identity.schema.json";

/// Canonical schema of `qualification-report.json`.
pub const REPORT_SCHEMA: &str = "schemas/qualification/qualification-report.schema.json";

/// Canonical schema of `review-bindings.json`.
pub const BINDINGS_SCHEMA: &str = "schemas/qualification/review-bindings.schema.json";

/// Canonical schema of the release-gate evidence document (owned by Todo 35).
pub const RELEASE_GATES_SCHEMA: &str = "schemas/gates/release-gates.schema.json";

/// Canonical schema of a B3/B4 approval manifest (owned by Todo 6).
pub const BATCH_APPROVAL_SCHEMA: &str = "schemas/gates/batch-approval-manifest.schema.json";

/// Evidence prefix that structurally disables every test seam.
pub const EVIDENCE_PREFIX: &str = ".omo/evidence/";

/// Optional override for the producer's qualification-evidence catalogue.
pub const EVIDENCE_PLAN_ENV: &str = "GODOT_MCP_QUALIFICATION_EVIDENCE_PLAN";

/// Optional override for a checker's executable gate/workflow catalogue.
pub const GATE_PLAN_ENV: &str = "GODOT_MCP_QUALIFICATION_GATE_PLAN";

/// Optional named fault-injection point.
pub const FAULT_ENV: &str = "GODOT_MCP_QUALIFICATION_FAULT";

/// Optional path of an already-built `release-package` binary, so the producer
/// does not have to shell out to `cargo run` for the frozen Todo 35 harness.
pub const HARNESS_BINARY_ENV: &str = "GODOT_MCP_RELEASE_GATES_HARNESS";

/// The six cancelled todos: the editor/runtime plane is not shipped.
pub const CANCELLED_TODOS: [u32; 6] = [20, 21, 22, 23, 24, 25];

/// Every todo the plan enumerates.
pub const ALL_TODOS: std::ops::RangeInclusive<u32> = 1..=37;

/// The four F1–F4 checker names, in receipt order.
pub const CHECKERS: [&str; 4] = [
    "f1-plan-compliance",
    "f2-code-quality",
    "f3-real-qa",
    "f4-scope-fidelity",
];

/// Whether `todo` is one of the cancelled editor-plane todos.
#[must_use]
pub fn is_cancelled(todo: u32) -> bool {
    CANCELLED_TODOS.contains(&todo)
}

/// The full-profile discovery manifest a contract 0.2 candidate ships.
pub const COMPLETE_RELEASE_MANIFEST: &str = "schemas/domain/profiles/v0.2/full.json";

/// Which release a frozen candidate is, read from the candidate itself: a tree
/// that ships the contract 0.2 discovery manifests is the complete-development
/// release, anything else is the first release.
///
/// The first release's checks prove the cancelled editor and runtime planes
/// absent; the complete release delivers them, so its checks prove them present
/// instead, and an absence check would only pass for the wrong reason there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateRelease {
    /// Version 0.1: Todos 20-25 are cancelled and must be provably absent.
    FirstRelease,
    /// Contract 0.2: the editor, runtime and export planes are delivered.
    Complete,
}

impl CandidateRelease {
    /// The release of the candidate tree at `worktree`.
    #[must_use]
    pub fn of(worktree: &Path) -> Self {
        if worktree.join(COMPLETE_RELEASE_MANIFEST).is_file() {
            Self::Complete
        } else {
            Self::FirstRelease
        }
    }
}

/// Source the complete release's delivered planes are built from, and the
/// real-entry checks that exercise them against real Godot builds.
pub const COMPLETE_RELEASE_EVIDENCE: [&str; 9] = [
    "crates/godot-mcp-bridge/addon/godot_mcp/plugin.cfg",
    "crates/godot-mcp-bridge/addon/godot_mcp/plugin.gd",
    "crates/godot-mcp-bridge/runtime/runtime_probe.gd",
    "crates/godot-mcp-bridge/runtime/runtime_launcher.gd",
    "scripts/real_client/m4_smoke.py",
    "scripts/real_client/m5_smoke.py",
    "scripts/real_client/m6_smoke.py",
    "scripts/real_client/e2e_complete.py",
    "docs/contributor/adr/0010-complete-development-bridge-strategy.md",
];

/// Tools the complete release's full profile must discover: one per delivered
/// plane, each with a production handler.
pub const COMPLETE_RELEASE_TOOLS: [&str; 6] = [
    "editor.scene.tree.get",
    "project.run.start",
    "runtime.tree.get",
    "runtime.screenshot.capture",
    "debug.command",
    "project.export",
];

/// The one directory a native extension may live in: the isolated,
/// non-shipping gdext reference lane.
pub const GDEXT_REFERENCE_LANE: &str = "gdext-reference";

// ---------------------------------------------------------------------------
// Strict command-line parsing
// ---------------------------------------------------------------------------

/// A strict `--flag value` command line.
///
/// Every flag must appear in the binary's declared set, must carry a value, and
/// must appear at most once. A positional argument, an unknown flag, a repeated
/// flag or a missing value is a usage error, so no invocation is ever silently
/// reinterpreted.
pub struct Flags {
    values: BTreeMap<String, String>,
    usage: &'static str,
}

impl Flags {
    /// Parse this process's arguments against the declared flag set.
    ///
    /// # Errors
    /// Returns [`GateError::Usage`] for a positional argument, an unknown flag, a
    /// repeated flag or a flag without a value.
    pub fn parse(usage: &'static str, known: &[&str]) -> Result<Self, GateError> {
        Self::from_iter(usage, known, std::env::args().skip(1))
    }

    /// Parse an explicit argument sequence; used by tests and by [`Flags::parse`].
    ///
    /// # Errors
    /// Same conditions as [`Flags::parse`].
    pub fn from_iter<I: IntoIterator<Item = String>>(
        usage: &'static str,
        known: &[&str],
        arguments: I,
    ) -> Result<Self, GateError> {
        let mut values: BTreeMap<String, String> = BTreeMap::new();
        let mut arguments = arguments.into_iter();
        while let Some(flag) = arguments.next() {
            if !known.contains(&flag.as_str()) {
                return Err(GateError::Usage(format!(
                    "unknown argument {flag:?}\n{usage}"
                )));
            }
            let value = arguments
                .next()
                .ok_or_else(|| GateError::Usage(format!("{flag} requires a value\n{usage}")))?;
            if values.insert(flag.clone(), value).is_some() {
                return Err(GateError::Usage(format!(
                    "{flag} was given more than once\n{usage}"
                )));
            }
        }
        Ok(Self { values, usage })
    }

    /// A required flag's raw value.
    ///
    /// # Errors
    /// Returns [`GateError::Usage`] when the flag is absent or empty.
    pub fn value(&self, flag: &str) -> Result<&str, GateError> {
        match self.values.get(flag) {
            Some(value) if !value.trim().is_empty() => Ok(value),
            Some(_) => Err(GateError::Usage(format!(
                "{flag} was given an empty value\n{}",
                self.usage
            ))),
            None => Err(GateError::Usage(format!(
                "{flag} is required\n{}",
                self.usage
            ))),
        }
    }

    /// A required flag's value as a path.
    ///
    /// # Errors
    /// Same conditions as [`Flags::value`].
    pub fn path(&self, flag: &str) -> Result<PathBuf, GateError> {
        Ok(PathBuf::from(self.value(flag)?))
    }
}

// ---------------------------------------------------------------------------
// Output-slot discipline
// ---------------------------------------------------------------------------

/// Reject an output path that already exists in any form.
///
/// # Errors
/// Returns [`GateError::Io`] when anything exists at `path`, so a run can never
/// overwrite, merge with or be confused by a previous emission.
pub fn require_absent(label: &str, path: &Path) -> Result<(), GateError> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(GateError::Io {
            path: path.display().to_string(),
            reason: format!(
                "{label} must be absent before this command runs; it already exists, and a \
                 qualification output slot is never overwritten"
            ),
        }),
        Err(error) => Err(GateError::Io {
            path: path.display().to_string(),
            reason: format!("{label}: {error}"),
        }),
    }
}

/// Require an existing regular file.
///
/// # Errors
/// Returns [`GateError::Io`] when the path is absent or is not a regular file.
pub fn require_file(label: &str, path: &Path) -> Result<(), GateError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(GateError::Io {
            path: path.display().to_string(),
            reason: format!("{label} exists but is not a regular file"),
        }),
        Err(error) => Err(GateError::Io {
            path: path.display().to_string(),
            reason: format!("{label} is required: {error}"),
        }),
    }
}

/// Require an existing directory.
///
/// # Errors
/// Returns [`GateError::Io`] when the path is absent or is not a directory.
pub fn require_dir(label: &str, path: &Path) -> Result<(), GateError> {
    if path.is_dir() {
        Ok(())
    } else {
        Err(GateError::Io {
            path: path.display().to_string(),
            reason: format!("{label} must be an existing directory"),
        })
    }
}

/// Slash-normalized textual form of a path, for prefix rules.
#[must_use]
pub fn normalize(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Whether any of these output paths lies inside the real evidence tree.
#[must_use]
pub fn targets_evidence_tree(outputs: &[PathBuf]) -> bool {
    outputs
        .iter()
        .any(|path| normalize(path).contains(EVIDENCE_PREFIX))
}

/// Read one of the structural test-seam environment overrides.
///
/// # Errors
/// Returns [`GateError::Usage`] when the override is set while any output path
/// lies under `.omo/evidence/`. The refusal is loud: a qualifying emission never
/// silently ignores a substituted catalogue or an injected fault.
pub fn read_seam(key: &str, outputs: &[PathBuf]) -> Result<Option<String>, GateError> {
    let Some(value) = std::env::var(key)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    if targets_evidence_tree(outputs) {
        return Err(GateError::Usage(format!(
            "{key} is set, but this invocation writes under `{EVIDENCE_PREFIX}`. The real \
             qualifying emission never accepts a substituted catalogue or an injected fault; \
             unset {key} or write outside the evidence tree"
        )));
    }
    Ok(Some(value))
}

/// Fail at a named injected fault point, when one is armed.
///
/// # Errors
/// Returns [`GateError::Io`] naming the point, so the caller's fail-closed
/// rollback runs exactly as it would for a real failure.
pub fn fault(point: &str, armed: Option<&str>) -> Result<(), GateError> {
    if armed == Some(point) {
        return Err(GateError::Io {
            path: format!("<fault:{point}>"),
            reason: format!("injected failure at {point}"),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Candidate identity
// ---------------------------------------------------------------------------

/// The frozen candidate's exact git identity, read from the candidate identity
/// document the freeze step writes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateIdentity {
    /// Frozen candidate commit object id: exactly 40 lowercase hex digits.
    pub commit: String,
    /// Frozen candidate tree object id: exactly 40 lowercase hex digits, never
    /// equal to `commit`.
    pub tree: String,
    /// Optional human freeze marker, carried verbatim and never interpreted.
    #[serde(default)]
    pub frozen_at: Option<String>,
}

impl CandidateIdentity {
    /// Load and validate the candidate identity document.
    ///
    /// # Errors
    /// Returns [`GateError`] when the file is unreadable, is not canonical JSON,
    /// carries an unknown field, or records ids that are not two distinct
    /// 40-hex-digit object ids.
    pub fn load(path: &Path) -> Result<Self, GateError> {
        require_file("--candidate-identity", path)?;
        let bytes = read_bytes(path)?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|error| GateError::Json {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
        let identity: Self = serde_json::from_value(value).map_err(|error| GateError::Schema {
            path: path.display().to_string(),
            reason: format!(
                "candidate identity must be exactly {{\"commit\":<40 hex>,\"tree\":<40 hex>}} \
                     with an optional \"frozen_at\": {error}"
            ),
        })?;
        for (field, value) in [("commit", &identity.commit), ("tree", &identity.tree)] {
            if !is_lower_hex_object_id(value) {
                return Err(GateError::Schema {
                    path: path.display().to_string(),
                    reason: format!(
                        "candidate `{field}` must be exactly 40 lowercase hexadecimal digits, got \
                         {value:?}"
                    ),
                });
            }
        }
        if identity.commit == identity.tree {
            return Err(GateError::Schema {
                path: path.display().to_string(),
                reason: "the candidate commit id can never equal its own tree id; two equal \
                         values mean one hash was copied into both fields"
                    .to_owned(),
            });
        }
        Ok(identity)
    }
}

// ---------------------------------------------------------------------------
// Findings
// ---------------------------------------------------------------------------

/// One finding recorded by an F1–F4 checker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// `Blocker`, `Major` or `Follow-up`.
    pub severity: String,
    /// `open` or `closed`.
    pub status: String,
    /// Nonempty summary of exactly what was observed.
    pub summary: String,
}

impl Finding {
    /// Whether this finding blocks approval: an open Blocker or open Major.
    #[must_use]
    pub fn blocks(&self) -> bool {
        self.status == "open" && matches!(self.severity.as_str(), "Blocker" | "Major")
    }
}

/// An ordered, deduplicating finding collector.
#[derive(Debug, Default)]
pub struct Findings {
    entries: Vec<Finding>,
}

impl Findings {
    /// A new empty collector.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, severity: &str, summary: String) {
        let finding = Finding {
            severity: severity.to_owned(),
            status: "open".to_owned(),
            summary,
        };
        if !self.entries.contains(&finding) {
            self.entries.push(finding);
        }
    }

    /// Record an open Blocker.
    pub fn blocker(&mut self, summary: impl Into<String>) {
        self.push("Blocker", summary.into());
    }

    /// Record an open Major.
    pub fn major(&mut self, summary: impl Into<String>) {
        self.push("Major", summary.into());
    }

    /// Record an open Blocker unless `ok` holds. Returns `ok` so callers can
    /// short-circuit dependent checks.
    pub fn require(&mut self, ok: bool, summary: impl Into<String>) -> bool {
        if !ok {
            self.blocker(summary);
        }
        ok
    }

    /// Whether any recorded finding blocks approval.
    #[must_use]
    pub fn blocking(&self) -> bool {
        self.entries.iter().any(Finding::blocks)
    }

    /// The blocking summaries, in order.
    #[must_use]
    pub fn blocking_summaries(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|finding| finding.blocks())
            .map(|finding| finding.summary.clone())
            .collect()
    }

    /// Consume the collector and return its findings in order.
    #[must_use]
    pub fn into_vec(self) -> Vec<Finding> {
        self.entries
    }
}

// ---------------------------------------------------------------------------
// The two bound qualification inputs
// ---------------------------------------------------------------------------

/// The two qualification documents every F1–F5 step binds, already verified
/// against their strict sidecars and canonical schemas.
pub struct Bound {
    /// Path of `input-identity.json` as supplied.
    pub identity_path: PathBuf,
    /// Path of `qualification-report.json` as supplied.
    pub report_path: PathBuf,
    /// `input-identity.sha256`'s digest, equal to the identity bytes' SHA-256.
    pub identity_sha256: String,
    /// `qualification-report.sha256`'s digest, equal to the report bytes' SHA-256.
    pub report_sha256: String,
    /// Parsed identity document.
    pub identity: Value,
    /// Parsed report document.
    pub report: Value,
}

/// The single hash a receipt records as `before_bound_hash` / `after_bound_hash`.
///
/// It is a hash *of the two sidecar digests*, never of the documents, so a
/// receipt still binds no identity or report body.
#[must_use]
pub fn bound_hash(identity_sha256: &str, report_sha256: &str) -> String {
    hex_sha256(format!("{identity_sha256}\n{report_sha256}\n").as_bytes())
}

/// Load, sidecar-verify and schema-validate the identity/report pair.
///
/// # Errors
/// Returns the first [`GateError`] explaining why the pair is unusable: a missing
/// or malformed sidecar, a digest mismatch, noncanonical bytes, a schema
/// violation, or a report that does not bind exactly this identity's digest.
pub fn load_bound(identity: &Path, report: &Path, workspace: &Path) -> Result<Bound, GateError> {
    let (identity_sha256, identity_value) = verify_artifact(identity, &implied_sidecar(identity))?;
    validate_against_schema(
        &identity.display().to_string(),
        &workspace.join(IDENTITY_SCHEMA),
        &identity_value,
    )?;
    let (report_sha256, report_value) = verify_artifact(report, &implied_sidecar(report))?;
    validate_against_schema(
        &report.display().to_string(),
        &workspace.join(REPORT_SCHEMA),
        &report_value,
    )?;
    let bound_identity = report_value
        .get("input_identity_sha256")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if bound_identity != identity_sha256 {
        return Err(GateError::ReceiptRejected {
            path: report.display().to_string(),
            reason: format!(
                "the report binds input identity {bound_identity} but {} hashes to \
                 {identity_sha256}; the qualification hash graph is strictly forward, so a report \
                 may only bind the identity it was produced against",
                identity.display()
            ),
        });
    }
    Ok(Bound {
        identity_path: identity.to_path_buf(),
        report_path: report.to_path_buf(),
        identity_sha256,
        report_sha256,
        identity: identity_value,
        report: report_value,
    })
}

/// Re-measure the two sidecar digests without reparsing, so a checker can prove
/// it did not mutate its own bound evidence.
///
/// # Errors
/// Returns [`GateError`] when either artifact/sidecar pair no longer verifies.
pub fn remeasure_bound_hash(bound: &Bound) -> Result<String, GateError> {
    let (identity_sha256, _) =
        verify_artifact_bytes(&bound.identity_path, &implied_sidecar(&bound.identity_path))?;
    let (report_sha256, _) =
        verify_artifact_bytes(&bound.report_path, &implied_sidecar(&bound.report_path))?;
    Ok(bound_hash(&identity_sha256, &report_sha256))
}

// ---------------------------------------------------------------------------
// Producer provenance
// ---------------------------------------------------------------------------

/// Provenance recorded in every qualification path this producer writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    /// Always [`PRODUCER_NAME`].
    pub name: String,
    /// SHA-256 of the producer binary's own bytes.
    pub binary_sha256: String,
}

impl Provenance {
    /// Provenance of the currently running producer binary.
    ///
    /// # Errors
    /// Returns [`GateError::Io`] when this process's own executable cannot be
    /// read, because unbound provenance is not publishable.
    pub fn of_current_binary() -> Result<Self, GateError> {
        let executable = std::env::current_exe().map_err(|error| GateError::Io {
            path: "<current-exe>".to_owned(),
            reason: error.to_string(),
        })?;
        Ok(Self {
            name: PRODUCER_NAME.to_owned(),
            binary_sha256: hex_sha256(&read_bytes(&executable)?),
        })
    }

    /// Serialize as the canonical provenance object.
    ///
    /// # Errors
    /// Returns [`GateError::Json`] when serialization fails.
    pub fn to_value(&self) -> Result<Value, GateError> {
        serde_json::to_value(self).map_err(|error| GateError::Json {
            path: "<in-memory>".to_owned(),
            reason: error.to_string(),
        })
    }
}

/// Check a document's `producer` provenance, recording findings rather than
/// erroring so a checker can report every mismatch at once.
pub fn check_provenance(findings: &mut Findings, label: &str, document: &Value) {
    let Some(producer) = document.get("producer") else {
        findings.blocker(format!(
            "{label} declares no `producer`, so its emission is not attributed to the \
             Todo-37-owned {PRODUCER_NAME} binary and cannot be assumed to have appeared \
             implicitly"
        ));
        return;
    };
    let name = producer.get("name").and_then(Value::as_str).unwrap_or("");
    findings.require(
        name == PRODUCER_NAME,
        format!("{label} names producer {name:?}, not {PRODUCER_NAME:?}"),
    );
    let binary = producer
        .get("binary_sha256")
        .and_then(Value::as_str)
        .unwrap_or("");
    findings.require(
        is_lower_hex_sha256(binary),
        format!("{label} records no valid producer binary hash"),
    );
}

/// Reject any text that attributes the final emission to Todo 35, whose harness
/// is owned and tested only as a harness and never emits the final artifact.
pub fn check_no_todo35_attribution(findings: &mut Findings, label: &str, document: &Value) {
    let text = document.to_string().to_ascii_lowercase();
    for needle in ["todo 35", "todo-35", "todo35"] {
        if text.contains(needle) {
            findings.blocker(format!(
                "{label} attributes emission to {needle:?}; Todo 35 owns and freezes only the \
                 release-gate harness and never emits the final artifact"
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Bound-input map
// ---------------------------------------------------------------------------

/// One content-addressed input bound into the frozen identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundInput {
    /// The path the digest was measured from, exactly as the producer received it.
    pub path: String,
    /// SHA-256 of that file's bytes.
    pub sha256: String,
}

/// The logical bound-input keys every frozen identity must carry.
pub const REQUIRED_BOUND_INPUTS: [&str; 14] = [
    "plan",
    "cargo-lock",
    "rust-toolchain",
    "tooling-lock",
    "godot-lock",
    "candidate-identity",
    "red-evidence-index",
    "architecture-momus-receipt",
    "architecture-codex-receipt",
    "b3-approval",
    "b4-approval",
    "release-gates",
    "review-bindings",
    "archive-checksums",
];

/// Read an identity's `bound_inputs` map.
///
/// # Errors
/// Returns [`GateError::Schema`] when the field is absent or malformed.
pub fn bound_inputs(identity: &Value) -> Result<BTreeMap<String, BoundInput>, GateError> {
    let value = identity
        .get("bound_inputs")
        .ok_or_else(|| GateError::Schema {
            path: "input-identity.json".to_owned(),
            reason: "the frozen identity declares no `bound_inputs`".to_owned(),
        })?;
    serde_json::from_value(value.clone()).map_err(|error| GateError::Schema {
        path: "input-identity.json".to_owned(),
        reason: format!("`bound_inputs` is malformed: {error}"),
    })
}

// ---------------------------------------------------------------------------
// Candidate input binding
// ---------------------------------------------------------------------------

/// The resolved candidate input an F1–F4 receipt records after checking it
/// against the frozen identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CandidateInput {
    /// `worktree` or `archive`.
    pub kind: String,
    /// The supplied input path, retained for later audit.
    pub path: String,
    /// Resolved `HEAD` for a worktree input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_commit: Option<String>,
    /// Resolved `HEAD^{tree}` for a worktree input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_tree: Option<String>,
    /// Whether a worktree had no tracked modifications.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracked_clean: Option<bool>,
    /// File name used to locate an archive in `bound_inputs`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_name: Option<String>,
    /// SHA-256 measured from an archive before extraction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_sha256: Option<String>,
    #[serde(skip)]
    verified: bool,
}

impl CandidateInput {
    /// Whether this input was proven equal to the candidate named by the identity.
    #[must_use]
    pub fn verified(&self) -> bool {
        self.verified
    }
}

/// Resolve a worktree's commit, tree and tracked-clean state and compare each
/// fact with the frozen candidate identity before a checker audits the tree.
#[must_use]
pub fn verify_candidate_worktree(
    findings: &mut Findings,
    worktree: &Path,
    bound: &Bound,
) -> CandidateInput {
    let resolved_commit = git_stdout(findings, worktree, &["rev-parse", "HEAD"]);
    let resolved_tree = git_stdout(findings, worktree, &["rev-parse", "HEAD^{tree}"]);
    let tracked_status = git_stdout(
        findings,
        worktree,
        &["status", "--porcelain=v1", "--untracked-files=no"],
    );
    let tracked_clean = tracked_status.as_ref().map(|status| status.is_empty());
    let expected_commit = bound
        .identity
        .get("candidate_commit")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let expected_tree = bound
        .identity
        .get("candidate_tree")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let commit_matches = resolved_commit.as_deref() == Some(expected_commit);
    findings.require(
        commit_matches,
        format!(
            "the audited worktree resolves candidate_commit {:?}, not the identity's {:?}",
            resolved_commit.as_deref().unwrap_or("<unresolved>"),
            expected_commit
        ),
    );
    let tree_matches = resolved_tree.as_deref() == Some(expected_tree);
    findings.require(
        tree_matches,
        format!(
            "the audited worktree resolves candidate_tree {:?}, not the identity's {:?}",
            resolved_tree.as_deref().unwrap_or("<unresolved>"),
            expected_tree
        ),
    );
    let clean = tracked_clean == Some(true);
    findings.require(
        clean,
        format!(
            "the audited worktree has tracked modifications, so its bytes are not exactly the bytes \
             named by candidate_tree {expected_tree}; git status was {:?}",
            tracked_status.as_deref().unwrap_or("<unresolved>")
        ),
    );

    CandidateInput {
        kind: "worktree".to_owned(),
        path: normalize(worktree),
        resolved_commit,
        resolved_tree,
        tracked_clean,
        archive_name: None,
        archive_sha256: None,
        verified: commit_matches && tree_matches && clean,
    }
}

fn git_stdout(findings: &mut Findings, worktree: &Path, args: &[&str]) -> Option<String> {
    match Command::new("git")
        .args(args)
        .current_dir(worktree)
        .output()
    {
        Ok(output) if output.status.success() => Some(
            String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_owned(),
        ),
        Ok(output) => {
            findings.blocker(format!(
                "git {} failed in {} with exit {}: {}",
                args.join(" "),
                worktree.display(),
                output
                    .status
                    .code()
                    .map_or_else(|| "by signal".to_owned(), |code| code.to_string()),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
            None
        }
        Err(error) => {
            findings.blocker(format!(
                "git {} could not run in {}: {error}",
                args.join(" "),
                worktree.display()
            ));
            None
        }
    }
}

/// Hash an archive and prove that its name and digest match one archive entry in
/// the identity before any extractor sees its bytes.
#[must_use]
pub fn verify_candidate_archive(
    findings: &mut Findings,
    archive: &Path,
    bound: &Bound,
) -> CandidateInput {
    let archive_name = archive
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let archive_sha256 = read_bytes(archive).ok().map(|bytes| hex_sha256(&bytes));
    if archive_sha256.is_none() {
        findings.blocker(format!(
            "the candidate archive {} is not readable, so its SHA-256 cannot be verified",
            archive.display()
        ));
    }
    let inputs = bound_inputs(&bound.identity);
    if let Err(error) = &inputs {
        findings.blocker(format!(
            "the frozen identity's archive bindings are unusable: {error}"
        ));
    }
    let binding = archive_name.as_ref().and_then(|name| {
        inputs
            .as_ref()
            .ok()
            .and_then(|inputs| inputs.get(&format!("archive:{name}")))
    });
    if archive_name.is_some() && binding.is_none() {
        findings.blocker(format!(
            "the identity binds no archive named {:?}; F3 never accepts a self-attested package",
            archive_name.as_deref().unwrap_or_default()
        ));
    }
    let matches = binding
        .zip(archive_sha256.as_ref())
        .is_some_and(|(binding, actual)| binding.sha256 == *actual);
    if let (Some(binding), Some(actual)) = (binding, archive_sha256.as_ref()) {
        findings.require(
            matches,
            format!(
                "the candidate archive SHA-256 is {actual}, not the identity-bound {} for {:?}",
                binding.sha256,
                archive_name.as_deref().unwrap_or_default()
            ),
        );
    }

    CandidateInput {
        kind: "archive".to_owned(),
        path: normalize(archive),
        resolved_commit: None,
        resolved_tree: None,
        tracked_clean: None,
        archive_name,
        archive_sha256,
        verified: matches,
    }
}

static EXTRACTION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One freshly extracted candidate archive. Its directory is removed on drop.
pub struct ExtractedArchive {
    root: PathBuf,
    package: PathBuf,
}

impl ExtractedArchive {
    /// The package directory containing `MANIFEST.json`.
    #[must_use]
    pub fn package(&self) -> &Path {
        &self.package
    }
}

impl Drop for ExtractedArchive {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Validate archive member paths, extract into a private fresh directory, and
/// locate the single package root.
pub fn extract_candidate_archive(
    archive: &Path,
    verified_sha256: &str,
) -> Result<ExtractedArchive, GateError> {
    let name = archive
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let (list_program, list_args, extract_program, mut extract_args) = if name.ends_with(".tar.gz")
    {
        (
            "tar",
            vec!["-tzf".to_owned(), archive.display().to_string()],
            "tar",
            vec![
                "-xzf".to_owned(),
                archive.display().to_string(),
                "--no-same-owner".to_owned(),
                "--no-same-permissions".to_owned(),
                "-C".to_owned(),
            ],
        )
    } else if name.ends_with(".zip") {
        (
            "unzip",
            vec!["-Z1".to_owned(), archive.display().to_string()],
            "unzip",
            vec![
                "-q".to_owned(),
                archive.display().to_string(),
                "-d".to_owned(),
            ],
        )
    } else {
        return Err(GateError::Io {
            path: archive.display().to_string(),
            reason: "candidate archive must end in .tar.gz or .zip".to_owned(),
        });
    };
    let listed = Command::new(list_program)
        .args(&list_args)
        .output()
        .map_err(|error| GateError::Io {
            path: archive.display().to_string(),
            reason: format!("archive member listing could not run: {error}"),
        })?;
    if !listed.status.success() {
        return Err(GateError::Io {
            path: archive.display().to_string(),
            reason: format!(
                "archive member listing exited {}: {}",
                listed
                    .status
                    .code()
                    .map_or_else(|| "by signal".to_owned(), |code| code.to_string()),
                String::from_utf8_lossy(&listed.stderr).trim()
            ),
        });
    }
    for member in String::from_utf8_lossy(&listed.stdout).lines() {
        let path = Path::new(member);
        if member.is_empty()
            || path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
        {
            return Err(GateError::Io {
                path: archive.display().to_string(),
                reason: format!("archive member {member:?} can escape the fresh extraction root"),
            });
        }
    }

    let root = allocate_extraction_root(verified_sha256)?;
    extract_args.push(root.display().to_string());
    let extracted = Command::new(extract_program)
        .args(&extract_args)
        .output()
        .map_err(|error| GateError::Io {
            path: archive.display().to_string(),
            reason: format!("archive extraction could not run: {error}"),
        })?;
    if !extracted.status.success() {
        let _ = std::fs::remove_dir_all(&root);
        return Err(GateError::Io {
            path: archive.display().to_string(),
            reason: format!(
                "archive extraction exited {}: {}",
                extracted
                    .status
                    .code()
                    .map_or_else(|| "by signal".to_owned(), |code| code.to_string()),
                String::from_utf8_lossy(&extracted.stderr).trim()
            ),
        });
    }
    let package = locate_package_root(&root)?;
    Ok(ExtractedArchive { root, package })
}

fn allocate_extraction_root(verified_sha256: &str) -> Result<PathBuf, GateError> {
    for _ in 0..128 {
        let serial = EXTRACTION_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "godot-mcp-f3-{}-{}-{serial}",
            std::process::id(),
            &verified_sha256[..verified_sha256.len().min(12)]
        ));
        match std::fs::create_dir(&root) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
                        .map_err(|error| GateError::Io {
                            path: root.display().to_string(),
                            reason: error.to_string(),
                        })?;
                }
                return Ok(root);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(GateError::Io {
                    path: root.display().to_string(),
                    reason: error.to_string(),
                });
            }
        }
    }
    Err(GateError::Io {
        path: std::env::temp_dir().display().to_string(),
        reason: "could not allocate a fresh F3 extraction directory".to_owned(),
    })
}

fn locate_package_root(root: &Path) -> Result<PathBuf, GateError> {
    if root.join("MANIFEST.json").is_file() {
        return Ok(root.to_path_buf());
    }
    let children: Vec<PathBuf> = std::fs::read_dir(root)
        .map_err(|error| GateError::Io {
            path: root.display().to_string(),
            reason: error.to_string(),
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    if children.len() == 1 && children[0].is_dir() && children[0].join("MANIFEST.json").is_file() {
        return Ok(children[0].clone());
    }
    Err(GateError::Io {
        path: root.display().to_string(),
        reason: "the verified archive must extract to one package containing MANIFEST.json"
            .to_owned(),
    })
}

// ---------------------------------------------------------------------------
// Executable step catalogues
// ---------------------------------------------------------------------------

/// One executable step in a producer evidence catalogue or a checker gate
/// catalogue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepSpec {
    /// Stable step name recorded in the emitted evidence.
    pub name: String,
    /// The exact command string recorded in the evidence.
    pub command: String,
    /// Program actually executed.
    pub program: String,
    /// Arguments actually passed.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables the step needs, applied to the child only.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The exit code the step must produce. Defaults to `0`.
    #[serde(default)]
    pub expected_exit: i64,
}

impl StepSpec {
    /// A cargo-driven step whose recorded command is the reconstructed invocation.
    #[must_use]
    pub fn cargo(name: &str, args: &[&str]) -> Self {
        let program = cargo_program();
        let command = format!("{} {}", normalize(Path::new(&program)), args.join(" "));
        Self {
            name: name.to_owned(),
            command,
            program,
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            env: BTreeMap::new(),
            expected_exit: 0,
        }
    }

    /// A step invoking an arbitrary program.
    #[must_use]
    pub fn program(name: &str, program: &str, args: &[String], expected_exit: i64) -> Self {
        let command = format!("{program} {}", args.join(" "));
        Self {
            name: name.to_owned(),
            command,
            program: program.to_owned(),
            args: args.to_vec(),
            env: BTreeMap::new(),
            expected_exit,
        }
    }

    /// The same step with one environment variable added.
    #[must_use]
    pub fn with_env(mut self, key: &str, value: &str) -> Self {
        self.env.insert(key.to_owned(), value.to_owned());
        self
    }
}

/// The `cargo` executable this build was driven by, so a nested invocation uses
/// the same toolchain rather than whatever happens to be on `PATH`.
#[must_use]
pub fn cargo_program() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned())
}

/// The outcome of one executed step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutcome {
    /// The step that ran.
    pub spec: StepSpec,
    /// Its real captured exit code, or a sentinel when it could not be spawned.
    pub exit_code: i64,
}

impl StepOutcome {
    /// Whether the step produced the exit code it was required to produce.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.exit_code == self.spec.expected_exit
    }

    /// The `{name, command, exit_code}` shape the qualification report records.
    #[must_use]
    pub fn to_gate_value(&self) -> Value {
        let mut object = Map::new();
        object.insert(
            "command".to_owned(),
            Value::String(self.spec.command.clone()),
        );
        object.insert("exit_code".to_owned(), Value::from(self.exit_code));
        object.insert("name".to_owned(), Value::String(self.spec.name.clone()));
        Value::Object(object)
    }
}

/// Sentinel exit code recorded for a step that could not be spawned at all. A
/// step that never ran is never a pass.
pub const EXIT_NOT_SPAWNED: i64 = 70;

/// Execute one step in `cwd` and capture its real exit code.
///
/// The child's own output is echoed to **stderr**, never stdout: every binary here
/// reserves stdout for exactly one machine-readable document, so a nested gate can
/// never corrupt that contract.
#[must_use]
pub fn run_step(spec: &StepSpec, cwd: &Path) -> StepOutcome {
    let exit_code = match Command::new(&spec.program)
        .args(&spec.args)
        .envs(&spec.env)
        .current_dir(cwd)
        .output()
    {
        Ok(output) => {
            echo_child_output(&spec.name, &output);
            output.status.code().map_or(EXIT_NOT_SPAWNED, i64::from)
        }
        Err(error) => {
            eprintln!("{}: could not be spawned: {error}", spec.name);
            EXIT_NOT_SPAWNED
        }
    };
    StepOutcome {
        spec: spec.clone(),
        exit_code,
    }
}

/// Echo one child process's captured output to stderr, prefixed by the step name.
pub fn echo_child_output(label: &str, output: &std::process::Output) {
    for (stream, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        let text = String::from_utf8_lossy(bytes);
        for line in text.lines() {
            eprintln!("{label} [{stream}] {line}");
        }
    }
}

/// Load a catalogue override from the path a test seam supplied.
///
/// # Errors
/// Returns [`GateError`] when the file is unreadable or is not an array of
/// [`StepSpec`].
pub fn load_step_plan(path: &Path) -> Result<Vec<StepSpec>, GateError> {
    let bytes = read_bytes(path)?;
    let specs: Vec<StepSpec> = serde_json::from_slice(&bytes).map_err(|error| GateError::Json {
        path: path.display().to_string(),
        reason: error.to_string(),
    })?;
    if specs.is_empty() {
        return Err(GateError::Schema {
            path: path.display().to_string(),
            reason: "a step catalogue is never empty".to_owned(),
        });
    }
    Ok(specs)
}

// ---------------------------------------------------------------------------
// Receipt emission
// ---------------------------------------------------------------------------

/// A checker's computed verdict plus the receipt it wrote.
pub struct ReceiptOutcome {
    /// `APPROVE` or `REJECT`.
    pub verdict: &'static str,
    /// SHA-256 of the receipt's canonical bytes.
    pub receipt_sha256: String,
}

impl ReceiptOutcome {
    /// The process exit code: `0` only for APPROVE.
    #[must_use]
    pub fn exit_code(&self) -> ExitCode {
        if self.verdict == "APPROVE" {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(crate::gates::REJECTED_EXIT)
        }
    }
}

/// Build, schema-validate and atomically publish one F1–F4 hash-only receipt.
///
/// The verdict is computed here, never supplied: APPROVE requires zero open
/// Blocker or Major findings *and* an unchanged bound hash. The receipt and its
/// content-hash sidecar are written as one pair, and the receipt never carries the
/// identity or report body.
///
/// # Errors
/// Returns [`GateError`] when the receipt does not validate against
/// `schemas/qualification/reviewer-receipt.schema.json` or cannot be published;
/// in either case no receipt is left behind.
pub fn emit_receipt(
    checker: &str,
    output: &Path,
    workspace: &Path,
    bound: &Bound,
    bound_hashes: (&str, &str),
    candidate_input: &CandidateInput,
    findings: Vec<Finding>,
) -> Result<ReceiptOutcome, GateError> {
    let (before, after) = bound_hashes;
    let mut findings = findings;
    if before != after {
        findings.push(Finding {
            severity: "Blocker".to_owned(),
            status: "open".to_owned(),
            summary: format!(
                "the bound qualification evidence changed while {checker} was running (bound hash \
                 {before} became {after}); receipt-time evidence mutation is never approvable"
            ),
        });
    }
    let verdict = if findings.iter().any(Finding::blocks) {
        "REJECT"
    } else {
        "APPROVE"
    };
    let round_id = derive_id(checker, before, "round");
    let launch_id = derive_id(checker, before, "launch");
    let receipt = serde_json::json!({
        "after_bound_hash": after,
        "before_bound_hash": before,
        "candidate_input": candidate_input,
        "findings": findings,
        "input_identity_sha256": bound.identity_sha256,
        "launch_id": launch_id,
        "qualification_report_sha256": bound.report_sha256,
        "round_id": round_id,
        "schema_version": QUALIFICATION_SCHEMA_VERSION,
        "verdict": verdict,
        "verifier_identity": checker,
    });
    validate_against_schema(
        &output.display().to_string(),
        &workspace.join(RECEIPT_SCHEMA),
        &receipt,
    )?;
    let bytes = canonical_bytes(&receipt)?;
    let receipt_sha256 = write_artifact(output, &implied_sidecar(output), &bytes)?;
    Ok(ReceiptOutcome {
        verdict,
        receipt_sha256,
    })
}

/// A deterministic, per-checker, per-bound-input identifier.
///
/// It is derived from the checker name plus the bound hash, so the four receipts
/// carry four independent ids, re-running a checker against the same frozen
/// inputs is reproducible, and no id can be reused across checkers or across a
/// different frozen identity.
#[must_use]
pub fn derive_id(checker: &str, bound: &str, role: &str) -> String {
    hex_sha256(format!("{role}\n{checker}\n{bound}\n").as_bytes())[..32].to_owned()
}

/// Print a checker's machine-readable verdict line on stdout.
///
/// The plan's `{"verdict":"REJECT", "failing_gates":[...]}` /
/// `{"creep_matches":[...]}` shapes are this stdout contract; the receipt file
/// itself stays exactly the canonical hash-only shape its schema pins, which
/// forbids additional properties.
pub fn print_verdict(outcome: &ReceiptOutcome, detail_key: &str, detail: &[String]) {
    let document = serde_json::json!({
        "receipt_sha256": outcome.receipt_sha256,
        "verdict": outcome.verdict,
        detail_key: detail,
    });
    println!("{document}");
}

// ---------------------------------------------------------------------------
// Shared readers used by more than one binary
// ---------------------------------------------------------------------------

/// Read and validate a release-gate evidence document plus its strict sidecar.
///
/// Every required check must have been `executed` with exit `0` and must be bound
/// to the frozen candidate commit/tree. A missing, skipped or unavailable check is
/// a failure here exactly as it is in the harness.
///
/// # Errors
/// Returns the first [`GateError`] explaining why the gates document does not
/// authorize a release.
pub fn verify_release_gates(
    gates: &Path,
    sidecar: &Path,
    workspace: &Path,
    candidate_commit: &str,
    candidate_tree: &str,
    require_qualifying: bool,
) -> Result<(String, Value), GateError> {
    let (digest, document) = verify_artifact(gates, sidecar)?;
    validate_against_schema(
        &gates.display().to_string(),
        &workspace.join(RELEASE_GATES_SCHEMA),
        &document,
    )?;
    let reject = |reason: String| GateError::ReceiptRejected {
        path: gates.display().to_string(),
        reason,
    };
    if require_qualifying && document.get("qualifying") != Some(&Value::Bool(true)) {
        return Err(reject(
            "the final release-gate evidence must be the qualifying emission produced against \
             the frozen candidate"
                .to_owned(),
        ));
    }
    let checks = document
        .get("checks")
        .and_then(Value::as_array)
        .ok_or_else(|| reject("`checks` is not an array".to_owned()))?;
    let mut required = 0_usize;
    for check in checks {
        if check.get("required") != Some(&Value::Bool(true)) {
            continue;
        }
        required += 1;
        let name = check
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("<unnamed>");
        let observation = check
            .get("observation")
            .and_then(Value::as_str)
            .unwrap_or("<absent>");
        if observation != "executed" {
            return Err(reject(format!(
                "required check `{name}` was {observation} on its runner, which is a failed entry \
                 and never a skipped non-failure"
            )));
        }
        let exit_code = check.get("exit_code").and_then(Value::as_i64);
        if exit_code != Some(0) {
            return Err(reject(format!(
                "required check `{name}` exited {}",
                exit_code.map_or_else(|| "<absent>".to_owned(), |code| code.to_string())
            )));
        }
        for (field, expected) in [
            ("source_commit", candidate_commit),
            ("source_tree", candidate_tree),
        ] {
            let observed = check.get(field).and_then(Value::as_str).unwrap_or("");
            if observed != expected {
                return Err(reject(format!(
                    "required check `{name}` bound {field} {observed:?} but the frozen candidate \
                     is {expected:?}; this is stale or replayed evidence, not a missing entry"
                )));
            }
        }
    }
    if required == 0 {
        return Err(reject(
            "the release-gate evidence names no required check at all".to_owned(),
        ));
    }
    Ok((digest, document))
}

/// An architecture-review receipt: one independent reviewer's verdict on the
/// exact frozen plan bytes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchitectureReviewReceipt {
    /// Gate schema version.
    pub schema_version: String,
    /// The current review round this receipt belongs to.
    pub round_id: String,
    /// The independent reviewer's identity.
    pub reviewer_identity: String,
    /// SHA-256 of the exact plan bytes reviewed.
    pub plan_sha256: String,
    /// `APPROVE` or `REJECT`.
    pub verdict: String,
    /// Review findings with severity and status.
    pub findings: Vec<Finding>,
}

impl ArchitectureReviewReceipt {
    /// Verify one architecture-review receipt against its sidecar and the live
    /// plan hash.
    ///
    /// # Errors
    /// Returns [`GateError::ReceiptRejected`] for a stale plan hash, a
    /// non-approving verdict or any open Blocker/Major, so a rejected round can
    /// never be bound as the current record.
    pub fn verify(
        receipt: &Path,
        sidecar: &Path,
        plan_sha256: &str,
    ) -> Result<(String, Self), GateError> {
        let (digest, value) = verify_artifact(receipt, sidecar)?;
        let parsed: Self = serde_json::from_value(value).map_err(|error| GateError::Schema {
            path: receipt.display().to_string(),
            reason: format!("architecture-review receipt is malformed: {error}"),
        })?;
        let reject = |reason: String| GateError::ReceiptRejected {
            path: receipt.display().to_string(),
            reason,
        };
        if !is_lower_hex_sha256(&parsed.plan_sha256) {
            return Err(reject(
                "`plan_sha256` is not 64 lowercase hex digits".to_owned(),
            ));
        }
        if parsed.plan_sha256 != plan_sha256 {
            return Err(reject(format!(
                "the receipt reviewed plan {} but the live plan hashes to {plan_sha256}; a \
                 historical round can never satisfy the current architecture review",
                parsed.plan_sha256
            )));
        }
        if parsed.verdict != "APPROVE" {
            return Err(reject(format!(
                "verdict is {:?}; qualification requires an unambiguous current APPROVE",
                parsed.verdict
            )));
        }
        let open: Vec<&Finding> = parsed.findings.iter().filter(|f| f.blocks()).collect();
        if !open.is_empty() {
            return Err(reject(format!(
                "the receipt records {} open Blocker/Major finding(s)",
                open.len()
            )));
        }
        if parsed.round_id.trim().is_empty() || parsed.reviewer_identity.trim().is_empty() {
            return Err(reject(
                "an architecture-review receipt needs a nonempty round id and reviewer identity"
                    .to_owned(),
            ));
        }
        Ok((digest, parsed))
    }
}

/// Verify a B3/B4 approval manifest against its sidecar, canonical schema and the
/// live plan hash.
///
/// # Errors
/// Returns [`GateError`] for a stale plan hash, a non-approving manifest, a wrong
/// batch, or receipts that are missing, duplicated or malformed.
pub fn verify_batch_approval(
    manifest: &Path,
    sidecar: &Path,
    workspace: &Path,
    batch: &str,
    plan_sha256: &str,
) -> Result<crate::gates::BatchApprovalManifest, GateError> {
    let (_, value) = verify_artifact(manifest, sidecar)?;
    validate_against_schema(
        &manifest.display().to_string(),
        &workspace.join(BATCH_APPROVAL_SCHEMA),
        &value,
    )?;
    let parsed: crate::gates::BatchApprovalManifest =
        serde_json::from_value(value).map_err(|error| GateError::Schema {
            path: manifest.display().to_string(),
            reason: format!("batch approval manifest is malformed: {error}"),
        })?;
    let reject = |reason: String| GateError::ReceiptRejected {
        path: manifest.display().to_string(),
        reason,
    };
    if parsed.batch != batch {
        return Err(reject(format!(
            "manifest is for batch {:?}, not {batch:?}",
            parsed.batch
        )));
    }
    if !parsed.approved {
        return Err(reject(
            "the manifest does not record an approval".to_owned(),
        ));
    }
    if parsed.plan_sha256 != plan_sha256 {
        return Err(reject(format!(
            "the manifest approved plan {} but the live plan hashes to {plan_sha256}; a stale \
             batch gate can never be bound as current",
            parsed.plan_sha256
        )));
    }
    if parsed.receipts.len() != 2 {
        return Err(reject(format!(
            "a dual review binds exactly two independent receipts, found {}",
            parsed.receipts.len()
        )));
    }
    if parsed.receipts[0].reviewer_identity == parsed.receipts[1].reviewer_identity {
        return Err(reject(
            "both receipts name the same reviewer, so the review was not independent".to_owned(),
        ));
    }
    if parsed.receipts[0].receipt_sha256 == parsed.receipts[1].receipt_sha256 {
        return Err(reject(
            "both receipts carry the same content hash, so one receipt was reused".to_owned(),
        ));
    }
    for receipt in &parsed.receipts {
        if !is_lower_hex_sha256(&receipt.receipt_sha256) {
            return Err(reject(format!(
                "receipt hash {:?} is not 64 lowercase hex digits",
                receipt.receipt_sha256
            )));
        }
    }
    Ok(parsed)
}

/// A Red-proof entry: the exact failing invocation before the fix, paired with the
/// passing invocation at the frozen tree.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedEvidenceEntry {
    /// The todo this proof belongs to.
    pub todo: u32,
    /// The exact command that reproduced the failure.
    pub command: String,
    /// Captured Red output artifact, relative to the workspace or absolute.
    pub red_output: String,
    /// SHA-256 of that Red output's bytes.
    pub red_output_sha256: String,
    /// The Red exit code, which is never zero.
    pub red_exit_code: i64,
    /// Captured Green output artifact.
    pub green_output: String,
    /// SHA-256 of that Green output's bytes.
    pub green_output_sha256: String,
    /// The Green exit code, which is always zero.
    pub green_exit_code: i64,
}

/// The Red evidence index the producer binds and F1 re-verifies.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedEvidenceIndex {
    /// Gate schema version.
    pub schema_version: String,
    /// One entry per proven Red/Green pair.
    pub entries: Vec<RedEvidenceEntry>,
}

impl RedEvidenceIndex {
    /// Load and fully verify the index against the tree it names.
    ///
    /// # Errors
    /// Returns [`GateError`] when the index is malformed, is empty, names a
    /// cancelled todo, names an absent artifact, records a digest the bytes do
    /// not produce, records a zero Red exit code, or records a nonzero Green exit
    /// code.
    pub fn verify(path: &Path, base: &Path) -> Result<Self, GateError> {
        require_file("--red-evidence-index", path)?;
        let bytes = read_bytes(path)?;
        let parsed: Self = serde_json::from_slice(&bytes).map_err(|error| GateError::Schema {
            path: path.display().to_string(),
            reason: format!("red evidence index is malformed: {error}"),
        })?;
        let reject = |reason: String| GateError::ReceiptRejected {
            path: path.display().to_string(),
            reason,
        };
        if parsed.entries.is_empty() {
            return Err(reject(
                "the Red evidence index proves nothing: it has no entries".to_owned(),
            ));
        }
        for entry in &parsed.entries {
            if is_cancelled(entry.todo) {
                return Err(reject(format!(
                    "todo {} is cancelled and delivered no Red proof, so claiming one is itself a \
                     rejection",
                    entry.todo
                )));
            }
            if entry.command.trim().is_empty() {
                return Err(reject(format!(
                    "todo {}'s Red proof records no command",
                    entry.todo
                )));
            }
            if entry.red_exit_code == 0 {
                return Err(reject(format!(
                    "todo {}'s Red proof exited 0, so it never reproduced a failure",
                    entry.todo
                )));
            }
            if entry.green_exit_code != 0 {
                return Err(reject(format!(
                    "todo {}'s Green run exited {}, so the frozen tree does not pass it",
                    entry.todo, entry.green_exit_code
                )));
            }
            for (role, relative, expected) in [
                ("Red", &entry.red_output, &entry.red_output_sha256),
                ("Green", &entry.green_output, &entry.green_output_sha256),
            ] {
                let resolved = resolve(base, Path::new(relative));
                let actual = hex_sha256(&read_bytes(&resolved)?);
                if &actual != expected {
                    return Err(reject(format!(
                        "todo {}'s {role} artifact {} hashes to {actual}, not the recorded \
                         {expected}",
                        entry.todo,
                        resolved.display()
                    )));
                }
            }
        }
        Ok(parsed)
    }
}

/// Resolve a possibly relative path against a base directory.
#[must_use]
pub fn resolve(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Read a canonical JSON artifact from disk without a sidecar.
///
/// # Errors
/// Returns [`GateError`] when the file is unreadable or is not canonical JSON.
pub fn read_canonical(path: &Path) -> Result<(String, Value), GateError> {
    let bytes = read_bytes(path)?;
    let value = parse_canonical_json(path, &bytes)?;
    Ok((hex_sha256(&bytes), value))
}

/// Read the pinned toolchain channel out of `rust-toolchain.toml`.
///
/// # Errors
/// Returns [`GateError::Schema`] when the file declares no channel or declares a
/// channel other than [`PINNED_TOOLCHAIN`].
pub fn pinned_channel(path: &Path) -> Result<String, GateError> {
    let text = String::from_utf8(read_bytes(path)?).map_err(|_| GateError::Schema {
        path: path.display().to_string(),
        reason: "rust-toolchain.toml is not valid UTF-8".to_owned(),
    })?;
    let channel = text
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("channel"))
        .and_then(|rest| rest.trim().strip_prefix('='))
        .map(|rest| rest.trim().trim_matches('"').to_owned())
        .ok_or_else(|| GateError::Schema {
            path: path.display().to_string(),
            reason: "no `channel = \"...\"` declaration found".to_owned(),
        })?;
    if channel != PINNED_TOOLCHAIN {
        return Err(GateError::Schema {
            path: path.display().to_string(),
            reason: format!(
                "channel is {channel:?}, but the release is pinned to {PINNED_TOOLCHAIN:?}"
            ),
        });
    }
    Ok(channel)
}

/// Read the digests published in an `ARCHIVES_DIR/SHA256SUMS.txt` manifest and
/// re-verify each named archive's bytes.
///
/// # Errors
/// Returns [`GateError`] when the manifest is absent or malformed, names an
/// absent archive, records a digest the bytes do not produce, or names nothing.
pub fn verified_archives(archives_dir: &Path) -> Result<BTreeMap<String, String>, GateError> {
    let manifest = archives_dir.join("SHA256SUMS.txt");
    let text = String::from_utf8(read_bytes(&manifest)?).map_err(|_| GateError::Schema {
        path: manifest.display().to_string(),
        reason: "checksum manifest is not valid UTF-8".to_owned(),
    })?;
    let mut digests = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let digest = parts.next().unwrap_or_default();
        let name = parts.next().unwrap_or_default().trim_start_matches('*');
        if !is_lower_hex_sha256(digest) || name.is_empty() || parts.next().is_some() {
            return Err(GateError::Schema {
                path: manifest.display().to_string(),
                reason: format!("line {} is not `<64 hex>  <file>`", index + 1),
            });
        }
        let archive = archives_dir.join(name);
        let actual = hex_sha256(&read_bytes(&archive)?);
        if actual != digest {
            return Err(GateError::Schema {
                path: manifest.display().to_string(),
                reason: format!(
                    "{} hashes to {actual}, not the published {digest}",
                    archive.display()
                ),
            });
        }
        if digests.insert(name.to_owned(), digest.to_owned()).is_some() {
            return Err(GateError::Schema {
                path: manifest.display().to_string(),
                reason: format!("archive {name:?} is listed twice"),
            });
        }
    }
    if digests.is_empty() {
        return Err(GateError::Schema {
            path: manifest.display().to_string(),
            reason: "the checksum manifest records no archive".to_owned(),
        });
    }
    Ok(digests)
}

/// Verify one already-written artifact/sidecar pair and its canonical schema.
///
/// # Errors
/// Returns the first [`GateError`] explaining why the pair is unusable.
pub fn verify_pair_against_schema(
    artifact: &Path,
    sidecar: &Path,
    schema: &Path,
) -> Result<(String, Value), GateError> {
    let (digest, value) = verify_artifact(artifact, sidecar)?;
    validate_against_schema(&artifact.display().to_string(), schema, &value)?;
    Ok((digest, value))
}

/// Read a strict sidecar's digest, exposed so the binaries do not need to import
/// [`crate::gates`] directly for this one call.
///
/// # Errors
/// Returns [`GateError`] for a missing or malformed sidecar.
pub fn sidecar_digest(path: &Path) -> Result<String, GateError> {
    read_sidecar_digest(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(arguments: &[&str]) -> Result<Flags, GateError> {
        Flags::from_iter(
            "usage",
            &["--alpha", "--beta"],
            arguments.iter().map(|a| (*a).to_owned()),
        )
    }

    #[test]
    fn unknown_repeated_and_valueless_flags_are_usage_errors() {
        assert!(matches!(flags(&["--gamma", "x"]), Err(GateError::Usage(_))));
        assert!(matches!(flags(&["--alpha"]), Err(GateError::Usage(_))));
        assert!(matches!(
            flags(&["--alpha", "x", "--alpha", "y"]),
            Err(GateError::Usage(_))
        ));
        assert!(matches!(flags(&["positional"]), Err(GateError::Usage(_))));
        let parsed = flags(&["--alpha", "one", "--beta", "two"]).expect("both flags parse");
        assert_eq!(parsed.value("--alpha").expect("alpha"), "one");
        assert!(matches!(
            parsed.value("--missing"),
            Err(GateError::Usage(_))
        ));
    }

    #[test]
    fn the_test_seam_is_refused_for_an_evidence_tree_output() {
        let outside = vec![PathBuf::from("target/x/report.json")];
        let inside = vec![PathBuf::from(
            ".omo/evidence/godot-mcp-server-architecture/final/f1.json",
        )];
        assert!(!targets_evidence_tree(&outside));
        assert!(targets_evidence_tree(&inside));
        // Reading a seam that is not set is always fine.
        assert_eq!(
            read_seam("GODOT_MCP_QUALIFICATION_SEAM_ABSENT", &inside).expect("unset"),
            None
        );
    }

    #[test]
    fn bound_hash_binds_only_the_two_sidecar_digests() {
        let identity = "a".repeat(64);
        let report = "b".repeat(64);
        let hash = bound_hash(&identity, &report);
        assert!(is_lower_hex_sha256(&hash));
        assert_ne!(hash, bound_hash(&report, &identity));
        assert_eq!(hash, bound_hash(&identity, &report));
    }

    #[test]
    fn derived_ids_are_independent_per_checker_and_stable() {
        let bound = "c".repeat(64);
        let f1_round = derive_id("f1-plan-compliance", &bound, "round");
        let f2_round = derive_id("f2-code-quality", &bound, "round");
        assert_ne!(f1_round, f2_round);
        assert_ne!(f1_round, derive_id("f1-plan-compliance", &bound, "launch"));
        assert_eq!(f1_round, derive_id("f1-plan-compliance", &bound, "round"));
        assert_eq!(f1_round.len(), 32);
    }

    #[test]
    fn findings_block_only_on_open_blocker_or_major() {
        let mut findings = Findings::new();
        assert!(!findings.blocking());
        findings.entries.push(Finding {
            severity: "Follow-up".to_owned(),
            status: "open".to_owned(),
            summary: "cosmetic".to_owned(),
        });
        assert!(!findings.blocking());
        findings.major("a real gap");
        assert!(findings.blocking());
        assert_eq!(findings.blocking_summaries(), vec!["a real gap".to_owned()]);
        // Duplicate summaries collapse so a repeated observation is reported once.
        findings.major("a real gap");
        assert_eq!(findings.into_vec().len(), 2);
    }

    #[test]
    fn cancelled_todos_are_exactly_twenty_through_twenty_five() {
        for todo in ALL_TODOS {
            assert_eq!(is_cancelled(todo), (20..=25).contains(&todo));
        }
    }

    #[test]
    fn a_fault_fires_only_at_its_own_named_point() {
        assert!(fault("stage1-before", Some("stage2-before")).is_ok());
        assert!(fault("stage1-before", None).is_ok());
        assert!(fault("stage1-before", Some("stage1-before")).is_err());
    }
}
