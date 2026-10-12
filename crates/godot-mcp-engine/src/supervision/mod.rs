//! Bounded cross-platform process supervision with a gapless launch protocol.
//!
//! Godot (or any supervised helper) is never started outside a containment
//! boundary, and never runs user code before its complete identity receipt is
//! durable. The module is split by responsibility:
//!
//! * [`journal`] — the port onto Todo 6's one durable journal. Exactly the four
//!   canonical fsynced phases, exactly three terminal dispositions, exactly Todo
//!   6's `process_launch` proof form.
//! * [`boundary`] — what a containment boundary is, the two platform paths
//!   (strong and trusted-Unix weak) and the fail-closed provider selection.
//! * [`adapter`], [`cgroup`], [`job_object`] — the three strong mechanisms.
//! * [`weak_unix`] — the trusted-Unix locator + blocked-supervisor protocol.
//! * [`gate`] — the one-shot gate, the exec barrier, output caps.
//! * [`procfs`] — the live OS observations reconciliation is allowed to trust.
//! * [`protocol`] — the launch driver that enforces the normative ordering.
//! * [`reconcile`] — the five disjoint, exhaustive crash-recovery cases.
//! * [`supervisor`] — the bundled minimal supervisor/barrier program's logic.

pub mod adapter;
pub mod boundary;
pub mod cgroup;
pub mod gate;
pub mod job_object;
pub mod journal;
pub mod procfs;
pub mod protocol;
pub mod reconcile;
pub mod supervisor;
pub mod weak_unix;
