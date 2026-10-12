//! The twelve `tests/fixtures/repair/*.json` proof files.
//!
//! Each file is the operator-supplied proof for one of the twelve base-plus-
//! composite transactions, in the exact discriminated-union shape
//! `schemas/transactions/repair-proof.schema.json` defines, and every file is
//! validated against that schema before it is written. A proof file that would not
//! validate is a fixture bug, not something for the repair command to discover.
//!
//! # Nothing here is pre-baked
//!
//! * file hashes are computed by **reading back the bytes on disk**, not from the
//!   generator that wrote them;
//! * touched object identities come from the same descriptor-relative walk the
//!   journal record used;
//! * PIDs, start times and boundary membership come from the live OS observations
//!   setup made of the processes it started;
//! * the bridge receipt and live-state probe are the bytes the running fake bridge
//!   put on the wire.
//!
//! The repair command re-derives all of it independently anyway and never trusts
//! these files — that is the point of the failure fixtures — but a proof file that
//! disagreed with observable reality would test the wrong thing.

use std::path::Path;

use godot_mcp_domain::jsonschema::validate;
use serde_json::{Map, Value, json};

use super::records::{LiveFixture, hex_sha256};
use super::{FixtureError, ids, tree};

/// The canonical repair-proof schema, embedded from the repository so a shipped
/// fixture can never disagree with it.
pub const REPAIR_PROOF_SCHEMA_JSON: &str =
    include_str!("../../../../schemas/transactions/repair-proof.schema.json");

/// Which state a proof claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    Pre,
    Post,
}

impl Disposition {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pre => "pre-state",
            Self::Post => "post-state",
        }
    }
}

/// Write all twelve proof files.
///
/// # Errors
/// Returns [`FixtureError::Proof`] when any file would not satisfy the canonical
/// schema or a required live observation is missing, and [`FixtureError::Io`] when
/// a file cannot be written.
pub fn write_all(live: &LiveFixture) -> Result<(), FixtureError> {
    let schema: Value =
        serde_json::from_str(REPAIR_PROOF_SCHEMA_JSON).map_err(|error| FixtureError::Proof {
            detail: format!("the canonical repair-proof schema is unusable: {error}"),
        })?;
    for (name, transaction) in ids::PROOF_FILES {
        let value = build(live, transaction)?;
        validate(&schema, &value).map_err(|error| FixtureError::Proof {
            detail: format!("{name} at {}: {}", error.pointer, error.reason),
        })?;
        let bytes = serde_json::to_vec_pretty(&value).map_err(|error| FixtureError::Proof {
            detail: format!("{name} could not be serialized: {error}"),
        })?;
        tree::write_file(&live.paths.repair_dir.join(name), &bytes)?;
    }
    Ok(())
}

fn build(live: &LiveFixture, transaction: &str) -> Result<Value, FixtureError> {
    match transaction {
        ids::FILE_PRE => file_proof(live, transaction, Disposition::Pre),
        ids::FILE_POST => file_proof(live, transaction, Disposition::Post),
        ids::PROCESS_PRE => process_proof(live, transaction, Disposition::Pre),
        ids::PROCESS_POST => process_proof(live, transaction, Disposition::Post),
        ids::EDITOR_PRE => editor_proof(live, transaction, Disposition::Pre),
        ids::EDITOR_POST => editor_proof(live, transaction, Disposition::Post),
        // Every composite is claimed in its post-state, which its live file bytes
        // genuinely satisfy. The refusal has to come from the linked process.
        other => file_proof(live, other, Disposition::Post),
    }
}

fn common(live: &LiveFixture, transaction: &str, kind: &str) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("effect_kind".to_owned(), json!(kind));
    fields.insert("transaction_id".to_owned(), json!(transaction));
    fields.insert(
        "project_object_identity".to_owned(),
        json!(live.held.object_identity),
    );
    fields.insert("project_generation".to_owned(), json!(live.held.generation));
    fields
}

fn file_proof(
    live: &LiveFixture,
    transaction: &str,
    disposition: Disposition,
) -> Result<Value, FixtureError> {
    let path = ids::touched_path(transaction).ok_or_else(|| FixtureError::Proof {
        detail: format!("{transaction} has no fixture path"),
    })?;
    let full = live.paths.project_root.join(path);
    let bytes = read_back(&full)?;
    let identity = live
        .object_identities
        .get(path)
        .cloned()
        .flatten()
        .ok_or_else(|| FixtureError::Proof {
            detail: format!("{path} has no observed object identity"),
        })?;

    let mut fields = common(live, transaction, "file");
    fields.insert(
        "observed_touched_object_identities".to_owned(),
        json!([identity]),
    );
    fields.insert("observed_existence".to_owned(), json!({ path: "present" }));
    fields.insert(
        "observed_hashes".to_owned(),
        json!({ path: hex_sha256(&bytes) }),
    );
    fields.insert(
        "claimed_disposition".to_owned(),
        json!(disposition.as_str()),
    );
    Ok(Value::Object(fields))
}

fn process_proof(
    live: &LiveFixture,
    transaction: &str,
    disposition: Disposition,
) -> Result<Value, FixtureError> {
    let facts = live
        .processes
        .get(transaction)
        .ok_or_else(|| FixtureError::Proof {
            detail: format!("{transaction} has no live process observation"),
        })?;
    let mut fields = common(live, transaction, "process_launch");
    fields.insert("observed_pid".to_owned(), json!(facts.pid));
    fields.insert("observed_start_time".to_owned(), json!(facts.start_time));
    fields.insert(
        "observed_boundary_membership".to_owned(),
        json!(if facts.membership_confirmed {
            "confirmed"
        } else {
            "unconfirmed"
        }),
    );
    fields.insert(
        "observed_release_status".to_owned(),
        json!(release_status(facts.release)),
    );
    fields.insert("observed_exit_code".to_owned(), json!(facts.exit_code));
    fields.insert(
        "observed_boundary_empty".to_owned(),
        json!(facts.boundary_empty),
    );
    fields.insert(
        "claimed_disposition".to_owned(),
        json!(disposition.as_str()),
    );
    Ok(Value::Object(fields))
}

fn editor_proof(
    live: &LiveFixture,
    transaction: &str,
    disposition: Disposition,
) -> Result<Value, FixtureError> {
    let receipt = live
        .receipts
        .get(transaction)
        .ok_or_else(|| FixtureError::Proof {
            detail: format!("{transaction} has no observed bridge receipt"),
        })?;
    let mut fields = common(live, transaction, "editor_runtime");
    fields.insert("bridge_reachable".to_owned(), json!(receipt.reachable));
    fields.insert("bridge_receipt".to_owned(), json!(receipt.action_id));
    fields.insert(
        "live_state_probe".to_owned(),
        json!(receipt.live_state_probe),
    );
    fields.insert(
        "claimed_disposition".to_owned(),
        json!(disposition.as_str()),
    );
    Ok(Value::Object(fields))
}

fn release_status(release: godot_mcp_server::test_support::Task34Release) -> &'static str {
    use godot_mcp_server::test_support::Task34Release as Release;
    match release {
        Release::Suspended => "suspended",
        Release::ResumedContained => "resumed-contained",
        Release::ExitedClean => "exited-clean",
        Release::Signaled => "signaled",
        Release::NonzeroExit => "nonzero-exit",
        Release::Unknown => "unknown",
    }
}

fn read_back(path: &Path) -> Result<Vec<u8>, FixtureError> {
    std::fs::read(path).map_err(|error| FixtureError::Io {
        path: path.display().to_string(),
        detail: error.to_string(),
    })
}
