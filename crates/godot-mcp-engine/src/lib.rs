//! # godot-mcp-engine
//!
//! Bounded Godot execution plane: probing the configured executable/version/build
//! type, process supervision and containment, validation/import/test, disposable
//! execution, engine-owned canonical `.tscn`/`.tres`/`ResourceUID` writes, and
//! LSP/DAP adapters bound to server-owned launch/editor receipts.
//!
//! ## Architectural boundary (frozen by Todo 1)
//!
//! Depends only on `godot-mcp-domain` and `godot-mcp-analysis`. Must never import
//! a transport or UI layer crate (`godot-mcp-server`, `godot-mcp-cli`); the
//! `architecture_boundaries` test enforces this at the manifest level.
//!
//! Todo 1 scaffolds the crate boundary only; the runner, launch protocol,
//! canonical writes and LSP/DAP adapters are completed by Todos 13–18.

// Unsafe is denied throughout the crate and relaxed only on the private Windows
// Job Object backend, where audited FFI calls are unavoidable. `forbid` would
// prevent that child-module exception from being expressed.
#![deny(unsafe_code)]

pub mod adapters;
pub mod canonical;
pub mod discovery;
pub mod disposable;
pub mod export;
pub mod probe;
pub mod releases;
pub mod supervision;
pub mod validation;

/// Version of the engine plane's launch/receipt contract.
pub const ENGINE_VERSION: &str = "0.1.0";
