//! Task-34's fixture-control implementation.
//!
//! `qa-fixture-34` is the **sole** owner of `tests/fixtures/quarantined` and
//! `tests/fixtures/repair/*.json`. No other Todo, test or descriptor may write or
//! reset those paths; everything they contain is (re)created from scratch here.
//!
//! # What `setup` produces
//!
//! A fresh project object with a new project-generation record, and nineteen
//! quarantined transaction records in Todo 6's one durable journal:
//!
//! * six base records — a `file`, `process_launch` and `editor_runtime` pair each,
//!   pre-state and post-state, under the fixed IDs in [`ids`];
//! * six Todo 17 composite `process_launch`+`file` records, each carrying
//!   `required_subproofs.process_launch_transaction_id`;
//! * six linked `process_launch` records, one created for each composite;
//! * one unrelated `file` record that the "wrong linked transaction" composite
//!   deliberately points at.
//!
//! Every composite's **file bytes already equal its intended post-hash**, so its
//! file proof matches exactly while its linked process proof independently fails
//! for the reason its ID encodes. That is the whole point of the composite set: a
//! matching file hash is never, on its own, sufficient.
//!
//! # Everything observed is observed
//!
//! No hash, PID, start time, boundary membership or bridge receipt in the twelve
//! proof files is a pre-baked constant. Hashes are computed over the bytes setup
//! just wrote, process identities come from live OS inspection of the processes
//! setup just started, and the bridge receipt and live-state probe are whatever
//! the running fake bridge answered over the wire.
//!
//! # `setup` leaves no partial state
//!
//! Any failure rolls back: every process already started is terminated by its
//! recorded identity, and the fixture tree is removed, before a nonzero exit.
//!
//! # `cleanup` is identity-matched, bounded and idempotent
//!
//! It terminates exactly the processes recorded in the fixture's own manifest,
//! matched on PID **plus** start time (plus the listening socket's inode for the
//! bridge), never by program name or command-line pattern. It exits 0 against an
//! already-clean or partially-torn-down tree, and nonzero — without hanging — if
//! it cannot prove a still-live survivor or bridge listener was terminated.

pub mod bridge;
pub mod ids;
pub mod proofs;
pub mod records;
pub mod state;
pub mod survivor;
pub mod tree;

use std::path::Path;

use tree::FixturePaths;

/// The only scenario this binary implements.
pub const SCENARIO: &str = "task34";

/// Why a fixture lifecycle step failed.
#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    /// The workspace layout could not be resolved.
    #[error("the Task-34 fixture layout could not be resolved: {0}")]
    Layout(String),

    /// A filesystem operation failed.
    #[error("the Task-34 fixture could not write {path}: {detail}")]
    Io {
        /// The path that failed.
        path: String,
        /// What the OS reported.
        detail: String,
    },

    /// The fixture manifest is unusable.
    #[error("the Task-34 fixture manifest is unusable: {detail}")]
    State {
        /// Exactly what was wrong.
        detail: String,
    },

    /// This platform cannot host the fixture.
    #[error("the Task-34 fixture cannot run here: {detail}")]
    Unsupported {
        /// Why not.
        detail: String,
    },

    /// A fixture process could not be started, observed or reaped.
    #[error("a Task-34 fixture process failed: {detail}")]
    Process {
        /// What went wrong.
        detail: String,
    },

    /// A process that had to be terminated is still live.
    #[error("the Task-34 fixture could not prove termination: {detail}")]
    NotTerminated {
        /// Which process, and how long it was waited for.
        detail: String,
    },

    /// The fake editor bridge failed.
    #[error("the Task-34 fixture bridge failed: {detail}")]
    Bridge {
        /// What went wrong.
        detail: String,
    },

    /// A durable journal record could not be written.
    #[error("a Task-34 fixture journal record failed: {detail}")]
    Journal {
        /// What the journal reported.
        detail: String,
    },

    /// A proof file does not satisfy the canonical repair-proof schema.
    #[error("a Task-34 proof file violates the canonical schema: {detail}")]
    Proof {
        /// Which file, and which schema location.
        detail: String,
    },
}

/// (Re)create the whole fixture from scratch.
///
/// An existing fixture tree is torn down first, so running `setup` twice without
/// an intervening `cleanup` deterministically yields the same state rather than
/// colliding with the previous run's journal records or leaking its processes.
///
/// # Errors
/// Returns the first [`FixtureError`] any step reports. On error every process
/// this call started has been terminated and the fixture tree removed, so no
/// partial state survives.
pub fn setup(start: &Path) -> Result<FixturePaths, FixtureError> {
    survivor::require_observability()?;
    let paths = FixturePaths::resolve(start)?;
    cleanup(start)?;
    match records::build(&paths) {
        Ok(()) => Ok(paths),
        Err(error) => {
            roll_back(&paths);
            Err(error)
        }
    }
}

/// Terminate everything this scenario started and remove the fixture tree.
///
/// # Errors
/// Returns [`FixtureError::NotTerminated`] when a still-live survivor or bridge
/// listener cannot be proven terminated, and [`FixtureError::Io`] when a present
/// path cannot be removed. An already-clean or partially-torn-down tree is not an
/// error.
pub fn cleanup(start: &Path) -> Result<(), FixtureError> {
    let paths = FixturePaths::resolve(start)?;
    if let Some(state) = state::FixtureState::load(&paths.state_file(), SCENARIO)? {
        if let Some(bridge) = &state.bridge {
            bridge::terminate(bridge)?;
        }
        for record in &state.survivors {
            survivor::terminate(record)?;
        }
    }
    tree::remove_tree(&paths)
}

/// Terminate whatever the failed `setup` had already started, then remove the
/// tree. Failures here are deliberately swallowed: the caller is already
/// returning an error, and the rollback must not mask it.
fn roll_back(paths: &FixturePaths) {
    if let Ok(Some(state)) = state::FixtureState::load(&paths.state_file(), SCENARIO) {
        if let Some(bridge) = &state.bridge {
            let _ = bridge::terminate(bridge);
        }
        for record in &state.survivors {
            let _ = survivor::terminate(record);
        }
    }
    let _ = tree::remove_tree(paths);
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::state::{FixtureState, SurvivorRecord};
    use super::tree::{scene_bytes, superseded_scene_bytes};
    use super::{SCENARIO, bridge, ids};

    fn descriptor() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../qa/tasks/34.json")
            .canonicalize()
            .expect("qa/tasks/34.json resolves");
        std::fs::read_to_string(&path).expect("qa/tasks/34.json is readable")
    }

    /// The descriptor is the contract. Every fixed ID and proof-file name this
    /// module owns must appear in it verbatim, or the fixture builds artefacts the
    /// Task-34 commands never reference.
    #[test]
    fn every_owned_identifier_appears_in_the_descriptor() {
        let text = descriptor();
        for (name, transaction) in ids::PROOF_FILES {
            assert!(
                text.contains(name),
                "qa/tasks/34.json must name the proof file {name}"
            );
            assert!(
                text.contains(transaction),
                "qa/tasks/34.json must name the transaction {transaction}"
            );
        }
    }

    /// The linked and decoy transactions are the fixture's own bookkeeping, so they
    /// must *not* leak into the descriptor's command surface.
    #[test]
    fn the_linked_and_decoy_transactions_are_not_descriptor_commands() {
        let text = descriptor();
        for transaction in ids::LINKED.into_iter().chain([ids::UNRELATED_DECOY]) {
            assert!(
                !text.contains(transaction),
                "{transaction} is fixture bookkeeping and must not appear in qa/tasks/34.json"
            );
        }
    }

    /// Nineteen records are written; nineteen distinct IDs must exist to write them
    /// under, and each must satisfy the canonical record schema's lowercase pattern.
    #[test]
    fn every_fixed_identifier_is_distinct_and_canonically_spelled() {
        let all: Vec<&str> = ids::BASE
            .into_iter()
            .chain(ids::COMPOSITE)
            .chain(ids::LINKED)
            .chain([ids::UNRELATED_DECOY])
            .collect();
        assert_eq!(all.len(), 19);
        assert_eq!(
            all.iter().collect::<BTreeSet<_>>().len(),
            19,
            "every fixture transaction ID is distinct"
        );
        for transaction in all {
            let groups: Vec<&str> = transaction.split('-').collect();
            assert_eq!(groups.len(), 5, "{transaction} is not a UUID");
            assert_eq!(
                groups.iter().map(|group| group.len()).collect::<Vec<_>>(),
                vec![8, 4, 4, 4, 12],
                "{transaction} has the wrong group widths"
            );
            assert!(
                transaction
                    .chars()
                    .all(|character| character == '-' || character.is_ascii_hexdigit()),
                "{transaction} is not hexadecimal"
            );
            assert_eq!(
                transaction.to_ascii_lowercase(),
                transaction,
                "the canonical record schema only accepts lowercase hex"
            );
        }
    }

    /// Two transactions sharing a path would let one's intended post-state satisfy
    /// the other's proof, which is exactly the confusion the composite set exists
    /// to rule out.
    #[test]
    fn every_file_transaction_owns_a_distinct_path() {
        let mut paths = BTreeSet::new();
        for transaction in [ids::FILE_PRE, ids::FILE_POST]
            .into_iter()
            .chain(ids::COMPOSITE)
            .chain([ids::UNRELATED_DECOY])
        {
            let path = ids::touched_path(transaction)
                .unwrap_or_else(|| panic!("{transaction} has no fixture path"));
            assert!(paths.insert(path), "{path} is claimed twice");
        }
        assert_eq!(paths.len(), 9);
        for transaction in [ids::PROCESS_PRE, ids::EDITOR_POST] {
            assert!(
                ids::touched_path(transaction).is_none(),
                "{transaction} touches no file"
            );
        }
    }

    /// The fixture must rebuild byte-identically, and a post-state fixture's live
    /// bytes must never also satisfy its recorded pre-state.
    #[test]
    fn scene_bytes_are_deterministic_and_never_their_own_pre_state() {
        for transaction in ids::COMPOSITE {
            assert_eq!(scene_bytes(transaction), scene_bytes(transaction));
            assert_ne!(
                scene_bytes(transaction),
                superseded_scene_bytes(transaction),
                "{transaction}'s post-state bytes must differ from its recorded pre-state"
            );
        }
        assert_ne!(
            scene_bytes(ids::COMPOSITE[0]),
            scene_bytes(ids::COMPOSITE[1]),
            "two fixtures must not share content"
        );
    }

    /// The bridge decides which receipt to present from the handshake nonce, so the
    /// encoding has to round-trip and a foreign nonce must not name a transaction.
    #[test]
    fn a_bridge_nonce_names_exactly_one_transaction() {
        for transaction in [ids::EDITOR_PRE, ids::EDITOR_POST] {
            assert_eq!(
                bridge::transaction_of_nonce(&bridge::repair_nonce(transaction)),
                Some(transaction)
            );
            assert_eq!(
                bridge::transaction_of_nonce(&bridge::probe_nonce(transaction)),
                Some(transaction)
            );
            assert_ne!(
                bridge::repair_nonce(transaction),
                bridge::probe_nonce(transaction),
                "setup's own observation must not consume the nonce a repair needs"
            );
        }
        assert_eq!(bridge::transaction_of_nonce("unrelated-nonce"), None);
    }

    /// Cleanup reads this manifest to decide what to terminate. A manifest from
    /// another scenario must never be consumed as this one's.
    #[test]
    fn the_manifest_round_trips_and_refuses_a_foreign_scenario() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("state.json");
        let mut state = FixtureState::new(SCENARIO, dir.path());
        state.survivors.push(SurvivorRecord {
            label: "suspended-survivor".to_owned(),
            transaction_id: ids::PROCESS_PRE.to_owned(),
            pid: 4242,
            start_time: Some("12345".to_owned()),
            expect_live: true,
        });
        state.save(&path).expect("manifest is written");
        assert_eq!(
            FixtureState::load(&path, SCENARIO).expect("manifest loads"),
            Some(state)
        );
        assert!(
            FixtureState::load(&path, "other-scenario").is_err(),
            "a foreign scenario's manifest is refused"
        );
        assert_eq!(
            FixtureState::load(&dir.path().join("absent.json"), SCENARIO)
                .expect("an absent manifest is not an error"),
            None,
            "an already-clean fixture tree has no manifest"
        );
    }

    /// A record with no recorded start time can never be matched again after PID
    /// reuse, so it is reported as not live rather than signalled.
    #[test]
    fn a_record_without_a_start_time_is_never_terminated() {
        let record = SurvivorRecord {
            label: "unidentifiable".to_owned(),
            transaction_id: ids::PROCESS_PRE.to_owned(),
            pid: std::process::id(),
            start_time: None,
            expect_live: true,
        };
        assert!(
            !super::survivor::is_live(&record).expect("a missing start time is answerable"),
            "an identity without a start time fails closed"
        );
        assert!(
            !super::survivor::terminate(&record).expect("nothing is signalled"),
            "this process must not be signalled by its own fixture"
        );
    }
}
