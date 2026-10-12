//! # godot-mcp-bridge
//!
//! Rust side of the loopback editor-bridge **protocol**, and the bundled
//! `godot_mcp` editor add-on that speaks it.
//!
//! ## What ships
//!
//! * [`protocol`] — the frozen, versioned wire contract of Todo 19 and the
//!   pinned GDScript codec `scripts/bridge/bridge_codec.gd`.
//! * [`addon`] — the GDScript `EditorPlugin` the server installs into a
//!   project's `addons/godot_mcp/`: it dials the bridge, runs every request on
//!   the editor main thread, answers live reads, applies semantic scene batches
//!   as one `EditorUndoRedoManager` action, and carries the tagged-value codec
//!   and the class catalogue. Version 0.1 shipped no plug-in (Todos 20–25 were
//!   cancelled, see `docs/contributor/adr/0004-editor-plane-cancelled.md`);
//!   contract 0.2 ships this one under
//!   `docs/contributor/adr/0010-complete-development-bridge-strategy.md`.
//!
//! The server side of a session — the listener, the rendezvous file and the
//! editor tools — lives in `godot-mcp-server`. No native gdext artifact ships.
//!
//! ## Architectural boundary (frozen by Todo 1)
//!
//! Depends only on `godot-mcp-domain`, `godot-mcp-analysis` and
//! `godot-mcp-engine`. Must never import a transport or UI layer crate
//! (`godot-mcp-server`, `godot-mcp-cli`); the `architecture_boundaries` test
//! enforces this at the manifest level.
//!
//! Todo 1 scaffolded the crate boundary; Todo 19 completed and froze the
//! protocol below. The catalog and mutation *services* that Todos 20–25 would
//! have added were cancelled, so this crate has no service layer.
//!
//! ## What Todo 19 froze
//!
//! [`protocol`] is the frozen wire contract this server would speak to an
//! editor-side client. It is complete and closed on its own terms — the
//! cancelled Todos 20–25 were to build on it without renegotiating it, and a
//! future plug-in would still find it frozen:
//!
//! | module | responsibility |
//! | --- | --- |
//! | [`protocol::ws`] | the minimal RFC 6455 subset, 4 MiB frame ceiling, masking rules |
//! | [`protocol::digest`] | base64 (RFC 4648) and SHA-1 (RFC 3174), for the opening handshake only |
//! | [`protocol::envelope`] | the versioned envelope: ten kinds, one fixed key set, canonical bytes |
//! | [`protocol::compat`] | the compatibility matrix: MAJOR, project object identity, generation |
//! | [`protocol::rendezvous`] | the mode-0600 file, the per-launch 256-bit secret, constant-time compare |
//! | [`protocol::handshake`] | the authenticated first message, and every way it is refused |
//! | [`protocol::listener`] | the loopback-only ephemeral listener and one session |
//! | [`protocol::mutation`] | `not_started \| committed \| unresolved`, and the Todo 6 quarantine handoff |
//! | [`protocol::harness`] | the pinned headless GDScript codec harness |
//!
//! Three rules hold across all of them: an editor-side client never sees an MCP
//! wire message, the listener never leaves loopback and never trusts loopback
//! alone, and a reconnect is never permission to replay an unresolved mutation.

#![forbid(unsafe_code)]

pub mod addon;
pub mod protocol;
pub mod runtime;

/// Version of the bridge wire protocol this side advertises to an editor-side
/// GDScript client.
///
/// One value, defined by [`protocol::compat::PROTOCOL_VERSION`], so the constant
/// Todo 1 introduced and the version the handshake actually negotiates cannot
/// drift apart.
pub const BRIDGE_PROTOCOL_VERSION: &str = protocol::compat::PROTOCOL_VERSION;
