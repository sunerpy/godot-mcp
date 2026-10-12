//! # godot-mcp-cli
//!
//! The `godot-mcp` command-line entry point plus the workspace's
//! audit/validation helpers that Todo 1's Green gate exercises.
//!
//! ## What the shipped binary actually exposes
//!
//! `src/bin/godot-mcp.rs` defines exactly six top-level subcommands and no
//! others: `version`, `check-config`, `serve`, `transaction`, `self-check` and
//! `assets`. `transaction` has one subcommand, `repair`. `assets` has four:
//! `install`, `update`, `remove` and `status`. There is no `init`, no `locate`
//! and no `configure` subcommand.
//!
//! Three distinctions are load-bearing, because each is easy to overstate:
//!
//! * **`serve` defaults to stdio and enables HTTP only through explicit
//!   configuration.** Omitting `--config` starts stdio and creates no listener or
//!   credential. A validated `--config` file may select authenticated loopback
//!   Streamable HTTP. Omitting `--stdio`, or passing it bare, affirms startup; an
//!   explicit `--stdio=false` is accepted by the parser and refused by `serve`,
//!   which exits 2 rather than selecting another transport implicitly.
//! * **`assets install` installs project assets, not an editor plug-in.** The
//!   payload is the Todo 19 bridge codec and the install root is `godot_mcp/`,
//!   deliberately not `addons/godot_mcp/`: the editor-plugin plane (Todos 20–25)
//!   was cancelled, no `addons/` tree exists in this workspace, and installing
//!   the payload does not make an editor plane available. See
//!   [`package::installer`] and
//!   `docs/contributor/adr/0004-editor-plane-cancelled.md`.
//! * **`self-check` verifies a package, not the running server.** It compares an
//!   extracted release package against its own `MANIFEST.json`. The server's
//!   health/readiness/self-diagnostic fact set lives in
//!   `godot_mcp_server::diagnostics` and is read through the MCP surface a
//!   `serve` session exposes; no subcommand in this crate calls it.
//!
//! ## Layer position
//!
//! This is a UI/transport-adjacent layer. It may depend on `godot-mcp-server`;
//! the domain/analysis/engine crates must never depend on it (enforced by the
//! `architecture_boundaries` test in `godot-mcp-domain`).
//!
//! The audit helpers here are intentionally small, deterministic, offline
//! functions so they can back the Todo 1 acceptance tests without a live
//! network or Godot binary.

#![forbid(unsafe_code)]

pub mod audit;
pub mod corrupt;
pub mod descriptors;
pub mod docs;
#[cfg(feature = "test-hooks")]
pub mod fixture34;
pub mod host_package;
pub mod package;
pub mod release_gates;
pub mod repair;
pub mod tooling_lock;
