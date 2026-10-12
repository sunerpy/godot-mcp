//! Startup reconciliation.
//!
//! Every transaction left open by a previous process is classified into exactly
//! one of three conclusions, never a guess:
//!
//! * [`Conclusion::ProvenPreState`] — the recorded pre-state is independently
//!   re-derived and matches, so the effect provably did not happen. The
//!   transaction is compensated.
//! * [`Conclusion::ProvenPostState`] — the recorded intended post-state is
//!   independently re-derived and matches (and, for a composite `file`
//!   transaction, the linked `process_launch` sub-proof independently confirms a
//!   contained, clean, terminal helper run), so the effect provably completed.
//!   The transaction is committed.
//! * [`Conclusion::Quarantine`] — anything else. The transaction becomes
//!   write-blocking `quarantined`; no ordinary permit can be issued afterwards
//!   and nothing is retried automatically.
//!
//! Reconciliation always begins by re-proving the held project object identity
//! and generation. If the held project no longer matches the journaled identity,
//! every open transaction is quarantined: a hash comparison against a *different*
//! project would prove nothing.

use std::collections::BTreeMap;

use super::proof::{ProofOutcome, ProofSource, verify_file_proof};
use godot_mcp_domain::ErrorKind;
use godot_mcp_engine::supervision::journal::LaunchRecord;

use super::record::{EffectKind, Phase, TerminalOutcome, TransactionRecord, TypedProof};
use super::store::{StoreError, TransactionStore};

/// The identity a reconciliation run holds, derived from live handles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeldProject {
    /// Public opaque project object identity.
    pub(crate) object_identity: String,
    /// Encoded root-directory OS object identity.
    pub(crate) root_object: String,
    /// Encoded `project.godot` OS object identity.
    pub(crate) file_object: String,
    /// Current project generation.
    pub(crate) generation: u64,
}

/// One of exactly three reconciliation conclusions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Conclusion {
    /// The pre-state is proven; the effect did not happen.
    ProvenPreState,
    /// The post-state is proven; the effect completed.
    ProvenPostState,
    /// Neither state is proven; the transaction blocks further writes.
    Quarantine,
}

impl Conclusion {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ProvenPreState => "proven_pre_state",
            Self::ProvenPostState => "proven_post_state",
            Self::Quarantine => "quarantine",
        }
    }

    const fn terminal_phase(self) -> Phase {
        match self {
            Self::ProvenPreState => Phase::Compensated,
            Self::ProvenPostState => Phase::CommittedFsynced,
            Self::Quarantine => Phase::Quarantined,
        }
    }

    const fn terminal_outcome(self) -> TerminalOutcome {
        match self {
            Self::ProvenPostState => TerminalOutcome::Succeeded,
            Self::ProvenPreState => TerminalOutcome::failed(ErrorKind::Unavailable),
            Self::Quarantine => TerminalOutcome::failed(ErrorKind::Uncertain),
        }
    }
}

/// The reconciled outcome for one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reconciled {
    pub(crate) transaction_id: String,
    pub(crate) conclusion: Conclusion,
    pub(crate) reason: String,
}

/// Classify and durably settle every open transaction.
///
/// The classification is computed first and journaled second, so a crash during
/// reconciliation leaves the transaction open and reconcilable again rather than
/// half-settled.
pub(crate) fn reconcile_open_transactions(
    store: &TransactionStore,
    held: &HeldProject,
    source: &dyn ProofSource,
) -> Result<Vec<Reconciled>, StoreError> {
    let open = store.open_transactions()?;
    let mut settled = Vec::with_capacity(open.len());
    for record in open {
        let (conclusion, reason) = classify(store, held, source, &record)?;
        store.audit(
            &record.transaction_id,
            "reconciliation_classified",
            &format!("{}: {reason}", conclusion.as_str()),
        )?;
        let proof = observed_proof(source, &record, conclusion);
        store.advance(
            &record.transaction_id,
            conclusion.terminal_phase(),
            proof,
            Some(reason.clone()),
            Some(conclusion.terminal_outcome()),
        )?;
        settled.push(Reconciled {
            transaction_id: record.transaction_id,
            conclusion,
            reason,
        });
    }
    Ok(settled)
}

pub(crate) fn reconcile_linked_process_transactions(
    store: &TransactionStore,
    held: &HeldProject,
) -> Result<Vec<Reconciled>, StoreError> {
    let open = store.open_transactions()?;
    let mut settled = Vec::new();
    for record in open {
        if record.effect_kind != EffectKind::ProcessLaunch
            || record.required_process_transaction().is_none()
        {
            continue;
        }
        let (conclusion, reason, proof) = classify_linked_process(store, held, &record)?;
        store.audit(
            &record.transaction_id,
            "linked_process_reconciliation_classified",
            &format!("{}: {reason}", conclusion.as_str()),
        )?;
        store.advance(
            &record.transaction_id,
            conclusion.terminal_phase(),
            proof,
            Some(reason.clone()),
            Some(conclusion.terminal_outcome()),
        )?;
        settled.push(Reconciled {
            transaction_id: record.transaction_id,
            conclusion,
            reason,
        });
    }
    Ok(settled)
}

fn classify_linked_process(
    store: &TransactionStore,
    held: &HeldProject,
    outer: &TransactionRecord,
) -> Result<(Conclusion, String, Option<TypedProof>), StoreError> {
    if !matches_held_project(outer, held) {
        return Ok((
            Conclusion::Quarantine,
            "held project object identity or generation does not match the outer process transaction"
                .to_owned(),
            None,
        ));
    }
    let linked_id = outer
        .required_process_transaction()
        .expect("the caller filters linked process transactions");
    let linked = match store.load(linked_id) {
        Ok(linked) => linked,
        Err(StoreError::NotFound(_)) => {
            return Ok((
                Conclusion::ProvenPreState,
                format!(
                    "linked supervised launch {linked_id} has no durable intent, so no process effect crossed its recovery anchor"
                ),
                None,
            ));
        }
        Err(error) => return Err(error),
    };
    let canonical_launch = linked.effect_kind == EffectKind::ProcessLaunch
        && linked
            .staged_write_set
            .iter()
            .any(|entry| entry.path == LaunchRecord::INTENT_PAYLOAD);
    if !canonical_launch || !same_project(outer, &linked) {
        return Ok((
            Conclusion::Quarantine,
            format!(
                "linked transaction {linked_id} is not a canonical supervised launch for the same held project"
            ),
            None,
        ));
    }
    match linked.phase {
        Phase::Compensated => Ok((
            Conclusion::ProvenPreState,
            format!(
                "linked supervised launch {linked_id} was phase-aware reconciled as compensated"
            ),
            linked.proof,
        )),
        Phase::CommittedFsynced
            if outer.phase == Phase::EffectStartedFsynced && linked.proof.is_some() =>
        {
            Ok((
                Conclusion::ProvenPostState,
                format!(
                    "linked supervised launch {linked_id} was phase-aware reconciled as committed"
                ),
                linked.proof,
            ))
        }
        Phase::Quarantined => Ok((
            Conclusion::Quarantine,
            format!(
                "linked supervised launch {linked_id} remains quarantined after phase-aware reconciliation"
            ),
            None,
        )),
        phase => Ok((
            Conclusion::Quarantine,
            format!(
                "linked supervised launch {linked_id} is in phase {phase}, which cannot prove the outer process outcome"
            ),
            None,
        )),
    }
}

fn matches_held_project(record: &TransactionRecord, held: &HeldProject) -> bool {
    record.project_object_identity == held.object_identity
        && record.project_root_object == held.root_object
        && record.project_file_object == held.file_object
        && record.project_generation == held.generation
}

fn same_project(left: &TransactionRecord, right: &TransactionRecord) -> bool {
    left.project_object_identity == right.project_object_identity
        && left.project_root_object == right.project_root_object
        && left.project_file_object == right.project_file_object
        && left.project_generation == right.project_generation
}

fn observed_proof(
    source: &dyn ProofSource,
    record: &TransactionRecord,
    conclusion: Conclusion,
) -> Option<TypedProof> {
    if record.effect_kind != EffectKind::File || conclusion == Conclusion::Quarantine {
        return None;
    }
    let entries = match conclusion {
        Conclusion::ProvenPreState => &record.pre_hashes,
        Conclusion::ProvenPostState => &record.intended_post_hashes,
        Conclusion::Quarantine => return None,
    };
    source
        .observe_file_states(entries)
        .ok()
        .map(|entries| TypedProof::File { entries })
}

fn classify(
    store: &TransactionStore,
    held: &HeldProject,
    source: &dyn ProofSource,
    record: &TransactionRecord,
) -> Result<(Conclusion, String), StoreError> {
    if !matches_held_project(record, held) {
        return Ok((
            Conclusion::Quarantine,
            "held project object identity or generation does not match the journaled transaction"
                .to_owned(),
        ));
    }

    match record.effect_kind {
        EffectKind::File => classify_file(store, source, record),
        EffectKind::ProcessLaunch => Ok(classify_process(record)),
        EffectKind::EditorRuntime => Ok(classify_editor_runtime(record)),
    }
}

fn classify_file(
    store: &TransactionStore,
    source: &dyn ProofSource,
    record: &TransactionRecord,
) -> Result<(Conclusion, String), StoreError> {
    // The intent declares the planned staged-write metadata before any payload is
    // written. Only phases at or beyond `staged_fsynced` may therefore require the
    // payloads themselves; requiring them at `intent_fsynced` would quarantine a
    // normal pre-staging crash even when the recorded pre-state is still proven.
    if matches!(
        record.phase,
        Phase::StagedFsynced | Phase::EffectStartedFsynced
    ) {
        for staged in &record.staged_write_set {
            if let Err(error) = store.staged_bytes(&record.transaction_id, &staged.staging_id) {
                return Ok((
                    Conclusion::Quarantine,
                    format!("staged write set is not intact: {error}"),
                ));
            }
        }
    }

    let post = verify_file_proof(source, &record.intended_post_hashes);
    let pre = verify_file_proof(source, &record.pre_hashes);
    let classified = classify_file_proofs(record.phase, post, pre);

    // A composite `file` transaction (Todo 17) can only reach a proven post-state
    // when its linked `process_launch` transaction independently proves a
    // contained helper run that exited cleanly with an empty boundary afterwards.
    if classified.0 == Conclusion::ProvenPostState
        && let Some(linked) = record.required_process_transaction()
    {
        match store.load(linked) {
            Ok(linked_record) => {
                let satisfied = linked_record
                    .proof
                    .as_ref()
                    .is_some_and(TypedProof::is_clean_terminal_process)
                    && linked_record.phase == Phase::CommittedFsynced;
                if !satisfied {
                    return Ok((
                        Conclusion::Quarantine,
                        format!(
                            "linked process_launch transaction {linked} does not prove a contained, \
                             clean, terminal helper run"
                        ),
                    ));
                }
            }
            Err(error) => {
                return Ok((
                    Conclusion::Quarantine,
                    format!("linked process_launch transaction {linked} is unusable: {error}"),
                ));
            }
        }
    }

    Ok(classified)
}

fn classify_file_proofs(
    phase: Phase,
    post: ProofOutcome,
    pre: ProofOutcome,
) -> (Conclusion, String) {
    match phase {
        // These phases precede the effect boundary. Even byte-identical pre/post
        // states cannot prove that an effect which never started succeeded.
        Phase::IntentFsynced | Phase::StagedFsynced => match pre {
            ProofOutcome::Matches => (
                Conclusion::ProvenPreState,
                "every touched path still matches its recorded pre-state before the effect boundary"
                    .to_owned(),
            ),
            pre => (
                Conclusion::Quarantine,
                format!(
                    "{phase} has not entered the effect boundary and its pre-state is not proven \
                     (post: {post}, pre: {pre})"
                ),
            ),
        },
        Phase::EffectStartedFsynced => match (post, pre) {
            (ProofOutcome::Matches, _) => (
                Conclusion::ProvenPostState,
                "every touched path matches its recorded intended post-state".to_owned(),
            ),
            (_, ProofOutcome::Matches) => (
                Conclusion::ProvenPreState,
                "every touched path still matches its recorded pre-state".to_owned(),
            ),
            (post, pre) => (
                Conclusion::Quarantine,
                format!("neither state is proven (post: {post}, pre: {pre})"),
            ),
        },
        Phase::CommittedFsynced | Phase::Compensated | Phase::Quarantined => (
            Conclusion::Quarantine,
            format!("terminal phase {phase} was unexpectedly presented for reconciliation"),
        ),
    }
}

fn classify_process(record: &TransactionRecord) -> (Conclusion, String) {
    // A `process_launch` transaction is never proven by a file hash. Without the
    // typed launch record from Todo 14's live OS inspection there is no proof at
    // all, so an interrupted launch is quarantined rather than guessed.
    match record.proof.as_ref() {
        Some(proof) if proof.is_clean_terminal_process() => (
            Conclusion::ProvenPostState,
            "process_launch proof shows a contained clean exit with an empty boundary".to_owned(),
        ),
        Some(_) => (
            Conclusion::Quarantine,
            "process_launch proof does not show a contained clean terminal run".to_owned(),
        ),
        None => (
            Conclusion::Quarantine,
            "process_launch transaction has no typed launch proof; a file hash can never \
             substitute for one"
                .to_owned(),
        ),
    }
}

fn classify_editor_runtime(record: &TransactionRecord) -> (Conclusion, String) {
    // A change to editor state alone (a selection, the current scene, an
    // unsaved undo) never had a project file in scope: whatever the editor
    // did, every project file still holds its pre-state.
    if record.touched_paths.is_empty() {
        return (
            Conclusion::ProvenPreState,
            "the editor-state change touched no project file; the project pre-state holds"
                .to_owned(),
        );
    }
    match record.proof.as_ref() {
        Some(TypedProof::EditorRuntime {
            bridge_reachable,
            receipt_transaction_id,
            receipt_action_id,
            live_state_probe,
        }) => {
            let receipt_bound =
                receipt_transaction_id.as_deref() == Some(record.transaction_id.as_str());
            if *bridge_reachable
                && receipt_bound
                && receipt_action_id.is_some()
                && live_state_probe.is_some()
            {
                (
                    Conclusion::ProvenPostState,
                    "bridge receipt is bound to this transaction and the live probe confirms the \
                     claimed post-state"
                        .to_owned(),
                )
            } else {
                (
                    Conclusion::Quarantine,
                    "editor_runtime proof is unreachable, unbound or missing its live probe; a \
                     file-level hash cannot prove editor-side state"
                        .to_owned(),
                )
            }
        }
        _ => (
            Conclusion::Quarantine,
            "editor_runtime transaction has no authoritative bridge receipt".to_owned(),
        ),
    }
}

/// Summary of one reconciliation run, exposed through health/scheduler state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReconciliationReport {
    pub(crate) counts: BTreeMap<&'static str, usize>,
    pub(crate) quarantined: Vec<String>,
}

impl ReconciliationReport {
    pub(crate) fn from_settled(settled: &[Reconciled]) -> Self {
        let mut counts = BTreeMap::new();
        let mut quarantined = Vec::new();
        for entry in settled {
            *counts.entry(entry.conclusion.as_str()).or_insert(0) += 1;
            if entry.conclusion == Conclusion::Quarantine {
                quarantined.push(entry.transaction_id.clone());
            }
        }
        Self {
            counts,
            quarantined,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::EffectBoundaryKind;
    use crate::journal::record::{OperationFamily, TouchedObject, TransportMode};

    fn open_editor_record(touched: &[&str]) -> TransactionRecord {
        TransactionRecord {
            schema_version: "1".to_owned(),
            transaction_id: "00000000-0000-4000-8000-000000000201".to_owned(),
            effect_kind: EffectKind::EditorRuntime,
            effect_boundary: EffectBoundaryKind::Editor,
            project_object_identity: format!("project_{}", "a".repeat(64)),
            project_root_object: "unix:dev=1,ino=2".to_owned(),
            project_file_object: "unix:dev=1,ino=3".to_owned(),
            project_generation: 0,
            transport_mode: TransportMode::Stdio,
            principal: None,
            session: None,
            operation_family: OperationFamily::EditorMutation,
            tool_name: "editor.selection.set".to_owned(),
            idempotency_id: "idem-editor".to_owned(),
            canonical_request_hash: "b".repeat(64),
            canonical_request_bytes: 2,
            touched_paths: touched.iter().map(|path| (*path).to_owned()).collect(),
            touched_object_identities: touched
                .iter()
                .map(|path| TouchedObject {
                    path: (*path).to_owned(),
                    object_identity: None,
                })
                .collect(),
            pre_hashes: Vec::new(),
            staged_write_set: Vec::new(),
            intended_post_hashes: Vec::new(),
            phase: Phase::EffectStartedFsynced,
            phase_history: Vec::new(),
            proof: None,
            required_subproofs: None,
            terminal_reason: None,
            terminal_outcome: None,
            opened_at_unix_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn an_open_editor_change_proves_its_pre_state_only_when_no_project_file_is_in_scope() {
        let (editor_only, _) = classify_editor_runtime(&open_editor_record(&[]));
        assert_eq!(editor_only, Conclusion::ProvenPreState);
        let (saving, _) = classify_editor_runtime(&open_editor_record(&["scenes/main.tscn"]));
        assert_eq!(saving, Conclusion::Quarantine);
    }

    #[test]
    fn file_reconciliation_decision_table_is_phase_aware() {
        let proof_cases = [
            ("pre-only", ProofOutcome::Mismatch, ProofOutcome::Matches),
            ("post-only", ProofOutcome::Matches, ProofOutcome::Mismatch),
            ("both", ProofOutcome::Matches, ProofOutcome::Matches),
            ("neither", ProofOutcome::Mismatch, ProofOutcome::Mismatch),
        ];
        let phases = [
            Phase::IntentFsynced,
            Phase::StagedFsynced,
            Phase::EffectStartedFsynced,
        ];

        for phase in phases {
            for (case, post, pre) in proof_cases {
                let expected = match (phase, case) {
                    (Phase::IntentFsynced | Phase::StagedFsynced, "pre-only" | "both") => {
                        Conclusion::ProvenPreState
                    }
                    (Phase::EffectStartedFsynced, "post-only" | "both") => {
                        Conclusion::ProvenPostState
                    }
                    (Phase::EffectStartedFsynced, "pre-only") => Conclusion::ProvenPreState,
                    (_, "post-only" | "neither") => Conclusion::Quarantine,
                    _ => unreachable!("the decision table enumerates every proof case"),
                };
                let (actual, _reason) = classify_file_proofs(phase, post, pre);
                assert_eq!(
                    actual, expected,
                    "{phase} with {case} must classify as {expected:?}"
                );

                let terminal = actual.terminal_phase();
                assert!(
                    phase.legal_successors().contains(&terminal),
                    "{phase} with {case} produced illegal reconciliation transition to {terminal}"
                );
                let expected_outcome = match actual {
                    Conclusion::ProvenPostState => TerminalOutcome::Succeeded,
                    Conclusion::ProvenPreState => TerminalOutcome::failed(ErrorKind::Unavailable),
                    Conclusion::Quarantine => TerminalOutcome::failed(ErrorKind::Uncertain),
                };
                assert_eq!(
                    actual.terminal_outcome(),
                    expected_outcome,
                    "{phase} with {case} must retain the typed terminal outcome"
                );
            }
        }
    }
}
