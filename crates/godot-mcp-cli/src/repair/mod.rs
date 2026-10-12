//! The local-only `transaction repair` operator command.
//!
//! This module implements exactly one interface:
//!
//! ```text
//! godot-mcp transaction repair --project <root> --transaction-id <id>
//!     --operation reconcile|compensate|terminate|acknowledge-safe-state
//!     --proof-file <path-to-proof.json> [--dry-run]
//! ```
//!
//! It is the **sole authorized caller** of Todo 6's operator-recovery authority
//! and its `RepairPermit`, reached through the one narrow
//! [`godot_mcp_server::operator`] port. There is no `--force` flag and no other
//! bypass: `--force` is not a recognized argument and produces a parse error.
//!
//! # Why it can never be reached over the network
//!
//! The refusal is structural, not a policy check:
//!
//! * this module is only ever called from `src/bin/godot-mcp.rs`'s `transaction
//!   repair` match arm, which is a `clap` subcommand and nothing else;
//! * the port it drives requires a *held* Todo 2 advisory OS file lock, which a
//!   network peer has no way to present; and
//! * nothing named here is registered in the MCP tool/resource registry or in any
//!   Todo 26 capability-profile table, and `tests/package_and_repair.rs` asserts
//!   that by scanning the sources.
//!
//! # The authorization order is normative
//!
//! 1. acquire the Todo 2 project-lease advisory OS file lock on the exact held
//!    root/project object identity, failing closed with `conflict` when another
//!    owner holds it;
//! 2. load the Todo 6 transaction record by `--transaction-id` and read its
//!    recorded `effect_kind`;
//! 3. verify the record is currently `quarantined` — any other status is refused
//!    with no state change, the single exception being
//!    `--operation acknowledge-safe-state` against a `process_launch`
//!    reconciliation left in Todo 14's case-4/5 ambiguous live-survivor state;
//! 4. read the `--proof-file` and require its `project_object_identity` and
//!    `project_generation` to match the currently held lease;
//! 5. **independently re-derive every proof field** for the record's own effect
//!    kind — descriptor-relative `SafeRoot` observation for `file`, live OS
//!    process/boundary inspection for `process_launch`, a bridge receipt and
//!    state-probe round trip for `editor_runtime` — never by trusting the proof
//!    file's claims;
//! 6. when the record carries `required_subproofs.process_launch_transaction_id`,
//!    unconditionally load that linked transaction and independently re-derive
//!    its clean-exit/empty-boundary terminal proof as well; and
//! 7. only then request a `RepairPermit` and journal exactly one transition
//!    through the same `EffectGateway` every other mutation uses.
//!
//! Any mismatch, any missing or null required field, a wrong project object or
//! generation, or any I/O error while re-deriving leaves the transaction
//! `quarantined`, writes no repair record and exits non-zero.

pub mod editor_kind;
pub mod file_kind;
pub mod linked;
pub mod process_kind;
pub mod supplied;

use std::path::{Path, PathBuf};

use godot_mcp_server::operator::{
    DerivedProof, ObservedProof, OperatorError, OperatorSession, ProofKind, ProvenDisposition,
    RepairTarget,
};
use godot_mcp_server::router::RepairIntent;

use supplied::{SuppliedBody, SuppliedError, SuppliedProof};

/// One complete, validated repair request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairRequest {
    /// The project root whose lease is taken and whose journal is read.
    pub project: PathBuf,
    /// The transaction to repair.
    pub transaction_id: String,
    /// Which proof-driven operation to apply.
    pub operation: RepairIntent,
    /// The operator-supplied proof file.
    pub proof_file: PathBuf,
    /// Re-derive and report without requesting a permit or writing anything.
    pub dry_run: bool,
}

/// Why a repair was refused. Every variant leaves the transaction unchanged.
#[derive(Debug, thiserror::Error)]
pub enum RepairError {
    /// The project lease, the journal or the recovery authority refused.
    #[error(transparent)]
    Operator(#[from] OperatorError),
    /// The proof file could not be used.
    #[error(transparent)]
    Supplied(#[from] SuppliedError),
    /// The proof file is for another transaction, kind, project or generation.
    #[error("the proof file does not bind to this repair: {0}")]
    WrongBinding(String),
    /// The transaction is not in a status this command may repair.
    #[error(
        "transaction {transaction_id} is {phase}, not quarantined; this command refuses it with no \
         state change"
    )]
    NotQuarantined {
        /// The transaction that was targeted.
        transaction_id: String,
        /// Its current durable phase.
        phase: String,
    },
    /// `terminate` was requested for an effect kind it can never apply to.
    #[error("terminate applies only to a process_launch transaction, not {0}")]
    NotTerminable(String),
    /// The re-derivation did not prove the state the operation requires.
    #[error("the state this operation requires is not proven: {0}")]
    Unproven(String),
    /// A live observation could not be made at all.
    #[error("a required live observation failed: {0}")]
    Observation(String),
}

/// A live survivor the re-derivation proved is still contained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSurvivor {
    /// The observed PID.
    pub pid: u32,
    /// The observed platform-native start time.
    pub start_time: String,
}

/// One successful re-derivation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proven {
    /// Which recorded state was proven live.
    pub disposition: ProvenDisposition,
    /// What was observed, in the record's own typed form.
    pub observed: ObservedProof,
    /// A diagnosable account of what matched.
    pub detail: String,
    /// The still-contained survivor, when one was proven live.
    pub live_survivor: Option<LiveSurvivor>,
}

/// The result of one kind's re-derivation and comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The recorded state was independently proven.
    Proven(Proven),
    /// It was not. The reason is deliberately specific: this is the operator's
    /// own context, so a diagnosable refusal is correct here.
    Refused(String),
}

/// What one repair invocation did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairReport {
    /// The transaction that was addressed.
    pub transaction_id: String,
    /// The record's recorded effect kind.
    pub effect_kind: &'static str,
    /// The requested operation.
    pub operation: &'static str,
    /// The proven disposition, when the re-derivation proved one.
    pub disposition: Option<&'static str>,
    /// The durable phase before the repair.
    pub phase_before: String,
    /// The durable phase after the repair.
    pub phase_after: String,
    /// Whether a journal transition was written.
    pub transitioned: bool,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// A diagnosable account of what was re-derived.
    pub detail: String,
}

impl RepairReport {
    /// Render the report as stable, line-oriented text.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut lines = vec![
            format!("transaction {}", self.transaction_id),
            format!("effect-kind {}", self.effect_kind),
            format!("operation {}", self.operation),
            format!("proven-disposition {}", self.disposition.unwrap_or("none")),
            format!("phase-before {}", self.phase_before),
            format!("phase-after {}", self.phase_after),
            format!(
                "transition {}",
                if self.transitioned { "written" } else { "none" }
            ),
        ];
        if self.dry_run {
            lines.push("dry-run yes".to_owned());
        }
        lines.push(format!("detail {}", self.detail));
        lines.join("\n")
    }
}

/// Run one repair, end to end, in the normative authorization order.
///
/// # Errors
/// Returns the first [`RepairError`] any gate reports. In every error case no
/// repair record was written and the transaction's durable phase is unchanged.
pub fn run(request: &RepairRequest) -> Result<RepairReport, RepairError> {
    // Every descriptor-relative re-derivation walks a path from the filesystem
    // root, so the project spelling is made absolute once, here. `absolute` is
    // used rather than `canonicalize` on purpose: it never resolves a symlink, so
    // a symlinked component still reaches Todo 2's own symlink refusal instead of
    // being silently rewritten into its target.
    let project = std::path::absolute(&request.project).map_err(|error| {
        RepairError::Observation(format!(
            "the project path {} could not be resolved: {error}",
            request.project.display()
        ))
    })?;
    let session = OperatorSession::open(&project)?;
    let target = session.load(&request.transaction_id)?;

    let repairable = target.quarantined
        || (target.ambiguous_survivor && request.operation == RepairIntent::AcknowledgeSafeState);
    if !repairable {
        return Err(RepairError::NotQuarantined {
            transaction_id: target.transaction_id.clone(),
            phase: target.phase.clone(),
        });
    }
    if request.operation == RepairIntent::Terminate
        && target.effect_kind != ProofKind::ProcessLaunch
    {
        return Err(RepairError::NotTerminable(
            target.effect_kind.as_str().to_owned(),
        ));
    }

    let supplied = supplied::load(&request.proof_file)?;
    check_binding(&session, &target, &supplied)?;

    let verdict = derive(&session, &project, &target, &supplied)?;
    let verdict = linked::narrow(&session, &target, verdict)?;

    let proven = match verdict {
        Verdict::Proven(proven) => proven,
        Verdict::Refused(detail) => {
            if request.operation == RepairIntent::Reconcile {
                // `reconcile` is report-only and always safe to run: it records
                // nothing, so an unproven state is the report, not a failure.
                return Ok(report(request, &target, None, &target.phase, false, detail));
            }
            return Err(RepairError::Unproven(detail));
        }
    };
    require_disposition_matches_operation(request.operation, proven.disposition)?;

    if request.dry_run {
        return Ok(report(
            request,
            &target,
            Some(proven.disposition),
            &target.phase,
            false,
            format!(
                "dry run: would transition to {} — {}",
                would_be_phase(request.operation),
                proven.detail
            ),
        ));
    }

    let survivor = proven.live_survivor.clone();
    let prelude: Option<Box<dyn Fn() -> Result<(), String>>> =
        if request.operation == RepairIntent::Terminate {
            let survivor = survivor.ok_or_else(|| {
                RepairError::Unproven(
                    "terminate requires a live, still-contained survivor and none was proven"
                        .to_owned(),
                )
            })?;
            Some(Box::new(move || process_kind::terminate(&survivor)))
        } else {
            None
        };

    let derived = DerivedProof {
        disposition: proven.disposition,
        observed: proven.observed.clone(),
    };
    let outcome = session.apply(
        &request.transaction_id,
        request.operation,
        Some(derived),
        prelude.as_deref(),
    )?;
    let phase_after = session.phase_of(&request.transaction_id)?;
    let transitioned = phase_after != target.phase;
    Ok(report(
        request,
        &target,
        Some(proven.disposition),
        &phase_after,
        transitioned,
        format!("{} — {}", outcome.reason, proven.detail),
    ))
}

fn report(
    request: &RepairRequest,
    target: &RepairTarget,
    disposition: Option<ProvenDisposition>,
    phase_after: &str,
    transitioned: bool,
    detail: String,
) -> RepairReport {
    RepairReport {
        transaction_id: target.transaction_id.clone(),
        effect_kind: target.effect_kind.as_str(),
        operation: request.operation.as_str(),
        disposition: disposition.map(ProvenDisposition::as_str),
        phase_before: target.phase.clone(),
        phase_after: phase_after.to_owned(),
        transitioned,
        dry_run: request.dry_run,
        detail,
    }
}

const fn would_be_phase(operation: RepairIntent) -> &'static str {
    match operation {
        RepairIntent::Reconcile => "quarantined (no transition)",
        RepairIntent::Compensate | RepairIntent::Terminate => "compensated",
        RepairIntent::AcknowledgeSafeState => "committed_fsynced",
    }
}

/// Exactly one disposition satisfies each operation. A `post-state` proof can
/// never compensate, and a `pre-state` proof can never acknowledge a safe state.
fn require_disposition_matches_operation(
    operation: RepairIntent,
    disposition: ProvenDisposition,
) -> Result<(), RepairError> {
    let required = match operation {
        RepairIntent::Reconcile => return Ok(()),
        RepairIntent::Compensate | RepairIntent::Terminate => ProvenDisposition::PreState,
        RepairIntent::AcknowledgeSafeState => ProvenDisposition::PostState,
    };
    if disposition == required {
        return Ok(());
    }
    Err(RepairError::Unproven(format!(
        "{} requires a {} proof, and the re-derivation proved a {} state",
        operation.as_str(),
        required.as_str(),
        disposition.as_str()
    )))
}

/// The proof file must bind to *this* repair: this transaction, this recorded
/// effect kind, and the project object identity and generation the held lease
/// currently designates.
fn check_binding(
    session: &OperatorSession,
    target: &RepairTarget,
    supplied: &SuppliedProof,
) -> Result<(), RepairError> {
    if supplied.header.transaction_id != target.transaction_id {
        return Err(RepairError::WrongBinding(format!(
            "it names transaction {}, not {}",
            supplied.header.transaction_id, target.transaction_id
        )));
    }
    if supplied.header.effect_kind != target.effect_kind {
        return Err(RepairError::WrongBinding(format!(
            "it is a {} proof, and transaction {} recorded effect kind {}",
            supplied.header.effect_kind.as_str(),
            target.transaction_id,
            target.effect_kind.as_str()
        )));
    }
    if supplied.header.project_object_identity != session.project_object_identity() {
        return Err(RepairError::WrongBinding(format!(
            "it names project object {}, and the held lease designates {}",
            supplied.header.project_object_identity,
            session.project_object_identity()
        )));
    }
    if supplied.header.project_generation != session.project_generation() {
        return Err(RepairError::WrongBinding(format!(
            "it names project generation {}, and the held lease designates {}",
            supplied.header.project_generation,
            session.project_generation()
        )));
    }
    if target.project_object_identity != session.project_object_identity()
        || target.project_generation != session.project_generation()
    {
        return Err(RepairError::WrongBinding(format!(
            "transaction {} was recorded against project object {}/generation {}, and the held \
             lease designates {}/{}",
            target.transaction_id,
            target.project_object_identity,
            target.project_generation,
            session.project_object_identity(),
            session.project_generation()
        )));
    }
    Ok(())
}

/// Dispatch to the one re-derivation the record's own recorded effect kind
/// requires. The proof file's claimed kind was already required to agree.
fn derive(
    session: &OperatorSession,
    project: &Path,
    target: &RepairTarget,
    supplied: &SuppliedProof,
) -> Result<Verdict, RepairError> {
    match (&supplied.body, target.effect_kind) {
        (SuppliedBody::File(body), ProofKind::File) => {
            file_kind::derive(session, project, target, &supplied.header, body)
        }
        (SuppliedBody::ProcessLaunch(body), ProofKind::ProcessLaunch) => {
            process_kind::derive(target, &supplied.header, body)
        }
        (SuppliedBody::EditorRuntime(body), ProofKind::EditorRuntime) => {
            editor_kind::derive(session, project, target, &supplied.header, body)
        }
        _ => Err(RepairError::WrongBinding(
            "the proof file's variant does not match the recorded effect kind".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A proof file can only settle the operation whose disposition it proves,
    /// which is what stops a pre-state proof from committing anything.
    #[test]
    fn each_operation_accepts_exactly_one_disposition() {
        assert!(
            require_disposition_matches_operation(
                RepairIntent::Compensate,
                ProvenDisposition::PreState
            )
            .is_ok()
        );
        assert!(
            require_disposition_matches_operation(
                RepairIntent::Terminate,
                ProvenDisposition::PreState
            )
            .is_ok()
        );
        assert!(
            require_disposition_matches_operation(
                RepairIntent::AcknowledgeSafeState,
                ProvenDisposition::PostState
            )
            .is_ok()
        );
        assert!(
            require_disposition_matches_operation(
                RepairIntent::Compensate,
                ProvenDisposition::PostState
            )
            .is_err()
        );
        assert!(
            require_disposition_matches_operation(
                RepairIntent::AcknowledgeSafeState,
                ProvenDisposition::PreState
            )
            .is_err()
        );
        for disposition in [ProvenDisposition::PreState, ProvenDisposition::PostState] {
            assert!(
                require_disposition_matches_operation(RepairIntent::Reconcile, disposition).is_ok(),
                "reconcile records nothing, so it accepts either report"
            );
        }
    }

    /// Only the two documented transitions are reachable, plus the report-only
    /// `reconcile`. A fourth target phase would mean a transition nobody proved.
    #[test]
    fn only_three_would_be_phases_exist() {
        let mut phases: Vec<&str> = RepairIntent::ALL
            .iter()
            .map(|intent| would_be_phase(*intent))
            .collect();
        phases.sort_unstable();
        phases.dedup();
        assert_eq!(
            phases,
            vec![
                "committed_fsynced",
                "compensated",
                "quarantined (no transition)"
            ]
        );
    }
}
