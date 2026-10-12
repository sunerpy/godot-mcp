//! Self-contained GDScript syntax layer.
//!
//! This module owns everything needed to turn a `.gd` source string into typed,
//! workspace-owned syntax records plus structured parser diagnostics. It depends
//! on `tree-sitter` and `tree-sitter-gdscript` only; no CodeGraph store, graph,
//! node-id or edge type is referenced, and no sibling checkout is read at
//! runtime.
//!
//! ## Attribution
//!
//! The GDScript node-kind / field vocabulary and the constructor / static-flag /
//! signature / return-type extraction behavior are adapted from the MIT-licensed
//! `codegraph-rust` project (`crates/codegraph-extract/src/lang/gdscript.rs`,
//! `GdscriptSpec`). Copyright (c) 2026 sunerpy, MIT. The full notice is in
//! `THIRD-PARTY-NOTICES.md` at the workspace root.
//!
//! ## Honesty rules enforced here
//!
//! * Every tree-sitter `ERROR` / `MISSING` node becomes a [`GdDiagnostic`];
//!   nothing is silently dropped.
//! * A record whose span is touched by a diagnostic is downgraded to
//!   [`ProvenanceSource::Heuristic`](godot_mcp_domain::contracts::ProvenanceSource::Heuristic)
//!   / [`Confidence::Inferred`](godot_mcp_domain::Confidence::Inferred).
//! * A call site is never labeled exact: syntax alone cannot prove the callee.
//! * Damage regions are not descended into, so no symbol is invented out of an
//!   `ERROR` subtree.

mod nodes;
mod parser;

pub use nodes::{
    GdCall, GdCallKind, GdClass, GdDiagnostic, GdDiagnosticKind, GdEnum, GdEnumMember, GdFunction,
    GdParameter, GdResourceRef, GdScriptFile, GdSignal, GdSpan, GdVariable, GdVariableKind,
};
pub use parser::{GdParseError, GdScriptParser};
