//! # godot-mcp-analysis
//!
//! Self-contained Godot/GDScript static-intelligence layer: Tree-sitter GDScript
//! parsing plus `project.godot`, `.tscn`, `.tres`, autoload, input-action,
//! signal, script-attachment, scene-instance, resource/UID and dangling-reference
//! semantics. Behavior and fixtures are derived from the MIT-licensed
//! `codegraph-rust` project; there is **no** runtime dependency on that sibling
//! checkout.
//!
//! ## Architectural boundary (frozen by Todo 1)
//!
//! Depends only on `godot-mcp-domain`. Must never import a transport or UI layer
//! crate (`godot-mcp-server`, `godot-mcp-cli`); the `architecture_boundaries`
//! test enforces this at the manifest level.
//!
//! Todo 1 scaffolds the crate boundary only; the parser, indexer and
//! semantic-graph behavior are completed by Todos 7–11. Todo 7 lands the
//! self-contained GDScript syntax layer in [`gdscript`]; Todo 8 lands canonical
//! project identity plus `project.godot` semantics in [`project`]; Todo 9 lands
//! the `.tscn` / `.tres` structural relationship model in [`scene`]; Todo 10
//! lands the cross-document semantic graph, reference/impact queries and the
//! honesty model in [`graph`]; Todo 11 lands the lease-owned incremental index
//! and its filesystem-coherence model in [`index`].

#![forbid(unsafe_code)]

pub mod gdscript;
pub mod graph;
pub mod index;
pub mod project;
pub mod scene;

/// Version of the analysis-layer schema/index format.
pub const ANALYSIS_VERSION: &str = "0.1.0";

/// Re-export of the domain confidence model so analysis outputs can attach
/// provenance without duplicating the type.
pub use godot_mcp_domain::Confidence;
