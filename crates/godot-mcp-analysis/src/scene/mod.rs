//! Godot text scene / text resource semantics.
//!
//! This module turns `.tscn` and `.tres` text into typed, workspace-owned
//! records: the section vocabulary, the external/internal resource identifier
//! tables, the node tree with parent, ownership and instance relationships,
//! script attachments, group memberships, properties, UIDs and signal
//! connections — each carrying a 1-based source span and honest
//! [`Provenance`](godot_mcp_domain::contracts::Provenance).
//!
//! ## Boundaries
//!
//! * Path authority and every byte read stay with Todo 8's
//!   [`ResolvedProject`](crate::project::ResolvedProject). This module adds no
//!   second path implementation.
//! * Parsing is strictly read-only: no file is written, no process is spawned and
//!   Godot is never invoked.
//! * `res://` / `uid://` classification reuses Todo 8's frozen
//!   [`ResourceTarget`](crate::project::ResourceTarget), so a `uid://` handle
//!   stays unresolved until the (Todo 11) index exists and a path escaping the
//!   project root stays unsupported.
//! * The binary `.scn` / `.res` formats are refused as `unsupported`.
//! * Resolving relationships *across* documents — scene→script symbols,
//!   autoloads, uid lookups, impact and dangling audits — is Todo 10's and Todo
//!   11's work. This layer models one document at a time.

mod model;
mod parser;

pub use model::{
    Connection, DocumentKind, ExtResourceDecl, GroupMembership, Property, PropertyValue,
    ResourceBody, ResourceRef, ResourceRefKind, SceneDiagnostic, SceneDiagnosticKind,
    SceneDocument, SceneNode, SceneSpan, SubResourceDecl, UnknownSection,
};
pub use parser::{
    BINARY_RESOURCE_EXTENSION, BINARY_SCENE_EXTENSION, SceneError, TEXT_RESOURCE_EXTENSION,
    TEXT_SCENE_EXTENSION, parse_document, read_document,
};
