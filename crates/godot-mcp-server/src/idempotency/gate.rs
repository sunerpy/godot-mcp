//! The idempotency gate over the *existing* Todo 6 journal.
//!
//! This is the only place that decides whether a reused idempotency identifier
//! replays or conflicts, and it decides it by reading Todo 6's
//! [`TransactionStore`](crate::journal::TransactionStore). It creates no second
//! journal, no competing schema and no in-memory ledger: the durable record of a
//! mutation already exists, and a parallel bookkeeping structure would be a second
//! source of truth that could disagree with the one that survives a crash.
//!
//! ## The comparison is total, and its answer is one bit
//!
//! [`IdempotencyGate::decide`] compares all eight tuple elements against the
//! journaled record. It returns the caller's own recorded status/result only for an
//! exact match; anything else returns the single byte-stable
//! [`ConflictEnvelope`](super::ConflictEnvelope). The gate never reports which
//! element differed, never reports whether a record existed at all, and never
//! varies its refusal by cause — those are all enumeration oracles over other
//! contexts' work.
//!
//! ## An unreadable journal is a refusal
//!
//! If the store cannot be read the gate returns `unavailable`. It does not fall
//! back to "assume this identifier is new", because that assumption is exactly how
//! a duplicate effect gets authorized.

use std::collections::BTreeMap;
use std::sync::Arc;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;

use super::identity::{
    ConnectionSession, IdempotencyId, ModePrincipal, MutationIdentity, ProjectBinding,
    ProjectObjectIdentity, RequestHash, TransportAuthMode, TransportBinding, family_of,
};
use super::replay::{
    RecordedMutation, RecordedStatus, RecordedVerdict, ReplayDecision, conflict_envelope,
};
use crate::journal::{
    Phase, StoreError, TerminalOutcome, TransactionRecord, TransactionStore, TransportMode,
};

/// The existing Todo 6 read this gate answers from. Naming the signature pins the
/// dependency direction: identity is *read out of* the durable records, never
/// tracked beside them.
type IdentityLookup = fn(&TransactionStore, &str) -> Result<Vec<TransactionRecord>, StoreError>;

const IDENTITY_LOOKUP: IdentityLookup = TransactionStore::transactions_for_idempotency_id;

/// The cross-plane idempotency gate bound to one held project's journal.
pub(crate) struct IdempotencyGate {
    store: Arc<TransactionStore>,
    project_object_identity: String,
    project_generation: u64,
}

impl IdempotencyGate {
    /// Bind the gate to the already-open Todo 6 store for one held project.
    pub(crate) fn new(
        store: Arc<TransactionStore>,
        project_object_identity: impl Into<String>,
        project_generation: u64,
    ) -> Self {
        Self {
            store,
            project_object_identity: project_object_identity.into(),
            project_generation,
        }
    }

    /// The held project's object identity, as the journal records it.
    pub(crate) fn project_object_identity(&self) -> &str {
        &self.project_object_identity
    }

    /// The held project's generation.
    pub(crate) const fn project_generation(&self) -> u64 {
        self.project_generation
    }

    /// Decide whether the presented identity replays, conflicts, or is new.
    ///
    /// # Errors
    /// Returns `unavailable` when the durable journal cannot be read or an exact
    /// replay lacks a typed terminal outcome. Never assumes the identifier is
    /// unused or infers a verdict from the human-readable terminal reason.
    pub(crate) fn decide(
        &self,
        identity: &MutationIdentity,
    ) -> Result<ReplayDecision, OutcomeError> {
        let candidates = IDENTITY_LOOKUP(self.store.as_ref(), identity.idempotency_id().as_str())
            .map_err(store_unavailable)?;
        if candidates.is_empty() {
            return Ok(ReplayDecision::Fresh);
        }
        for record in &candidates {
            if journaled_identity(record)
                .is_some_and(|journaled| identity.matches_exactly(&journaled))
            {
                return recorded_of(record).map(ReplayDecision::RecordedResult);
            }
        }
        // Every non-exact match lands here and produces the same bytes. The loop
        // deliberately never records *why* a candidate failed, so there is no
        // cause for a later change to accidentally surface.
        Ok(ReplayDecision::Conflict(conflict_envelope()))
    }

    /// Read one journaled mutation's status/result, for the context that owns it.
    ///
    /// # Errors
    /// Returns `unavailable` when the durable journal cannot be read or the
    /// record predates typed terminal outcomes.
    pub(crate) fn recorded(&self, transaction_id: &str) -> Result<RecordedMutation, OutcomeError> {
        let record = self.store.load(transaction_id).map_err(store_unavailable)?;
        recorded_of(&record)
    }
}

/// Rebuild the eight-element identity a journaled record represents.
///
/// A record with no recorded principal or session cannot be an exact same-context
/// match for any request — every request carries both — so it yields `None` and
/// falls through to the conflict path rather than matching loosely.
fn journaled_identity(record: &TransactionRecord) -> Option<MutationIdentity> {
    let mode = match record.transport_mode {
        TransportMode::Stdio => TransportAuthMode::Stdio,
        TransportMode::TrustedLocal => TransportAuthMode::TrustedLocal,
        TransportMode::ProxiedOidc => TransportAuthMode::ProxiedOidc,
    };
    let principal = ModePrincipal::from_journal_string(mode, record.principal.as_deref()?).ok()?;
    let transport = TransportBinding::new(
        mode,
        principal,
        ConnectionSession::new(record.session.clone()?).ok()?,
    )
    .ok()?;
    Some(MutationIdentity::new(
        ProjectBinding::new(
            ProjectObjectIdentity::new(record.project_object_identity.clone()).ok()?,
            record.project_generation,
        ),
        transport,
        family_of(&record.tool_name).ok()?,
        IdempotencyId::new(record.idempotency_id.clone()).ok()?,
        RequestHash::parse(record.canonical_request_hash.clone()).ok()?,
    ))
}

/// Project one durable phase onto the recorded status the owning caller sees.
///
/// `effect_started_fsynced` becomes `dispatched`, never `not_started`: the
/// boundary was already touched, so "no answer came back" is not "it did not
/// happen".
fn recorded_of(record: &TransactionRecord) -> Result<RecordedMutation, OutcomeError> {
    let status = match record.phase {
        Phase::IntentFsynced | Phase::StagedFsynced => RecordedStatus::NotStarted,
        Phase::EffectStartedFsynced => RecordedStatus::Dispatched,
        Phase::CommittedFsynced => RecordedStatus::Committed,
        Phase::Compensated => RecordedStatus::Compensated,
        Phase::Quarantined => RecordedStatus::Unresolved,
    };
    let result = record.terminal_reason.as_ref().map(|reason| {
        serde_json::json!({
            "reason": reason,
            "status": status.as_str(),
            "transaction_id": record.transaction_id,
        })
    });
    let verdict = match record.terminal_outcome {
        Some(TerminalOutcome::Succeeded) => RecordedVerdict::Succeeded,
        Some(TerminalOutcome::Failed { code }) => RecordedVerdict::Failed(code),
        None => {
            return Err(OutcomeError::new(
                ErrorKind::Unavailable,
                "the durable transaction record has no typed terminal outcome",
                false,
                BTreeMap::new(),
            ));
        }
    };
    Ok(RecordedMutation::new(
        record.transaction_id.clone(),
        status,
        result,
        verdict,
    ))
}

fn store_unavailable(error: StoreError) -> OutcomeError {
    OutcomeError::new(
        ErrorKind::Unavailable,
        format!("the durable transaction journal could not be read: {error}"),
        false,
        BTreeMap::new(),
    )
}

#[cfg(test)]
mod tests {
    use super::recorded_of;
    use crate::journal::{
        EffectBoundaryKind, EffectKind, OperationFamily, Phase, TransactionRecord, TransportMode,
    };

    #[test]
    fn legacy_terminal_record_is_not_guessed_during_exact_replay() {
        let record = TransactionRecord {
            schema_version: "1".to_owned(),
            transaction_id: "00000000-0000-4000-8000-000000004302".to_owned(),
            effect_kind: EffectKind::ProcessLaunch,
            effect_boundary: EffectBoundaryKind::Process,
            project_object_identity: format!("project_{}", "a".repeat(64)),
            project_root_object: "unix:dev=1,ino=2".to_owned(),
            project_file_object: "unix:dev=1,ino=3".to_owned(),
            project_generation: 1,
            transport_mode: TransportMode::Stdio,
            principal: Some("local-user".to_owned()),
            session: Some("legacy-session".to_owned()),
            operation_family: OperationFamily::RuntimeExecution,
            tool_name: "project.run".to_owned(),
            idempotency_id: "legacy-timeout-replay".to_owned(),
            canonical_request_hash: "b".repeat(64),
            canonical_request_bytes: 12,
            touched_paths: Vec::new(),
            touched_object_identities: Vec::new(),
            pre_hashes: Vec::new(),
            staged_write_set: Vec::new(),
            intended_post_hashes: Vec::new(),
            phase: Phase::Compensated,
            phase_history: Vec::new(),
            proof: None,
            required_subproofs: None,
            terminal_reason: Some("operation timed out".to_owned()),
            terminal_outcome: None,
            opened_at_unix_ms: 1_700_000_000_000,
        };

        let error = recorded_of(&record)
            .expect_err("a legacy terminal reason must not be parsed into a replay verdict");

        assert_eq!(error.code(), "unavailable");
        assert_eq!(
            error.message(),
            "the durable transaction record has no typed terminal outcome"
        );
    }
}
