//! # godot-mcp-domain
//!
//! Lowest architectural layer of the godot-mcp workspace. It owns the versioned
//! domain contracts, the stable error taxonomy, the provenance/confidence model,
//! typed Godot value shapes, and the capability-profile manifests.
//!
//! ## Architectural boundary (frozen by Todo 1)
//!
//! This crate — like `godot-mcp-analysis` and `godot-mcp-engine` — must never
//! import a transport or UI layer crate (`godot-mcp-server`, `godot-mcp-cli`).
//! Dependencies flow strictly upward toward the server; the domain layer has no
//! knowledge of stdio, HTTP, MCP wiring, or CLI presentation. The
//! `architecture_boundaries` integration test enforces this at the manifest
//! level so a forbidden edge fails closed.
//!
//! Todo 1 scaffolds only the boundary and the contract skeleton; the concrete
//! DTOs, error values and profile algebra are completed by Todo 3.

// Unsafe is denied throughout the crate and relaxed only on the private Windows
// sysfs module, where audited FFI calls are unavoidable. `forbid` would prevent
// that child-module exception from being expressed.
#![deny(unsafe_code)]

pub mod catalog;
pub mod config;
pub mod contracts;
pub mod jsonschema;
pub mod lease;
pub mod manifests;
pub mod objid;
pub mod project;
pub mod safe_root;
#[doc(hidden)]
pub mod sysfs;
pub mod workspace_root;

/// Semantic version of the domain contract surface.
///
/// Bumped whenever a DTO, error variant, or capability manifest changes shape.
/// Version 0.2.0 makes tool discovery implementation-backed: a profile lists
/// only methods the running build implements.
pub const CONTRACT_VERSION: &str = "0.2.0";

/// Capability profiles exposed by the public contract. Selection is explicit
/// and independent of how an outcome is requested.
///
/// `Full` is the union of `Core`, `Editor` and `Runtime`, excluding the
/// separately gated (and first-release-absent) unsafe execution capability.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityProfile {
    /// Read-only discovery and analysis.
    Core,
    /// Adds the project-write and editor-mutation methods this build implements.
    Editor,
    /// Adds the runtime-execution and export methods this build implements.
    Runtime,
    /// Union of the above, excluding unsafe execution.
    Full,
}

impl CapabilityProfile {
    /// Stable manifest name reflected by `tools/list`.
    #[must_use]
    pub const fn manifest_name(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Editor => "editor",
            Self::Runtime => "runtime",
            Self::Full => "full",
        }
    }
}

/// Confidence of a static answer. Partial scanners and dynamic GDScript are
/// never represented as certain: every static answer carries provenance and one
/// of these confidence levels.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Statically proven.
    Exact,
    /// Inferred from heuristics; the single best candidate is not guaranteed.
    Inferred,
    /// Could not be resolved.
    Unresolved,
}

/// Stable outcome-error taxonomy shared by every public operation.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidInput,
    NotFound,
    Conflict,
    Unsupported,
    PermissionDenied,
    Timeout,
    Cancelled,
    Unavailable,
    Uncertain,
    Internal,
}

impl ErrorKind {
    /// Stable wire code for this error kind.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::Unsupported => "unsupported",
            Self::PermissionDenied => "permission_denied",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Unavailable => "unavailable",
            Self::Uncertain => "uncertain",
            Self::Internal => "internal",
        }
    }
}
