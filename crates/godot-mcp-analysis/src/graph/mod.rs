//! Godot semantic graph: cross-document references, impact and the honesty
//! model.
//!
//! This layer is the first that resolves relationships *between* documents. It
//! consumes the records Todo 7 ([`gdscript`](crate::gdscript)), Todo 8
//! ([`project`](crate::project)) and Todo 9 ([`scene`](crate::scene)) already
//! produced and derives a classified dependency graph plus the queries built on
//! it: [`SemanticGraph::references`], [`SemanticGraph::callers`],
//! [`SemanticGraph::impact`] and the [`SemanticGraph::dangling`] audit.
//!
//! ## Boundaries
//!
//! * No I/O and no path authority. Every byte was read by Todo 8's
//!   [`ResolvedProject`](crate::project::ResolvedProject); nothing here opens a
//!   file, resolves a path against the filesystem or starts a process, and Godot
//!   is never invoked.
//! * `res://` / `uid://` classification reuses Todo 8's frozen
//!   [`ResourceTarget`](crate::project::ResourceTarget) and its single
//!   [`resource_target`](crate::project) rule. There is no second resource-path
//!   implementation.
//! * Scene relationships reuse Todo 9's records verbatim
//!   ([`SceneDocument`](crate::scene::SceneDocument) node paths, script
//!   attachments, instances, connections); this layer adds the *cross-document*
//!   half only.
//! * Persisting the graph and watching for changes is Todo 11's work; exposing
//!   it as MCP tools is Todo 12's. Nothing here writes a store or registers a
//!   tool.
//!
//! ## Honesty model
//!
//! Three edge resolutions, and the choice between them is enforced by Todo 3's
//! [`Provenance`](godot_mcp_domain::contracts::Provenance) constructor rather
//! than by convention:
//!
//! | Resolution | Meaning | Evidence |
//! | --- | --- | --- |
//! | [`EdgeResolution::Exact`] | exactly one target, statically proven | `StaticIndex` / `Exact` |
//! | [`EdgeResolution::Inferred`] | every candidate this layer could see, ranked | `Heuristic` or `DynamicReference` / `Inferred` |
//! | [`EdgeResolution::Unresolved`] | the relationship exists, its target cannot be decided | `Unresolved` / `Unresolved` |
//!
//! Load-bearing consequences:
//!
//! * A GDScript call is **never** exact — a single visible candidate is not a
//!   proof that no other target exists at run time, so it is still inferred.
//! * An undecidable relationship is **emitted** as unresolved, never dropped: a
//!   missing edge would read as "no such relationship".
//! * `[sub_resource]` semantics are not modelled, so that relation is
//!   permanently unresolved and the limitation is disclosed in
//!   [`SemanticGraph::limitations`].
//! * [`DanglingKind::TargetAbsent`] and [`DanglingKind::TargetUndecidable`] are
//!   separate facts. Absence is only claimed when the caller supplied a file
//!   inventory to check against.
//! * Grammar and scope gaps are never hidden: [`LimitationKind`] states each one
//!   with the subjects that triggered it.
//!
//! ## Determinism
//!
//! [`SemanticGraph`] keeps nodes in a [`std::collections::BTreeMap`] and stores
//! edges, dangling entries, limitations and each edge's candidate list
//! pre-sorted by a total order over their own content, so no hash iteration
//! order can reach the bytes. [`SemanticGraph::to_canonical_json`] is therefore
//! byte-stable across repeated calls and across builds over the same input.
//!
//! ## Attribution
//!
//! The Godot relationship vocabulary — autoload singletons as project-level
//! names, `class_name` as a global script class, scene→script binding through
//! `script = ExtResource(...)`, `[connection method=...]` → handler function,
//! and the "dynamic GDScript dispatch is not statically resolvable" posture — is
//! adapted from the MIT-licensed `codegraph-rust` project, file
//! `crates/codegraph-resolve/src/frameworks/godot.rs`. Copyright (c) 2026
//! sunerpy, MIT. See `THIRD-PARTY-NOTICES.md` at the workspace root. Only
//! behavior is adapted; nothing links to, or reads from, that checkout.

mod builder;
mod model;

pub use builder::{GRAPH_SCHEMA, GraphError, SemanticGraphBuilder};
pub use model::{
    DanglingKind, DanglingReference, EdgeCandidate, EdgeKind, EdgeResolution, GraphEdge,
    GraphLimitation, GraphNode, GraphNodeId, GraphNodeKind, GraphSpan, LimitationKind,
    SemanticGraph,
};
