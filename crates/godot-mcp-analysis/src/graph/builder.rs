//! Derivation of the Godot semantic graph from already-parsed documents.
//!
//! This module is the first layer that resolves relationships *across*
//! documents: a `class_name` registry, a `uid://` registry, autoload bindings,
//! scene→script attachment, `[connection]` → handler function, and
//! `preload`/`load` → resource file. It parses nothing itself and reads no
//! file: it consumes Todo 7's [`GdScriptFile`], Todo 8's [`ProjectSettings`] and
//! Todo 9's [`SceneDocument`] records.
//!
//! ## Honesty rules enforced here
//!
//! * A call site is **never** exact. Syntax cannot prove a GDScript callee, so a
//!   call edge lists every candidate it could see — one candidate included — and
//!   stays [`EdgeResolution::Inferred`].
//! * A relationship whose target cannot be decided is emitted as
//!   [`EdgeResolution::Unresolved`], never omitted. An omitted edge would read
//!   as "no such relationship exists".
//! * `[sub_resource]` semantics are not modelled, so the relation is recorded
//!   as unresolved and the limitation is disclosed.
//! * A damaged source region downgrades every edge derived from it: a record
//!   that Todo 7 or Todo 9 already marked
//!   [`Confidence::Inferred`](godot_mcp_domain::Confidence::Inferred) can never
//!   produce an exact edge.
//! * Whether a target is *absent* or merely *undecidable* is a distinction the
//!   audit keeps. Absence can only be claimed when the caller supplied a file
//!   inventory to check against.

use std::collections::{BTreeMap, BTreeSet};

use godot_mcp_domain::Confidence;
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{ContractViolation, Provenance, ProvenanceSource};

use super::model::{
    DanglingKind, DanglingReference, EdgeCandidate, EdgeKind, EdgeResolution, GraphEdge,
    GraphLimitation, GraphNode, GraphNodeId, GraphNodeKind, GraphSpan, LimitationKind,
    SemanticGraph,
};
use crate::gdscript::{GdCall, GdCallKind, GdScriptFile};
use crate::project::{ProjectSettings, ResourceTarget};
use crate::scene::{DocumentKind, ResourceRef, ResourceRefKind, SceneDocument};

/// Stable schema identity of the serialized graph.
pub const GRAPH_SCHEMA: &str = "godot-mcp.semantic-graph.v1";

/// A hard failure of the graph layer itself.
///
/// Damaged *input* never produces one of these: it produces a
/// [`SemanticGraph`] carrying weaker resolutions, dangling entries and disclosed
/// limitations. This type is reserved for a rejected input or a provenance value
/// this layer tried to emit dishonestly, which would be a defect here rather
/// than a property of the project.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct GraphError {
    kind: ErrorKind,
    message: String,
}

impl GraphError {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Stable outcome category for this failure.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Human-readable explanation.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<ContractViolation> for GraphError {
    fn from(violation: ContractViolation) -> Self {
        Self::new(violation.kind(), violation.to_string())
    }
}

/// Where a named target landed.
enum Resolved {
    /// A graph node was proven.
    Node(GraphNodeId),
    /// The target was named precisely and provably is not in the project.
    Absent(String),
    /// The target cannot be decided at this static stage.
    Undecidable(String),
}

/// Accumulates parsed documents and derives one [`SemanticGraph`].
///
/// The builder owns no path authority and performs no I/O; every input is a
/// record another layer already produced from bytes it was allowed to read.
#[derive(Debug, Default)]
pub struct SemanticGraphBuilder {
    generation: u64,
    settings: Option<ProjectSettings>,
    scripts: Vec<GdScriptFile>,
    documents: Vec<SceneDocument>,
    inventory: Option<BTreeSet<String>>,
}

impl SemanticGraphBuilder {
    /// Start a builder attributing every record to `generation`.
    #[must_use]
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            settings: None,
            scripts: Vec::new(),
            documents: Vec::new(),
            inventory: None,
        }
    }

    /// Supply the parsed `project.godot` manifest.
    #[must_use]
    pub fn with_settings(mut self, settings: ProjectSettings) -> Self {
        self.settings = Some(settings);
        self
    }

    /// Add one parsed `.gd` script. Its `source_path` is taken as the
    /// project-relative identity of the file.
    #[must_use]
    pub fn with_script(mut self, script: GdScriptFile) -> Self {
        self.scripts.push(script);
        self
    }

    /// Add one parsed `.tscn` / `.tres` document. Its `source_path` is taken as
    /// the project-relative identity of the file.
    #[must_use]
    pub fn with_document(mut self, document: SceneDocument) -> Self {
        self.documents.push(document);
        self
    }

    /// Declare the complete set of project-relative files that exist.
    ///
    /// Supplying an inventory is what makes
    /// [`DanglingKind::TargetAbsent`] provable. Without one, a target this layer
    /// cannot find is reported as [`DanglingKind::TargetUndecidable`], because
    /// "not supplied to the builder" is not the same fact as "not in the
    /// project".
    #[must_use]
    pub fn with_file_inventory<I, S>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.inventory = Some(paths.into_iter().map(Into::into).collect());
        self
    }

    /// Derive the graph.
    ///
    /// # Errors
    /// Returns [`GraphError`] only when a provenance value this layer wants to
    /// emit would be dishonest, or when an input record carries an empty
    /// `source_path`; both are defects here rather than properties of the
    /// project.
    pub fn build(self) -> Result<SemanticGraph, GraphError> {
        Derivation::new(self)?.run()
    }
}

/// One function declaration, indexed for candidate lookup.
#[derive(Debug, Clone)]
struct FunctionEntry {
    path: String,
    owner: Option<String>,
    id: GraphNodeId,
}

/// Everything derived for one build. Two passes: index, then edges.
struct Derivation {
    generation: u64,
    settings: Option<ProjectSettings>,
    scripts: Vec<GdScriptFile>,
    documents: Vec<SceneDocument>,
    inventory: Option<BTreeSet<String>>,
    nodes: BTreeMap<GraphNodeId, GraphNode>,
    edges: Vec<GraphEdge>,
    dangling: Vec<DanglingReference>,
    limitations: BTreeMap<LimitationKind, BTreeSet<String>>,
    /// `class_name` → declaring script paths, sorted. More than one entry means
    /// the project registers the name twice, which stays ambiguous rather than
    /// being silently collapsed.
    class_index: BTreeMap<String, Vec<String>>,
    /// `uid://…` → declaring document path.
    uid_index: BTreeMap<String, String>,
    /// function name → every declaration of that name.
    functions_by_name: BTreeMap<String, Vec<FunctionEntry>>,
    /// script path → top-level function names declared there.
    top_level_functions: BTreeMap<String, BTreeSet<String>>,
    /// script path → verbatim file-level `extends`.
    extends_by_path: BTreeMap<String, String>,
    /// autoload name → its declared target.
    autoload_targets: BTreeMap<String, ResourceTarget>,
}

impl Derivation {
    fn new(builder: SemanticGraphBuilder) -> Result<Self, GraphError> {
        for script in &builder.scripts {
            if script.source_path().trim().is_empty() {
                return Err(GraphError::new(
                    ErrorKind::InvalidInput,
                    "a script record carries an empty source path",
                ));
            }
        }
        for document in &builder.documents {
            if document.source_path().trim().is_empty() {
                return Err(GraphError::new(
                    ErrorKind::InvalidInput,
                    "a document record carries an empty source path",
                ));
            }
        }
        Ok(Self {
            generation: builder.generation,
            settings: builder.settings,
            scripts: builder.scripts,
            documents: builder.documents,
            inventory: builder.inventory,
            nodes: BTreeMap::new(),
            edges: Vec::new(),
            dangling: Vec::new(),
            limitations: BTreeMap::new(),
            class_index: BTreeMap::new(),
            uid_index: BTreeMap::new(),
            functions_by_name: BTreeMap::new(),
            top_level_functions: BTreeMap::new(),
            extends_by_path: BTreeMap::new(),
            autoload_targets: BTreeMap::new(),
        })
    }

    fn run(mut self) -> Result<SemanticGraph, GraphError> {
        self.index_nodes()?;
        self.derive_project_edges()?;
        self.derive_script_edges()?;
        self.derive_document_edges()?;
        self.disclose_standing_limitations();

        let partial = self.scripts.iter().any(GdScriptFile::is_partial)
            || self.documents.iter().any(SceneDocument::is_partial);
        let (source, confidence) = if partial {
            (ProvenanceSource::Heuristic, Confidence::Inferred)
        } else {
            (ProvenanceSource::StaticIndex, Confidence::Exact)
        };
        let provenance = self.provenance(source, confidence, "semantic graph")?;

        let limitations = self
            .limitations
            .into_iter()
            .map(|(kind, subjects)| GraphLimitation {
                kind,
                subjects: subjects.into_iter().collect(),
            })
            .collect();

        let mut graph = SemanticGraph {
            schema: GRAPH_SCHEMA.to_owned(),
            project_generation: self.generation,
            nodes: self.nodes,
            edges: self.edges,
            dangling: self.dangling,
            limitations,
            provenance,
        };
        graph.finalize_order();
        Ok(graph)
    }

    // -- provenance ---------------------------------------------------------

    fn provenance(
        &self,
        source: ProvenanceSource,
        confidence: Confidence,
        subject: impl Into<String>,
    ) -> Result<Provenance, GraphError> {
        Provenance::new(source, confidence, subject, self.generation).map_err(GraphError::from)
    }

    /// Evidence matching an edge resolution. `dynamic` selects
    /// `DynamicReference` over `Heuristic` for an inferred edge, so a runtime
    /// dispatch is distinguishable from a static heuristic.
    fn edge_provenance(
        &self,
        resolution: EdgeResolution,
        dynamic: bool,
        subject: impl Into<String>,
    ) -> Result<Provenance, GraphError> {
        let (source, confidence) = match resolution {
            EdgeResolution::Exact => (ProvenanceSource::StaticIndex, Confidence::Exact),
            EdgeResolution::Inferred if dynamic => {
                (ProvenanceSource::DynamicReference, Confidence::Inferred)
            }
            EdgeResolution::Inferred => (ProvenanceSource::Heuristic, Confidence::Inferred),
            EdgeResolution::Unresolved => (ProvenanceSource::Unresolved, Confidence::Unresolved),
        };
        self.provenance(source, confidence, subject)
    }

    fn metadata_provenance(&self, subject: impl Into<String>) -> Result<Provenance, GraphError> {
        self.provenance(
            ProvenanceSource::ProjectMetadata,
            Confidence::Exact,
            subject,
        )
    }

    fn syntax_provenance(
        &self,
        exact: bool,
        subject: impl Into<String>,
    ) -> Result<Provenance, GraphError> {
        if exact {
            self.provenance(ProvenanceSource::StaticSyntax, Confidence::Exact, subject)
        } else {
            self.provenance(ProvenanceSource::Heuristic, Confidence::Inferred, subject)
        }
    }

    // -- indexing -----------------------------------------------------------

    fn note(&mut self, kind: LimitationKind, subject: impl Into<String>) {
        self.limitations
            .entry(kind)
            .or_default()
            .insert(subject.into());
    }

    fn insert_node(
        &mut self,
        id: GraphNodeId,
        kind: GraphNodeKind,
        name: impl Into<String>,
        file: Option<String>,
        span: Option<GraphSpan>,
        provenance: Provenance,
    ) {
        self.nodes.entry(id.clone()).or_insert(GraphNode {
            id,
            kind,
            name: name.into(),
            file,
            span,
            provenance,
        });
    }

    #[allow(clippy::too_many_lines)]
    fn index_nodes(&mut self) -> Result<(), GraphError> {
        // The project node owns every manifest-level fact.
        if self.settings.is_some() {
            let provenance = self.metadata_provenance("project")?;
            self.insert_node(
                GraphNodeId::project(),
                GraphNodeKind::Project,
                ".",
                None,
                None,
                provenance,
            );
        }

        // Scripts, their global class registration, functions and signals.
        let scripts = std::mem::take(&mut self.scripts);
        for script in &scripts {
            let path = script.source_path().to_owned();
            let exact = !script.is_partial();
            let provenance = self.syntax_provenance(exact, format!("script {path}"))?;
            self.insert_node(
                GraphNodeId::script(&path),
                GraphNodeKind::Script,
                path.clone(),
                Some(path.clone()),
                None,
                provenance,
            );
            if let Some(extends) = script.extends() {
                self.extends_by_path
                    .insert(path.clone(), extends.to_owned());
            }
            if let Some(class_name) = script.class_name_declaration() {
                let provenance =
                    self.syntax_provenance(exact, format!("script class {class_name}"))?;
                self.insert_node(
                    GraphNodeId::script_class(class_name),
                    GraphNodeKind::ScriptClass,
                    class_name,
                    Some(path.clone()),
                    None,
                    provenance,
                );
                let entry = self.class_index.entry(class_name.to_owned()).or_default();
                entry.push(path.clone());
                entry.sort();
                entry.dedup();
            }
            for function in script.functions() {
                let owner = function.owner().map(str::to_owned);
                let id = GraphNodeId::function(&path, owner.as_deref(), function.name());
                let provenance = self.provenance(
                    function.provenance().source(),
                    function.provenance().confidence(),
                    format!("function {}", function.name()),
                )?;
                self.insert_node(
                    id.clone(),
                    GraphNodeKind::Function,
                    function.name(),
                    Some(path.clone()),
                    Some(GraphSpan::from_gdscript(function.span())),
                    provenance,
                );
                self.functions_by_name
                    .entry(function.name().to_owned())
                    .or_default()
                    .push(FunctionEntry {
                        path: path.clone(),
                        owner: owner.clone(),
                        id,
                    });
                if owner.is_none() {
                    self.top_level_functions
                        .entry(path.clone())
                        .or_default()
                        .insert(function.name().to_owned());
                }
            }
            for signal in script.signals() {
                let provenance = self.provenance(
                    signal.provenance().source(),
                    signal.provenance().confidence(),
                    format!("signal {}", signal.name()),
                )?;
                self.insert_node(
                    GraphNodeId::signal(&path, signal.owner(), signal.name()),
                    GraphNodeKind::Signal,
                    signal.name(),
                    Some(path.clone()),
                    Some(GraphSpan::from_gdscript(signal.span())),
                    provenance,
                );
            }
        }
        self.scripts = scripts;

        // Documents, their nodes and their internal sub-resources.
        let documents = std::mem::take(&mut self.documents);
        for document in &documents {
            let path = document.source_path().to_owned();
            let exact = !document.is_partial();
            let (id, kind) = match document.kind() {
                DocumentKind::Scene => (GraphNodeId::scene(&path), GraphNodeKind::Scene),
                DocumentKind::Resource => (
                    GraphNodeId::text_resource(&path),
                    GraphNodeKind::TextResource,
                ),
            };
            let provenance = self.provenance(
                if exact {
                    ProvenanceSource::ProjectMetadata
                } else {
                    ProvenanceSource::Heuristic
                },
                if exact {
                    Confidence::Exact
                } else {
                    Confidence::Inferred
                },
                format!("document {path}"),
            )?;
            self.insert_node(id, kind, path.clone(), Some(path.clone()), None, provenance);
            if let Some(uid) = document.self_uid() {
                self.uid_index.insert(uid.to_owned(), path.clone());
            }
            for node in document.nodes() {
                let Some(node_path) = node.node_path() else {
                    continue;
                };
                let provenance = self.provenance(
                    node.provenance().source(),
                    node.provenance().confidence(),
                    format!("scene node {node_path}"),
                )?;
                self.insert_node(
                    GraphNodeId::scene_node(&path, node_path),
                    GraphNodeKind::SceneNode,
                    node.name().unwrap_or(node_path),
                    Some(path.clone()),
                    Some(GraphSpan::from_scene(node.span())),
                    provenance,
                );
            }
            for sub in document.sub_resources() {
                let Some(sub_id) = sub.id() else {
                    continue;
                };
                let provenance = self.provenance(
                    sub.provenance().source(),
                    sub.provenance().confidence(),
                    format!("sub resource {sub_id}"),
                )?;
                self.insert_node(
                    GraphNodeId::sub_resource(&path, sub_id),
                    GraphNodeKind::SubResource,
                    sub.type_name().unwrap_or(sub_id),
                    Some(path.clone()),
                    Some(GraphSpan::from_scene(sub.span())),
                    provenance,
                );
            }
        }
        self.documents = documents;

        // Autoload singletons and input actions are project-level names.
        if let Some(settings) = self.settings.take() {
            for autoload in settings.autoloads() {
                let provenance = self.provenance(
                    autoload.provenance().source(),
                    autoload.provenance().confidence(),
                    format!("autoload {}", autoload.name()),
                )?;
                self.insert_node(
                    GraphNodeId::autoload(autoload.name()),
                    GraphNodeKind::Autoload,
                    autoload.name(),
                    Some(crate::project::PROJECT_MANIFEST.to_owned()),
                    None,
                    provenance,
                );
                self.autoload_targets
                    .insert(autoload.name().to_owned(), autoload.target().clone());
            }
            for action in settings.input_actions() {
                let provenance = self.provenance(
                    action.provenance().source(),
                    action.provenance().confidence(),
                    format!("input action {}", action.name()),
                )?;
                self.insert_node(
                    GraphNodeId::input_action(action.name()),
                    GraphNodeKind::InputAction,
                    action.name(),
                    Some(crate::project::PROJECT_MANIFEST.to_owned()),
                    None,
                    provenance,
                );
            }
            self.settings = Some(settings);
        }

        // Files the caller vouched for that this layer does not parse stay
        // addressable, so a texture or plugin descriptor reference can be exact.
        if let Some(inventory) = self.inventory.clone() {
            for path in inventory {
                let id = path_node_id(&path);
                if self.nodes.contains_key(&id) {
                    continue;
                }
                if matches!(node_kind_for_path(&path), GraphNodeKind::Asset) {
                    let provenance = self.metadata_provenance(format!("asset {path}"))?;
                    self.insert_node(
                        id,
                        GraphNodeKind::Asset,
                        path.clone(),
                        Some(path),
                        None,
                        provenance,
                    );
                }
            }
        }
        Ok(())
    }

    // -- resolution ---------------------------------------------------------

    /// Map a Todo 8 [`ResourceTarget`] onto a graph node, honestly.
    fn resolve_resource(&mut self, target: &ResourceTarget) -> Resolved {
        match target {
            ResourceTarget::ProjectRelative { path, .. } => self.resolve_path(path),
            ResourceTarget::Uid(uid) => match self.uid_index.get(uid).cloned() {
                Some(path) => {
                    let id = path_node_id(&path);
                    if self.nodes.contains_key(&id) {
                        Resolved::Node(id)
                    } else {
                        Resolved::Undecidable(uid.clone())
                    }
                }
                None => {
                    self.note(LimitationKind::UidIndexIncomplete, uid.clone());
                    Resolved::Undecidable(uid.clone())
                }
            },
            ResourceTarget::Unsupported(value) => Resolved::Undecidable(value.clone()),
        }
    }

    fn resolve_path(&self, path: &str) -> Resolved {
        let id = path_node_id(path);
        if self.nodes.contains_key(&id) {
            return Resolved::Node(id);
        }
        match &self.inventory {
            Some(inventory) if inventory.contains(path) => Resolved::Node(id),
            Some(_) => Resolved::Absent(path.to_owned()),
            None => Resolved::Undecidable(path.to_owned()),
        }
    }

    /// The chain of indexed script paths reachable from `path` through file-level
    /// `extends`, nearest base first.
    ///
    /// The chain always ends where this layer's view ends — a GDScript hierarchy
    /// ultimately derives from an engine class, which no static index here can
    /// confirm — so a name absent from the chain is *undecidable*, never proven
    /// absent.
    fn base_chain(&self, path: &str) -> Vec<String> {
        let mut chain: Vec<String> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        seen.insert(path.to_owned());
        let mut current = path.to_owned();
        loop {
            let Some(extends) = self.extends_by_path.get(&current) else {
                return chain;
            };
            let next = match self.class_index.get(extends) {
                Some(paths) if paths.len() == 1 => paths[0].clone(),
                Some(_) => return chain,
                None => {
                    let target = crate::project::resource_target(extends);
                    match target {
                        ResourceTarget::ProjectRelative { ref path, .. }
                            if self.nodes.contains_key(&GraphNodeId::script(path)) =>
                        {
                            path.clone()
                        }
                        _ => return chain,
                    }
                }
            };
            if !seen.insert(next.clone()) {
                return chain;
            }
            chain.push(next.clone());
            current = next;
        }
    }

    // -- edge emission ------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn push_edge(
        &mut self,
        kind: EdgeKind,
        source: GraphNodeId,
        resolution: EdgeResolution,
        candidates: Vec<EdgeCandidate>,
        file: String,
        span: Option<GraphSpan>,
        detail: String,
        dynamic: bool,
    ) -> Result<(), GraphError> {
        let provenance = self.edge_provenance(resolution, dynamic, detail.clone())?;
        self.edges.push(GraphEdge {
            kind,
            source,
            resolution,
            candidates,
            file,
            span,
            detail,
            provenance,
        });
        Ok(())
    }

    fn candidate(
        &self,
        rank: u32,
        target: Option<GraphNodeId>,
        hint: impl Into<String>,
        resolution: EdgeResolution,
        dynamic: bool,
    ) -> Result<EdgeCandidate, GraphError> {
        let hint = hint.into();
        let provenance = self.edge_provenance(resolution, dynamic, hint.clone())?;
        Ok(EdgeCandidate {
            rank,
            target,
            hint,
            provenance,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn push_dangling(
        &mut self,
        kind: DanglingKind,
        edge_kind: EdgeKind,
        source: GraphNodeId,
        hint: impl Into<String>,
        file: String,
        span: Option<GraphSpan>,
        reason: impl Into<String>,
    ) {
        self.dangling.push(DanglingReference {
            kind,
            edge_kind,
            source,
            hint: hint.into(),
            file,
            span,
            reason: reason.into(),
        });
    }

    /// Emit one single-target relationship whose target came from a
    /// [`ResourceTarget`], classifying it and auditing a failure.
    #[allow(clippy::too_many_arguments)]
    fn push_resource_edge(
        &mut self,
        kind: EdgeKind,
        source: GraphNodeId,
        target: &ResourceTarget,
        file: String,
        span: Option<GraphSpan>,
        detail: String,
        floor: EdgeResolution,
    ) -> Result<(), GraphError> {
        let resolved = self.resolve_resource(target);
        match resolved {
            Resolved::Node(node) => {
                let resolution = EdgeResolution::Exact.weaker(floor);
                let hint = node.as_str().to_owned();
                let candidate = self.candidate(0, Some(node), hint, resolution, false)?;
                self.push_edge(
                    kind,
                    source,
                    resolution,
                    vec![candidate],
                    file,
                    span,
                    detail,
                    false,
                )
            }
            Resolved::Absent(hint) => {
                let candidate =
                    self.candidate(0, None, hint.clone(), EdgeResolution::Unresolved, false)?;
                self.push_dangling(
                    DanglingKind::TargetAbsent,
                    kind,
                    source.clone(),
                    hint,
                    file.clone(),
                    span,
                    "the named project-relative file is not present in the supplied inventory",
                );
                self.push_edge(
                    kind,
                    source,
                    EdgeResolution::Unresolved,
                    vec![candidate],
                    file,
                    span,
                    detail,
                    false,
                )
            }
            Resolved::Undecidable(hint) => {
                let candidate =
                    self.candidate(0, None, hint.clone(), EdgeResolution::Unresolved, false)?;
                self.push_dangling(
                    DanglingKind::TargetUndecidable,
                    kind,
                    source.clone(),
                    hint,
                    file.clone(),
                    span,
                    "the target cannot be decided statically: an unindexed uid://, a path escaping the project root, or a file the caller did not vouch for",
                );
                self.push_edge(
                    kind,
                    source,
                    EdgeResolution::Unresolved,
                    vec![candidate],
                    file,
                    span,
                    detail,
                    false,
                )
            }
        }
    }

    // -- project.godot ------------------------------------------------------

    fn derive_project_edges(&mut self) -> Result<(), GraphError> {
        let Some(settings) = self.settings.clone() else {
            return Ok(());
        };
        let manifest = crate::project::PROJECT_MANIFEST.to_owned();
        let project = GraphNodeId::project();

        if let Some(main_scene) = settings.main_scene() {
            let floor = damage_ceiling(main_scene.provenance().confidence());
            let span = Some(GraphSpan::new(main_scene.line(), 1, main_scene.line(), 1));
            self.push_resource_edge(
                EdgeKind::MainScene,
                project.clone(),
                main_scene.target(),
                manifest.clone(),
                span,
                "application/run/main_scene".to_owned(),
                floor,
            )?;
        }

        let manifest_floor = damage_ceiling(settings.provenance().confidence());
        for autoload in settings.autoloads() {
            let span = Some(GraphSpan::new(autoload.line(), 1, autoload.line(), 1));
            let floor = damage_ceiling(autoload.provenance().confidence());
            let node = GraphNodeId::autoload(autoload.name());
            // The registration of the *name* is written verbatim in the
            // manifest; only the backing target can be uncertain.
            let resolution = EdgeResolution::Exact.weaker(manifest_floor);
            let candidate =
                self.candidate(0, Some(node.clone()), autoload.name(), resolution, false)?;
            self.push_edge(
                EdgeKind::AutoloadRegistration,
                project.clone(),
                resolution,
                vec![candidate],
                manifest.clone(),
                span,
                format!("[autoload] {}", autoload.name()),
                false,
            )?;
            self.push_resource_edge(
                EdgeKind::AutoloadScript,
                node,
                autoload.target(),
                manifest.clone(),
                span,
                format!(
                    "[autoload] {} -> {}",
                    autoload.name(),
                    if autoload.is_singleton() {
                        "singleton"
                    } else {
                        "node"
                    }
                ),
                floor,
            )?;
        }

        for plugin in settings.enabled_plugins() {
            let span = Some(GraphSpan::new(plugin.line(), 1, plugin.line(), 1));
            let floor = damage_ceiling(plugin.provenance().confidence());
            self.push_resource_edge(
                EdgeKind::EnabledPlugin,
                project.clone(),
                plugin.target(),
                manifest.clone(),
                span,
                "[editor_plugins] enabled".to_owned(),
                floor,
            )?;
        }

        for action in settings.input_actions() {
            let span = Some(GraphSpan::new(action.line(), 1, action.line(), 1));
            let resolution = EdgeResolution::Exact
                .weaker(manifest_floor)
                .weaker(damage_ceiling(action.provenance().confidence()));
            let candidate = self.candidate(
                0,
                Some(GraphNodeId::input_action(action.name())),
                action.name(),
                resolution,
                false,
            )?;
            self.push_edge(
                EdgeKind::InputActionDeclaration,
                project.clone(),
                resolution,
                vec![candidate],
                manifest.clone(),
                span,
                format!("[input] {}", action.name()),
                false,
            )?;
        }
        Ok(())
    }

    // -- GDScript ----------------------------------------------------------

    fn derive_script_edges(&mut self) -> Result<(), GraphError> {
        let scripts = std::mem::take(&mut self.scripts);
        for script in &scripts {
            let path = script.source_path().to_owned();
            let script_node = GraphNodeId::script(&path);
            let floor = damage_ceiling(script.provenance().confidence());

            self.derive_inherits(script, &path, &script_node, floor)?;

            for signal in script.signals() {
                let signal_floor = damage_ceiling(signal.provenance().confidence());
                let resolution = EdgeResolution::Exact.weaker(signal_floor);
                let target = GraphNodeId::signal(&path, signal.owner(), signal.name());
                let candidate =
                    self.candidate(0, Some(target), signal.name(), resolution, false)?;
                self.push_edge(
                    EdgeKind::SignalDeclaration,
                    script_node.clone(),
                    resolution,
                    vec![candidate],
                    path.clone(),
                    Some(GraphSpan::from_gdscript(signal.span())),
                    format!("signal {}", signal.name()),
                    false,
                )?;
            }

            for reference in script.preloads() {
                let kind = if reference.is_preload() {
                    EdgeKind::Preloads
                } else {
                    EdgeKind::Loads
                };
                let reference_floor = damage_ceiling(reference.provenance().confidence());
                let target = crate::project::resource_target(reference.path());
                let source = reference.enclosing_function().map_or_else(
                    || script_node.clone(),
                    |function| GraphNodeId::function(&path, None, function),
                );
                let source = if self.nodes.contains_key(&source) {
                    source
                } else {
                    script_node.clone()
                };
                self.push_resource_edge(
                    kind,
                    source,
                    &target,
                    path.clone(),
                    Some(GraphSpan::from_gdscript(reference.span())),
                    format!(
                        "{}(\"{}\")",
                        if reference.is_preload() {
                            "preload"
                        } else {
                            "load"
                        },
                        reference.path()
                    ),
                    reference_floor,
                )?;
            }

            for call in script.calls() {
                self.derive_call(script, &path, &script_node, call)?;
            }

            self.disclose_dynamic_resource_paths(script, &path);
        }
        self.scripts = scripts;
        Ok(())
    }

    fn derive_inherits(
        &mut self,
        script: &GdScriptFile,
        path: &str,
        script_node: &GraphNodeId,
        floor: EdgeResolution,
    ) -> Result<(), GraphError> {
        let Some(extends) = script.extends() else {
            return Ok(());
        };
        let detail = format!("extends {extends}");
        let span = None;

        // A `class_name` registration is a complete deterministic lookup, so a
        // single registration is exact and a double registration stays inferred
        // with both candidates.
        if let Some(paths) = self.class_index.get(extends).cloned() {
            if paths.len() == 1 {
                let resolution = EdgeResolution::Exact.weaker(floor);
                let candidate = self.candidate(
                    0,
                    Some(GraphNodeId::script(&paths[0])),
                    extends,
                    resolution,
                    false,
                )?;
                return self.push_edge(
                    EdgeKind::Inherits,
                    script_node.clone(),
                    resolution,
                    vec![candidate],
                    path.to_owned(),
                    span,
                    detail,
                    false,
                );
            }
            let mut candidates = Vec::with_capacity(paths.len());
            for (rank, declaring) in paths.iter().enumerate() {
                candidates.push(self.candidate(
                    u32::try_from(rank).unwrap_or(u32::MAX),
                    Some(GraphNodeId::script(declaring)),
                    format!("{extends} ({declaring})"),
                    EdgeResolution::Inferred,
                    false,
                )?);
            }
            return self.push_edge(
                EdgeKind::Inherits,
                script_node.clone(),
                EdgeResolution::Inferred,
                candidates,
                path.to_owned(),
                span,
                detail,
                false,
            );
        }

        // `extends "res://other.gd"` names a file rather than a class.
        let target = crate::project::resource_target(extends);
        if !matches!(target, ResourceTarget::Unsupported(_)) {
            return self.push_resource_edge(
                EdgeKind::Inherits,
                script_node.clone(),
                &target,
                path.to_owned(),
                span,
                detail,
                floor,
            );
        }

        // Anything else is either an engine built-in or a typo, and this layer
        // has no engine class registry to tell them apart.
        self.note(LimitationKind::EngineClassRegistryUnavailable, extends);
        let candidate = self.candidate(0, None, extends, EdgeResolution::Unresolved, false)?;
        self.push_dangling(
            DanglingKind::TargetUndecidable,
            EdgeKind::Inherits,
            script_node.clone(),
            extends,
            path.to_owned(),
            span,
            "no engine class registry is available, so a built-in base type cannot be distinguished from a missing script class",
        );
        self.push_edge(
            EdgeKind::Inherits,
            script_node.clone(),
            EdgeResolution::Unresolved,
            vec![candidate],
            path.to_owned(),
            span,
            detail,
            false,
        )
    }

    /// Derive one call edge.
    ///
    /// A call edge is never exact: GDScript dispatch depends on runtime types.
    /// Every visible declaration of the callee name becomes a ranked candidate,
    /// and a lone candidate is **not** promoted.
    fn derive_call(
        &mut self,
        script: &GdScriptFile,
        path: &str,
        script_node: &GraphNodeId,
        call: &GdCall,
    ) -> Result<(), GraphError> {
        // `preload` / `load` are modelled as resource relationships, not calls.
        if matches!(call.callee(), "preload" | "load") {
            return Ok(());
        }
        let source = call
            .enclosing_function()
            .map(|function| GraphNodeId::function(path, None, function))
            .filter(|id| self.nodes.contains_key(id))
            .unwrap_or_else(|| script_node.clone());
        let span = Some(GraphSpan::from_gdscript(call.span()));
        let detail = match call.receiver() {
            Some(receiver) => format!("{receiver}.{}()", call.callee()),
            None => match call.kind() {
                GdCallKind::Base => format!("super.{}()", call.callee()),
                _ => format!("{}()", call.callee()),
            },
        };

        let declarations = self
            .functions_by_name
            .get(call.callee())
            .cloned()
            .unwrap_or_default();

        // An autoload receiver narrows the search to that singleton's backing
        // script, which is the one dynamic form the project itself pins down.
        let autoload_target = call
            .receiver()
            .and_then(|receiver| self.autoload_targets.get(receiver).cloned());
        let autoload_scope = autoload_target.map(|target| match self.resolve_resource(&target) {
            Resolved::Node(node) => Some(node),
            Resolved::Absent(_) | Resolved::Undecidable(_) => None,
        });

        let chain = self.base_chain(path);
        let mut scored: Vec<(u32, FunctionEntry)> = Vec::new();
        for entry in declarations {
            let rank = if let Some(scope) = autoload_scope.as_ref() {
                match scope {
                    Some(node) if *node == GraphNodeId::script(&entry.path) => 0,
                    // The receiver *is* an autoload, so a declaration in an
                    // unrelated script is not a candidate for it.
                    _ => continue,
                }
            } else if entry.path == path {
                match call.kind() {
                    // `super.foo()` deliberately skips this script's own
                    // declaration: the base is what runs.
                    GdCallKind::Base => continue,
                    _ => 0,
                }
            } else if let Some(position) = chain.iter().position(|base| base == &entry.path) {
                1 + u32::try_from(position).unwrap_or(u32::MAX - 2)
            } else {
                match call.kind() {
                    // An unqualified call cannot reach an unrelated script.
                    GdCallKind::Local | GdCallKind::Base => continue,
                    GdCallKind::Qualified => u32::MAX - 1,
                }
            };
            scored.push((rank, entry));
        }
        scored.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.path.cmp(&right.1.path))
                .then_with(|| left.1.owner.cmp(&right.1.owner))
        });

        if scored.is_empty() {
            self.note(
                LimitationKind::EngineClassRegistryUnavailable,
                call.callee(),
            );
            let candidate =
                self.candidate(0, None, call.callee(), EdgeResolution::Unresolved, true)?;
            self.push_dangling(
                DanglingKind::TargetUndecidable,
                EdgeKind::Calls,
                source.clone(),
                call.callee(),
                path.to_owned(),
                span,
                "no declaration of this name is visible; it may be an engine API, an inherited built-in method, or a runtime-only member",
            );
            return self.push_edge(
                EdgeKind::Calls,
                source,
                EdgeResolution::Unresolved,
                vec![candidate],
                path.to_owned(),
                span,
                detail,
                true,
            );
        }

        let floor = damage_ceiling(script.provenance().confidence())
            .weaker(damage_ceiling(call.provenance().confidence()));
        let resolution = EdgeResolution::Inferred.weaker(floor);
        let mut candidates = Vec::with_capacity(scored.len());
        for (rank, entry) in scored {
            let hint = match &entry.owner {
                Some(owner) => format!("{}::{owner}.{}", entry.path, call.callee()),
                None => format!("{}::{}", entry.path, call.callee()),
            };
            candidates.push(self.candidate(rank, Some(entry.id), hint, resolution, true)?);
        }
        self.push_edge(
            EdgeKind::Calls,
            source,
            resolution,
            candidates,
            path.to_owned(),
            span,
            detail,
            true,
        )
    }

    /// Disclose every `load(...)` / `preload(...)` whose path was not a literal:
    /// the syntax layer records no resource reference for it, so the graph is
    /// silently missing that edge unless the gap is stated.
    fn disclose_dynamic_resource_paths(&mut self, script: &GdScriptFile, path: &str) {
        let literal_lines: BTreeSet<u32> = script
            .preloads()
            .iter()
            .map(|reference| reference.span().start_line())
            .collect();
        for call in script.calls() {
            if !matches!(call.callee(), "preload" | "load") {
                continue;
            }
            if literal_lines.contains(&call.span().start_line()) {
                continue;
            }
            self.note(
                LimitationKind::DynamicResourcePathNotRecorded,
                format!("{path}:{}", call.span().start_line()),
            );
        }
    }

    // -- scenes and text resources -----------------------------------------

    #[allow(clippy::too_many_lines)]
    fn derive_document_edges(&mut self) -> Result<(), GraphError> {
        let documents = std::mem::take(&mut self.documents);
        for document in &documents {
            let path = document.source_path().to_owned();
            let document_node = match document.kind() {
                DocumentKind::Scene => GraphNodeId::scene(&path),
                DocumentKind::Resource => GraphNodeId::text_resource(&path),
            };

            for declaration in document.ext_resources() {
                let floor = damage_ceiling(declaration.provenance().confidence());
                self.push_resource_edge(
                    EdgeKind::ResourceReference,
                    document_node.clone(),
                    declaration.target(),
                    path.clone(),
                    Some(GraphSpan::from_scene(declaration.span())),
                    format!(
                        "[ext_resource id={}]",
                        declaration.id().unwrap_or("<undeclared>")
                    ),
                    floor,
                )?;
            }

            for node in document.nodes() {
                let Some(node_path) = node.node_path() else {
                    continue;
                };
                let node_id = GraphNodeId::scene_node(&path, node_path);
                let floor = damage_ceiling(node.provenance().confidence());
                let span = Some(GraphSpan::from_scene(node.span()));

                if let Some(parent_path) = node.parent_path() {
                    let parent_absolute = if parent_path == "." {
                        ".".to_owned()
                    } else {
                        parent_path.to_owned()
                    };
                    let parent_id = GraphNodeId::scene_node(&path, &parent_absolute);
                    if self.nodes.contains_key(&parent_id) {
                        let resolution = EdgeResolution::Exact.weaker(floor);
                        let candidate =
                            self.candidate(0, Some(parent_id), parent_absolute, resolution, false)?;
                        self.push_edge(
                            EdgeKind::SceneParent,
                            node_id.clone(),
                            resolution,
                            vec![candidate],
                            path.clone(),
                            span,
                            format!("parent=\"{parent_path}\""),
                            false,
                        )?;
                    } else {
                        let candidate = self.candidate(
                            0,
                            None,
                            parent_absolute.clone(),
                            EdgeResolution::Unresolved,
                            false,
                        )?;
                        self.push_dangling(
                            DanglingKind::TargetAbsent,
                            EdgeKind::SceneParent,
                            node_id.clone(),
                            parent_absolute,
                            path.clone(),
                            span,
                            "the parent path names no node declared in this document",
                        );
                        self.push_edge(
                            EdgeKind::SceneParent,
                            node_id.clone(),
                            EdgeResolution::Unresolved,
                            vec![candidate],
                            path.clone(),
                            span,
                            format!("parent=\"{parent_path}\""),
                            false,
                        )?;
                    }
                }

                if let Some(script) = node.script() {
                    self.push_reference_edge(
                        EdgeKind::ScriptAttachment,
                        node_id.clone(),
                        script,
                        &path,
                        format!("script = {}", handle_text(script)),
                        floor,
                    )?;
                }
                if let Some(instance) = node.instance() {
                    self.push_reference_edge(
                        EdgeKind::SceneInstance,
                        node_id.clone(),
                        instance,
                        &path,
                        format!("instance={}", handle_text(instance)),
                        floor,
                    )?;
                }
                if let Some(placeholder) = node.instance_placeholder() {
                    self.push_resource_edge(
                        EdgeKind::ScenePlaceholder,
                        node_id.clone(),
                        placeholder,
                        path.clone(),
                        span,
                        "instance_placeholder=".to_owned(),
                        floor,
                    )?;
                }
                for property in node.properties() {
                    if let crate::scene::PropertyValue::Resource(reference) = property.value() {
                        self.push_reference_edge(
                            EdgeKind::ResourceReference,
                            node_id.clone(),
                            reference,
                            &path,
                            format!("{} = {}", property.key(), handle_text(reference)),
                            floor,
                        )?;
                    }
                }
            }

            if let Some(body) = document.resource_body() {
                let floor = damage_ceiling(body.provenance().confidence());
                if let Some(script) = body.script() {
                    self.push_reference_edge(
                        EdgeKind::ScriptAttachment,
                        document_node.clone(),
                        script,
                        &path,
                        format!("script = {}", handle_text(script)),
                        floor,
                    )?;
                }
                for property in body.properties() {
                    if let crate::scene::PropertyValue::Resource(reference) = property.value() {
                        self.push_reference_edge(
                            EdgeKind::ResourceReference,
                            document_node.clone(),
                            reference,
                            &path,
                            format!("{} = {}", property.key(), handle_text(reference)),
                            floor,
                        )?;
                    }
                }
            }

            for sub in document.sub_resources() {
                let Some(sub_id) = sub.id() else {
                    continue;
                };
                let owner = GraphNodeId::sub_resource(&path, sub_id);
                let floor = damage_ceiling(sub.provenance().confidence());
                for property in sub.properties() {
                    if let crate::scene::PropertyValue::Resource(reference) = property.value() {
                        self.push_reference_edge(
                            EdgeKind::ResourceReference,
                            owner.clone(),
                            reference,
                            &path,
                            format!("{} = {}", property.key(), handle_text(reference)),
                            floor,
                        )?;
                    }
                }
            }

            self.derive_connections(document, &path)?;
        }
        self.documents = documents;
        Ok(())
    }

    /// Emit an edge whose target came from an `ExtResource("id")` handle. A
    /// handle whose identifier was never declared is unresolved rather than
    /// guessed.
    fn push_reference_edge(
        &mut self,
        kind: EdgeKind,
        source: GraphNodeId,
        reference: &ResourceRef,
        path: &str,
        detail: String,
        floor: EdgeResolution,
    ) -> Result<(), GraphError> {
        let span = Some(GraphSpan::from_scene(reference.span()));
        if reference.kind() == ResourceRefKind::SubResource {
            return self.push_sub_resource_edge(source, reference, path, detail);
        }
        match reference.target() {
            Some(target) => {
                let target = target.clone();
                let floor = floor.weaker(damage_ceiling(reference.provenance().confidence()));
                self.push_resource_edge(kind, source, &target, path.to_owned(), span, detail, floor)
            }
            None => {
                let hint = format!("{}(\"{}\")", reference.kind().code(), reference.id());
                let candidate =
                    self.candidate(0, None, hint.clone(), EdgeResolution::Unresolved, false)?;
                self.push_dangling(
                    DanglingKind::TargetAbsent,
                    kind,
                    source.clone(),
                    hint,
                    path.to_owned(),
                    span,
                    "the handle's identifier has no declaration in this document",
                );
                self.push_edge(
                    kind,
                    source,
                    EdgeResolution::Unresolved,
                    vec![candidate],
                    path.to_owned(),
                    span,
                    detail,
                    false,
                )
            }
        }
    }

    /// Emit a `SubResource("id")` relation.
    ///
    /// What an internal sub-resource *means* is not modelled by this layer, so
    /// the relation is always [`EdgeResolution::Unresolved`]: the fact that the
    /// relationship exists is recorded (never absent) but no exact target is
    /// claimed.
    fn push_sub_resource_edge(
        &mut self,
        source: GraphNodeId,
        reference: &ResourceRef,
        path: &str,
        detail: String,
    ) -> Result<(), GraphError> {
        let span = Some(GraphSpan::from_scene(reference.span()));
        let declared = GraphNodeId::sub_resource(path, reference.id());
        let target = self.nodes.contains_key(&declared).then_some(declared);
        let hint = format!("SubResource(\"{}\")", reference.id());
        self.note(
            LimitationKind::SubResourceSemanticsUnsupported,
            format!("{path}:{}", reference.span().start_line()),
        );
        let candidate =
            self.candidate(0, target, hint.clone(), EdgeResolution::Unresolved, false)?;
        self.push_dangling(
            DanglingKind::TargetUndecidable,
            EdgeKind::SubResourceRelation,
            source.clone(),
            hint,
            path.to_owned(),
            span,
            "internal [sub_resource] semantics are not modelled by this layer, so the relation stays unresolved rather than being reported as exact or omitted",
        );
        self.push_edge(
            EdgeKind::SubResourceRelation,
            source,
            EdgeResolution::Unresolved,
            vec![candidate],
            path.to_owned(),
            span,
            detail,
            false,
        )
    }

    /// Wire every `[connection]` to the handler function it names.
    ///
    /// The receiving node's attached script is statically known, so a handler
    /// declared directly in that script is exact. A handler found only in the
    /// base chain stays inferred with every candidate, and a handler found
    /// nowhere is unresolved — absent when the whole chain was resolvable and
    /// undecidable when it was not.
    #[allow(clippy::too_many_lines)]
    fn derive_connections(
        &mut self,
        document: &SceneDocument,
        path: &str,
    ) -> Result<(), GraphError> {
        for connection in document.connections() {
            let (Some(signal), Some(method)) = (connection.signal(), connection.method()) else {
                continue;
            };
            let span = Some(GraphSpan::from_scene(connection.span()));
            let detail = format!(
                "[connection signal=\"{signal}\" from=\"{}\" to=\"{}\" method=\"{method}\"]",
                connection.from_path().unwrap_or("<missing>"),
                connection.to_path().unwrap_or("<missing>")
            );
            let floor = damage_ceiling(connection.provenance().confidence());
            let source = connection
                .from_path()
                .map(|from| GraphNodeId::scene_node(path, from))
                .filter(|id| self.nodes.contains_key(id))
                .unwrap_or_else(|| GraphNodeId::scene(path));

            // Which script the receiving node runs.
            let receiver_script = connection
                .to_path()
                .and_then(|to| document.node_at(to))
                .and_then(|node| node.script().and_then(ResourceRef::target).cloned())
                .map(|target| self.resolve_resource(&target));

            let receiver_path = match receiver_script {
                Some(Resolved::Node(ref node)) => node
                    .as_str()
                    .strip_prefix("script:")
                    .map(std::borrow::ToOwned::to_owned),
                _ => None,
            };

            let Some(receiver_path) = receiver_path else {
                let candidate =
                    self.candidate(0, None, method, EdgeResolution::Unresolved, true)?;
                self.push_dangling(
                    DanglingKind::TargetUndecidable,
                    EdgeKind::SignalConnection,
                    source.clone(),
                    method,
                    path.to_owned(),
                    span,
                    "the receiving node's script could not be resolved, so the handler cannot be located",
                );
                self.push_edge(
                    EdgeKind::SignalConnection,
                    source,
                    EdgeResolution::Unresolved,
                    vec![candidate],
                    path.to_owned(),
                    span,
                    detail,
                    true,
                )?;
                continue;
            };

            let declared_here = self
                .top_level_functions
                .get(&receiver_path)
                .is_some_and(|names| names.contains(method));
            if declared_here {
                let resolution = EdgeResolution::Exact.weaker(floor);
                let target = GraphNodeId::function(&receiver_path, None, method);
                let candidate = self.candidate(
                    0,
                    Some(target),
                    format!("{receiver_path}::{method}"),
                    resolution,
                    false,
                )?;
                self.push_edge(
                    EdgeKind::SignalConnection,
                    source,
                    resolution,
                    vec![candidate],
                    path.to_owned(),
                    span,
                    detail,
                    false,
                )?;
                continue;
            }

            let chain = self.base_chain(&receiver_path);
            let mut candidates = Vec::new();
            for (position, base) in chain.iter().enumerate() {
                if self
                    .top_level_functions
                    .get(base)
                    .is_some_and(|names| names.contains(method))
                {
                    candidates.push(self.candidate(
                        u32::try_from(position).unwrap_or(u32::MAX),
                        Some(GraphNodeId::function(base, None, method)),
                        format!("{base}::{method}"),
                        EdgeResolution::Inferred,
                        false,
                    )?);
                }
            }
            if !candidates.is_empty() {
                self.push_edge(
                    EdgeKind::SignalConnection,
                    source,
                    EdgeResolution::Inferred,
                    candidates,
                    path.to_owned(),
                    span,
                    detail,
                    false,
                )?;
                continue;
            }

            self.note(
                LimitationKind::EngineClassRegistryUnavailable,
                receiver_path.clone(),
            );
            let candidate = self.candidate(0, None, method, EdgeResolution::Unresolved, false)?;
            self.push_dangling(
                DanglingKind::TargetUndecidable,
                EdgeKind::SignalConnection,
                source.clone(),
                method,
                path.to_owned(),
                span,
                "neither the receiving script nor its indexed base chain declares this handler, and the chain ends at an engine class this layer cannot inspect, so the handler may still be inherited",
            );
            self.push_edge(
                EdgeKind::SignalConnection,
                source,
                EdgeResolution::Unresolved,
                vec![candidate],
                path.to_owned(),
                span,
                detail,
                false,
            )?;
        }
        Ok(())
    }

    // -- standing limitations ----------------------------------------------

    fn disclose_standing_limitations(&mut self) {
        let damaged: Vec<String> = self
            .scripts
            .iter()
            .filter(|script| script.is_partial())
            .map(|script| script.source_path().to_owned())
            .chain(
                self.documents
                    .iter()
                    .filter(|document| document.is_partial())
                    .map(|document| document.source_path().to_owned()),
            )
            .collect();
        for subject in damaged {
            self.note(LimitationKind::DamagedRegionNotTraversed, subject);
        }
        // Call arguments are never retained by the syntax layer, so every
        // string-keyed relationship is invisible whenever a script is present.
        let call_sites: Vec<String> = self
            .scripts
            .iter()
            .filter(|script| !script.calls().is_empty())
            .map(|script| script.source_path().to_owned())
            .collect();
        for subject in call_sites {
            self.note(LimitationKind::CallArgumentsNotRecorded, subject);
        }
    }
}

/// The ceiling a source record's own confidence puts on an edge derived from it.
///
/// Only [`Confidence::Inferred`] lowers the ceiling: on a Todo 7 / Todo 9 record
/// it means the construct sits in a *damaged region*, and no relationship read
/// out of damaged text may be called exact.
///
/// [`Confidence::Unresolved`] does **not** lower it. On a source record that
/// value means the *earlier* layer could not map the target — a `uid://` handle
/// with no project-wide index, or a path it refused to classify. Deciding those
/// targets is precisely this layer's job, and [`Derivation::resolve_resource`]
/// re-decides each one independently, so inheriting the earlier verdict would
/// permanently hide the very edges this todo exists to resolve. A target this
/// layer also cannot decide still ends up unresolved, because
/// `resolve_resource` says so on its own evidence.
const fn damage_ceiling(confidence: Confidence) -> EdgeResolution {
    match confidence {
        Confidence::Exact | Confidence::Unresolved => EdgeResolution::Exact,
        Confidence::Inferred => EdgeResolution::Inferred,
    }
}

/// Which node kind a project-relative path denotes, by extension.
fn node_kind_for_path(path: &str) -> GraphNodeKind {
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match base.rsplit_once('.').map(|(_, extension)| extension) {
        Some("gd") => GraphNodeKind::Script,
        Some("tscn") => GraphNodeKind::Scene,
        Some("tres") => GraphNodeKind::TextResource,
        _ => GraphNodeKind::Asset,
    }
}

/// The graph identity of a project-relative path.
fn path_node_id(path: &str) -> GraphNodeId {
    match node_kind_for_path(path) {
        GraphNodeKind::Script => GraphNodeId::script(path),
        GraphNodeKind::Scene => GraphNodeId::scene(path),
        GraphNodeKind::TextResource => GraphNodeId::text_resource(path),
        _ => GraphNodeId::asset(path),
    }
}

/// The handle as written, for an edge's human-readable detail.
fn handle_text(reference: &ResourceRef) -> String {
    match reference.kind() {
        ResourceRefKind::ExtResource => format!("ExtResource(\"{}\")", reference.id()),
        ResourceRefKind::SubResource => format!("SubResource(\"{}\")", reference.id()),
    }
}
