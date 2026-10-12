//! Replay decisions and the single non-enumerating conflict envelope.
//!
//! Two outcomes, and only two, are possible when a request presents an
//! idempotency ID the journal has already seen:
//!
//! * [`ReplayDecision::RecordedResult`] — the presented tuple is identical in all
//!   eight elements, so the caller is asking about *its own* prior work and gets
//!   its own recorded status/result back without the effect running again.
//! * [`ReplayDecision::Conflict`] — any element differs, so the caller is asking
//!   about work that is not theirs. They get one generic envelope.
//!
//! ## Why the envelope is deliberately uninformative
//!
//! Todo 17's `LinkedProof::refusal` expands all five of its conditions, because
//! there the caller already owns the whole context and a vague refusal would just
//! be undiagnosable. Here the situation is inverted: the caller may be probing
//! *another* context. A refusal that varied by cause — or merely varied in length,
//! ordering or timing — would let a caller enumerate which idempotency IDs exist,
//! which principals or sessions are active, which projects are held, or which
//! request hashes were recorded. So [`conflict_envelope`] is the single
//! construction site and it takes no arguments: there is nothing to leak because
//! there is nothing to pass in.
//!
//! [`ConflictEnvelope::as_bytes`] is byte-identical across every call, which is
//! what the acceptance suite asserts across all eight tuple dimensions.

use std::collections::BTreeMap;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{GodotValue, OutcomeError};

/// The one and only conflict message. It names no project, principal, session,
/// operation, hash, existence bit or prior status.
const CONFLICT_MESSAGE: &str = "idempotency identifier is not available for this request";

/// The generic, byte-stable conflict a mismatched reuse produces.
///
/// The type has no public constructor: [`conflict_envelope`] is the sole
/// construction site, and it is argument-free, so no variant of this envelope can
/// ever carry a detail that varies by cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictEnvelope {
    // A private unit field, mirroring `AuthoritySeal`: it makes
    // `ConflictEnvelope { .. }` unconstructable outside this module even for
    // sibling modules in this crate.
    seal: (),
}

impl ConflictEnvelope {
    /// The envelope's canonical bytes. Identical for every conflict, always.
    ///
    /// The bytes are derived from the fixed outcome, and the outcome is built
    /// from constants only, so no mismatch can influence a single byte. A
    /// serialization failure would itself be a variation, so it collapses to the
    /// same fixed byte string rather than to an error that could differ.
    #[must_use]
    pub fn as_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.to_outcome())
            .unwrap_or_else(|_| CONFLICT_MESSAGE.as_bytes().to_vec())
    }

    /// The envelope as the public domain error taxonomy sees it: always
    /// [`ErrorKind::Conflict`], never retryable, with no details map.
    #[must_use]
    pub fn to_outcome(&self) -> OutcomeError {
        OutcomeError::new(
            ErrorKind::Conflict,
            CONFLICT_MESSAGE,
            false,
            BTreeMap::new(),
        )
    }

    /// The fixed caller-facing message.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        CONFLICT_MESSAGE
    }
}

/// The single construction site for a reuse conflict.
///
/// It takes no arguments on purpose. Every mismatched tuple element — family,
/// operation, session, principal, auth mode, request hash, project generation or
/// project object — routes here and gets the same bytes.
#[must_use]
pub fn conflict_envelope() -> ConflictEnvelope {
    ConflictEnvelope { seal: () }
}

/// The status a recorded mutation settled at, as the *owning* caller may see it.
///
/// This mirrors the bridge's three-way mutation status and keeps its replay rule:
/// an unresolved mutation is never replayable (nobody may guess it happened or
/// did not), and a committed mutation needs no replay (it already happened). A
/// dispatched-but-unanswered mutation is likewise not replayable, because "the
/// request went out and no answer came back" is not the same as "it did not
/// happen".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordedStatus {
    /// Proven that nothing outside this process changed.
    NotStarted,
    /// Dispatched, with no authoritative answer observed yet.
    Dispatched,
    /// Proven durable, with its typed proof recorded.
    Committed,
    /// Proven rolled back to the pre-state.
    Compensated,
    /// Not decidable from here; the transaction blocks further writes.
    Unresolved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordedVerdict {
    Succeeded,
    Failed(ErrorKind),
}

impl RecordedStatus {
    /// Every status, in a stable order.
    pub const ALL: [Self; 5] = [
        Self::NotStarted,
        Self::Dispatched,
        Self::Committed,
        Self::Compensated,
        Self::Unresolved,
    ];

    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Dispatched => "dispatched",
            Self::Committed => "committed",
            Self::Compensated => "compensated",
            Self::Unresolved => "unresolved",
        }
    }

    /// Whether the effect may be executed again for this status.
    ///
    /// Only `not_started` qualifies: `dispatched` may already have taken effect,
    /// `committed` and `compensated` are settled, and `unresolved` is exactly the
    /// case nobody may guess.
    #[must_use]
    pub const fn may_execute_again(self) -> bool {
        matches!(self, Self::NotStarted)
    }

    /// Whether this status blocks every further mutation until an operator repair
    /// produces a proof.
    #[must_use]
    pub const fn blocks_further_mutations(self) -> bool {
        matches!(self, Self::Unresolved)
    }
}

/// One recorded mutation, returned only to the context that created it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedMutation {
    transaction_id: String,
    status: RecordedStatus,
    result: Option<serde_json::Value>,
    verdict: RecordedVerdict,
}

impl RecordedMutation {
    /// Bind one recorded mutation read back from the durable journal.
    #[must_use]
    pub fn new(
        transaction_id: impl Into<String>,
        status: RecordedStatus,
        result: Option<serde_json::Value>,
        verdict: RecordedVerdict,
    ) -> Self {
        Self {
            transaction_id: transaction_id.into(),
            status,
            result,
            verdict,
        }
    }

    /// Journaled transaction this mutation settled.
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    /// Recorded status.
    #[must_use]
    pub const fn status(&self) -> RecordedStatus {
        self.status
    }

    /// Recorded result, where the status carries one.
    #[must_use]
    pub const fn result(&self) -> Option<&serde_json::Value> {
        self.result.as_ref()
    }

    #[must_use]
    pub const fn verdict(&self) -> RecordedVerdict {
        self.verdict
    }
}

pub(crate) fn replay_outcome(
    transaction_id: &str,
    status: RecordedStatus,
    result: Option<&serde_json::Value>,
    verdict: RecordedVerdict,
) -> Result<serde_json::Value, OutcomeError> {
    let projection = result.cloned().unwrap_or_else(|| {
        serde_json::json!({
            "result": serde_json::Value::Null,
            "status": status.as_str(),
            "transaction_id": transaction_id,
        })
    });
    match verdict {
        RecordedVerdict::Succeeded => Ok(projection),
        RecordedVerdict::Failed(kind) => {
            let reason = projection
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("the recorded operation failed");
            let mut details = BTreeMap::new();
            details.insert(
                "transaction_id".to_owned(),
                GodotValue::String(transaction_id.to_owned()),
            );
            details.insert(
                "status".to_owned(),
                GodotValue::String(status.as_str().to_owned()),
            );
            details.insert("reason".to_owned(), GodotValue::String(reason.to_owned()));
            Err(OutcomeError::new(kind, reason, false, details))
        }
    }
}

/// What the scheduler decided about a reused idempotency identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayDecision {
    /// No prior record: this request is new and must execute normally.
    Fresh,
    /// Exact same-context reuse: the caller's own prior status/result, returned
    /// without executing the effect again.
    RecordedResult(RecordedMutation),
    /// Any tuple element differed: one generic, byte-stable envelope.
    Conflict(ConflictEnvelope),
}

impl ReplayDecision {
    /// Whether the effect must still be executed.
    #[must_use]
    pub const fn must_execute(&self) -> bool {
        matches!(self, Self::Fresh)
    }

    /// The recorded mutation, when this decision returns the caller's own work.
    #[must_use]
    pub const fn recorded(&self) -> Option<&RecordedMutation> {
        match self {
            Self::RecordedResult(recorded) => Some(recorded),
            Self::Fresh | Self::Conflict(_) => None,
        }
    }

    /// The conflict envelope, when this decision refuses the reuse.
    #[must_use]
    pub const fn conflict(&self) -> Option<&ConflictEnvelope> {
        match self {
            Self::Conflict(envelope) => Some(envelope),
            Self::Fresh | Self::RecordedResult(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use godot_mcp_domain::ErrorKind;
    use godot_mcp_domain::contracts::GodotValue;
    use serde_json::json;

    use super::{RecordedStatus, RecordedVerdict, replay_outcome};

    const TRANSACTION_ID: &str = "00000000-0000-4000-8000-000000004301";

    #[test]
    fn timeout_replay_remains_an_error_with_its_transaction_projection() {
        let result = json!({
            "reason": "the operation exceeded its execution deadline; its effect is not yet proven",
            "status": "compensated",
            "transaction_id": TRANSACTION_ID,
        });

        let error = replay_outcome(
            TRANSACTION_ID,
            RecordedStatus::Compensated,
            Some(&result),
            RecordedVerdict::Failed(ErrorKind::Timeout),
        )
        .expect_err("a recorded timeout must never replay as success");

        assert_eq!(error.code(), "timeout");
        assert!(!error.retryable());
        assert_eq!(
            error.details().get("transaction_id"),
            Some(&GodotValue::String(TRANSACTION_ID.to_owned()))
        );
        assert_eq!(
            error.details().get("status"),
            Some(&GodotValue::String("compensated".to_owned()))
        );
        assert_eq!(
            error.details().get("reason"),
            Some(&GodotValue::String(
                "the operation exceeded its execution deadline; its effect is not yet proven"
                    .to_owned()
            ))
        );
    }

    #[test]
    fn successful_replay_remains_successful_and_keeps_the_small_projection() {
        let result = json!({
            "reason": "supervised process launch completed with a durable proof",
            "status": "committed",
            "transaction_id": TRANSACTION_ID,
        });

        let replayed = replay_outcome(
            TRANSACTION_ID,
            RecordedStatus::Committed,
            Some(&result),
            RecordedVerdict::Succeeded,
        )
        .expect("a recorded success remains successful");

        assert_eq!(replayed, result);
        assert_eq!(
            replayed
                .as_object()
                .expect("replay projection is an object")
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            ["reason", "status", "transaction_id"]
        );
    }
}
