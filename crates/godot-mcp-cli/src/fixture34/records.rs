//! The nineteen durable fixture records, and the live state they describe.
//!
//! Every record here goes through Todo 6's one durable journal via the
//! `test-hooks` port [`Task34FixtureJournal`], is validated against
//! `schemas/transactions/v1.json` on the way in, and reaches `quarantined` over
//! the phase machine's own legal transition. No second journal, no second schema,
//! no hand-written record file.
//!
//! # Which record carries which proof
//!
//! A `file` record's recorded state lives in `pre_hashes` and
//! `intended_post_hashes`, so it carries no observed typed proof — that absence is
//! precisely what "quarantined: neither state is proven" means. A
//! `process_launch` or `editor_runtime` record's recorded identity has nowhere else
//! to live, so those records do carry their observed typed proof, and the repair
//! command re-derives against it.
//!
//! # The composite set's one job
//!
//! Each composite's file bytes are written to equal its intended post-hash, so its
//! file proof matches exactly. The only thing that can refuse it is the linked
//! `process_launch` re-derivation, and each linked transaction is left in the
//! exact state its composite's ID encodes.

use std::collections::BTreeMap;

use godot_mcp_server::test_support::{
    Task34EffectKind, Task34FileOperation, Task34FileState, Task34FixtureJournal, Task34Held,
    Task34Proof, Task34RecordSpec, Task34Release,
};
use sha2::{Digest as _, Sha256};

use super::state::FixtureState;
use super::tree::{FixturePaths, scene_bytes, superseded_scene_bytes};
use super::{FixtureError, SCENARIO, bridge, ids, proofs, survivor, tree};

/// The live facts one `process_launch` fixture is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessFacts {
    /// The containment boundary's own concrete identity.
    pub boundary_identity: String,
    /// Live-observed PID.
    pub pid: Option<u32>,
    /// Live-observed platform-native start time.
    pub start_time: Option<String>,
    /// Whether boundary membership was independently confirmed.
    pub membership_confirmed: bool,
    /// Observed release/terminal disposition.
    pub release: Task34Release,
    /// Observed exit code, when the process exited normally.
    pub exit_code: Option<i64>,
    /// Observed terminating signal, when the process was signalled.
    pub signal: Option<String>,
    /// Whether the boundary was proven empty after exit.
    pub boundary_empty: Option<bool>,
}

/// Everything setup observed while building the fixture. The proof writer reads
/// only from here, so no proof field can be invented.
#[derive(Debug)]
pub struct LiveFixture {
    /// The resolved fixture paths.
    pub paths: FixturePaths,
    /// The held project identity every record binds to.
    pub held: Task34Held,
    /// Live process facts, keyed by the transaction they belong to.
    pub processes: BTreeMap<String, ProcessFacts>,
    /// Bridge receipts, keyed by the `editor_runtime` transaction they prove.
    pub receipts: BTreeMap<String, bridge::ObservedReceipt>,
    /// The touched object identity of every fixture path, keyed by path.
    pub object_identities: BTreeMap<String, Option<String>>,
}

/// Lowercase hexadecimal SHA-256 of `bytes`.
#[must_use]
pub fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

/// Every `file`-kind transaction whose bytes the fixture writes, in write order.
fn file_transactions() -> Vec<&'static str> {
    let mut all = vec![ids::FILE_PRE, ids::FILE_POST];
    all.extend_from_slice(&ids::COMPOSITE);
    all.push(ids::UNRELATED_DECOY);
    all
}

/// Build the entire fixture: tree, live processes, bridge, records and proofs.
///
/// # Errors
/// Returns the first [`FixtureError`] any step reports. The caller is responsible
/// for rolling back; this function deliberately leaves the partially written
/// manifest in place so the rollback can read it.
pub fn build(paths: &FixturePaths) -> Result<(), FixtureError> {
    let scenes: Vec<(&str, Vec<u8>)> = file_transactions()
        .into_iter()
        .map(|transaction| {
            let path = ids::touched_path(transaction).unwrap_or_default();
            (path, scene_bytes(transaction))
        })
        .collect();
    tree::create_project_object(paths, &scenes)?;

    let journal =
        Task34FixtureJournal::open(&paths.project_root).map_err(|error| FixtureError::Journal {
            detail: format!(
                "the fixture project {} could not be owned: {error}",
                paths.project_root.display()
            ),
        })?;
    let held = journal.held().clone();

    let mut state = FixtureState::new(SCENARIO, &paths.project_root);
    state.save(&paths.state_file())?;

    let processes = start_processes(paths, &mut state)?;
    let receipts = start_bridge(paths, &held, &mut state)?;

    let object_identities = scenes
        .iter()
        .map(|(path, _bytes)| ((*path).to_owned(), journal.touched_object_identity(path)))
        .collect();

    let live = LiveFixture {
        paths: paths.clone(),
        held,
        processes,
        receipts,
        object_identities,
    };

    for spec in specifications(&live) {
        journal
            .write_quarantined(&spec)
            .map_err(|error| FixtureError::Journal {
                detail: format!(
                    "record {} could not be written: {error}",
                    spec.transaction_id
                ),
            })?;
    }

    proofs::write_all(&live)?;
    state.save(&paths.state_file())
}

fn start_processes(
    paths: &FixturePaths,
    state: &mut FixtureState,
) -> Result<BTreeMap<String, ProcessFacts>, FixtureError> {
    let mut facts = BTreeMap::new();

    let blocked = survivor::spawn_blocked(&paths.survivor_fifo(ids::PROCESS_PRE))?;
    state
        .survivors
        .push(blocked.record("suspended-survivor", ids::PROCESS_PRE, true));
    state.save(&paths.state_file())?;
    facts.insert(
        ids::PROCESS_PRE.to_owned(),
        ProcessFacts {
            boundary_identity: boundary_identity(paths, ids::PROCESS_PRE),
            pid: Some(blocked.pid),
            start_time: blocked.start_time.clone(),
            membership_confirmed: true,
            release: Task34Release::Suspended,
            exit_code: None,
            signal: None,
            boundary_empty: None,
        },
    );

    let resumed = survivor::spawn_live()?;
    state
        .survivors
        .push(resumed.record("resumed-contained-survivor", ids::PROCESS_POST, true));
    state.save(&paths.state_file())?;
    facts.insert(
        ids::PROCESS_POST.to_owned(),
        ProcessFacts {
            boundary_identity: boundary_identity(paths, ids::PROCESS_POST),
            pid: Some(resumed.pid),
            start_time: resumed.start_time.clone(),
            membership_confirmed: true,
            release: Task34Release::ResumedContained,
            exit_code: None,
            signal: None,
            boundary_empty: Some(false),
        },
    );

    // `ids::LINKED[0]` belongs to the composite whose linked proof is deliberately
    // unrecorded, so no process is started and no facts are recorded for it.

    let clean = survivor::spawn_then_exit(0)?;
    state.survivors.push(clean.observed.record(
        "unreferenced-clean-linked-process",
        ids::LINKED[1],
        false,
    ));
    state.save(&paths.state_file())?;
    facts.insert(
        ids::LINKED[1].to_owned(),
        ProcessFacts {
            boundary_identity: boundary_identity(paths, ids::LINKED[1]),
            pid: Some(clean.observed.pid),
            start_time: clean.observed.start_time.clone(),
            membership_confirmed: true,
            release: Task34Release::ExitedClean,
            exit_code: clean.exit_code,
            signal: None,
            boundary_empty: Some(true),
        },
    );

    let live = survivor::spawn_live()?;
    state
        .survivors
        .push(live.record("live-linked-process", ids::LINKED[2], true));
    state.save(&paths.state_file())?;
    facts.insert(
        ids::LINKED[2].to_owned(),
        ProcessFacts {
            boundary_identity: boundary_identity(paths, ids::LINKED[2]),
            pid: Some(live.pid),
            start_time: live.start_time.clone(),
            membership_confirmed: true,
            release: Task34Release::ResumedContained,
            exit_code: None,
            signal: None,
            boundary_empty: Some(false),
        },
    );

    let signaled = survivor::spawn_then_signal()?;
    state.survivors.push(signaled.observed.record(
        "signaled-linked-process",
        ids::LINKED[3],
        false,
    ));
    state.save(&paths.state_file())?;
    facts.insert(
        ids::LINKED[3].to_owned(),
        ProcessFacts {
            boundary_identity: boundary_identity(paths, ids::LINKED[3]),
            pid: Some(signaled.observed.pid),
            start_time: signaled.observed.start_time.clone(),
            membership_confirmed: true,
            release: Task34Release::Signaled,
            exit_code: signaled.exit_code,
            signal: signaled.signal.clone(),
            boundary_empty: Some(true),
        },
    );

    let nonzero = survivor::spawn_then_exit(7)?;
    state.survivors.push(nonzero.observed.record(
        "nonzero-exit-linked-process",
        ids::LINKED[4],
        false,
    ));
    state.save(&paths.state_file())?;
    facts.insert(
        ids::LINKED[4].to_owned(),
        ProcessFacts {
            boundary_identity: boundary_identity(paths, ids::LINKED[4]),
            pid: Some(nonzero.observed.pid),
            start_time: nonzero.observed.start_time.clone(),
            membership_confirmed: true,
            release: Task34Release::NonzeroExit,
            exit_code: nonzero.exit_code,
            signal: None,
            boundary_empty: Some(true),
        },
    );

    let (parent, descendant) = survivor::spawn_leaving_descendant()?;
    state.survivors.push(parent.observed.record(
        "boundary-leak-linked-process",
        ids::LINKED[5],
        false,
    ));
    state
        .survivors
        .push(descendant.record("boundary-leak-descendant", ids::LINKED[5], true));
    state.save(&paths.state_file())?;
    facts.insert(
        ids::LINKED[5].to_owned(),
        ProcessFacts {
            boundary_identity: boundary_identity(paths, ids::LINKED[5]),
            pid: Some(parent.observed.pid),
            start_time: parent.observed.start_time.clone(),
            membership_confirmed: true,
            release: Task34Release::ExitedClean,
            exit_code: parent.exit_code,
            signal: None,
            boundary_empty: Some(false),
        },
    );

    for (transaction, entry) in &facts {
        let dir = paths.boundary_dir(transaction);
        tree::create_dir(&dir)?;
        let members: Vec<String> = state
            .survivors
            .iter()
            .filter(|record| &record.transaction_id == transaction)
            .map(|record| {
                format!(
                    "{} {}\n",
                    record.pid,
                    record.start_time.as_deref().unwrap_or("-")
                )
            })
            .collect();
        tree::write_file(&dir.join("members"), members.concat().as_bytes())?;
        tree::write_file(&dir.join("identity"), entry.boundary_identity.as_bytes())?;
    }

    Ok(facts)
}

fn start_bridge(
    paths: &FixturePaths,
    held: &Task34Held,
    state: &mut FixtureState,
) -> Result<BTreeMap<String, bridge::ObservedReceipt>, FixtureError> {
    let config = bridge::BridgeConfig {
        project_object_identity: held.object_identity.clone(),
        project_root_object: held.root_object.clone(),
        project_file_object: held.file_object.clone(),
        project_generation: held.generation,
        rendezvous_path: paths.bridge_rendezvous().display().to_string(),
        ready_path: paths.bridge_ready().display().to_string(),
        transactions: vec![ids::EDITOR_PRE.to_owned(), ids::EDITOR_POST.to_owned()],
    };
    let (record, endpoint) = bridge::spawn(paths, &config)?;
    state.bridge = Some(record);
    state.save(&paths.state_file())?;

    let mut receipts = BTreeMap::new();
    for transaction in [ids::EDITOR_PRE, ids::EDITOR_POST] {
        let observed = bridge::probe(
            &paths.bridge_rendezvous(),
            endpoint,
            transaction,
            &bridge::probe_nonce(transaction),
        )?;
        if !observed.reachable
            || observed.action_id.is_none()
            || observed.live_state_probe.is_none()
        {
            return Err(FixtureError::Bridge {
                detail: format!(
                    "the fake bridge returned no usable receipt for {transaction}: {observed:?}"
                ),
            });
        }
        receipts.insert(transaction.to_owned(), observed);
    }
    Ok(receipts)
}

fn boundary_identity(paths: &FixturePaths, transaction: &str) -> String {
    format!("unix_locator:{}", paths.boundary_dir(transaction).display())
}

fn file_state(path: &str, bytes: &[u8]) -> Task34FileState {
    Task34FileState {
        path: path.to_owned(),
        operation: Task34FileOperation::Update,
        sha256: Some(hex_sha256(bytes)),
    }
}

/// The record spec for a `file` transaction whose live bytes still equal its
/// recorded pre-state, so only a pre-state claim can ever match.
fn pre_state_file(transaction: &str, linked: Option<&str>) -> Task34RecordSpec {
    let path = ids::touched_path(transaction).unwrap_or_default();
    Task34RecordSpec {
        transaction_id: transaction.to_owned(),
        effect_kind: Task34EffectKind::File,
        tool_name: "resource.update".to_owned(),
        touched_paths: vec![path.to_owned()],
        pre_hashes: vec![file_state(path, &scene_bytes(transaction))],
        intended_post_hashes: vec![file_state(path, &superseded_scene_bytes(transaction))],
        proof: Task34Proof::Absent,
        required_process_transaction: linked.map(str::to_owned),
        terminal_reason: "task-34 fixture: quarantined with an unproven file effect".to_owned(),
    }
}

/// The record spec for a `file` transaction whose live bytes already equal its
/// intended post-state, so its file proof matches exactly.
fn post_state_file(transaction: &str, linked: Option<&str>) -> Task34RecordSpec {
    let path = ids::touched_path(transaction).unwrap_or_default();
    Task34RecordSpec {
        transaction_id: transaction.to_owned(),
        effect_kind: Task34EffectKind::File,
        tool_name: "resource.update".to_owned(),
        touched_paths: vec![path.to_owned()],
        pre_hashes: vec![file_state(path, &superseded_scene_bytes(transaction))],
        intended_post_hashes: vec![file_state(path, &scene_bytes(transaction))],
        proof: Task34Proof::Absent,
        required_process_transaction: linked.map(str::to_owned),
        terminal_reason: "task-34 fixture: quarantined with an unproven file effect".to_owned(),
    }
}

fn process_record(
    transaction: &str,
    facts: Option<&ProcessFacts>,
    reason: &str,
) -> Task34RecordSpec {
    Task34RecordSpec {
        transaction_id: transaction.to_owned(),
        effect_kind: Task34EffectKind::ProcessLaunch,
        tool_name: "project.run".to_owned(),
        touched_paths: Vec::new(),
        pre_hashes: Vec::new(),
        intended_post_hashes: Vec::new(),
        proof: facts.map_or(Task34Proof::Absent, |facts| Task34Proof::ProcessLaunch {
            boundary_identity: facts.boundary_identity.clone(),
            pid: facts.pid,
            start_time: facts.start_time.clone(),
            membership_confirmed: facts.membership_confirmed,
            release_status: facts.release,
            exit_code: facts.exit_code,
            signal: facts.signal.clone(),
            boundary_empty_after_exit: facts.boundary_empty,
        }),
        required_process_transaction: None,
        terminal_reason: reason.to_owned(),
    }
}

fn editor_record(transaction: &str, receipt: &bridge::ObservedReceipt) -> Task34RecordSpec {
    Task34RecordSpec {
        transaction_id: transaction.to_owned(),
        effect_kind: Task34EffectKind::EditorRuntime,
        tool_name: "editor.mutate".to_owned(),
        touched_paths: Vec::new(),
        pre_hashes: Vec::new(),
        intended_post_hashes: Vec::new(),
        proof: Task34Proof::EditorRuntime {
            bridge_reachable: receipt.reachable,
            receipt_transaction_id: receipt.transaction_id.clone(),
            receipt_action_id: receipt.action_id.clone(),
            live_state_probe: receipt.live_state_probe.clone(),
        },
        required_process_transaction: None,
        terminal_reason: "task-34 fixture: quarantined with an unresolved editor mutation"
            .to_owned(),
    }
}

/// Every record the fixture writes, in a fixed order.
fn specifications(live: &LiveFixture) -> Vec<Task34RecordSpec> {
    let mut specs = vec![
        pre_state_file(ids::FILE_PRE, None),
        post_state_file(ids::FILE_POST, None),
        process_record(
            ids::PROCESS_PRE,
            live.processes.get(ids::PROCESS_PRE),
            "task-34 fixture: quarantined with a still-suspended survivor",
        ),
        process_record(
            ids::PROCESS_POST,
            live.processes.get(ids::PROCESS_POST),
            "task-34 fixture: quarantined with a resumed, still-contained survivor",
        ),
    ];
    for transaction in [ids::EDITOR_PRE, ids::EDITOR_POST] {
        if let Some(receipt) = live.receipts.get(transaction) {
            specs.push(editor_record(transaction, receipt));
        }
    }

    specs.push(pre_state_file(ids::UNRELATED_DECOY, None));

    // The linked `process_launch` records, one per composite. The first is written
    // with no observed proof at all: that absence is the refusal reason its
    // composite's ID encodes.
    let linked_reasons = [
        "task-34 fixture: linked process proof deliberately unrecorded",
        "task-34 fixture: unreferenced clean linked process",
        "task-34 fixture: linked process deliberately left live",
        "task-34 fixture: linked process deliberately left signaled",
        "task-34 fixture: linked process deliberately left with a nonzero exit",
        "task-34 fixture: linked process deliberately left a non-empty boundary",
    ];
    for (index, transaction) in ids::LINKED.into_iter().enumerate() {
        specs.push(process_record(
            transaction,
            live.processes.get(transaction),
            linked_reasons[index],
        ));
    }

    // Each composite's file bytes already equal its intended post-hash, so its
    // file proof matches and only the linked re-derivation can refuse it.
    // `COMPOSITE_WRONG_TRANSACTION` names the unrelated `file` decoy rather than
    // the linked process transaction setup created for it.
    let composites = [
        (ids::COMPOSITE_PROOF_MISSING, ids::LINKED[0]),
        (ids::COMPOSITE_WRONG_TRANSACTION, ids::UNRELATED_DECOY),
        (ids::COMPOSITE_PROCESS_LIVE, ids::LINKED[2]),
        (ids::COMPOSITE_PROCESS_SIGNALED, ids::LINKED[3]),
        (ids::COMPOSITE_PROCESS_NONZERO_EXIT, ids::LINKED[4]),
        (ids::COMPOSITE_BOUNDARY_NONEMPTY, ids::LINKED[5]),
    ];
    for (composite, linked) in composites {
        specs.push(post_state_file(composite, Some(linked)));
    }
    specs
}
