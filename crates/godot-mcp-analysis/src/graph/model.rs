//! Typed, workspace-owned records for the Godot semantic graph.
//!
//! Every type here is a plain value: nothing in this module opens a file,
//! resolves a path against the filesystem or starts a process. Path authority
//! and every byte read stay with Todo 8's
//! [`ResolvedProject`](crate::project::ResolvedProject); the document records the
//! graph is derived from stay owned by Todo 7's [`gdscript`](crate::gdscript)
//! and Todo 9's [`scene`](crate::scene) layers.
//!
//! ## Honesty model
//!
//! An edge is not a pointer. It is a *classified claim* about a relationship:
//!
//! * [`EdgeResolution::Exact`] — one target, statically proven.
//! * [`EdgeResolution::Inferred`] — every candidate this layer could see, each
//!   carrying its own evidence. A single candidate is **still** inferred: one
//!   visible candidate is not a proof that no other exists at run time.
//! * [`EdgeResolution::Unresolved`] — the relationship exists in the source but
//!   this layer cannot decide its target. The edge is still emitted, so the
//!   relationship is never silently absent.
//!
//! [`Provenance`]'s `permits` rule is the enforcement point: `Heuristic` /
//! `DynamicReference` evidence can only be labeled
//! [`Confidence::Inferred`](godot_mcp_domain::Confidence::Inferred) and
//! `Unresolved` evidence only
//! [`Confidence::Unresolved`](godot_mcp_domain::Confidence::Unresolved), so a
//! dishonest edge cannot be constructed at all.
//!
//! ## Attribution
//!
//! The relationship vocabulary modelled here — autoload singletons as
//! project-level nodes, `class_name` as a global script class, scene→script
//! binding through `script = ExtResource(...)`, `[connection method=...]` →
//! handler function, and the "dynamic dispatch cannot be resolved by syntax
//! alone" posture — is adapted from the MIT-licensed `codegraph-rust` project,
//! file `crates/codegraph-resolve/src/frameworks/godot.rs`. Copyright (c) 2026
//! sunerpy, MIT. See `THIRD-PARTY-NOTICES.md` at the workspace root. Only
//! behavior is adapted; there is no build-time or runtime dependency on that
//! checkout.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use godot_mcp_domain::Confidence;
use godot_mcp_domain::contracts::Provenance;
use serde::{Deserialize, Serialize};

use crate::gdscript::GdSpan;
use crate::scene::SceneSpan;

/// A stable, deterministic graph node identity.
///
/// The spelling is `kind:payload`, where the payload is built only from
/// project-relative paths and declared names, never from an absolute filesystem
/// path. Two runs over the same project bytes therefore produce the same ids in
/// the same order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct GraphNodeId(String);

impl GraphNodeId {
    /// Wrap an already-built identity spelling.
    #[must_use]
    pub fn new(spelling: impl Into<String>) -> Self {
        Self(spelling.into())
    }

    /// The identity spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Identity of the whole project (the owner of manifest-level facts).
    #[must_use]
    pub fn project() -> Self {
        Self("project:.".to_owned())
    }

    /// Identity of one `.gd` script file, by project-relative path.
    #[must_use]
    pub fn script(path: &str) -> Self {
        Self(format!("script:{path}"))
    }

    /// Identity of one `.tscn` text scene, by project-relative path.
    #[must_use]
    pub fn scene(path: &str) -> Self {
        Self(format!("scene:{path}"))
    }

    /// Identity of one `.tres` text resource, by project-relative path.
    #[must_use]
    pub fn text_resource(path: &str) -> Self {
        Self(format!("resource:{path}"))
    }

    /// Identity of a global script class registered by `class_name`.
    #[must_use]
    pub fn script_class(name: &str) -> Self {
        Self(format!("class:{name}"))
    }

    /// Identity of a function, qualified by its declaring file and owner.
    #[must_use]
    pub fn function(path: &str, owner: Option<&str>, name: &str) -> Self {
        match owner {
            Some(owner) => Self(format!("function:{path}::{owner}.{name}")),
            None => Self(format!("function:{path}::{name}")),
        }
    }

    /// Identity of a declared signal, qualified by its declaring file and owner.
    #[must_use]
    pub fn signal(path: &str, owner: Option<&str>, name: &str) -> Self {
        match owner {
            Some(owner) => Self(format!("signal:{path}::{owner}.{name}")),
            None => Self(format!("signal:{path}::{name}")),
        }
    }

    /// Identity of an `[autoload]` singleton, by its global name.
    #[must_use]
    pub fn autoload(name: &str) -> Self {
        Self(format!("autoload:{name}"))
    }

    /// Identity of one `[node]` entry inside a scene document.
    #[must_use]
    pub fn scene_node(path: &str, node_path: &str) -> Self {
        Self(format!("scene_node:{path}::{node_path}"))
    }

    /// Identity of one `[sub_resource]` declaration inside a document.
    #[must_use]
    pub fn sub_resource(path: &str, id: &str) -> Self {
        Self(format!("sub_resource:{path}::{id}"))
    }

    /// Identity of one `[input]` action, by its action name.
    #[must_use]
    pub fn input_action(name: &str) -> Self {
        Self(format!("input_action:{name}"))
    }

    /// Identity of any other in-project file (a texture, a font, a plugin
    /// descriptor): a referenced asset this layer does not parse.
    #[must_use]
    pub fn asset(path: &str) -> Self {
        Self(format!("asset:{path}"))
    }
}

impl std::fmt::Display for GraphNodeId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// What kind of project construct a graph node stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphNodeKind {
    /// The project itself: the owner of `project.godot` facts.
    Project,
    /// A `.gd` script file.
    Script,
    /// A `.tscn` text scene.
    Scene,
    /// A `.tres` text resource.
    TextResource,
    /// A global script class registered by `class_name`.
    ScriptClass,
    /// A `func` declaration.
    Function,
    /// A `signal` declaration.
    Signal,
    /// An `[autoload]` singleton.
    Autoload,
    /// One `[node]` entry of a scene tree.
    SceneNode,
    /// One `[sub_resource]` declaration, internal to a document.
    SubResource,
    /// One `[input]` action name.
    InputAction,
    /// Any other in-project file this layer does not parse.
    Asset,
}

impl GraphNodeKind {
    /// Stable wire code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Script => "script",
            Self::Scene => "scene",
            Self::TextResource => "text_resource",
            Self::ScriptClass => "script_class",
            Self::Function => "function",
            Self::Signal => "signal",
            Self::Autoload => "autoload",
            Self::SceneNode => "scene_node",
            Self::SubResource => "sub_resource",
            Self::InputAction => "input_action",
            Self::Asset => "asset",
        }
    }
}

/// 1-based, inclusive line/column span of a graph fact inside its source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GraphSpan {
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
}

impl GraphSpan {
    /// Construct a span from already-1-based line/column pairs.
    #[must_use]
    pub const fn new(start_line: u32, start_column: u32, end_line: u32, end_column: u32) -> Self {
        Self {
            start_line,
            start_column,
            end_line,
            end_column,
        }
    }

    /// Adopt a Todo 9 scene span unchanged.
    #[must_use]
    pub const fn from_scene(span: &SceneSpan) -> Self {
        Self::new(
            span.start_line(),
            span.start_column(),
            span.end_line(),
            span.end_column(),
        )
    }

    /// Adopt a Todo 7 GDScript span unchanged (line/column only).
    #[must_use]
    pub const fn from_gdscript(span: &GdSpan) -> Self {
        Self::new(
            span.start_line(),
            span.start_column(),
            span.end_line(),
            span.end_column(),
        )
    }

    /// First line of the construct (1-based).
    #[must_use]
    pub const fn start_line(&self) -> u32 {
        self.start_line
    }

    /// First column of the construct (1-based).
    #[must_use]
    pub const fn start_column(&self) -> u32 {
        self.start_column
    }

    /// Last line of the construct (1-based, inclusive).
    #[must_use]
    pub const fn end_line(&self) -> u32 {
        self.end_line
    }

    /// One past the last column of the construct (1-based).
    #[must_use]
    pub const fn end_column(&self) -> u32 {
        self.end_column
    }
}

/// One node of the semantic graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphNode {
    pub(super) id: GraphNodeId,
    pub(super) kind: GraphNodeKind,
    pub(super) name: String,
    pub(super) file: Option<String>,
    pub(super) span: Option<GraphSpan>,
    pub(super) provenance: Provenance,
}

impl GraphNode {
    /// Stable identity.
    #[must_use]
    pub const fn id(&self) -> &GraphNodeId {
        &self.id
    }

    /// What project construct this node stands for.
    #[must_use]
    pub const fn kind(&self) -> GraphNodeKind {
        self.kind
    }

    /// Display name: a declared name, or a project-relative path for a file.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Project-relative file the node was declared in, when it has one.
    #[must_use]
    pub fn file(&self) -> Option<&str> {
        self.file.as_deref()
    }

    /// Source span of the declaration, when the source layer supplied one.
    #[must_use]
    pub const fn span(&self) -> Option<&GraphSpan> {
        self.span.as_ref()
    }

    /// Evidence for the node's existence.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// What relationship an edge asserts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// `extends <base>` — a script's base type.
    Inherits,
    /// A GDScript call site.
    Calls,
    /// `preload("res://…")` — a compile-time resource reference.
    Preloads,
    /// `load("res://…")` — a run-time resource reference.
    Loads,
    /// `project.godot` registers an `[autoload]` global.
    AutoloadRegistration,
    /// An `[autoload]` global is backed by a script or scene file.
    AutoloadScript,
    /// `application/run/main_scene` names the boot scene.
    MainScene,
    /// `[editor_plugins] enabled` names a plugin descriptor.
    EnabledPlugin,
    /// `project.godot` declares an `[input]` action.
    InputActionDeclaration,
    /// A scene node's `parent` relationship.
    SceneParent,
    /// A scene node instances another scene (`instance=ExtResource(...)`).
    SceneInstance,
    /// A scene node defers another scene (`instance_placeholder=`). A
    /// placeholder is deliberately *not* a loaded instance.
    ScenePlaceholder,
    /// A scene node or resource body attaches a script (`script = …`).
    ScriptAttachment,
    /// A script declares a `signal`.
    SignalDeclaration,
    /// A `[connection]` wires a signal to a handler method.
    SignalConnection,
    /// A document references an external resource file.
    ResourceReference,
    /// A document property uses an internal `SubResource("id")`. What an
    /// internal sub-resource resolves to is not modelled by this layer, so the
    /// relation is always [`EdgeResolution::Unresolved`] — recorded, never
    /// absent and never exact.
    SubResourceRelation,
}

impl EdgeKind {
    /// Stable wire code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Inherits => "inherits",
            Self::Calls => "calls",
            Self::Preloads => "preloads",
            Self::Loads => "loads",
            Self::AutoloadRegistration => "autoload_registration",
            Self::AutoloadScript => "autoload_script",
            Self::MainScene => "main_scene",
            Self::EnabledPlugin => "enabled_plugin",
            Self::InputActionDeclaration => "input_action_declaration",
            Self::SceneParent => "scene_parent",
            Self::SceneInstance => "scene_instance",
            Self::ScenePlaceholder => "scene_placeholder",
            Self::ScriptAttachment => "script_attachment",
            Self::SignalDeclaration => "signal_declaration",
            Self::SignalConnection => "signal_connection",
            Self::ResourceReference => "resource_reference",
            Self::SubResourceRelation => "sub_resource_relation",
        }
    }
}

/// How certainly an edge's target is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeResolution {
    /// Exactly one target, statically proven.
    Exact,
    /// Every candidate this layer could see. A lone candidate stays inferred.
    Inferred,
    /// The relationship exists but its target cannot be decided statically.
    Unresolved,
}

impl EdgeResolution {
    /// Stable wire code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Inferred => "inferred",
            Self::Unresolved => "unresolved",
        }
    }

    /// The confidence this resolution corresponds to.
    #[must_use]
    pub const fn confidence(self) -> Confidence {
        match self {
            Self::Exact => Confidence::Exact,
            Self::Inferred => Confidence::Inferred,
            Self::Unresolved => Confidence::Unresolved,
        }
    }

    /// The strictly less certain of the two. Used to inherit a source record's
    /// downgrade: an edge derived from a damaged region can never be exact.
    #[must_use]
    pub const fn weaker(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unresolved, _) | (_, Self::Unresolved) => Self::Unresolved,
            (Self::Inferred, _) | (_, Self::Inferred) => Self::Inferred,
            (Self::Exact, Self::Exact) => Self::Exact,
        }
    }
}

/// One possible target of an edge, with its own evidence.
///
/// A candidate whose [`target`](Self::target) is `None` names something this
/// layer could not map to a graph node; the [`hint`](Self::hint) still records
/// exactly what the source said, so the fact is queryable rather than lost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeCandidate {
    pub(super) rank: u32,
    pub(super) target: Option<GraphNodeId>,
    pub(super) hint: String,
    pub(super) provenance: Provenance,
}

impl EdgeCandidate {
    /// Preference order among the candidates of one edge: a lower rank had
    /// stronger evidence. A rank is **not** a probability and a rank-0
    /// candidate is still only a candidate.
    #[must_use]
    pub const fn rank(&self) -> u32 {
        self.rank
    }

    /// The graph node this candidate names, when one was found.
    #[must_use]
    pub const fn target(&self) -> Option<&GraphNodeId> {
        self.target.as_ref()
    }

    /// What the source wrote, verbatim enough to be checked by a human.
    #[must_use]
    pub fn hint(&self) -> &str {
        &self.hint
    }

    /// Evidence for this individual candidate.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    fn sort_key(&self) -> (u32, &str, &str) {
        (
            self.rank,
            &self.hint,
            self.target.as_ref().map_or("", GraphNodeId::as_str),
        )
    }
}

/// One classified relationship of the semantic graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphEdge {
    pub(super) kind: EdgeKind,
    pub(super) source: GraphNodeId,
    pub(super) resolution: EdgeResolution,
    pub(super) candidates: Vec<EdgeCandidate>,
    pub(super) file: String,
    pub(super) span: Option<GraphSpan>,
    pub(super) detail: String,
    pub(super) provenance: Provenance,
}

impl GraphEdge {
    /// What relationship the edge asserts.
    #[must_use]
    pub const fn kind(&self) -> EdgeKind {
        self.kind
    }

    /// The node the relationship starts from.
    #[must_use]
    pub const fn source(&self) -> &GraphNodeId {
        &self.source
    }

    /// How certainly the target is known.
    #[must_use]
    pub const fn resolution(&self) -> EdgeResolution {
        self.resolution
    }

    /// Every candidate target, in a deterministic order.
    ///
    /// For [`EdgeResolution::Exact`] there is exactly one. For
    /// [`EdgeResolution::Inferred`] there is at least one and the list is
    /// complete as far as this layer could see. For
    /// [`EdgeResolution::Unresolved`] the list may be empty or may carry hints
    /// with no mapped node.
    #[must_use]
    pub fn candidates(&self) -> &[EdgeCandidate] {
        &self.candidates
    }

    /// The single proven target, and only for an exact edge. An inferred edge
    /// deliberately has no "the" target, even with one candidate.
    #[must_use]
    pub fn exact_target(&self) -> Option<&GraphNodeId> {
        if self.resolution != EdgeResolution::Exact {
            return None;
        }
        match self.candidates.as_slice() {
            [only] => only.target.as_ref(),
            _ => None,
        }
    }

    /// Project-relative file the relationship was written in.
    #[must_use]
    pub fn file(&self) -> &str {
        &self.file
    }

    /// Source span of the relationship, when the source layer supplied one.
    #[must_use]
    pub const fn span(&self) -> Option<&GraphSpan> {
        self.span.as_ref()
    }

    /// Human-readable description of what the source wrote.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// Evidence for the edge as a whole.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    /// Whether any candidate names `target`.
    #[must_use]
    pub fn targets(&self, target: &GraphNodeId) -> bool {
        self.candidates
            .iter()
            .any(|candidate| candidate.target.as_ref() == Some(target))
    }

    fn sort_key(&self) -> (&'static str, &str, u32, u32, &str) {
        (
            self.kind.code(),
            self.source.as_str(),
            self.span.map_or(0, |span| span.start_line),
            self.span.map_or(0, |span| span.start_column),
            &self.detail,
        )
    }
}

/// Why a reference could not be satisfied.
///
/// The two variants are deliberately distinct: "the target really is not there"
/// is an actionable defect, while "this layer cannot decide" is a statement
/// about the analyzer, not about the project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DanglingKind {
    /// The target was named precisely and provably is not present in the
    /// indexed project.
    TargetAbsent,
    /// The target cannot be decided at this static stage: an engine-provided
    /// base type, an unindexed `uid://`, a path escaping the project root, or a
    /// relation this layer does not model.
    TargetUndecidable,
}

impl DanglingKind {
    /// Stable wire code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::TargetAbsent => "target_absent",
            Self::TargetUndecidable => "target_undecidable",
        }
    }
}

/// One audited reference that did not reach a proven target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DanglingReference {
    pub(super) kind: DanglingKind,
    pub(super) edge_kind: EdgeKind,
    pub(super) source: GraphNodeId,
    pub(super) hint: String,
    pub(super) file: String,
    pub(super) span: Option<GraphSpan>,
    pub(super) reason: String,
}

impl DanglingReference {
    /// Whether the target is absent or merely undecidable.
    #[must_use]
    pub const fn kind(&self) -> DanglingKind {
        self.kind
    }

    /// Which relationship failed to resolve.
    #[must_use]
    pub const fn edge_kind(&self) -> EdgeKind {
        self.edge_kind
    }

    /// The node the failed relationship starts from.
    #[must_use]
    pub const fn source(&self) -> &GraphNodeId {
        &self.source
    }

    /// What the source wrote as the target.
    #[must_use]
    pub fn hint(&self) -> &str {
        &self.hint
    }

    /// Project-relative file the reference was written in.
    #[must_use]
    pub fn file(&self) -> &str {
        &self.file
    }

    /// Source span of the reference.
    #[must_use]
    pub const fn span(&self) -> Option<&GraphSpan> {
        self.span.as_ref()
    }

    /// Explanation suitable for a human or an agent.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    fn sort_key(&self) -> (&'static str, &'static str, &str, &str, u32) {
        (
            self.kind.code(),
            self.edge_kind.code(),
            self.source.as_str(),
            &self.hint,
            self.span.map_or(0, |span| span.start_line),
        )
    }
}

/// A grammar or scope limitation of this layer, always disclosed.
///
/// These are not diagnostics about the project: they are statements about what
/// the analyzer *cannot* see. Hiding them would make an incomplete graph look
/// complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitationKind {
    /// `load(expr)` / `preload(expr)` with a non-literal path is not recorded by
    /// the syntax layer, so such resource edges are missing entirely.
    DynamicResourcePathNotRecorded,
    /// Call arguments are not retained, so string-keyed relationships
    /// (`Input.is_action_pressed("jump")`, `get_node("Path")`,
    /// `connect("sig", …)`) cannot be turned into edges.
    CallArgumentsNotRecorded,
    /// No engine class registry is available, so an `extends` naming a built-in
    /// type cannot be distinguished from a typo.
    EngineClassRegistryUnavailable,
    /// What an internal `[sub_resource]` resolves to is not modelled.
    SubResourceSemanticsUnsupported,
    /// At least one input document parsed only partially; relationships inside
    /// the damaged region were not derived.
    DamagedRegionNotTraversed,
    /// A `uid://` handle cannot be mapped without the project-wide UID index,
    /// and the documents supplied to this build did not declare it.
    UidIndexIncomplete,
}

impl LimitationKind {
    /// Stable wire code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::DynamicResourcePathNotRecorded => "dynamic_resource_path_not_recorded",
            Self::CallArgumentsNotRecorded => "call_arguments_not_recorded",
            Self::EngineClassRegistryUnavailable => "engine_class_registry_unavailable",
            Self::SubResourceSemanticsUnsupported => "sub_resource_semantics_unsupported",
            Self::DamagedRegionNotTraversed => "damaged_region_not_traversed",
            Self::UidIndexIncomplete => "uid_index_incomplete",
        }
    }

    /// Fixed explanation of the limitation.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::DynamicResourcePathNotRecorded => {
                "a load()/preload() whose path is a computed expression is not recorded by the syntax layer, so no resource edge exists for it"
            }
            Self::CallArgumentsNotRecorded => {
                "call arguments are not retained, so string-keyed relationships such as Input.is_action_pressed(\"jump\"), get_node(\"Path\") and connect(\"signal\", ...) produce no edge"
            }
            Self::EngineClassRegistryUnavailable => {
                "no engine class registry is available, so an `extends` naming a built-in type stays unresolved rather than being confirmed"
            }
            Self::SubResourceSemanticsUnsupported => {
                "what an internal [sub_resource] resolves to is not modelled; the relation is recorded as unresolved"
            }
            Self::DamagedRegionNotTraversed => {
                "at least one supplied document parsed only partially; relationships inside the damaged region were not derived"
            }
            Self::UidIndexIncomplete => {
                "a uid:// handle was seen whose declaring document was not supplied to this build, so it cannot be mapped"
            }
        }
    }
}

/// One disclosed limitation, with the subjects that triggered it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphLimitation {
    pub(super) kind: LimitationKind,
    pub(super) subjects: Vec<String>,
}

impl GraphLimitation {
    /// Which limitation this is.
    #[must_use]
    pub const fn kind(&self) -> LimitationKind {
        self.kind
    }

    /// Fixed explanation of the limitation.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        self.kind.message()
    }

    /// Sorted, de-duplicated subjects (files or names) that triggered it.
    #[must_use]
    pub fn subjects(&self) -> &[String] {
        &self.subjects
    }
}

/// The whole Godot semantic graph for one project generation.
///
/// Serialization is byte-stable: nodes live in a [`BTreeMap`], and edges,
/// dangling references and limitations are stored pre-sorted by a total order
/// over their own content, so no [`std::collections::HashMap`] iteration order
/// can leak into the bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticGraph {
    pub(super) schema: String,
    pub(super) project_generation: u64,
    pub(super) nodes: BTreeMap<GraphNodeId, GraphNode>,
    pub(super) edges: Vec<GraphEdge>,
    pub(super) dangling: Vec<DanglingReference>,
    pub(super) limitations: Vec<GraphLimitation>,
    pub(super) provenance: Provenance,
}

impl SemanticGraph {
    /// Stable schema identity of the serialized graph.
    #[must_use]
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// Project generation every record is attributed to.
    #[must_use]
    pub const fn project_generation(&self) -> u64 {
        self.project_generation
    }

    /// Every node, ordered by identity.
    #[must_use]
    pub const fn nodes(&self) -> &BTreeMap<GraphNodeId, GraphNode> {
        &self.nodes
    }

    /// One node by identity.
    #[must_use]
    pub fn node(&self, id: &GraphNodeId) -> Option<&GraphNode> {
        self.nodes.get(id)
    }

    /// Every edge, in the graph's deterministic order.
    #[must_use]
    pub fn edges(&self) -> &[GraphEdge] {
        &self.edges
    }

    /// The dangling-reference audit, in deterministic order.
    #[must_use]
    pub fn dangling(&self) -> &[DanglingReference] {
        &self.dangling
    }

    /// Dangling references of one class only.
    #[must_use]
    pub fn dangling_of(&self, kind: DanglingKind) -> Vec<&DanglingReference> {
        self.dangling
            .iter()
            .filter(|entry| entry.kind == kind)
            .collect()
    }

    /// Disclosed grammar/scope limitations, in deterministic order.
    #[must_use]
    pub fn limitations(&self) -> &[GraphLimitation] {
        &self.limitations
    }

    /// Whether `kind` was disclosed for this graph.
    #[must_use]
    pub fn has_limitation(&self, kind: LimitationKind) -> bool {
        self.limitations
            .iter()
            .any(|limitation| limitation.kind == kind)
    }

    /// Evidence for the graph as a whole. `Exact` only when every input parsed
    /// cleanly and no limitation had to be disclosed for a partial document.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    /// Every edge leaving `source`, in deterministic order.
    #[must_use]
    pub fn edges_from(&self, source: &GraphNodeId) -> Vec<&GraphEdge> {
        self.edges
            .iter()
            .filter(|edge| &edge.source == source)
            .collect()
    }

    /// Every edge naming `target` as a candidate, in deterministic order.
    ///
    /// This is the reference query: an inferred edge with five candidates is
    /// returned for each of the five, because each is a genuine possibility.
    #[must_use]
    pub fn references(&self, target: &GraphNodeId) -> Vec<&GraphEdge> {
        self.edges
            .iter()
            .filter(|edge| edge.targets(target))
            .collect()
    }

    /// Every call edge that may reach `target`, in deterministic order.
    #[must_use]
    pub fn callers(&self, target: &GraphNodeId) -> Vec<&GraphEdge> {
        self.edges
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Calls && edge.targets(target))
            .collect()
    }

    /// The transitive set of nodes that reach `target`, i.e. what a change to
    /// `target` could affect. Sorted, and never containing `target` itself
    /// unless a genuine cycle leads back to it.
    #[must_use]
    pub fn impact(&self, target: &GraphNodeId) -> Vec<GraphNodeId> {
        let mut seen: BTreeSet<GraphNodeId> = BTreeSet::new();
        let mut queue: VecDeque<GraphNodeId> = VecDeque::new();
        queue.push_back(target.clone());
        let mut visited: BTreeSet<GraphNodeId> = BTreeSet::new();
        visited.insert(target.clone());
        while let Some(current) = queue.pop_front() {
            for edge in self.edges.iter().filter(|edge| edge.targets(&current)) {
                if seen.insert(edge.source.clone()) && visited.insert(edge.source.clone()) {
                    queue.push_back(edge.source.clone());
                }
            }
        }
        seen.remove(target);
        seen.into_iter().collect()
    }

    /// Deterministic JSON rendering of the whole graph.
    ///
    /// Repeated calls on the same graph produce byte-identical output, and so do
    /// two builds over the same project bytes.
    ///
    /// # Errors
    /// Returns a [`serde_json::Error`] only if a record cannot be serialized,
    /// which would be a defect in this crate rather than a property of the
    /// input.
    pub fn to_canonical_json(&self) -> Result<String, serde_json::Error> {
        let mut rendered = serde_json::to_string_pretty(self)?;
        rendered.push('\n');
        Ok(rendered)
    }

    pub(super) fn finalize_order(&mut self) {
        self.edges.sort_by(|left, right| {
            left.sort_key()
                .cmp(&right.sort_key())
                .then_with(|| candidate_order(left).cmp(&candidate_order(right)))
        });
        for edge in &mut self.edges {
            edge.candidates
                .sort_by(|left, right| left.sort_key().cmp(&right.sort_key()));
        }
        self.edges.sort_by(|left, right| {
            left.sort_key()
                .cmp(&right.sort_key())
                .then_with(|| candidate_order(left).cmp(&candidate_order(right)))
        });
        self.dangling
            .sort_by(|left, right| left.sort_key().cmp(&right.sort_key()));
        self.limitations
            .sort_by(|left, right| left.kind.code().cmp(right.kind.code()));
        for limitation in &mut self.limitations {
            limitation.subjects.sort();
            limitation.subjects.dedup();
        }
    }
}

fn candidate_order(edge: &GraphEdge) -> Vec<(u32, String, String)> {
    edge.candidates
        .iter()
        .map(|candidate| {
            (
                candidate.rank,
                candidate.hint.clone(),
                candidate
                    .target
                    .as_ref()
                    .map_or_else(String::new, |target| target.as_str().to_owned()),
            )
        })
        .collect()
}
