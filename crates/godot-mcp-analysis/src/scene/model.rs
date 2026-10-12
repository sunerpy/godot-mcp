//! Typed records for Godot text scenes (`.tscn`) and text resources (`.tres`).
//!
//! Every type here is owned by this workspace and is a plain value: the records
//! carry [`Provenance`] plus 1-based source spans, and nothing in this module
//! opens a file, resolves a path against the filesystem or starts a process.
//! Path authority and reads stay with Todo 8's
//! [`ResolvedProject`](crate::project::ResolvedProject); resource-target
//! classification reuses Todo 8's frozen
//! [`ResourceTarget`](crate::project::ResourceTarget).
//!
//! ## Attribution
//!
//! The parsed vocabulary modelled here — the `[ext_resource type/path/id]`
//! identifier table, `ExtResource("id")` / `SubResource("id")` handles, the
//! `[node name/type/parent]` tree with `script =` bindings, `groups = [...]`
//! membership and `instance=` sub-scene instantiation, and the
//! `[connection signal/from/to/method]` wiring with `.` as the scene-root marker
//! — is adapted from the MIT-licensed `codegraph-rust` project, files
//! `crates/codegraph-resolve/src/frameworks/godot_scene.rs` and
//! `godot_resource.rs`. Copyright (c) 2026 sunerpy, MIT. See
//! `THIRD-PARTY-NOTICES.md` at the workspace root. Only behavior is adapted;
//! there is no build-time or runtime dependency on that checkout.

use godot_mcp_domain::contracts::Provenance;
use serde::{Deserialize, Serialize};

use crate::project::ResourceTarget;

/// Which text document grammar a record set came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    /// A `.tscn` text scene: a node tree plus its resources and connections.
    Scene,
    /// A `.tres` text resource: exactly one resource body, no node tree.
    Resource,
}

impl DocumentKind {
    /// Stable wire code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Scene => "scene",
            Self::Resource => "resource",
        }
    }
}

/// 1-based, inclusive line span of a document construct.
///
/// Columns are 1-based too, so a span can be handed to a human or an agent
/// without an off-by-one translation step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SceneSpan {
    pub(super) start_line: u32,
    pub(super) start_column: u32,
    pub(super) end_line: u32,
    pub(super) end_column: u32,
}

impl SceneSpan {
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

    /// Whether `line` falls inside this span.
    #[must_use]
    pub const fn contains_line(&self, line: u32) -> bool {
        self.start_line <= line && line <= self.end_line
    }
}

/// Class of problem surfaced for a text scene or resource.
///
/// Each variant names one *specific* defect: an unsupported or damaged construct
/// is never collapsed into a single generic bucket, and never silently dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SceneDiagnosticKind {
    /// A line opened with `[` but is not a well-formed `[section ...]` header.
    MalformedSectionHeader,
    /// A well-formed header is missing an attribute its section requires (an
    /// `ext_resource` without `id`, a `node` without `name`, a `sub_resource`
    /// without `type`, a `connection` without `method`).
    MissingRequiredAttribute,
    /// A section name this workspace assigns no meaning to. The section's text
    /// is retained so a newer engine's output is never lost.
    UnknownSection,
    /// A non-comment, non-header line inside a section has no `=`.
    MissingAssignment,
    /// A line assigns to an empty key.
    EmptyKey,
    /// A bracket/brace/paren-opened value was never closed before the next
    /// well-formed section header or the end of the document.
    UnterminatedValue,
    /// An `ExtResource("id")` / `SubResource("id")` handle names an identifier no
    /// declaration in this document defines. The handle stays queryable and
    /// unresolved rather than being mapped to a guessed target.
    UnknownResourceId,
    /// Two declarations in the same document claim one resource identifier. The
    /// first wins, matching the engine, and the later one is reported here.
    DuplicateResourceId,
    /// A scene declared no root node (no `[node]` without a `parent`).
    MissingSceneRoot,
    /// A scene declared more than one root node. The first wins and the later
    /// one is reported here.
    MultipleSceneRoots,
    /// A node's `parent` path names no node declared earlier in the document, so
    /// its place in the tree cannot be proven.
    MissingParentNode,
    /// Two nodes resolve to the same in-scene path. Both stay queryable.
    DuplicateNodePath,
    /// A `[connection]` endpoint names no node in this scene.
    DanglingConnectionEndpoint,
}

impl SceneDiagnosticKind {
    /// Stable wire code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::MalformedSectionHeader => "malformed_section_header",
            Self::MissingRequiredAttribute => "missing_required_attribute",
            Self::UnknownSection => "unknown_section",
            Self::MissingAssignment => "missing_assignment",
            Self::EmptyKey => "empty_key",
            Self::UnterminatedValue => "unterminated_value",
            Self::UnknownResourceId => "unknown_resource_id",
            Self::DuplicateResourceId => "duplicate_resource_id",
            Self::MissingSceneRoot => "missing_scene_root",
            Self::MultipleSceneRoots => "multiple_scene_roots",
            Self::MissingParentNode => "missing_parent_node",
            Self::DuplicateNodePath => "duplicate_node_path",
            Self::DanglingConnectionEndpoint => "dangling_connection_endpoint",
        }
    }
}

/// One document problem, always carrying its exact source span.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneDiagnostic {
    pub(super) kind: SceneDiagnosticKind,
    pub(super) span: SceneSpan,
    pub(super) message: String,
}

impl SceneDiagnostic {
    /// Problem class.
    #[must_use]
    pub const fn kind(&self) -> SceneDiagnosticKind {
        self.kind
    }

    /// Source span of the offending text.
    #[must_use]
    pub const fn span(&self) -> &SceneSpan {
        &self.span
    }

    /// Human-readable explanation.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Which identifier table a resource handle was written against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceRefKind {
    /// `ExtResource("id")`: an external file declared by `[ext_resource]`.
    ExtResource,
    /// `SubResource("id")`: an internal resource declared by `[sub_resource]`.
    SubResource,
}

impl ResourceRefKind {
    /// Stable wire code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ExtResource => "ext_resource",
            Self::SubResource => "sub_resource",
        }
    }
}

/// One use of an `ExtResource("id")` / `SubResource("id")` handle.
///
/// A handle whose identifier has no declaration in this document keeps
/// [`target`](Self::target) `None` and `Unresolved` provenance: the layer will
/// not invent a target for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceRef {
    pub(super) kind: ResourceRefKind,
    pub(super) id: String,
    pub(super) target: Option<ResourceTarget>,
    pub(super) span: SceneSpan,
    pub(super) provenance: Provenance,
}

impl ResourceRef {
    /// Which identifier table the handle was written against.
    #[must_use]
    pub const fn kind(&self) -> ResourceRefKind {
        self.kind
    }

    /// The identifier exactly as written inside the handle.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// What the handle resolves to, when a declaration proved it. Always `None`
    /// for a [`ResourceRefKind::SubResource`] (an internal resource names no
    /// project path) and for an identifier with no declaration.
    #[must_use]
    pub const fn target(&self) -> Option<&ResourceTarget> {
        self.target.as_ref()
    }

    /// Source span of the handle's own line.
    #[must_use]
    pub const fn span(&self) -> &SceneSpan {
        &self.span
    }

    /// Evidence for the handle.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// The modelled meaning of one `key = value` line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertyValue {
    /// The value is a resource handle.
    Resource(ResourceRef),
    /// The value is a `NodePath("...")`, carrying the path exactly as written.
    NodePath(String),
    /// The value's text is exactly known but this workspace assigns it no
    /// structural meaning. It stays queryable through
    /// [`Property::raw_value`].
    RetainedUnknown,
}

/// One retained `key = value` line inside a section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Property {
    pub(super) key: String,
    pub(super) raw_value: String,
    pub(super) value: PropertyValue,
    pub(super) span: SceneSpan,
    pub(super) provenance: Provenance,
}

impl Property {
    /// Key exactly as written.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Value exactly as written. A multi-line value keeps its newlines.
    #[must_use]
    pub fn raw_value(&self) -> &str {
        &self.raw_value
    }

    /// The modelled meaning of the value.
    #[must_use]
    pub const fn value(&self) -> &PropertyValue {
        &self.value
    }

    /// Source span, from the key's line through the last consumed line.
    #[must_use]
    pub const fn span(&self) -> &SceneSpan {
        &self.span
    }

    /// Evidence for the retained text.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// One `[ext_resource ...]` declaration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtResourceDecl {
    pub(super) id: Option<String>,
    pub(super) type_name: Option<String>,
    pub(super) uid: Option<String>,
    pub(super) target: ResourceTarget,
    pub(super) span: SceneSpan,
    pub(super) provenance: Provenance,
}

impl ExtResourceDecl {
    /// In-document identifier `ExtResource("...")` handles refer to, when
    /// declared.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// Declared resource type, when present.
    #[must_use]
    pub fn type_name(&self) -> Option<&str> {
        self.type_name.as_deref()
    }

    /// The `uid://…` handle of the referenced file, when present. Mapping a uid
    /// to a file needs the project-wide UID index, which this layer does not
    /// own, so the value is reported rather than resolved.
    #[must_use]
    pub fn uid(&self) -> Option<&str> {
        self.uid.as_deref()
    }

    /// What the declaration's `path` points at, classified with Todo 8's frozen
    /// rule.
    #[must_use]
    pub const fn target(&self) -> &ResourceTarget {
        &self.target
    }

    /// Source span of the declaration.
    #[must_use]
    pub const fn span(&self) -> &SceneSpan {
        &self.span
    }

    /// Evidence for the declaration.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// One `[sub_resource ...]` declaration: a resource internal to the document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubResourceDecl {
    pub(super) id: Option<String>,
    pub(super) type_name: Option<String>,
    pub(super) properties: Vec<Property>,
    pub(super) span: SceneSpan,
    pub(super) provenance: Provenance,
}

impl SubResourceDecl {
    /// In-document identifier `SubResource("...")` handles refer to.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// Declared resource type, when present.
    #[must_use]
    pub fn type_name(&self) -> Option<&str> {
        self.type_name.as_deref()
    }

    /// Retained `key = value` lines of the internal resource, in source order.
    #[must_use]
    pub fn properties(&self) -> &[Property] {
        &self.properties
    }

    /// Source span of the declaration header.
    #[must_use]
    pub const fn span(&self) -> &SceneSpan {
        &self.span
    }

    /// Evidence for the declaration.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// One group a node declares membership in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupMembership {
    pub(super) name: String,
    pub(super) span: SceneSpan,
    pub(super) provenance: Provenance,
}

impl GroupMembership {
    /// Group name exactly as written.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Source span of the declaring line.
    #[must_use]
    pub const fn span(&self) -> &SceneSpan {
        &self.span
    }

    /// Evidence for the membership.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// One `[node ...]` entry of a scene tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneNode {
    pub(super) name: Option<String>,
    pub(super) type_name: Option<String>,
    pub(super) parent_path: Option<String>,
    pub(super) node_path: Option<String>,
    pub(super) owner_path: Option<String>,
    pub(super) index: Option<String>,
    pub(super) unique_id: Option<String>,
    pub(super) node_paths: Option<String>,
    pub(super) instance: Option<ResourceRef>,
    pub(super) instance_placeholder: Option<ResourceTarget>,
    pub(super) script: Option<ResourceRef>,
    pub(super) groups: Vec<GroupMembership>,
    pub(super) properties: Vec<Property>,
    pub(super) extra_attributes: Vec<(String, String)>,
    pub(super) span: SceneSpan,
    /// Last line of the node's `key = value` block, used to decide whether the
    /// node sits inside a damaged region. Not part of the public surface.
    pub(super) block_end_line: u32,
    pub(super) provenance: Provenance,
}

impl SceneNode {
    /// Declared node name, when the header carried one.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Declared node type. `None` for an instanced node, which inherits its type
    /// from the scene it instances.
    #[must_use]
    pub fn type_name(&self) -> Option<&str> {
        self.type_name.as_deref()
    }

    /// The `parent` attribute exactly as written. `None` marks the scene root.
    #[must_use]
    pub fn parent_path(&self) -> Option<&str> {
        self.parent_path.as_deref()
    }

    /// Absolute in-scene path of this node (`.` for the root), when it could be
    /// computed. `None` for a node with no name, which is unaddressable.
    #[must_use]
    pub fn node_path(&self) -> Option<&str> {
        self.node_path.as_deref()
    }

    /// Whether this node is a scene root (it declares no `parent`).
    #[must_use]
    pub const fn is_root(&self) -> bool {
        self.parent_path.is_none()
    }

    /// The `owner` attribute exactly as written. Godot uses it to decide whether
    /// a node is saved as part of the scene.
    #[must_use]
    pub fn owner_path(&self) -> Option<&str> {
        self.owner_path.as_deref()
    }

    /// The `index` attribute exactly as written (sibling order).
    #[must_use]
    pub fn index(&self) -> Option<&str> {
        self.index.as_deref()
    }

    /// The `unique_id` attribute, present only in scenes saved by Godot 4.6 or
    /// later.
    #[must_use]
    pub fn unique_id(&self) -> Option<&str> {
        self.unique_id.as_deref()
    }

    /// The `node_paths` attribute exactly as written.
    #[must_use]
    pub fn node_paths(&self) -> Option<&str> {
        self.node_paths.as_deref()
    }

    /// The scene this node instances, when the header carried `instance=`.
    #[must_use]
    pub const fn instance(&self) -> Option<&ResourceRef> {
        self.instance.as_ref()
    }

    /// The deferred scene named by `instance_placeholder=`, when present. A
    /// placeholder is not a loaded instance.
    #[must_use]
    pub const fn instance_placeholder(&self) -> Option<&ResourceTarget> {
        self.instance_placeholder.as_ref()
    }

    /// The script attached by a `script =` line, when present.
    #[must_use]
    pub const fn script(&self) -> Option<&ResourceRef> {
        self.script.as_ref()
    }

    /// Groups this node declares membership in, in source order.
    #[must_use]
    pub fn groups(&self) -> &[GroupMembership] {
        &self.groups
    }

    /// Every retained `key = value` line of the node, in source order.
    #[must_use]
    pub fn properties(&self) -> &[Property] {
        &self.properties
    }

    /// Header attributes this workspace assigns no meaning to, retained
    /// verbatim.
    #[must_use]
    pub fn extra_attributes(&self) -> &[(String, String)] {
        &self.extra_attributes
    }

    /// Source span of the node's header line.
    #[must_use]
    pub const fn span(&self) -> &SceneSpan {
        &self.span
    }

    /// Evidence for the node record.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// One `[connection ...]` signal wiring.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Connection {
    pub(super) signal: Option<String>,
    pub(super) from_path: Option<String>,
    pub(super) to_path: Option<String>,
    pub(super) method: Option<String>,
    pub(super) from_resolved: bool,
    pub(super) to_resolved: bool,
    pub(super) extra_attributes: Vec<(String, String)>,
    pub(super) span: SceneSpan,
    pub(super) provenance: Provenance,
}

impl Connection {
    /// Emitting signal name.
    #[must_use]
    pub fn signal(&self) -> Option<&str> {
        self.signal.as_deref()
    }

    /// Emitting node path, exactly as written (`.` is the scene root).
    #[must_use]
    pub fn from_path(&self) -> Option<&str> {
        self.from_path.as_deref()
    }

    /// Receiving node path, exactly as written (`.` is the scene root).
    #[must_use]
    pub fn to_path(&self) -> Option<&str> {
        self.to_path.as_deref()
    }

    /// Handler method name on the receiving node.
    #[must_use]
    pub fn method(&self) -> Option<&str> {
        self.method.as_deref()
    }

    /// Whether [`from_path`](Self::from_path) named a node this document
    /// declares.
    #[must_use]
    pub const fn from_resolved(&self) -> bool {
        self.from_resolved
    }

    /// Whether [`to_path`](Self::to_path) named a node this document declares.
    #[must_use]
    pub const fn to_resolved(&self) -> bool {
        self.to_resolved
    }

    /// Header attributes this workspace assigns no meaning to (`binds`,
    /// `unbinds`, `flags`, …), retained verbatim.
    #[must_use]
    pub fn extra_attributes(&self) -> &[(String, String)] {
        &self.extra_attributes
    }

    /// Source span of the connection's header line.
    #[must_use]
    pub const fn span(&self) -> &SceneSpan {
        &self.span
    }

    /// Evidence for the wiring.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// The `[resource]` body of a text resource.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceBody {
    pub(super) script: Option<ResourceRef>,
    pub(super) properties: Vec<Property>,
    pub(super) span: SceneSpan,
    pub(super) provenance: Provenance,
}

impl ResourceBody {
    /// The script the resource is an instance of, when a `script =` line bound
    /// one.
    #[must_use]
    pub const fn script(&self) -> Option<&ResourceRef> {
        self.script.as_ref()
    }

    /// Every retained `key = value` line of the body, in source order.
    #[must_use]
    pub fn properties(&self) -> &[Property] {
        &self.properties
    }

    /// Source span of the `[resource]` header line.
    #[must_use]
    pub const fn span(&self) -> &SceneSpan {
        &self.span
    }

    /// Evidence for the body.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// A section whose name this workspace assigns no meaning to, retained verbatim
/// so a newer engine's output is queryable instead of lost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnknownSection {
    pub(super) name: String,
    pub(super) attributes: Vec<(String, String)>,
    pub(super) properties: Vec<Property>,
    pub(super) span: SceneSpan,
    pub(super) provenance: Provenance,
}

impl UnknownSection {
    /// Section name exactly as written.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Header attributes exactly as written.
    #[must_use]
    pub fn attributes(&self) -> &[(String, String)] {
        &self.attributes
    }

    /// Retained `key = value` lines of the section, in source order.
    #[must_use]
    pub fn properties(&self) -> &[Property] {
        &self.properties
    }

    /// Source span of the section header.
    #[must_use]
    pub const fn span(&self) -> &SceneSpan {
        &self.span
    }

    /// Evidence for the retained text.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// Everything this layer recovered from one text scene or resource.
///
/// A document is always returned when the bytes decoded: damage becomes
/// [`diagnostics`](Self::diagnostics) plus a downgraded
/// [`provenance`](Self::provenance), never a dropped result and never a
/// fabricated exact answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneDocument {
    pub(super) kind: DocumentKind,
    pub(super) source_path: String,
    pub(super) format_version: Option<u32>,
    pub(super) self_uid: Option<String>,
    pub(super) root_type: Option<String>,
    pub(super) descriptor_attributes: Vec<(String, String)>,
    pub(super) ext_resources: Vec<ExtResourceDecl>,
    pub(super) sub_resources: Vec<SubResourceDecl>,
    pub(super) nodes: Vec<SceneNode>,
    pub(super) connections: Vec<Connection>,
    pub(super) resource_body: Option<ResourceBody>,
    pub(super) unknown_sections: Vec<UnknownSection>,
    pub(super) diagnostics: Vec<SceneDiagnostic>,
    pub(super) provenance: Provenance,
}

impl SceneDocument {
    /// Which grammar the document was parsed as.
    #[must_use]
    pub const fn kind(&self) -> DocumentKind {
        self.kind
    }

    /// Path the source was read from, as supplied by the caller.
    #[must_use]
    pub fn source_path(&self) -> &str {
        &self.source_path
    }

    /// The descriptor's `format=` version, when present and numeric.
    #[must_use]
    pub const fn format_version(&self) -> Option<u32> {
        self.format_version
    }

    /// The document's own `uid://…` handle, when the descriptor carried one.
    #[must_use]
    pub fn self_uid(&self) -> Option<&str> {
        self.self_uid.as_deref()
    }

    /// The `[gd_resource type="..."]` type of a text resource.
    #[must_use]
    pub fn root_type(&self) -> Option<&str> {
        self.root_type.as_deref()
    }

    /// Descriptor attributes this workspace assigns no meaning to (including the
    /// deprecated `load_steps`), retained verbatim.
    #[must_use]
    pub fn descriptor_attributes(&self) -> &[(String, String)] {
        &self.descriptor_attributes
    }

    /// `[ext_resource]` declarations in source order.
    #[must_use]
    pub fn ext_resources(&self) -> &[ExtResourceDecl] {
        &self.ext_resources
    }

    /// `[sub_resource]` declarations in source order.
    #[must_use]
    pub fn sub_resources(&self) -> &[SubResourceDecl] {
        &self.sub_resources
    }

    /// `[node]` entries in source order.
    #[must_use]
    pub fn nodes(&self) -> &[SceneNode] {
        &self.nodes
    }

    /// `[connection]` entries in source order.
    #[must_use]
    pub fn connections(&self) -> &[Connection] {
        &self.connections
    }

    /// The `[resource]` body, for a text resource.
    #[must_use]
    pub const fn resource_body(&self) -> Option<&ResourceBody> {
        self.resource_body.as_ref()
    }

    /// Sections whose names carry no modelled meaning, retained verbatim.
    #[must_use]
    pub fn unknown_sections(&self) -> &[UnknownSection] {
        &self.unknown_sections
    }

    /// Problems found while parsing. Never silently discarded.
    #[must_use]
    pub fn diagnostics(&self) -> &[SceneDiagnostic] {
        &self.diagnostics
    }

    /// Whether the document parsed with any problem.
    #[must_use]
    pub fn is_partial(&self) -> bool {
        !self.diagnostics.is_empty()
    }

    /// Evidence for the document as a whole.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    /// The scene root: the first node declaring no `parent`.
    #[must_use]
    pub fn root(&self) -> Option<&SceneNode> {
        self.nodes.iter().find(|node| node.is_root())
    }

    /// The first node whose absolute in-scene path is `path`.
    #[must_use]
    pub fn node_at(&self, path: &str) -> Option<&SceneNode> {
        self.nodes
            .iter()
            .find(|node| node.node_path.as_deref() == Some(path))
    }

    /// Every node whose `parent` attribute is exactly `parent_path`, in source
    /// order.
    #[must_use]
    pub fn children_of(&self, parent_path: &str) -> Vec<&SceneNode> {
        self.nodes
            .iter()
            .filter(|node| node.parent_path.as_deref() == Some(parent_path))
            .collect()
    }

    /// Every `(node, script)` attachment relationship, in source order.
    #[must_use]
    pub fn script_attachments(&self) -> Vec<(&SceneNode, &ResourceRef)> {
        self.nodes
            .iter()
            .filter_map(|node| node.script.as_ref().map(|script| (node, script)))
            .collect()
    }

    /// Every `(node, group)` membership relationship, in source order.
    #[must_use]
    pub fn group_memberships(&self) -> Vec<(&SceneNode, &GroupMembership)> {
        self.nodes
            .iter()
            .flat_map(|node| node.groups.iter().map(move |group| (node, group)))
            .collect()
    }

    /// Every `(node, instanced scene)` relationship, in source order. A deferred
    /// [`SceneNode::instance_placeholder`] is deliberately excluded: it is not a
    /// loaded instance.
    #[must_use]
    pub fn scene_instances(&self) -> Vec<(&SceneNode, &ResourceRef)> {
        self.nodes
            .iter()
            .filter_map(|node| node.instance.as_ref().map(|instance| (node, instance)))
            .collect()
    }

    /// Every resource handle used anywhere in the document, in source order.
    #[must_use]
    pub fn resource_references(&self) -> Vec<&ResourceRef> {
        let mut out: Vec<&ResourceRef> = Vec::new();
        for sub in &self.sub_resources {
            out.extend(property_refs(&sub.properties));
        }
        for node in &self.nodes {
            if let Some(instance) = &node.instance {
                out.push(instance);
            }
            if let Some(script) = &node.script {
                out.push(script);
            }
            out.extend(property_refs(&node.properties));
        }
        if let Some(body) = &self.resource_body {
            if let Some(script) = &body.script {
                out.push(script);
            }
            out.extend(property_refs(&body.properties));
        }
        for section in &self.unknown_sections {
            out.extend(property_refs(&section.properties));
        }
        out
    }
}

fn property_refs(properties: &[Property]) -> impl Iterator<Item = &ResourceRef> {
    properties
        .iter()
        .filter_map(|property| match &property.value {
            PropertyValue::Resource(reference) => Some(reference),
            PropertyValue::NodePath(_) | PropertyValue::RetainedUnknown => None,
        })
}
