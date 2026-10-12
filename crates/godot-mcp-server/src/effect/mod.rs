//! The crate-private, permit-typed effect gateway and its five effect
//! boundaries.
//!
//! Nothing in this module is `pub`. The gateway, the permit types, the permit
//! authorities and the boundary traits are all crate-private, so an adapter, a
//! plugin or any external crate cannot construct a permit, cannot name a
//! boundary and therefore cannot reach an effect at all. Every effect that
//! mutates anything outside this process flows through
//! [`EffectGateway`], which:
//!
//! 1. validates the permit (seal, epoch, project object identity/generation,
//!    capability, operation family and boundary), rejecting a missing or stale
//!    permit;
//! 2. re-checks quarantine at execution time, so a permit minted before a
//!    quarantine event cannot be replayed through it;
//! 3. requires the transaction to already be journaled and `staged_fsynced`, so
//!    an unjournaled effect is structurally impossible;
//! 4. journals `effect_started_fsynced` before the boundary is touched; and
//! 5. journals exactly one terminal record — `committed_fsynced`,
//!    `compensated` or `quarantined` — derived from an independently re-derived
//!    typed proof, never from a guess.

// Todo 6 froze this foundation. The production file and process boundaries are
// live. The LSP/DAP caller and bounded adapters landed in Todos 13–18, but the
// `lsp_dap` boundary is fail-closed: no production path supplies the server-owned
// editor launch receipt that `EndpointRegistry::connect` requires before dialing.
// That graceful-unavailable fallback satisfies Todo 18; it is not an adapter
// defect. The editor and runtime boundaries remain fail-closed because Todos 23
// and 25 were cancelled. Todo 26 supplied the ordinary permit authority's policy
// tables, and Todo 34 supplied operator recovery. Under the `test-hooks` feature
// every item below is exercised by the Todo 6 acceptance suite, so this relaxation
// applies only to a build without that test harness. It is deliberately scoped to
// this module and carries a reason rather than being a blanket allow.
#![cfg_attr(
    not(feature = "test-hooks"),
    allow(
        dead_code,
        unused_imports,
        reason = "Todo 6 froze the effect foundation; file/process are live, LSP-DAP fails closed because no production path supplies its required editor launch receipt, editor/runtime stay unavailable after cancelled Todos 23/25, and test-hooks exercises every item"
    )
)]

pub(crate) mod boundary;
pub(crate) mod gateway;
pub(crate) mod permit;

pub(crate) use boundary::{
    ProcessEffectOutcome, ProcessLaunchRequest, ProcessOutputMetadata, SafeRootFileBoundary,
    UnavailableBoundaries,
};
pub(crate) use gateway::{
    DerivedDisposition, DerivedTerminal, EffectBoundaries, EffectError, EffectGateway,
    EffectOutcome, FileEffectIntent, PlannedWrite, ProcessEffectIntent, WritePrecondition,
};
pub(crate) use permit::{
    EffectPermit, LocalLeaseEvidence, OperatorRecoveryAuthority, PermitAuthority, PermitError,
    PermitRequest, RepairOperation,
};
