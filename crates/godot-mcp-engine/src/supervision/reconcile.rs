//! The crash-recovery state machine.
//!
//! Reconciliation classifies each open transaction by inspecting **exactly which
//! of the four canonical fsync marks are present, in ascending order**. The five
//! cases below are disjoint and exhaustive, and every one maps to exactly one of
//! the three terminal dispositions — `compensated`, `committed` or `quarantined`.
//! None is left undefined and none allows a fourth classification.
//!
//! | case | marks present | how it resolves |
//! | --- | --- | --- |
//! | 1 [`ReconcileCase::NothingDurable`] | none | no-op; proven pre-state, no record written |
//! | 2 [`ReconcileCase::IntentWithoutStaged`] | intent | enumerate the boundary/locator **from the intent record's already-durable identity**; empty → `compensated`; members → terminate and prove empty → `compensated`; cannot prove → `quarantined` |
//! | 3 [`ReconcileCase::StagedWithoutReceipt`] | intent, staged | provably absent or still blocked → kill, prove empty, `compensated`; live and not provably blocked → `quarantined`; weak path additionally requires an exact PID/start-time/PGID/nonce match |
//! | 4 [`ReconcileCase::ReceiptWithoutCommit`] | intent, staged, receipt | resume provably never happened → terminate, `compensated`; resume proven **and** a live contained survivor → case 5; neither provable → `quarantined` |
//! | 5 [`ReconcileCase::SurvivorReconciliation`] | committed, or case 4(ii) | live contained survivor matching its receipt → `committed`; dead with resume already proven → `committed`; unconfirmable → `quarantined` |
//!
//! `committed` is never assigned unless resume/release is independently proven, so
//! a transaction can never commit despite an incomplete release. Death alone never
//! substitutes for a proven resume outcome: a dead process with an uncertain
//! release history is `quarantined`, never inferred as `compensated`.
//!
//! Nothing is ever retried automatically, and a `quarantined` transaction blocks
//! every further mutation pending Todo-34 operator repair.

use std::path::{Path, PathBuf};

use super::boundary::{
    BoundaryError, BoundaryIdentity, BoundaryProvider, BoundaryStrength, ContainmentBoundary,
    ContainmentState, MemberObservation,
};
use super::gate::{self, GateState};
use super::journal::{
    BoundaryMembership, Disposition, JournalError, LAUNCH_ABANDONED_PRE_RELEASE, LaunchJournal,
    LaunchPhase, LaunchRecord, ProcessLaunchProof, ReleaseStatus,
};
use super::procfs;
use super::protocol::facts;

/// The five disjoint, exhaustive crash-recovery cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReconcileCase {
    /// Case 1: no `intent_fsynced` mark exists. Nothing was ever durably
    /// recorded, so this is a proven pre-state and not a disposition needing a
    /// record.
    NothingDurable,
    /// Case 2: `intent_fsynced` present, `staged_fsynced` absent.
    IntentWithoutStaged,
    /// Case 3: `staged_fsynced` present, `effect_started_fsynced` absent.
    StagedWithoutReceipt,
    /// Case 4: `effect_started_fsynced` present, `committed_fsynced` absent.
    ReceiptWithoutCommit,
    /// Case 5: `committed_fsynced` present, or case 4 outcome (ii).
    SurvivorReconciliation,
}

impl ReconcileCase {
    /// Every case. There are five, permanently.
    pub const ALL: [Self; 5] = [
        Self::NothingDurable,
        Self::IntentWithoutStaged,
        Self::StagedWithoutReceipt,
        Self::ReceiptWithoutCommit,
        Self::SurvivorReconciliation,
    ];

    /// Stable spelling for diagnostics and audit records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NothingDurable => "nothing_durable",
            Self::IntentWithoutStaged => "intent_without_staged",
            Self::StagedWithoutReceipt => "staged_without_receipt",
            Self::ReceiptWithoutCommit => "receipt_without_commit",
            Self::SurvivorReconciliation => "survivor_reconciliation",
        }
    }

    /// Which case one durable record's present marks select.
    #[must_use]
    pub const fn of(phase: LaunchPhase) -> Self {
        match phase {
            LaunchPhase::IntentFsynced => Self::IntentWithoutStaged,
            LaunchPhase::StagedFsynced => Self::StagedWithoutReceipt,
            LaunchPhase::EffectStartedFsynced => Self::ReceiptWithoutCommit,
            LaunchPhase::CommittedFsynced => Self::SurvivorReconciliation,
        }
    }
}

/// One reconciled transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciledLaunch {
    /// The transaction that was reconciled.
    pub transaction_id: String,
    /// Which of the five disjoint cases applied.
    pub case: ReconcileCase,
    /// The one terminal disposition recorded.
    pub disposition: Disposition,
    /// Why, in terms of what was independently proven.
    pub reason: String,
}

/// Reconcile every launch transaction a previous process left behind, against
/// live OS state.
///
/// Both durable populations are reconciled: the transactions still open at one of
/// the first three canonical marks (cases 2–4), and the transactions already at
/// `committed_fsynced` whose survivor a restart still owns (case 5). A committed
/// survivor that is re-confirmed keeps its `committed` disposition and its
/// receipt; one that cannot be confirmed is quarantined and blocks mutation.
///
/// # Errors
/// Returns [`JournalError`] when the durable store cannot be read, or when a
/// terminal record cannot be durably written.
pub fn reconcile_launches(
    journal: &dyn LaunchJournal,
    providers: &[&dyn BoundaryProvider],
) -> Result<Vec<ReconciledLaunch>, JournalError> {
    let mut settled = Vec::new();
    for record in journal.open_launches()? {
        let case = ReconcileCase::of(record.phase);
        let (disposition, reason) = classify(&record, providers);
        let proof = terminal_proof(&record, disposition);
        journal.settle(&record, disposition, &reason, proof.as_ref())?;
        cleanup_terminal_scratch(&record, disposition, providers);
        settled.push(ReconciledLaunch {
            transaction_id: record.envelope.transaction_id.clone(),
            case: effective_case(case, disposition),
            disposition,
            reason,
        });
    }
    for record in journal.committed_launches()? {
        let (disposition, reason) = classify(&record, providers);
        // A re-confirmed survivor keeps the receipt it already has, so nothing is
        // rewritten; only a survivor that can no longer be confirmed is settled
        // again, into the write-blocking quarantine.
        if disposition != Disposition::Committed {
            journal.settle(
                &record,
                disposition,
                &reason,
                terminal_proof(&record, disposition).as_ref(),
            )?;
        }
        cleanup_terminal_scratch(&record, disposition, providers);
        settled.push(ReconciledLaunch {
            transaction_id: record.envelope.transaction_id.clone(),
            case: ReconcileCase::SurvivorReconciliation,
            disposition,
            reason,
        });
    }
    Ok(settled)
}

fn cleanup_terminal_scratch(
    record: &LaunchRecord,
    disposition: Disposition,
    providers: &[&dyn BoundaryProvider],
) {
    if disposition == Disposition::Quarantined || !terminal_scratch_reclaimable(record, providers) {
        return;
    }
    if let Some(gate_path) = recorded_gate(record) {
        let _ = gate::remove_recorded_scratch(&gate_path, &record.envelope.transaction_id);
    }
}

/// Whether a terminal launch's boundary is absent or provably empty and closed,
/// making its scratch safe to reclaim. Unprovable states fail toward retention.
#[must_use]
pub fn terminal_scratch_reclaimable(
    record: &LaunchRecord,
    providers: &[&dyn BoundaryProvider],
) -> bool {
    let Some(identity) = record.boundary_identity() else {
        return false;
    };
    match reopen(&identity, providers) {
        Ok(None) => true,
        Ok(Some(boundary)) => {
            matches!(boundary.state(), Ok(ContainmentState::Empty)) && boundary.close().is_ok()
        }
        Err(_) => false,
    }
}

/// Case 4 outcome (ii) falls through to case 5, so a retroactively committed
/// receipt is reported as survivor reconciliation rather than as case 4.
const fn effective_case(case: ReconcileCase, disposition: Disposition) -> ReconcileCase {
    match (case, disposition) {
        (ReconcileCase::ReceiptWithoutCommit, Disposition::Committed) => {
            ReconcileCase::SurvivorReconciliation
        }
        (case, _) => case,
    }
}

/// The unresolved-survivor state that blocks every mutation, if any.
///
/// # Errors
/// Returns [`JournalError`] when the durable store cannot be read.
pub fn mutation_block(journal: &dyn LaunchJournal) -> Result<Option<String>, JournalError> {
    let quarantined = journal.quarantined()?;
    if quarantined.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!(
        "{} unresolved launch survivor(s) block every mutation pending operator repair: {}",
        quarantined.len(),
        quarantined.join(", ")
    )))
}

/// The typed proof written alongside a terminal record.
///
/// A `process_launch` transaction is never proven by a content hash, so the only
/// proof form here is Todo 6's `process_launch` shape.
fn terminal_proof(record: &LaunchRecord, disposition: Disposition) -> Option<ProcessLaunchProof> {
    let identity = record.boundary_identity()?;
    let child = recorded_child(record);
    Some(match disposition {
        Disposition::Compensated => ProcessLaunchProof {
            boundary_identity: identity.identity,
            pid: child.as_ref().map(|child| child.pid),
            start_time: child.and_then(|child| child.start_time),
            boundary_membership: if record.phase >= LaunchPhase::EffectStartedFsynced {
                BoundaryMembership::Confirmed
            } else {
                BoundaryMembership::Unconfirmed
            },
            release_status: ReleaseStatus::Suspended,
            exit_code: None,
            signal: None,
            boundary_empty_after_exit: Some(true),
        },
        Disposition::Committed => ProcessLaunchProof {
            boundary_identity: identity.identity,
            pid: child.as_ref().map(|child| child.pid),
            start_time: child.and_then(|child| child.start_time),
            boundary_membership: BoundaryMembership::Confirmed,
            release_status: ReleaseStatus::ResumedContained,
            exit_code: None,
            signal: None,
            boundary_empty_after_exit: Some(false),
        },
        // A quarantined transaction has, by definition, nothing independently
        // proven to record.
        Disposition::Quarantined => return None,
    })
}

/// One recorded process identity: the tuple that must match exactly before a live
/// process is trusted as the one the durable record named.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedProcess {
    pid: u32,
    start_time: Option<String>,
    process_group: Option<u32>,
}

fn recorded_anchor(record: &LaunchRecord) -> Option<RecordedProcess> {
    let staged = record.facts(LaunchRecord::STAGED_PAYLOAD).ok()?;
    Some(RecordedProcess {
        pid: u32::try_from(staged.get(facts::ANCHOR_PID)?.as_u64()?).ok()?,
        start_time: staged
            .get(facts::ANCHOR_START_TIME)
            .and_then(|value| value.as_str())
            .map(str::to_owned),
        process_group: staged
            .get(facts::ANCHOR_PROCESS_GROUP)
            .and_then(|value| value.as_u64())
            .and_then(|group| u32::try_from(group).ok()),
    })
}

fn recorded_child(record: &LaunchRecord) -> Option<RecordedProcess> {
    let receipt = record.facts(LaunchRecord::RECEIPT_PAYLOAD).ok()?;
    Some(RecordedProcess {
        pid: u32::try_from(receipt.get(facts::CHILD_PID)?.as_u64()?).ok()?,
        start_time: receipt
            .get(facts::CHILD_START_TIME)
            .and_then(|value| value.as_str())
            .map(str::to_owned),
        process_group: recorded_anchor(record).and_then(|anchor| anchor.process_group),
    })
}

fn recorded_gate(record: &LaunchRecord) -> Option<PathBuf> {
    let intent = record.facts(LaunchRecord::INTENT_PAYLOAD).ok()?;
    intent
        .get(facts::GATE_PATH)
        .and_then(|value| value.as_str())
        .map(PathBuf::from)
}

fn reopen<'p>(
    identity: &BoundaryIdentity,
    providers: &[&'p dyn BoundaryProvider],
) -> Result<Option<Box<dyn ContainmentBoundary + 'p>>, BoundaryError> {
    for provider in providers {
        if provider.kind() != identity.kind {
            continue;
        }
        if let Some(boundary) = provider.reopen(identity)? {
            return Ok(Some(boundary));
        }
        return Ok(None);
    }
    Err(BoundaryError::Unavailable(format!(
        "no provider for boundary kind {} is configured, so its contents cannot be inspected",
        identity.kind.as_str()
    )))
}

fn classify(record: &LaunchRecord, providers: &[&dyn BoundaryProvider]) -> (Disposition, String) {
    let Some(identity) = record.boundary_identity() else {
        return (
            Disposition::Quarantined,
            "the durable intent record does not bind a containment-boundary identity, so the \
             boundary cannot be enumerated"
                .to_owned(),
        );
    };
    let boundary = match reopen(&identity, providers) {
        Ok(boundary) => boundary,
        Err(error) => {
            return (
                Disposition::Quarantined,
                format!("the recorded boundary could not be inspected: {error}"),
            );
        }
    };

    match record.phase {
        LaunchPhase::IntentFsynced => case_two(boundary.as_deref(), &identity),
        LaunchPhase::StagedFsynced => case_three(record, boundary.as_deref()),
        LaunchPhase::EffectStartedFsynced => case_four(record, boundary.as_deref()),
        LaunchPhase::CommittedFsynced => case_five(record, boundary.as_deref(), true),
    }
}

/// Case 2: the intent is already durable, so the boundary's identity is known and
/// its members are enumerated **directly**. Absence is never inferred from the
/// missing `staged_fsynced` mark.
fn case_two(
    boundary: Option<&dyn ContainmentBoundary>,
    identity: &BoundaryIdentity,
) -> (Disposition, String) {
    let Some(boundary) = boundary else {
        // The boundary object itself is gone. For a strong boundary that is proof
        // of emptiness (kill-on-close guarantees no member outlived it); for the
        // weak locator it means the private reservation was already reclaimed.
        return (
            Disposition::Compensated,
            format!(
                "{LAUNCH_ABANDONED_PRE_RELEASE}: the recorded boundary {} no longer exists, so it \
                 provably contains no member process",
                identity.identity
            ),
        );
    };
    match boundary.state() {
        Ok(ContainmentState::Empty) => {
            if let Err(error) = boundary.close() {
                return (
                    Disposition::Quarantined,
                    format!("the empty boundary could not be reclaimed: {error}"),
                );
            }
            (
                Disposition::Compensated,
                LAUNCH_ABANDONED_PRE_RELEASE.to_owned(),
            )
        }
        Ok(ContainmentState::Contained(members)) => {
            let count = members.len();
            if let Err(error) = boundary.terminate_all() {
                return (
                    Disposition::Quarantined,
                    format!(
                        "the boundary contained {count} orphaned member(s) that could not be \
                         terminated or proven gone: {error}"
                    ),
                );
            }
            if let Err(error) = boundary.close() {
                return (
                    Disposition::Quarantined,
                    format!("the emptied boundary could not be reclaimed: {error}"),
                );
            }
            (
                Disposition::Compensated,
                LAUNCH_ABANDONED_PRE_RELEASE.to_owned(),
            )
        }
        Ok(ContainmentState::Unprovable(reason)) | Err(BoundaryError::Enumerate(reason)) => (
            Disposition::Quarantined,
            format!("the boundary's contents could not be proven either way: {reason}"),
        ),
        Err(error) => (
            Disposition::Quarantined,
            format!("the boundary's contents could not be proven either way: {error}"),
        ),
    }
}

/// Case 3: the anchor's exact identity is durable. A live process is trusted as
/// that anchor only on an exact PID/start-time/PGID match (plus, on the weak path,
/// the authenticated endpoint/nonce credential). Reuse never counts.
fn case_three(
    record: &LaunchRecord,
    boundary: Option<&dyn ContainmentBoundary>,
) -> (Disposition, String) {
    let Some(boundary) = boundary else {
        return (
            Disposition::Compensated,
            format!(
                "{LAUNCH_ABANDONED_PRE_RELEASE}: the recorded boundary no longer exists, so the \
                 gated anchor provably no longer runs"
            ),
        );
    };
    let Some(anchor) = recorded_anchor(record) else {
        return (
            Disposition::Quarantined,
            "the staged record does not bind the anchor's PID/start-time identity, so no live \
             process can be matched against it"
                .to_owned(),
        );
    };

    let adopted = boundary.adopt_recorded_member(
        anchor.pid,
        anchor.start_time.as_deref(),
        anchor.process_group,
    );
    match adopted {
        Err(error) => (
            Disposition::Quarantined,
            format!(
                "a live process contradicts the durable anchor identity, so it is never trusted as \
                 the recorded anchor: {error}"
            ),
        ),
        Ok(None) => match terminate_and_prove_empty(boundary) {
            Ok(()) => (
                Disposition::Compensated,
                LAUNCH_ABANDONED_PRE_RELEASE.to_owned(),
            ),
            Err(reason) => (Disposition::Quarantined, reason),
        },
        Ok(Some(member)) => {
            // A live match may only be compensated when it is provably still
            // blocked at its barrier. Otherwise it might have run user code, and
            // this sub-case can never resolve to `compensated` or `committed` —
            // not even if the process is later found dead.
            if !provably_blocked(record, &member) {
                return (
                    Disposition::Quarantined,
                    format!(
                        "live process {} matches the recorded anchor but cannot be proven to have \
                         remained blocked at every observation point since creation",
                        member.pid
                    ),
                );
            }
            match terminate_recorded(boundary, &anchor) {
                Ok(()) => (
                    Disposition::Compensated,
                    LAUNCH_ABANDONED_PRE_RELEASE.to_owned(),
                ),
                Err(reason) => (Disposition::Quarantined, reason),
            }
        }
    }
}

/// Case 4: the receipt already proves containment for the pre-resume state, so the
/// **resume** state is what must be independently determined. There are exactly
/// three outcomes and never a fourth.
fn case_four(
    record: &LaunchRecord,
    boundary: Option<&dyn ContainmentBoundary>,
) -> (Disposition, String) {
    let resume = match recorded_gate(record) {
        Some(path) => gate::state(&path),
        None => GateState::Unknown("the receipt does not bind a gate artifact".to_owned()),
    };
    match resume {
        // (i) Resume provably never happened.
        GateState::Closed => {
            let Some(boundary) = boundary else {
                return (
                    Disposition::Compensated,
                    format!(
                        "{LAUNCH_ABANDONED_PRE_RELEASE}: the gate is provably still closed and the \
                         boundary no longer exists"
                    ),
                );
            };
            match terminate_and_prove_empty(boundary) {
                // A live-but-unresumed child is unconditionally terminated and never
                // adopted as a survivor: unresumed containment alone does not prove
                // the effect completed.
                Ok(()) => (
                    Disposition::Compensated,
                    format!(
                        "{LAUNCH_ABANDONED_PRE_RELEASE}: the gate is provably still closed, so \
                         resume never happened"
                    ),
                ),
                Err(reason) => (Disposition::Quarantined, reason),
            }
        }
        // (ii) Resume provably completed — fall through to case 5, which requires a
        // live contained survivor before it commits anything.
        GateState::Open => case_five(record, boundary, false),
        // (iii) Neither outcome is provable.
        GateState::Unknown(reason) => (
            Disposition::Quarantined,
            format!(
                "the release/resume history cannot be determined either way, and death alone never \
                 substitutes for a proven resume outcome: {reason}"
            ),
        ),
    }
}

/// Case 5: survivor reconciliation. A live, still-contained survivor matching its
/// receipt is adopted and committed; a survivor whose live-contained state or
/// resume proof cannot be independently confirmed is quarantined.
fn case_five(
    record: &LaunchRecord,
    boundary: Option<&dyn ContainmentBoundary>,
    resume_already_durable: bool,
) -> (Disposition, String) {
    let Some(child) = recorded_child(record) else {
        return (
            Disposition::Quarantined,
            "the receipt does not bind the supervised child's PID/start-time identity, so no \
             survivor can be matched against it"
                .to_owned(),
        );
    };
    let Some(boundary) = boundary else {
        return if resume_already_durable {
            (
                Disposition::Committed,
                "the durable receipt and committed mark already prove containment and resume, and \
                 the boundary is gone, so the survivor is confirmed dead"
                    .to_owned(),
            )
        } else {
            (
                Disposition::Quarantined,
                "the boundary is gone and the survivor's resume history cannot be independently \
                 confirmed; death alone is never treated as safe"
                    .to_owned(),
            )
        };
    };

    match boundary.adopt_recorded_member(
        child.pid,
        child.start_time.as_deref(),
        child.process_group,
    ) {
        Ok(Some(member)) => (
            Disposition::Committed,
            format!(
                "live contained survivor {} matches its receipt and its resume is independently \
                 proven, so containment and resume are proven together",
                member.pid
            ),
        ),
        Ok(None) => {
            if resume_already_durable {
                (
                    Disposition::Committed,
                    "the committed mark already proves resume completed, and the survivor is \
                     confirmed dead, so no further action is required"
                        .to_owned(),
                )
            } else {
                (
                    Disposition::Quarantined,
                    "the gate is open but the recorded child is gone, so it cannot be proven which \
                     side of the resume boundary the crash occurred on; death alone never proves a \
                     resume outcome"
                        .to_owned(),
                )
            }
        }
        Err(error) => (
            Disposition::Quarantined,
            format!("the survivor's live-contained state could not be confirmed: {error}"),
        ),
    }
}

/// Whether a live member is provably still blocked pre-release: it is observably
/// the gated barrier **and** its gate is provably still closed. Either
/// observation alone would be too weak.
fn provably_blocked(record: &LaunchRecord, member: &MemberObservation) -> bool {
    let gated = member.gated
        || procfs::process_identity(member.pid)
            .ok()
            .flatten()
            .is_some_and(|identity| identity.is_gated_barrier());
    gated
        && recorded_gate(record)
            .as_deref()
            .map(gate::state)
            .is_some_and(|state| state == GateState::Closed)
}

fn terminate_and_prove_empty(boundary: &dyn ContainmentBoundary) -> Result<(), String> {
    boundary.terminate_all().map_err(|error| {
        format!("the boundary could not be terminated or proven empty: {error}")
    })?;
    boundary
        .close()
        .map_err(|error| format!("the emptied boundary could not be reclaimed: {error}"))
}

fn terminate_recorded(
    boundary: &dyn ContainmentBoundary,
    anchor: &RecordedProcess,
) -> Result<(), String> {
    if boundary.strength() == BoundaryStrength::Weak
        && let Some(group) = anchor.process_group
    {
        boundary.terminate_recorded_group(group).map_err(|error| {
            format!("the recorded supervisor's process group could not be proven empty: {error}")
        })?;
    }
    terminate_and_prove_empty(boundary)
}

/// The gate artifact one durable record binds, exposed so a caller can report the
/// provable release state without re-deriving the path.
#[must_use]
pub fn recorded_gate_state(record: &LaunchRecord) -> GateState {
    match recorded_gate(record) {
        Some(path) => gate::state(Path::new(&path)),
        None => GateState::Unknown("the record does not bind a gate artifact".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn there_are_exactly_five_disjoint_cases_mapping_to_three_dispositions() {
        assert_eq!(ReconcileCase::ALL.len(), 5);
        assert_eq!(Disposition::ALL.len(), 3);
        let mut sorted = ReconcileCase::ALL;
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            ReconcileCase::ALL,
            "the cases are in ascending order"
        );
    }

    #[test]
    fn every_durable_phase_selects_exactly_one_case() {
        let selected: Vec<ReconcileCase> = LaunchPhase::ALL
            .iter()
            .copied()
            .map(ReconcileCase::of)
            .collect();
        assert_eq!(
            selected,
            vec![
                ReconcileCase::IntentWithoutStaged,
                ReconcileCase::StagedWithoutReceipt,
                ReconcileCase::ReceiptWithoutCommit,
                ReconcileCase::SurvivorReconciliation
            ]
        );
    }

    #[test]
    fn case_four_outcome_two_is_reported_as_survivor_reconciliation() {
        assert_eq!(
            effective_case(ReconcileCase::ReceiptWithoutCommit, Disposition::Committed),
            ReconcileCase::SurvivorReconciliation
        );
        assert_eq!(
            effective_case(
                ReconcileCase::ReceiptWithoutCommit,
                Disposition::Compensated
            ),
            ReconcileCase::ReceiptWithoutCommit
        );
        assert_eq!(
            effective_case(
                ReconcileCase::ReceiptWithoutCommit,
                Disposition::Quarantined
            ),
            ReconcileCase::ReceiptWithoutCommit
        );
    }

    #[test]
    fn a_quarantined_transaction_never_carries_a_proof() {
        let record = LaunchRecord {
            envelope: super::super::journal::TransactionEnvelope {
                transaction_id: "00000000-0000-4000-8000-000000000141".to_owned(),
                project: super::super::journal::ProjectIdentityClaim {
                    object_identity: format!("project_{}", "a".repeat(64)),
                    root_object: "unix:dev=1,ino=2".to_owned(),
                    file_object: "unix:dev=1,ino=3".to_owned(),
                    generation: 1,
                },
                transport_mode: super::super::journal::TransportMode::Stdio,
                principal: None,
                session: None,
                operation_family: super::super::journal::OperationFamily::RuntimeExecution,
                tool_name: "process.launch".to_owned(),
                idempotency_id: "idem".to_owned(),
                canonical_request_hash: "b".repeat(64),
                canonical_request_bytes: 1,
                opened_at_unix_ms: 1,
            },
            phase: LaunchPhase::IntentFsynced,
            payloads: Vec::new(),
            proof: None,
        };
        assert!(terminal_proof(&record, Disposition::Quarantined).is_none());
    }
}
