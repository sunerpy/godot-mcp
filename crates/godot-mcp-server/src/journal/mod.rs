//! The durable effect journal.
//!
//! This is the single durable transaction store for the whole product. Every
//! implemented effect reuses it: there is exactly one canonical schema
//! (`schemas/transactions/v1.json`), one SQLite store, one phase machine and
//! exactly three typed proof forms. No module may introduce a second journal, a
//! second schema, an in-memory stand-in or a fourth proof kind.

// The journal is fully implemented and fully exercised today: the stdio lifecycle
// drives `TransactionStore::open` plus startup reconciliation, and the Todo 6
// acceptance suite drives every phase, proof form and terminal record. A build
// that omits `test-hooks` still compiles the editor/runtime proof constructors
// retained by the canonical schema, although the corresponding production planes
// were cancelled with Todos 20–25. The relaxation below is scoped to that
// configuration and carries its reason.
#![cfg_attr(
    not(feature = "test-hooks"),
    allow(
        dead_code,
        unused_imports,
        reason = "process_launch has production callers; editor_runtime remains schema-only after cancelled                   Todos 20-25; the test-hooks acceptance suite exercises both proof forms"
    )
)]

pub(crate) mod proof;
pub(crate) mod reconcile;
pub(crate) mod record;
pub(crate) mod store;

pub(crate) use proof::{
    ProofOutcome, ProofSource, SafeRootProofSource, intended_post_entry, pre_state_entry,
    verify_file_proof,
};
pub(crate) use reconcile::{
    Conclusion, HeldProject, ReconciliationReport, reconcile_linked_process_transactions,
    reconcile_open_transactions,
};
pub(crate) use record::{
    BoundaryMembership, EffectBoundaryKind, EffectKind, Existence, FileOperation, FileStateEntry,
    OperationFamily, Phase, ReleaseStatus, RequiredSubproofs, StagedWrite, TerminalOutcome,
    TouchedObject, TransactionRecord, TransportMode, TypedProof, canonical_request,
    encode_object_identity, hex_sha256,
};
pub(crate) use store::{LegacyStoreSummary, StoreError, TransactionStore, inspect_legacy_store};

#[cfg(feature = "test-hooks")]
pub(crate) use store::{FaultPoint, PhaseName};
