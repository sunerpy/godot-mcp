//! The operator-supplied proof file: parsed, schema-validated, never trusted.
//!
//! Everything here is *input validation only*. A parsed value in this module is
//! a **claim**, never evidence: the repair command re-derives every field it
//! compares against, and a claim that disagrees with the re-derivation is a
//! mismatch. The only reason to parse strictly is so a malformed or partially
//! filled proof file is refused with a diagnosable reason instead of silently
//! comparing against a default.
//!
//! Two gates apply, in order:
//!
//! 1. the canonical `schemas/transactions/repair-proof.schema.json` discriminated
//!    union, evaluated by Todo 2's validator, so an unknown field or a wrong
//!    variant shape is refused before anything is read; and
//! 2. this module's own stricter typed parse, which additionally requires every
//!    field the normative interface calls required to be **present and
//!    non-null** — including `claimed_disposition`, which the schema leaves
//!    optional on two variants but the command never accepts absent, and
//!    `bridge_receipt`/`live_state_probe`, whose null values always fail proof
//!    for the `editor_runtime` kind by design.

use std::collections::BTreeMap;
use std::path::Path;

use godot_mcp_domain::jsonschema::validate;
use godot_mcp_server::operator::{
    ObservedRelease, ProofKind, ProvenDisposition, RecordedExistence,
};
use serde_json::Value;

/// The canonical repair-proof schema, embedded so a shipped binary can never
/// disagree with the repository's own contract.
pub const REPAIR_PROOF_SCHEMA_JSON: &str =
    include_str!("../../../../schemas/transactions/repair-proof.schema.json");

/// Why a proof file could not be used.
#[derive(Debug, thiserror::Error)]
pub enum SuppliedError {
    /// The file could not be read.
    #[error("the proof file {path} could not be read: {detail}")]
    Unreadable {
        /// The path that failed.
        path: String,
        /// What the OS reported.
        detail: String,
    },
    /// The file is not JSON.
    #[error("the proof file {path} is not JSON: {detail}")]
    Malformed {
        /// The path that failed.
        path: String,
        /// What the parser reported.
        detail: String,
    },
    /// The file violates the canonical schema.
    #[error("the proof file {path} violates the canonical schema at {pointer}: {reason}")]
    Schema {
        /// The path that failed.
        path: String,
        /// The schema location that refused it.
        pointer: String,
        /// Why it was refused.
        reason: String,
    },
    /// A field the command requires is missing, null or unusable.
    #[error("the proof file {path} is unusable: {detail}")]
    Field {
        /// The path that failed.
        path: String,
        /// Exactly which field, and why.
        detail: String,
    },
}

/// The fields every variant carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppliedHeader {
    /// Which typed proof form the file claims.
    pub effect_kind: ProofKind,
    /// The transaction the file claims to prove.
    pub transaction_id: String,
    /// The project object identity the file claims.
    pub project_object_identity: String,
    /// The project generation the file claims.
    pub project_generation: u64,
    /// Which recorded state the file claims.
    pub claimed_disposition: ProvenDisposition,
}

/// A `file`-kind claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppliedFile {
    /// The claimed touched object identities.
    pub observed_touched_object_identities: Vec<String>,
    /// The claimed existence of every touched path.
    pub observed_existence: BTreeMap<String, RecordedExistence>,
    /// The claimed content hash of every present path.
    pub observed_hashes: BTreeMap<String, String>,
}

/// A `process_launch`-kind claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppliedProcess {
    /// The claimed PID.
    pub observed_pid: u32,
    /// The claimed platform-native start time.
    pub observed_start_time: String,
    /// Whether boundary membership is claimed confirmed.
    pub observed_boundary_membership_confirmed: bool,
    /// The claimed release or terminal disposition.
    pub observed_release_status: ObservedRelease,
    /// The claimed exit code, when the claim names one.
    pub observed_exit_code: Option<i64>,
    /// The claimed post-exit boundary emptiness, when the claim names one.
    pub observed_boundary_empty: Option<bool>,
}

/// An `editor_runtime`-kind claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppliedEditor {
    /// Whether the bridge is claimed reachable.
    pub bridge_reachable: bool,
    /// The claimed plugin-acknowledged action identifier.
    pub bridge_receipt: String,
    /// The claimed live state-probe answer.
    pub live_state_probe: String,
}

/// One parsed claim, in exactly the three canonical variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuppliedBody {
    /// A `file` claim.
    File(SuppliedFile),
    /// A `process_launch` claim.
    ProcessLaunch(SuppliedProcess),
    /// An `editor_runtime` claim.
    EditorRuntime(SuppliedEditor),
}

/// The whole parsed claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppliedProof {
    /// The fields every variant carries.
    pub header: SuppliedHeader,
    /// The variant-specific fields.
    pub body: SuppliedBody,
}

/// Read, schema-validate and strictly parse one proof file.
///
/// # Errors
/// Returns the first [`SuppliedError`] any gate reports. Nothing is journaled or
/// mutated on the way, so a refusal here leaves the transaction untouched.
pub fn load(path: &Path) -> Result<SuppliedProof, SuppliedError> {
    let display = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|error| SuppliedError::Unreadable {
        path: display.clone(),
        detail: error.to_string(),
    })?;
    let value: Value = serde_json::from_str(&text).map_err(|error| SuppliedError::Malformed {
        path: display.clone(),
        detail: error.to_string(),
    })?;
    let schema: Value =
        serde_json::from_str(REPAIR_PROOF_SCHEMA_JSON).map_err(|error| SuppliedError::Schema {
            path: display.clone(),
            pointer: "#".to_owned(),
            reason: format!("the embedded canonical schema is unusable: {error}"),
        })?;
    validate(&schema, &value).map_err(|error| SuppliedError::Schema {
        path: display.clone(),
        pointer: error.pointer.clone(),
        reason: error.reason.clone(),
    })?;
    parse(&display, &value)
}

fn parse(path: &str, value: &Value) -> Result<SuppliedProof, SuppliedError> {
    let field = |detail: String| SuppliedError::Field {
        path: path.to_owned(),
        detail,
    };
    let effect_kind = match required_str(path, value, "effect_kind")? {
        "file" => ProofKind::File,
        "process_launch" => ProofKind::ProcessLaunch,
        "editor_runtime" => ProofKind::EditorRuntime,
        other => {
            return Err(field(format!("effect_kind {other:?} is not a proof kind")));
        }
    };
    let claimed_disposition = match value.get("claimed_disposition").and_then(Value::as_str) {
        Some("pre-state") => ProvenDisposition::PreState,
        Some("post-state") => ProvenDisposition::PostState,
        Some(other) => {
            return Err(field(format!(
                "claimed_disposition {other:?} is neither pre-state nor post-state"
            )));
        }
        None => {
            return Err(field(
                "claimed_disposition is missing or null; the command never infers it".to_owned(),
            ));
        }
    };
    let header = SuppliedHeader {
        effect_kind,
        transaction_id: required_str(path, value, "transaction_id")?.to_owned(),
        project_object_identity: required_str(path, value, "project_object_identity")?.to_owned(),
        project_generation: value
            .get("project_generation")
            .and_then(Value::as_u64)
            .ok_or_else(|| field("project_generation is missing or null".to_owned()))?,
        claimed_disposition,
    };
    let body = match effect_kind {
        ProofKind::File => SuppliedBody::File(parse_file(path, value)?),
        ProofKind::ProcessLaunch => SuppliedBody::ProcessLaunch(parse_process(path, value)?),
        ProofKind::EditorRuntime => SuppliedBody::EditorRuntime(parse_editor(path, value)?),
    };
    Ok(SuppliedProof { header, body })
}

fn parse_file(path: &str, value: &Value) -> Result<SuppliedFile, SuppliedError> {
    let field = |detail: String| SuppliedError::Field {
        path: path.to_owned(),
        detail,
    };
    let identities = value
        .get("observed_touched_object_identities")
        .and_then(Value::as_array)
        .ok_or_else(|| field("observed_touched_object_identities is missing".to_owned()))?;
    let mut observed_touched_object_identities = Vec::with_capacity(identities.len());
    for entry in identities {
        let identity = entry.as_str().ok_or_else(|| {
            field("observed_touched_object_identities holds a non-string entry".to_owned())
        })?;
        observed_touched_object_identities.push(identity.to_owned());
    }
    if observed_touched_object_identities.is_empty() {
        return Err(field(
            "observed_touched_object_identities is empty, so it can prove nothing".to_owned(),
        ));
    }

    let existence = value
        .get("observed_existence")
        .and_then(Value::as_object)
        .ok_or_else(|| field("observed_existence is missing".to_owned()))?;
    let mut observed_existence = BTreeMap::new();
    for (touched, state) in existence {
        let state = match state.as_str() {
            Some("present") => RecordedExistence::Present,
            Some("absent") => RecordedExistence::Absent,
            _ => {
                return Err(field(format!(
                    "observed_existence[{touched:?}] is neither present nor absent"
                )));
            }
        };
        observed_existence.insert(touched.clone(), state);
    }
    if observed_existence.is_empty() {
        return Err(field(
            "observed_existence is empty, so it can prove nothing".to_owned(),
        ));
    }

    let mut observed_hashes = BTreeMap::new();
    if let Some(hashes) = value.get("observed_hashes") {
        let hashes = hashes
            .as_object()
            .ok_or_else(|| field("observed_hashes is not an object".to_owned()))?;
        for (touched, hash) in hashes {
            let hash = hash
                .as_str()
                .ok_or_else(|| field(format!("observed_hashes[{touched:?}] is not a string")))?;
            observed_hashes.insert(touched.clone(), hash.to_owned());
        }
    }
    for (touched, state) in &observed_existence {
        if *state == RecordedExistence::Present && !observed_hashes.contains_key(touched) {
            return Err(field(format!(
                "{touched} is claimed present but carries no observed hash"
            )));
        }
        if *state == RecordedExistence::Absent && observed_hashes.contains_key(touched) {
            return Err(field(format!(
                "{touched} is claimed absent yet carries an observed hash"
            )));
        }
    }
    Ok(SuppliedFile {
        observed_touched_object_identities,
        observed_existence,
        observed_hashes,
    })
}

fn parse_process(path: &str, value: &Value) -> Result<SuppliedProcess, SuppliedError> {
    let field = |detail: String| SuppliedError::Field {
        path: path.to_owned(),
        detail,
    };
    let observed_pid = value
        .get("observed_pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .ok_or_else(|| field("observed_pid is missing, null or not a PID".to_owned()))?;
    let observed_start_time = value
        .get("observed_start_time")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            field(
                "observed_start_time is missing or null; an identity without a start time can \
                 never be matched again after PID reuse"
                    .to_owned(),
            )
        })?
        .to_owned();
    let observed_boundary_membership_confirmed = match value
        .get("observed_boundary_membership")
        .and_then(Value::as_str)
    {
        Some("confirmed") => true,
        Some("unconfirmed") => false,
        Some(other) => {
            return Err(field(format!(
                "observed_boundary_membership {other:?} is neither confirmed nor unconfirmed"
            )));
        }
        None => {
            return Err(field(
                "observed_boundary_membership is missing or null".to_owned(),
            ));
        }
    };
    let observed_release_status = value
        .get("observed_release_status")
        .and_then(Value::as_str)
        .and_then(ObservedRelease::parse)
        .ok_or_else(|| field("observed_release_status is missing or not canonical".to_owned()))?;
    Ok(SuppliedProcess {
        observed_pid,
        observed_start_time,
        observed_boundary_membership_confirmed,
        observed_release_status,
        observed_exit_code: value.get("observed_exit_code").and_then(Value::as_i64),
        observed_boundary_empty: value
            .get("observed_boundary_empty")
            .and_then(Value::as_bool),
    })
}

fn parse_editor(path: &str, value: &Value) -> Result<SuppliedEditor, SuppliedError> {
    let field = |detail: String| SuppliedError::Field {
        path: path.to_owned(),
        detail,
    };
    let bridge_reachable = value
        .get("bridge_reachable")
        .and_then(Value::as_bool)
        .ok_or_else(|| field("bridge_reachable is missing or null".to_owned()))?;
    // A null receipt or a null probe always fails proof for this effect kind, by
    // design: a file hash can never substitute for editor-side state.
    let bridge_receipt = value
        .get("bridge_receipt")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            field(
                "bridge_receipt is missing or null, which always fails editor_runtime proof"
                    .to_owned(),
            )
        })?
        .to_owned();
    let live_state_probe = value
        .get("live_state_probe")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            field(
                "live_state_probe is missing or null, which always fails editor_runtime proof"
                    .to_owned(),
            )
        })?
        .to_owned();
    Ok(SuppliedEditor {
        bridge_reachable,
        bridge_receipt,
        live_state_probe,
    })
}

fn required_str<'a>(path: &str, value: &'a Value, name: &str) -> Result<&'a str, SuppliedError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| SuppliedError::Field {
            path: path.to_owned(),
            detail: format!("{name} is missing or null"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(value: &Value) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("proof.json");
        std::fs::write(&path, serde_json::to_vec_pretty(value).expect("json")).expect("write");
        (dir, path)
    }

    fn file_claim() -> Value {
        serde_json::json!({
            "effect_kind": "file",
            "transaction_id": "34000000-0000-4000-8000-000000000101",
            "project_object_identity": format!("project_{}", "a".repeat(64)),
            "project_generation": 0,
            "observed_touched_object_identities": ["unix:dev=1,ino=2"],
            "observed_existence": { "scenes/base_pre.tscn": "present" },
            "observed_hashes": { "scenes/base_pre.tscn": "0".repeat(64) },
            "claimed_disposition": "pre-state",
        })
    }

    /// The embedded schema is the repository's own, so a claim the fixture writes
    /// must parse and a claim the schema forbids must not.
    #[test]
    fn a_canonical_file_claim_parses() {
        let (_dir, path) = write(&file_claim());
        let proof = load(&path).expect("a canonical file claim parses");
        assert_eq!(proof.header.effect_kind, ProofKind::File);
        assert_eq!(
            proof.header.claimed_disposition,
            ProvenDisposition::PreState
        );
        let SuppliedBody::File(body) = proof.body else {
            panic!("a file claim parses as a file body");
        };
        assert_eq!(body.observed_existence.len(), 1);
        assert_eq!(body.observed_hashes.len(), 1);
    }

    /// `claimed_disposition` is required by the command for every kind. The
    /// canonical schema already requires it on the `file` variant, and leaves it
    /// optional on the other two; both routes must refuse, so its absence can
    /// never be defaulted.
    #[test]
    fn a_missing_claimed_disposition_is_refused_for_every_kind() {
        for mut claim in [
            file_claim(),
            serde_json::json!({
                "effect_kind": "process_launch",
                "transaction_id": "34000000-0000-4000-8000-000000000201",
                "project_object_identity": format!("project_{}", "a".repeat(64)),
                "project_generation": 0,
                "observed_pid": 4242,
                "observed_start_time": "12345",
                "observed_boundary_membership": "confirmed",
                "observed_release_status": "suspended",
                "claimed_disposition": "pre-state",
            }),
            serde_json::json!({
                "effect_kind": "editor_runtime",
                "transaction_id": "34000000-0000-4000-8000-000000000301",
                "project_object_identity": format!("project_{}", "a".repeat(64)),
                "project_generation": 0,
                "bridge_reachable": true,
                "bridge_receipt": "action-1",
                "live_state_probe": "node:/root",
                "claimed_disposition": "pre-state",
            }),
        ] {
            claim
                .as_object_mut()
                .expect("object")
                .remove("claimed_disposition");
            let (_dir, path) = write(&claim);
            let error = load(&path).expect_err("a missing claimed_disposition is refused");
            match &error {
                SuppliedError::Field { detail, .. } => assert!(
                    detail.contains("claimed_disposition"),
                    "{error} must name the missing field"
                ),
                SuppliedError::Schema { .. } => {}
                other => panic!("{other} must be a schema or field refusal"),
            }
        }
    }

    /// A null receipt or probe can never be substituted by a file hash, so both
    /// nulls must be refused before any comparison happens.
    #[test]
    fn a_null_receipt_or_probe_always_fails_editor_runtime_proof() {
        for absent in ["bridge_receipt", "live_state_probe"] {
            let mut claim = serde_json::json!({
                "effect_kind": "editor_runtime",
                "transaction_id": "34000000-0000-4000-8000-000000000301",
                "project_object_identity": format!("project_{}", "a".repeat(64)),
                "project_generation": 0,
                "bridge_reachable": true,
                "bridge_receipt": "action-1",
                "live_state_probe": "node:/root",
                "claimed_disposition": "pre-state",
            });
            claim
                .as_object_mut()
                .expect("object")
                .insert(absent.to_owned(), Value::Null);
            let (_dir, path) = write(&claim);
            let error = load(&path).expect_err("a null field is refused");
            assert!(error.to_string().contains(absent), "{error} names {absent}");
        }
    }

    /// A present path with no hash proves nothing, and an absent path with a hash
    /// is self-contradictory. Both are partial proofs, which are mismatches.
    #[test]
    fn a_partial_file_claim_is_refused() {
        let mut claim = file_claim();
        claim
            .as_object_mut()
            .expect("object")
            .remove("observed_hashes");
        let (_dir, path) = write(&claim);
        assert!(load(&path).is_err(), "a present path needs a hash");

        let mut claim = file_claim();
        claim.as_object_mut().expect("object").insert(
            "observed_existence".to_owned(),
            serde_json::json!({ "scenes/base_pre.tscn": "absent" }),
        );
        let (_dir, path) = write(&claim);
        assert!(load(&path).is_err(), "an absent path carries no hash");
    }

    /// An unknown field means the operator is proving something this command does
    /// not evaluate, so the canonical schema's `additionalProperties: false`
    /// refusal must reach the caller.
    #[test]
    fn an_unknown_field_is_refused_by_the_canonical_schema() {
        let mut claim = file_claim();
        claim
            .as_object_mut()
            .expect("object")
            .insert("force".to_owned(), serde_json::json!(true));
        let (_dir, path) = write(&claim);
        let error = load(&path).expect_err("an unknown field is refused");
        assert!(
            matches!(error, SuppliedError::Schema { .. }),
            "{error} must be a schema refusal"
        );
    }

    /// A start time is what distinguishes one process from a later reuse of its
    /// PID, so a claim without one can never be compared.
    #[test]
    fn a_process_claim_without_a_start_time_is_refused() {
        let mut claim = serde_json::json!({
            "effect_kind": "process_launch",
            "transaction_id": "34000000-0000-4000-8000-000000000201",
            "project_object_identity": format!("project_{}", "a".repeat(64)),
            "project_generation": 0,
            "observed_pid": 4242,
            "observed_start_time": "12345",
            "observed_boundary_membership": "confirmed",
            "observed_release_status": "suspended",
            "claimed_disposition": "pre-state",
        });
        claim
            .as_object_mut()
            .expect("object")
            .insert("observed_start_time".to_owned(), Value::Null);
        let (_dir, path) = write(&claim);
        assert!(load(&path).is_err(), "a null start time is refused");
    }
}
