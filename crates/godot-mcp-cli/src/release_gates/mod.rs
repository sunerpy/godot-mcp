//! Release-gates CI harness, lock-driven matrix derivation and the canonical
//! output contract.
//!
//! Three pieces, deliberately separated:
//!
//! * [`lock_matrix`] is the single place a version label, download URL or
//!   expected digest enters CI. Everything else derives from it, and
//!   [`lock_matrix::scan_for_restated_lock_values`] proves nothing restated one.
//! * [`schema`] embeds and enforces `schemas/gates/release-gates.schema.json`.
//! * [`harness`] binds each required external check to its exact command and
//!   runner identity, turns a missing/skipped/unavailable observation into an
//!   explicit failed entry, and refuses a qualifying emission outside Todo 37's
//!   producer-owned orchestration context.
//!
//! This module owns the harness, its Make target and its schema. It never
//! produces the final qualifying gates document.

pub mod harness;
pub mod lock_matrix;
pub mod schema;
