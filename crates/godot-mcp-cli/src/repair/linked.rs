//! Todo 17's composite requirement: the conditional linked-transaction loader.
//!
//! A Todo 17 canonical text-resource/UID write is journaled as a single
//! `file`-kind transaction that carries durable metadata
//! `required_subproofs.process_launch_transaction_id` naming the supervised Godot
//! helper's own `process_launch` transaction. It is a composite of the two
//! already-defined proof forms, never a fourth kind.
//!
//! Whenever that metadata is present, this module **unconditionally** loads the
//! linked transaction and independently re-derives its terminal proof. Three
//! properties are normative:
//!
//! * matching file bytes alone are never sufficient. The file transaction can
//!   reach a proven post-state, or be compensated from a proven pre-state, only
//!   when the re-derived file proof **and** the linked transaction's re-derived
//!   clean-exit/empty-boundary terminal proof independently match;
//! * the supplied `--proof-file` can never substitute for, suppress or override
//!   this re-derivation. Nothing in [`narrow`] reads the proof file at all; it is
//!   driven purely by the journaled metadata and live OS observation; and
//! * a missing, mismatched, still-live, signalled, nonzero-exit or
//!   non-empty-boundary linked proof is treated as a **mismatch**, leaving the
//!   file transaction `quarantined`.
//!
//! An unconfirmed helper run can never be trusted to have produced the bytes on
//! disk deterministically, which is exactly why a matching hash proves nothing
//! here.
//!
//! # Every refusal is enumerated
//!
//! Following Todo 17's own practice, [`refusals`] returns the complete list of
//! conditions that failed rather than a blanket "the linked proof is invalid", so
//! an operator can see which of the six distinct composite failures they are
//! looking at.

use godot_mcp_server::operator::{
    ObservedProof, ObservedRelease, OperatorError, OperatorSession, ProofKind, RepairTarget,
};

use super::{RepairError, Verdict, process_kind};

/// Apply the conditional composite gate to one already-computed verdict.
///
/// A transaction with no `required_subproofs.process_launch_transaction_id` is
/// returned unchanged. A transaction that carries one is narrowed: the linked
/// terminal proof is re-derived and, unless it independently matches, the verdict
/// becomes [`Verdict::Refused`] no matter what the file proof said.
///
/// # Errors
/// Returns [`RepairError::Observation`] when the live OS observation the linked
/// re-derivation needs cannot be made at all. The transaction is left unchanged.
pub fn narrow(
    session: &OperatorSession,
    target: &RepairTarget,
    verdict: Verdict,
) -> Result<Verdict, RepairError> {
    let Some(linked_id) = target.required_process_transaction.as_deref() else {
        return Ok(verdict);
    };
    let refusals = refusals(session, target, linked_id)?;
    if refusals.is_empty() {
        return Ok(verdict);
    }
    let file_proof = match &verdict {
        Verdict::Proven(proven) => format!(
            "the file proof matches the recorded {} state",
            proven.disposition.as_str()
        ),
        Verdict::Refused(reason) => format!("the file proof itself is refused: {reason}"),
    };
    Ok(Verdict::Refused(format!(
        "transaction {} is a Todo 17 composite requiring linked process_launch transaction \
         {linked_id}; {file_proof}, but the linked terminal proof does not independently confirm a \
         contained, clean helper run: {}",
        target.transaction_id,
        refusals.join("; ")
    )))
}

/// Every condition the linked terminal proof failed, in a stable order.
///
/// An empty list means the linked helper's run was independently re-derived as
/// contained, released cleanly, exited 0 with no signal, and left a boundary this
/// command proved empty by live inspection.
///
/// # Errors
/// Returns [`RepairError::Observation`] when the live observation cannot be made.
pub fn refusals(
    session: &OperatorSession,
    target: &RepairTarget,
    linked_id: &str,
) -> Result<Vec<String>, RepairError> {
    let linked = match session.load(linked_id) {
        Ok(linked) => linked,
        Err(OperatorError::NotFound(_)) => {
            return Ok(vec![format!(
                "the linked transaction {linked_id} is not journaled for this project, so its \
                 terminal proof is missing"
            )]);
        }
        Err(error) => return Err(RepairError::Operator(error)),
    };

    if linked.transaction_id == target.transaction_id {
        return Ok(vec![format!(
            "the linked transaction {linked_id} is the composite itself, so it can never be an \
             independent sub-proof"
        )]);
    }
    if linked.project_object_identity != session.project_object_identity()
        || linked.project_generation != session.project_generation()
    {
        return Ok(vec![format!(
            "the linked transaction {linked_id} was recorded against project object {}/generation \
             {}, and the held lease designates {}/{}",
            linked.project_object_identity,
            linked.project_generation,
            session.project_object_identity(),
            session.project_generation()
        )]);
    }
    if linked.effect_kind != ProofKind::ProcessLaunch {
        return Ok(vec![format!(
            "the linked transaction {linked_id} records effect kind {}, not process_launch, so it \
             names another transaction than the supervised helper's launch",
            linked.effect_kind.as_str()
        )]);
    }
    let Some(recorded) = linked.recorded_proof.clone() else {
        return Ok(vec![format!(
            "the linked transaction {linked_id} records no observed process_launch proof at all, \
             so the helper's contained run was never proven"
        )]);
    };
    let ObservedProof::ProcessLaunch {
        boundary_identity,
        pid,
        start_time,
        boundary_membership_confirmed,
        release_status,
        exit_code,
        signal,
        boundary_empty_after_exit,
    } = recorded.clone()
    else {
        return Ok(vec![format!(
            "the linked transaction {linked_id} records a {} proof, which can never prove a \
             supervised helper's contained, terminal run",
            recorded.kind().as_str()
        )]);
    };
    let (Some(pid), Some(start_time)) = (pid, start_time) else {
        return Ok(vec![format!(
            "the linked transaction {linked_id} records no PID/start-time pair, so no live \
             observation can ever be matched to the helper it launched"
        )]);
    };

    let observation = process_kind::observe(&boundary_identity, pid, &start_time)?;
    let mut refusals = Vec::new();
    if !boundary_membership_confirmed {
        refusals.push(format!(
            "the linked transaction {linked_id} recorded unconfirmed boundary membership, so its \
             helper can never be trusted to have run inside {boundary_identity}"
        ));
    }
    if !observation.membership_confirmed {
        refusals.push(format!(
            "pid {pid} (start time {start_time}) is not a recorded member of the linked helper's \
             boundary {boundary_identity}"
        ));
    }
    if observation.live {
        refusals.push(format!(
            "the linked helper (pid {pid}, start time {start_time}) is still live, and a live \
             survivor is never accepted as terminal proof"
        ));
    }
    if !observation.boundary.is_empty_now() {
        refusals.push(format!(
            "the linked helper's boundary {boundary_identity} still holds {} live \
             descendant/member process(es) after exit",
            observation.boundary.live_members.len()
        ));
    }
    if release_status != ObservedRelease::ExitedClean {
        refusals.push(format!(
            "the linked helper's recorded release status is {}, not exited-clean",
            release_status.as_str()
        ));
    }
    if let Some(signal) = &signal {
        refusals.push(format!(
            "the linked helper was terminated by {signal}, so it never ran to completion"
        ));
    }
    match exit_code {
        Some(0) => {}
        Some(code) => refusals.push(format!("the linked helper exited with code {code}, not 0")),
        None => refusals.push(
            "the linked helper records no exit code, so its completion was never observed"
                .to_owned(),
        ),
    }
    if boundary_empty_after_exit != Some(true) {
        refusals.push(format!(
            "the linked helper's recorded post-exit boundary emptiness is {boundary_empty_after_exit:?}, \
             not a proven-empty boundary"
        ));
    }
    // The record's own terminal-completion rule is Todo 6's, and it is applied as
    // a whole in addition to the itemised conditions above, so a future field
    // added to that rule can never be silently omitted here.
    if refusals.is_empty() && !recorded.is_clean_terminal_process() {
        refusals.push(format!(
            "the linked transaction {linked_id}'s recorded proof does not satisfy the journal's \
             own terminal-completion rule"
        ));
    }
    Ok(refusals)
}

#[cfg(test)]
mod tests {
    use godot_mcp_server::operator::{ObservedProof, ObservedRelease};

    /// The journal's terminal-completion rule is the authority for the recorded
    /// half of the composite gate, so each of the six composite failure shapes
    /// must fail it while the accepted shape passes.
    #[test]
    fn each_recorded_composite_failure_shape_fails_the_terminal_rule() {
        let clean =
            |release, exit_code, signal: Option<&str>, empty| ObservedProof::ProcessLaunch {
                boundary_identity: "unix_locator:/tmp/boundary".to_owned(),
                pid: Some(4242),
                start_time: Some("12345".to_owned()),
                boundary_membership_confirmed: true,
                release_status: release,
                exit_code,
                signal: signal.map(str::to_owned),
                boundary_empty_after_exit: empty,
            };
        assert!(
            clean(ObservedRelease::ExitedClean, Some(0), None, Some(true))
                .is_clean_terminal_process()
        );
        for refusal in [
            clean(ObservedRelease::ResumedContained, None, None, Some(false)),
            clean(ObservedRelease::Signaled, None, Some("SIGKILL"), Some(true)),
            clean(ObservedRelease::NonzeroExit, Some(7), None, Some(true)),
            clean(ObservedRelease::ExitedClean, Some(0), None, Some(false)),
            clean(ObservedRelease::ExitedClean, Some(0), None, None),
            clean(ObservedRelease::Unknown, None, None, None),
        ] {
            assert!(
                !refusal.is_clean_terminal_process(),
                "{refusal:?} must never be accepted as a clean terminal helper run"
            );
        }
    }
}
