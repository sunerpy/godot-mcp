//! Mutation outcomes: three states, and what an `unresolved` one obliges.
//!
//! # Why three states and not two
//!
//! A live-editor mutation crosses a boundary this process does not control. The
//! plugin runs inside the editor, applies the change on the main thread through
//! `EditorUndoRedoManager`, and then answers. Between "we sent the request" and
//! "we read the answer" the editor can crash, the socket can drop, or the whole
//! machine can lose power. So there are exactly three honest answers:
//!
//! | status | meaning | who may act on it |
//! | --- | --- | --- |
//! | [`MutationStatus::NotStarted`] | proven that no editor state changed | the caller may safely retry |
//! | [`MutationStatus::Committed`] | proven durable, with a bridge receipt | the caller may report success |
//! | [`MutationStatus::Unresolved`] | **not decidable from here** | nobody; it goes to Todo 6 |
//!
//! There is deliberately no fourth. In particular there is no "probably
//! succeeded" and no "assume failed": Todo 14's lesson was that the *absence* of a
//! durability mark written after a risky action cannot prove the action did not
//! happen. So [`MutationStatus::ALL`] has three members and the canonical schema
//! enumerates the same three.
//!
//! # An `unresolved` handoff goes to Todo 6, and only to Todo 6
//!
//! [`QuarantineHandoff::to_canonical_value`] renders a record in **Todo 6's own
//! canonical shape**, validated against `schemas/transactions/v1.json` — the same
//! file Todo 6 and Todo 14 embed, not a copy — with:
//!
//! * `effect_kind: "editor_runtime"`, one of Todo 6's three frozen effect kinds;
//! * `phase: "quarantined"`, one of Todo 6's three terminal dispositions;
//! * an `editor_runtime` typed proof carrying `bridge_reachable`, the receipt
//!   transaction/action IDs and the live-state probe, exactly the four members
//!   Todo 6 froze for this proof kind.
//!
//! This crate does not depend on `godot-mcp-server` (the architecture boundary
//! forbids it), so the record is *handed* to the server layer through
//! [`QuarantineSink`], the same port shape Todo 14 uses for
//! `LaunchJournal`. There is no second store, no bridge-local journal and no
//! alternative schema.
//!
//! # A reconnect is not permission to replay
//!
//! [`MutationLedger::reconnect`] returns the mutations a fresh session may
//! resubmit, and it returns **only** those still in [`MutationStatus::NotStarted`]
//! that were never dispatched. Anything unresolved is in
//! [`MutationLedger::quarantined`] instead, permanently, for this ledger's
//! lifetime. Reconnecting to the same port proves the port is reachable; it proves
//! nothing about what the previous editor process did or did not commit.

use std::collections::BTreeMap;

use godot_mcp_engine::supervision::journal::{
    JournalError, TransactionEnvelope, hex_sha256, validate_canonical,
};
use serde_json::{Value, json};

use super::envelope::OperationClass;

/// The three mutation outcomes, and no fourth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MutationStatus {
    /// Proven that no editor state changed. Safe to retry.
    NotStarted,
    /// Proven durable, with a bridge receipt. Report success.
    Committed,
    /// Not decidable from here. Enters Todo 6 reconciliation/quarantine and is
    /// never replayed.
    Unresolved,
}

impl MutationStatus {
    /// Every status. There are three, permanently.
    pub const ALL: [Self; 3] = [Self::NotStarted, Self::Committed, Self::Unresolved];

    /// Canonical wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Committed => "committed",
            Self::Unresolved => "unresolved",
        }
    }

    /// Parse the canonical wire spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|status| status.as_str() == text)
    }

    /// Whether a caller may safely resubmit work in this state.
    ///
    /// True only for [`MutationStatus::NotStarted`]. An unresolved mutation is
    /// never replayable, and a committed one needs no replay.
    #[must_use]
    pub const fn is_replayable(self) -> bool {
        matches!(self, Self::NotStarted)
    }

    /// Whether this state obliges a Todo 6 quarantine handoff.
    #[must_use]
    pub const fn requires_quarantine(self) -> bool {
        matches!(self, Self::Unresolved)
    }
}

impl std::fmt::Display for MutationStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Why a ledger operation was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MutationOutcome {
    /// A request ID already in this ledger. Never silently coalesced: an
    /// ambiguous second attempt under one ID is exactly how a double-apply
    /// happens.
    #[error("mutation request ID {0} is already open or settled in this session")]
    DuplicateRequestId(String),

    /// An idempotency ID already in this ledger under a different request ID.
    #[error("idempotency ID {0} is already bound to a different mutation request")]
    DuplicateIdempotencyId(String),

    /// A request ID no ledger entry matches.
    #[error("mutation request ID {0} is not open in this session")]
    UnknownRequestId(String),

    /// An attempt to settle an already-terminal mutation.
    #[error("mutation {request_id} is already settled as {settled}")]
    AlreadySettled {
        /// Which mutation.
        request_id: String,
        /// The terminal state it already holds.
        settled: MutationStatus,
    },

    /// A non-mutation operation class was offered to the mutation ledger.
    #[error("operation class {0} is not a mutation")]
    NotAMutation(&'static str),
}

/// One tracked mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationRecord {
    request_id: String,
    idempotency_id: String,
    status: MutationStatus,
    dispatched: bool,
    action_id: Option<String>,
}

impl MutationRecord {
    /// The request this mutation was carried by.
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// The caller's idempotency key.
    #[must_use]
    pub fn idempotency_id(&self) -> &str {
        &self.idempotency_id
    }

    /// The current status.
    #[must_use]
    pub const fn status(&self) -> MutationStatus {
        self.status
    }

    /// Whether the request was actually written to the bridge.
    #[must_use]
    pub const fn dispatched(&self) -> bool {
        self.dispatched
    }

    /// The editor-side action identity, when the plugin reported one.
    #[must_use]
    pub fn action_id(&self) -> Option<&str> {
        self.action_id.as_deref()
    }

    /// The Todo 6 handoff this record obliges, if any.
    ///
    /// `Some` exactly when the status is [`MutationStatus::Unresolved`].
    #[must_use]
    pub fn quarantine_handoff(&self) -> Option<QuarantineHandoff> {
        if !self.status.requires_quarantine() {
            return None;
        }
        Some(QuarantineHandoff {
            request_id: self.request_id.clone(),
            idempotency_id: self.idempotency_id.clone(),
            action_id: self.action_id.clone(),
            bridge_reachable: false,
        })
    }
}

/// The `unresolved` handoff, rendered into Todo 6's canonical record shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineHandoff {
    request_id: String,
    idempotency_id: String,
    action_id: Option<String>,
    bridge_reachable: bool,
}

/// The stable terminal reason every bridge quarantine carries.
pub const BRIDGE_OUTCOME_UNRESOLVED: &str = "editor_bridge_outcome_unresolved";

impl QuarantineHandoff {
    /// The bridge request whose outcome is unresolved.
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Note that the bridge was still reachable when the outcome was classified.
    ///
    /// Reachability is recorded because it is a *fact about the probe*, not
    /// evidence about the mutation: a reachable bridge that cannot say whether the
    /// action committed is still unresolved.
    #[must_use]
    pub const fn with_bridge_reachable(mut self, reachable: bool) -> Self {
        self.bridge_reachable = reachable;
        self
    }

    /// Render Todo 6's canonical `editor_runtime` quarantine record.
    ///
    /// The launch-specific facts travel in `staged_write_set` /
    /// `touched_object_identities`, exactly as Todo 14 does, never as invented
    /// top-level fields.
    ///
    /// # Errors
    /// Returns [`JournalError::Schema`] when the assembled record violates Todo
    /// 6's canonical schema, or [`JournalError::SchemaUnusable`] when that schema
    /// cannot be parsed. A record the schema rejects can never be handed on.
    pub fn to_canonical_value(
        &self,
        envelope: &TransactionEnvelope,
    ) -> Result<Value, JournalError> {
        let mut facts: BTreeMap<String, Value> = BTreeMap::new();
        facts.insert("bridge_request_id".to_owned(), json!(self.request_id));
        facts.insert(
            "bridge_idempotency_id".to_owned(),
            json!(self.idempotency_id),
        );
        facts.insert("bridge_action_id".to_owned(), json!(self.action_id));
        facts.insert("bridge_reachable".to_owned(), json!(self.bridge_reachable));
        facts.insert(
            "mutation_status".to_owned(),
            json!(MutationStatus::Unresolved.as_str()),
        );
        let payload_value = Value::Object(facts.into_iter().collect());
        let payload_bytes = serde_json::to_vec(&payload_value).unwrap_or_default();
        let payload_name = format!("bridge_mutation/{}", self.request_id);
        let staging_id: String = hex_sha256(&payload_bytes).chars().take(16).collect();

        let record = json!({
            "schema_version": "1",
            "transaction_id": envelope.transaction_id,
            "effect_kind": "editor_runtime",
            "effect_boundary": "editor",
            "project_object_identity": envelope.project.object_identity,
            "project_root_object": envelope.project.root_object,
            "project_file_object": envelope.project.file_object,
            "project_generation": envelope.project.generation,
            "transport_mode": transport_mode(envelope),
            "principal": envelope.principal,
            "session": envelope.session,
            "operation_family": operation_family(envelope),
            "tool_name": envelope.tool_name,
            "idempotency_id": envelope.idempotency_id,
            "canonical_request_hash": envelope.canonical_request_hash,
            "canonical_request_bytes": envelope.canonical_request_bytes,
            "touched_paths": [payload_name.clone()],
            "touched_object_identities": [
                {"path": payload_name.clone(), "object_identity": Value::Null}
            ],
            "pre_hashes": [],
            "staged_write_set": [{
                "path": payload_name,
                "staging_id": staging_id,
                "sha256": hex_sha256(&payload_bytes),
                "byte_length": payload_bytes.len(),
            }],
            "intended_post_hashes": [],
            "phase": "quarantined",
            "phase_history": [
                {"phase": "intent_fsynced", "sequence": 1},
                {"phase": "staged_fsynced", "sequence": 2},
                {"phase": "quarantined", "sequence": 3},
            ],
            "proof": {
                "kind": "editor_runtime",
                "bridge_reachable": self.bridge_reachable,
                "receipt_transaction_id": Value::Null,
                "receipt_action_id": self.action_id,
                "live_state_probe": Value::Null,
            },
            "required_subproofs": Value::Null,
            "terminal_reason": BRIDGE_OUTCOME_UNRESOLVED,
            "opened_at_unix_ms": envelope.opened_at_unix_ms,
        });
        validate_canonical(&record)?;
        Ok(record)
    }
}

fn transport_mode(envelope: &TransactionEnvelope) -> &'static str {
    // The engine's `TransportMode` renders its own canonical spelling through
    // Todo 6's record assembly; reproduce exactly those three spellings here
    // rather than inventing a fourth.
    use godot_mcp_engine::supervision::journal::TransportMode;
    match envelope.transport_mode {
        TransportMode::Stdio => "stdio",
        TransportMode::TrustedLocal => "trusted_local",
        TransportMode::ProxiedOidc => "proxied_oidc",
    }
}

fn operation_family(envelope: &TransactionEnvelope) -> &'static str {
    use godot_mcp_engine::supervision::journal::OperationFamily;
    match envelope.operation_family {
        OperationFamily::ProjectMutation => "project_mutation",
        OperationFamily::EditorMutation => "editor_mutation",
        OperationFamily::RuntimeExecution => "runtime_execution",
        OperationFamily::DestructiveExport => "destructive_export",
        OperationFamily::DiagnosticProbe => "diagnostic_probe",
    }
}

/// Where an `unresolved` handoff is delivered.
///
/// A narrow port, implemented by the server layer over Todo 6's
/// `TransactionStore`. This crate never opens a durable store itself; see
/// `godot-mcp-engine`'s `LaunchJournal` for the same pattern.
pub trait QuarantineSink {
    /// Durably record one `editor_runtime` quarantine, fsyncing before returning.
    ///
    /// # Errors
    /// Returns [`JournalError`] when the record is not canonical or the durable
    /// write failed. The caller must treat any error as "this handoff is not
    /// durable" and must not report the mutation as resolved.
    fn quarantine(&self, record: &Value, handoff: &QuarantineHandoff) -> Result<(), JournalError>;
}

/// Per-session mutation bookkeeping.
///
/// Ordered by insertion via a `BTreeMap` keyed on request ID, so iteration and
/// therefore every diagnostic is deterministic.
#[derive(Debug, Clone, Default)]
pub struct MutationLedger {
    records: BTreeMap<String, MutationRecord>,
    idempotency: BTreeMap<String, String>,
}

impl MutationLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Track a new mutation.
    ///
    /// # Errors
    /// Returns [`MutationOutcome::NotAMutation`] for a non-mutation class,
    /// [`MutationOutcome::DuplicateRequestId`] for a reused request ID, and
    /// [`MutationOutcome::DuplicateIdempotencyId`] for an idempotency ID already
    /// bound to a different request.
    pub fn open(
        &mut self,
        request_id: &str,
        idempotency_id: &str,
        class: OperationClass,
    ) -> Result<MutationRecord, MutationOutcome> {
        if class != OperationClass::Mutation {
            return Err(MutationOutcome::NotAMutation(class.as_str()));
        }
        if self.records.contains_key(request_id) {
            return Err(MutationOutcome::DuplicateRequestId(request_id.to_owned()));
        }
        if let Some(existing) = self.idempotency.get(idempotency_id)
            && existing != request_id
        {
            return Err(MutationOutcome::DuplicateIdempotencyId(
                idempotency_id.to_owned(),
            ));
        }
        let record = MutationRecord {
            request_id: request_id.to_owned(),
            idempotency_id: idempotency_id.to_owned(),
            status: MutationStatus::NotStarted,
            dispatched: false,
            action_id: None,
        };
        self.idempotency
            .insert(idempotency_id.to_owned(), request_id.to_owned());
        self.records.insert(request_id.to_owned(), record.clone());
        Ok(record)
    }

    /// Mark a mutation as actually written to the bridge.
    ///
    /// A dispatched-but-unanswered mutation is never replayable, because "we sent
    /// it" is precisely the state in which the editor may already have applied it.
    ///
    /// # Errors
    /// Returns [`MutationOutcome::UnknownRequestId`] for an untracked ID.
    pub fn mark_dispatched(&mut self, request_id: &str) -> Result<(), MutationOutcome> {
        let record = self
            .records
            .get_mut(request_id)
            .ok_or_else(|| MutationOutcome::UnknownRequestId(request_id.to_owned()))?;
        record.dispatched = true;
        Ok(())
    }

    /// Settle a mutation into one of the three terminal states.
    ///
    /// # Errors
    /// Returns [`MutationOutcome::UnknownRequestId`] for an untracked ID or
    /// [`MutationOutcome::AlreadySettled`] for a second settlement.
    pub fn settle(
        &mut self,
        request_id: &str,
        status: MutationStatus,
        action_id: Option<&str>,
    ) -> Result<MutationRecord, MutationOutcome> {
        let record = self
            .records
            .get_mut(request_id)
            .ok_or_else(|| MutationOutcome::UnknownRequestId(request_id.to_owned()))?;
        if record.status != MutationStatus::NotStarted || record.action_id.is_some() {
            return Err(MutationOutcome::AlreadySettled {
                request_id: request_id.to_owned(),
                settled: record.status,
            });
        }
        record.status = status;
        record.action_id = action_id.map(ToOwned::to_owned);
        Ok(record.clone())
    }

    /// The record for `request_id`, if tracked.
    #[must_use]
    pub fn get(&self, request_id: &str) -> Option<&MutationRecord> {
        self.records.get(request_id)
    }

    /// The record for `request_id` **only if it may safely be resubmitted**.
    ///
    /// `None` for anything committed, unresolved, or already dispatched.
    #[must_use]
    pub fn replayable(&self, request_id: &str) -> Option<&MutationRecord> {
        self.records
            .get(request_id)
            .filter(|record| record.status.is_replayable() && !record.dispatched)
    }

    /// Every request ID whose outcome is unresolved and therefore quarantined.
    #[must_use]
    pub fn quarantined(&self) -> Vec<String> {
        self.records
            .values()
            .filter(|record| record.status.requires_quarantine())
            .map(|record| record.request_id.clone())
            .collect()
    }

    /// What a fresh session may resubmit after a reconnect.
    ///
    /// Only never-dispatched [`MutationStatus::NotStarted`] work. A reconnect is
    /// never permission to replay an unresolved or dispatched mutation.
    #[must_use]
    pub fn reconnect(&self) -> Vec<String> {
        self.records
            .values()
            .filter(|record| record.status.is_replayable() && !record.dispatched)
            .map(|record| record.request_id.clone())
            .collect()
    }

    /// Deliver every outstanding `unresolved` handoff to `sink`.
    ///
    /// # Errors
    /// Returns the first [`JournalError`] the sink or the canonical schema
    /// produced; the handoffs already delivered stay delivered, and the caller
    /// must not treat the remainder as resolved.
    pub fn drain_quarantine(
        &self,
        envelope: &TransactionEnvelope,
        sink: &dyn QuarantineSink,
    ) -> Result<usize, JournalError> {
        let mut delivered = 0_usize;
        for record in self.records.values() {
            if let Some(handoff) = record.quarantine_handoff() {
                let value = handoff.to_canonical_value(envelope)?;
                sink.quarantine(&value, &handoff)?;
                delivered += 1;
            }
        }
        Ok(delivered)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use godot_mcp_engine::supervision::journal::{
        JournalError, OperationFamily, ProjectIdentityClaim, TransactionEnvelope, TransportMode,
    };
    use serde_json::{Value, json};

    use super::super::envelope::OperationClass;
    use super::{
        BRIDGE_OUTCOME_UNRESOLVED, MutationLedger, MutationOutcome, MutationStatus,
        QuarantineHandoff, QuarantineSink,
    };

    fn envelope() -> TransactionEnvelope {
        TransactionEnvelope {
            transaction_id: "3f2504e0-4f89-41d3-9a0c-0305e82c3301".to_owned(),
            project: ProjectIdentityClaim {
                object_identity: format!("project_{}", "a".repeat(64)),
                root_object: "unix:dev=1,ino=2".to_owned(),
                file_object: "unix:dev=1,ino=3".to_owned(),
                generation: 3,
            },
            transport_mode: TransportMode::Stdio,
            principal: None,
            session: None,
            operation_family: OperationFamily::EditorMutation,
            tool_name: "editor.set_property".to_owned(),
            idempotency_id: "idem-1".to_owned(),
            canonical_request_hash: "c".repeat(64),
            canonical_request_bytes: 32,
            opened_at_unix_ms: 1_700_000_000_000,
        }
    }

    struct RecordingSink {
        seen: RefCell<Vec<String>>,
    }

    impl QuarantineSink for RecordingSink {
        fn quarantine(
            &self,
            record: &Value,
            handoff: &QuarantineHandoff,
        ) -> Result<(), JournalError> {
            assert_eq!(record["phase"], json!("quarantined"));
            self.seen.borrow_mut().push(handoff.request_id().to_owned());
            Ok(())
        }
    }

    #[test]
    fn there_are_exactly_three_statuses_with_stable_spellings() {
        assert_eq!(MutationStatus::ALL.len(), 3);
        assert_eq!(
            MutationStatus::ALL.map(MutationStatus::as_str),
            ["not_started", "committed", "unresolved"]
        );
        for status in MutationStatus::ALL {
            assert_eq!(MutationStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(MutationStatus::parse("probably"), None);
        assert_eq!(MutationStatus::parse("rolled_back"), None);
    }

    #[test]
    fn only_a_never_dispatched_not_started_mutation_is_replayable() {
        assert!(MutationStatus::NotStarted.is_replayable());
        assert!(!MutationStatus::Committed.is_replayable());
        assert!(!MutationStatus::Unresolved.is_replayable());

        let mut ledger = MutationLedger::new();
        ledger
            .open("m1", "i1", OperationClass::Mutation)
            .expect("open");
        assert!(ledger.replayable("m1").is_some());
        ledger.mark_dispatched("m1").expect("dispatch");
        assert!(
            ledger.replayable("m1").is_none(),
            "a dispatched mutation is never replayable, answered or not"
        );
    }

    #[test]
    fn a_duplicate_request_or_idempotency_id_is_refused() {
        let mut ledger = MutationLedger::new();
        ledger
            .open("m1", "i1", OperationClass::Mutation)
            .expect("open");
        assert_eq!(
            ledger.open("m1", "i1", OperationClass::Mutation),
            Err(MutationOutcome::DuplicateRequestId("m1".to_owned()))
        );
        assert_eq!(
            ledger.open("m2", "i1", OperationClass::Mutation),
            Err(MutationOutcome::DuplicateIdempotencyId("i1".to_owned()))
        );
        assert_eq!(
            ledger.open("m3", "i3", OperationClass::Read),
            Err(MutationOutcome::NotAMutation("read"))
        );
    }

    #[test]
    fn a_settled_mutation_cannot_be_settled_twice() {
        let mut ledger = MutationLedger::new();
        ledger
            .open("m1", "i1", OperationClass::Mutation)
            .expect("open");
        ledger
            .settle("m1", MutationStatus::Committed, Some("action-1"))
            .expect("settle");
        assert_eq!(
            ledger.settle("m1", MutationStatus::NotStarted, None),
            Err(MutationOutcome::AlreadySettled {
                request_id: "m1".to_owned(),
                settled: MutationStatus::Committed
            })
        );
        assert_eq!(
            ledger.settle("absent", MutationStatus::Committed, None),
            Err(MutationOutcome::UnknownRequestId("absent".to_owned()))
        );
    }

    #[test]
    fn an_unresolved_mutation_renders_todo_sixs_canonical_quarantine_record() {
        let mut ledger = MutationLedger::new();
        ledger
            .open("m1", "i1", OperationClass::Mutation)
            .expect("open");
        ledger.mark_dispatched("m1").expect("dispatch");
        let settled = ledger
            .settle("m1", MutationStatus::Unresolved, None)
            .expect("settle");
        let handoff = settled.quarantine_handoff().expect("handoff");
        let record = handoff
            .to_canonical_value(&envelope())
            .expect("canonical record");

        assert_eq!(record["effect_kind"], json!("editor_runtime"));
        assert_eq!(record["effect_boundary"], json!("editor"));
        assert_eq!(record["phase"], json!("quarantined"));
        assert_eq!(record["proof"]["kind"], json!("editor_runtime"));
        assert_eq!(record["terminal_reason"], json!(BRIDGE_OUTCOME_UNRESOLVED));
        assert_eq!(
            record["staged_write_set"][0]["path"],
            json!("bridge_mutation/m1"),
            "the bridge facts travel in the staged write set, not in invented fields"
        );
        // Todo 6's own validator, over Todo 6's own embedded schema.
        godot_mcp_engine::supervision::journal::validate_canonical(&record)
            .expect("Todo 6 accepts the record");
    }

    #[test]
    fn a_committed_or_not_started_mutation_produces_no_handoff() {
        let mut ledger = MutationLedger::new();
        ledger
            .open("m1", "i1", OperationClass::Mutation)
            .expect("open");
        assert!(
            ledger
                .get("m1")
                .expect("record")
                .quarantine_handoff()
                .is_none()
        );
        ledger
            .settle("m1", MutationStatus::Committed, Some("a1"))
            .expect("settle");
        assert!(
            ledger
                .get("m1")
                .expect("record")
                .quarantine_handoff()
                .is_none()
        );
        assert!(ledger.quarantined().is_empty());
    }

    #[test]
    fn a_reconnect_never_offers_unresolved_or_dispatched_work() {
        let mut ledger = MutationLedger::new();
        for (id, idem) in [("m1", "i1"), ("m2", "i2"), ("m3", "i3")] {
            ledger
                .open(id, idem, OperationClass::Mutation)
                .expect("open");
        }
        ledger.mark_dispatched("m2").expect("dispatch");
        ledger
            .settle("m2", MutationStatus::Unresolved, None)
            .expect("settle");
        ledger.mark_dispatched("m3").expect("dispatch");
        ledger
            .settle("m3", MutationStatus::Committed, Some("a3"))
            .expect("settle");

        assert_eq!(ledger.reconnect(), vec!["m1".to_owned()]);
        assert_eq!(ledger.quarantined(), vec!["m2".to_owned()]);
        assert!(ledger.replayable("m2").is_none());
        assert!(ledger.replayable("m3").is_none());
    }

    #[test]
    fn draining_quarantine_delivers_every_unresolved_handoff_once() {
        let mut ledger = MutationLedger::new();
        for (id, idem) in [("m1", "i1"), ("m2", "i2")] {
            ledger
                .open(id, idem, OperationClass::Mutation)
                .expect("open");
            ledger
                .settle(id, MutationStatus::Unresolved, None)
                .expect("settle");
        }
        let sink = RecordingSink {
            seen: RefCell::new(Vec::new()),
        };
        assert_eq!(
            ledger.drain_quarantine(&envelope(), &sink).expect("drain"),
            2
        );
        assert_eq!(
            sink.seen.into_inner(),
            vec!["m1".to_owned(), "m2".to_owned()]
        );
    }

    #[test]
    fn bridge_reachability_is_recorded_but_never_resolves_the_outcome() {
        let mut ledger = MutationLedger::new();
        ledger
            .open("m1", "i1", OperationClass::Mutation)
            .expect("open");
        let settled = ledger
            .settle("m1", MutationStatus::Unresolved, None)
            .expect("settle");
        let handoff = settled
            .quarantine_handoff()
            .expect("handoff")
            .with_bridge_reachable(true);
        let record = handoff
            .to_canonical_value(&envelope())
            .expect("canonical record");
        assert_eq!(record["proof"]["bridge_reachable"], json!(true));
        assert_eq!(
            record["phase"],
            json!("quarantined"),
            "a reachable bridge does not turn an unresolved outcome into a commit"
        );
    }
}
