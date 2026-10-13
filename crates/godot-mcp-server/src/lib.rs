//! # godot-mcp-server
//!
//! Transport and kernel layer. Hosts the `rmcp` MCP handler, capability policy,
//! the private permit-typed effect gateway, the durable transaction journal, and
//! both transports: stdio is always available and is the default; optional
//! Streamable HTTP (feature `http`) and optional Resource-Server auth (feature
//! `auth`) invoke the *same* handler/kernel and must be contract-equivalent.
//!
//! ## Transport-clean stdout (frozen by Todo 1)
//!
//! In stdio mode, protocol JSON-RPC frames are the only thing written to
//! stdout. All diagnostics/logs go to stderr or a file sink — never stdout.
//!
//! Todo 1 scaffolds the crate boundary and feature flags only; the kernel,
//! journal, policy and transports are completed by Todos 5–6, 26–31.

#![forbid(unsafe_code)]

pub(crate) mod arguments;
pub mod artifacts;
pub mod audit;
#[cfg(feature = "auth")]
pub mod auth;
pub(crate) mod authoring;
#[cfg(feature = "http")]
pub mod binding;
#[cfg(feature = "stdio")]
pub mod bootstrap;
pub(crate) mod bootstrap_schema;
pub mod core;
pub mod diagnostics;
pub(crate) mod editor;
pub(crate) mod effect;
pub(crate) mod engine_plane;
pub mod evidence;
#[cfg(feature = "http")]
pub mod http;
pub mod idempotency;
pub(crate) mod journal;
pub mod kernel;
pub mod limits;
pub(crate) mod node_handle;
pub mod observability;
pub mod operator;
pub(crate) mod policy;
pub mod project_context;
pub mod rmcp_adapter;
pub mod router;
pub mod scheduler;
pub(crate) mod schema;
#[cfg(feature = "stdio")]
pub mod stdio;
#[cfg(feature = "test-hooks")]
pub mod test_support;
pub mod workspace;

/// The version of this server build: the workspace version Release Please sets.
///
/// `serverInfo` in `initialize`, the health document and the editor bridge's
/// handshake all report it. The domain contract has a version of its own,
/// [`godot_mcp_domain::CONTRACT_VERSION`].
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Whether the optional Streamable HTTP transport was compiled in.
#[must_use]
pub const fn http_enabled() -> bool {
    cfg!(feature = "http")
}

/// Whether the optional Resource-Server auth layer was compiled in.
#[must_use]
pub const fn auth_enabled() -> bool {
    cfg!(feature = "auth")
}
