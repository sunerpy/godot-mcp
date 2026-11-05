//! The file reference graph, and the two views built on it: unused files and
//! dependency cycles.
//!
//! A [`Reference`] says one project file uses another, and how. The graph is
//! derived from the same parsed records the semantic graph uses — scene and
//! resource `[ext_resource]` declarations (a known `uid` winning over `path`,
//! as Godot loads them), `instance_placeholder`s, script `extends`, `preload`
//! and `load`-family calls, `class_name` usages, plugin descriptors — plus
//! quoted `res://` strings in other text files, which are only evidence and so
//! only `inferred`.
//!
//! ## Unused files
//!
//! A file is *used* when it is reachable from a [`Root`]: the main scene,
//! autoloads, every `res://`/`uid://` value in `project.godot`, enabled
//! plugins, the default bus layout, export-preset filters and selections, and
//! native-extension descriptors. Every other loadable file is reported unused,
//! as a candidate rather than a proof: a load whose path is computed at run
//! time ([`DynamicLoad`]) may reach it, and the entry names the sites whose
//! literal prefix covers it. The *used* set is therefore a lower bound.
//!
//! ## Cycles
//!
//! Strongly connected components of the graph, classified by what Godot does
//! with them, as observed on Godot 4.4.1, 4.5.1, 4.6.1 and 4.7.1:
//!
//! | Classification | Cycle made of | Effect |
//! | --- | --- | --- |
//! | [`CycleKind::Inheritance`] | `extends` alone | the scripts fail to compile ("Could not resolve super class inheritance"); the 4.4.1 and 4.5.1 editors crash (SIGSEGV) while scanning a project that holds one |
//! | [`CycleKind::ResourceLoad`] | scene/resource `[ext_resource]`s alone | the loader rejects the reference that closes the ring ("[ext_resource] referenced non-existent resource") and loads the rest without it |
//! | [`CycleKind::Tolerated`] | anything involving `preload`, `load`, a class-name usage or a path string | loads |

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use godot_mcp_domain::Confidence;
use serde::Serialize;

use super::glob::GlobSet;
use super::model::{
    Coverage, CoverageBuilder, FileKind, ProjectModel, Target, describe_target, weakest,
};
use super::presets::{parse_export_presets, plugin_script};
use super::script_facts::{LoadCallKind, PathArgument, scan_text_paths};
use crate::project::{ResourceTarget, resource_target};
use crate::scene::{PropertyValue, SceneDocument};

/// Most referrers or dynamic-load sites listed on one unused entry; the count
/// is always exact.
const MAX_LISTED: usize = 10;

/// Most edges or members listed for one cycle; the counts are always exact.
const MAX_CYCLE_EDGES: usize = 100;
const MAX_CYCLE_MEMBERS: usize = 100;

/// How one file uses another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    /// An `[ext_resource]` of a scene or resource that is neither its script
    /// nor an instance.
    ExtResource,
    /// A scene node instances the target (`instance=ExtResource(...)`).
    SceneInstance,
    /// A scene node or resource attaches the target script.
    ScriptAttachment,
    /// A scene node defers the target (`instance_placeholder=`).
    Placeholder,
    /// A script `extends` the target script.
    Extends,
    /// A script `preload`s the target.
    Preload,
    /// A script loads the target at run time with a literal path.
    Load,
    /// A script uses the `class_name` the target registers.
    ClassReference,
    /// A script or document holds the target's path in a string.
    PathLiteral,
    /// Another text file (a shader, JSON, configuration) names the target.
    TextReference,
    /// A `plugin.cfg` names the target as its script.
    PluginScript,
    /// The target is the source an imported file was generated from.
    ImportSource,
}

impl ReferenceKind {
    /// Stable wire code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ExtResource => "ext_resource",
            Self::SceneInstance => "scene_instance",
            Self::ScriptAttachment => "script_attachment",
            Self::Placeholder => "placeholder",
            Self::Extends => "extends",
            Self::Preload => "preload",
            Self::Load => "load",
            Self::ClassReference => "class_reference",
            Self::PathLiteral => "path_literal",
            Self::TextReference => "text_reference",
            Self::PluginScript => "plugin_script",
            Self::ImportSource => "import_source",
        }
    }

    /// Whether the reference is a scene or resource dependency the loader
    /// resolves while loading the source.
    const fn is_document_load(self) -> bool {
        matches!(
            self,
            Self::ExtResource | Self::SceneInstance | Self::ScriptAttachment
        )
    }
}

/// One file using another.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Reference {
    /// The using file.
    pub from: String,
    /// The used file.
    pub to: String,
    /// How it is used.
    pub kind: ReferenceKind,
    /// 1-based line in [`Self::from`].
    pub line: u32,
    /// What the source wrote.
    pub detail: String,
    /// The `uid://` handle the target was found through, when it was.
    pub via_uid: Option<String>,
    /// `exact` for a parsed declaration, `inferred` for a damaged one or a
    /// path found in a string.
    pub confidence: Confidence,
}

/// Why a file counts as used without anything referencing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RootReason {
    /// `application/run/main_scene`.
    MainScene,
    /// An `[autoload]` entry.
    Autoload,
    /// Any other `project.godot` value naming the file.
    ProjectSetting,
    /// `res://default_bus_layout.tres`, which Godot loads when
    /// `audio/buses/default_bus_layout` is not set.
    DefaultBusLayout,
    /// An enabled editor plugin's descriptor.
    EnabledPlugin,
    /// An export preset's `include_filter` selects the file.
    ExportIncludeFilter,
    /// An export preset in `resources` or `scenes` mode lists the file.
    ExportSelected,
    /// A `.gdextension` descriptor, which Godot loads at startup.
    Extension,
    /// An `EditorScript`, run by hand from the script editor.
    EditorScript,
}

/// One root of the reachability walk.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Root {
    /// The root file.
    pub path: String,
    /// Why it is a root.
    pub reason: RootReason,
    /// The setting key, autoload name, preset name or filter.
    pub detail: String,
    /// The file the reason was read from.
    pub declared_in: String,
    /// 1-based line in [`Self::declared_in`], when known.
    pub line: Option<u32>,
}

/// A load whose path the static layer cannot know.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct DynamicLoad {
    /// The script making the call.
    pub file: String,
    /// 1-based line.
    pub line: u32,
    /// 1-based column.
    pub column: u32,
    /// Which call (`load`, `method_load`, `directory_list`, ...).
    pub call: &'static str,
    /// The argument exactly as written.
    pub argument: String,
    /// `res://` prefixes the computed path is built from; a file under one of
    /// them may be what the call loads.
    pub literal_prefixes: Vec<String>,
    /// The enclosing function.
    pub function: Option<String>,
}

/// A reference whose target is not an inventoried file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrokenReference {
    /// The referring file.
    pub from: String,
    /// 1-based line.
    pub line: u32,
    /// How the target was referenced.
    pub kind: ReferenceKind,
    /// What the target turned out to be.
    pub target: Target,
}

/// Every reference between project files, and the roots of reachability.
#[derive(Debug, Clone)]
pub struct ReferenceGraph {
    references: Vec<Reference>,
    roots: Vec<Root>,
    dynamic_loads: Vec<DynamicLoad>,
    broken: Vec<BrokenReference>,
    coverage: Coverage,
}

impl ReferenceGraph {
    /// Derive the graph from a parsed project.
    #[must_use]
    pub fn build(model: &ProjectModel<'_>) -> Self {
        let mut builder = Builder {
            model,
            references: BTreeMap::new(),
            roots: BTreeSet::new(),
            dynamic_loads: Vec::new(),
            broken: Vec::new(),
            coverage: CoverageBuilder::default(),
        };
        builder.documents();
        builder.scripts();
        builder.text_files();
        builder.roots();
        model.note_read_gaps(
            &mut builder.coverage,
            &[
                FileKind::ProjectSettings,
                FileKind::ExportPresets,
                FileKind::Script,
                FileKind::CSharpScript,
                FileKind::Scene,
                FileKind::BinaryScene,
                FileKind::Resource,
                FileKind::BinaryResource,
                FileKind::Shader,
                FileKind::ShaderInclude,
                FileKind::PluginDescriptor,
                FileKind::Text,
            ],
        );
        for load in &builder.dynamic_loads {
            builder.coverage.note(
                "dynamic_loads",
                "these calls load a path computed at run time; whatever they load is not counted as used",
                format!("{}:{}", load.file, load.line),
            );
        }
        if model.settings().is_none() {
            builder.coverage.note(
                "settings_unavailable",
                "project.godot was not available, so no setting-based root is known",
                "project.godot",
            );
        }
        let Builder {
            references,
            roots,
            mut dynamic_loads,
            broken,
            coverage,
            ..
        } = builder;
        dynamic_loads.sort();
        Self {
            references: references.into_values().collect(),
            roots: roots.into_iter().collect(),
            dynamic_loads,
            broken,
            coverage: coverage.finish(),
        }
    }

    /// Every reference, ordered by source, target, kind and line; one entry
    /// per `(source, target, kind)` at its first line.
    #[must_use]
    pub fn references(&self) -> &[Reference] {
        &self.references
    }

    /// Every root, in order.
    #[must_use]
    pub fn roots(&self) -> &[Root] {
        &self.roots
    }

    /// Every load whose path is computed at run time.
    #[must_use]
    pub fn dynamic_loads(&self) -> &[DynamicLoad] {
        &self.dynamic_loads
    }

    /// Every reference to something that is not an inventoried file.
    #[must_use]
    pub fn broken(&self) -> &[BrokenReference] {
        &self.broken
    }

    /// What the graph could not see.
    #[must_use]
    pub const fn coverage(&self) -> &Coverage {
        &self.coverage
    }

    /// Every file reachable from the roots.
    #[must_use]
    pub fn reachable(&self) -> BTreeSet<String> {
        let mut adjacency: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for reference in &self.references {
            adjacency
                .entry(reference.from.as_str())
                .or_default()
                .push(reference.to.as_str());
        }
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<&str> = VecDeque::new();
        for root in &self.roots {
            if seen.insert(root.path.clone()) {
                queue.push_back(&root.path);
            }
        }
        while let Some(current) = queue.pop_front() {
            for next in adjacency.get(current).into_iter().flatten() {
                if seen.insert((*next).to_owned()) {
                    queue.push_back(next);
                }
            }
        }
        seen
    }
}

struct Builder<'m, 's> {
    model: &'m ProjectModel<'s>,
    references: BTreeMap<(String, String, ReferenceKind), Reference>,
    roots: BTreeSet<Root>,
    dynamic_loads: Vec<DynamicLoad>,
    broken: Vec<BrokenReference>,
    coverage: CoverageBuilder,
}

impl Builder<'_, '_> {
    fn push(
        &mut self,
        from: &str,
        target: Target,
        kind: ReferenceKind,
        line: u32,
        detail: String,
        confidence: Confidence,
    ) {
        match target {
            Target::File { path, via_uid } => {
                let key = (from.to_owned(), path.clone(), kind);
                let candidate = Reference {
                    from: from.to_owned(),
                    to: path,
                    kind,
                    line,
                    detail,
                    via_uid,
                    confidence,
                };
                match self.references.get(&key) {
                    Some(existing) if existing.line <= line => {}
                    _ => {
                        self.references.insert(key, candidate);
                    }
                }
            }
            other => self.broken.push(BrokenReference {
                from: from.to_owned(),
                line,
                kind,
                target: other,
            }),
        }
    }

    // -- scenes and resources -------------------------------------------------

    fn documents(&mut self) {
        let model = self.model;
        for (path, document) in model.documents() {
            let Ok(document) = document else {
                continue;
            };
            self.document(path, document);
        }
    }

    fn document(&mut self, path: &str, document: &SceneDocument) {
        let confidence = if document.is_partial() {
            Confidence::Inferred
        } else {
            Confidence::Exact
        };
        let instance_ids: BTreeSet<&str> = document
            .nodes()
            .iter()
            .filter_map(|node| node.instance().map(|handle| handle.id()))
            .collect();
        let script_ids: BTreeSet<&str> = document
            .nodes()
            .iter()
            .filter_map(|node| node.script().map(|handle| handle.id()))
            .chain(
                document
                    .resource_body()
                    .and_then(|body| body.script())
                    .map(|handle| handle.id()),
            )
            .collect();
        for declaration in document.ext_resources() {
            let id = declaration.id().unwrap_or_default();
            let kind = if instance_ids.contains(id) {
                ReferenceKind::SceneInstance
            } else if script_ids.contains(id) {
                ReferenceKind::ScriptAttachment
            } else {
                ReferenceKind::ExtResource
            };
            let target = self
                .model
                .resolve_ext_resource(declaration.uid(), declaration.target());
            let written = match declaration.target() {
                ResourceTarget::ProjectRelative { res_uri, .. } => res_uri.clone(),
                ResourceTarget::Uid(uid) => uid.clone(),
                ResourceTarget::Unsupported(value) => value.clone(),
            };
            let detail = match declaration.type_name() {
                Some(type_name) => {
                    format!("[ext_resource type=\"{type_name}\" path=\"{written}\"]")
                }
                None => format!("[ext_resource path=\"{written}\"]"),
            };
            self.push(
                path,
                target,
                kind,
                declaration.span().start_line(),
                detail,
                confidence,
            );
        }
        for node in document.nodes() {
            if let Some(placeholder) = node.instance_placeholder() {
                let target = self.model.resolve_ext_resource(None, placeholder);
                self.push(
                    path,
                    target,
                    ReferenceKind::Placeholder,
                    node.span().start_line(),
                    format!(
                        "instance_placeholder on {}",
                        node.node_path().unwrap_or("an unnamed node")
                    ),
                    confidence,
                );
            }
        }
        // Paths held in plain string properties: an exported `String` a
        // script later loads, for example.
        let properties = document
            .nodes()
            .iter()
            .flat_map(|node| node.properties())
            .chain(
                document
                    .sub_resources()
                    .iter()
                    .flat_map(|sub| sub.properties()),
            )
            .chain(
                document
                    .resource_body()
                    .into_iter()
                    .flat_map(|body| body.properties()),
            );
        let mut literals = Vec::new();
        for property in properties {
            if !matches!(property.value(), PropertyValue::RetainedUnknown) {
                continue;
            }
            for literal in scan_text_paths(property.raw_value()) {
                literals.push((
                    literal.value,
                    property.span().start_line() + literal.line - 1,
                    property.key().to_owned(),
                ));
            }
        }
        for (value, line, key) in literals {
            let target = self.model.resolve(&value);
            self.push(
                path,
                target,
                ReferenceKind::PathLiteral,
                line,
                format!("{key} = \"{value}\""),
                Confidence::Inferred,
            );
        }
        if document
            .sub_resources()
            .iter()
            .any(|sub| sub.type_name() == Some("GDScript"))
        {
            self.coverage.note(
                "built_in_scripts_not_analysed",
                "these documents embed built-in scripts, whose code is not analysed",
                path.to_owned(),
            );
        }
    }

    // -- scripts ----------------------------------------------------------------

    fn scripts(&mut self) {
        let model = self.model;
        for (path, script) in model.scripts() {
            let Some(facts) = script.facts() else {
                continue;
            };
            let damaged = script.is_damaged();
            let floor = if damaged {
                Confidence::Inferred
            } else {
                Confidence::Exact
            };

            if let Some(base) = script.symbols().and_then(|symbols| symbols.extends()) {
                let line = facts.extends_line().unwrap_or(1);
                match model.base_script(path, base) {
                    Ok(Some(target)) => self.push(
                        path,
                        Target::File {
                            path: target,
                            via_uid: None,
                        },
                        ReferenceKind::Extends,
                        line,
                        format!("extends {base}"),
                        floor,
                    ),
                    Ok(None) => {}
                    Err(_) => {
                        if base.starts_with("res://")
                            || base.starts_with("uid://")
                            || base.ends_with(".gd")
                        {
                            let target = model.resolve(&absolute_from(path, base, true));
                            self.push(
                                path,
                                target,
                                ReferenceKind::Extends,
                                line,
                                format!("extends {base}"),
                                floor,
                            );
                        }
                    }
                }
            }

            for call in facts.load_calls() {
                let kind = if call.kind == LoadCallKind::Preload {
                    ReferenceKind::Preload
                } else {
                    ReferenceKind::Load
                };
                match (&call.path, call.argument_kind) {
                    (Some(value), PathArgument::Literal | PathArgument::Constant)
                        if call.kind != LoadCallKind::DirectoryList =>
                    {
                        let relative = !value.starts_with("res://") && !value.starts_with("uid://");
                        let target = model.resolve(&absolute_from(
                            path,
                            value,
                            call.kind == LoadCallKind::Preload,
                        ));
                        let confidence = if relative {
                            weakest(floor, Confidence::Inferred)
                        } else {
                            floor
                        };
                        self.push(
                            path,
                            target,
                            kind,
                            call.line,
                            format!("{}({})", call.kind.code(), call.argument),
                            confidence,
                        );
                    }
                    _ => {
                        let mut prefixes = call.literal_prefixes.clone();
                        if call.kind == LoadCallKind::DirectoryList
                            && let Some(value) = &call.path
                        {
                            let directory = value.trim_end_matches('/');
                            prefixes.push(format!("{directory}/"));
                        }
                        self.dynamic_loads.push(DynamicLoad {
                            file: path.clone(),
                            line: call.line,
                            column: call.column,
                            call: call.kind.code(),
                            argument: call.argument.clone(),
                            literal_prefixes: prefixes,
                            function: call.enclosing_function.clone(),
                        });
                    }
                }
            }

            let load_paths: BTreeSet<&str> = facts
                .load_calls()
                .iter()
                .filter_map(|call| call.path.as_deref())
                .collect();
            let fragments: BTreeSet<&str> = facts
                .load_calls()
                .iter()
                .flat_map(|call| call.literal_prefixes.iter().map(String::as_str))
                .collect();
            for literal in facts.literals() {
                if literal.load_argument || literal.extends {
                    continue;
                }
                if literal.constant && load_paths.contains(literal.value.as_str()) {
                    continue;
                }
                // A directory prefix or a fragment of a computed path names no
                // file; the dynamic load it belongs to is reported instead.
                if literal.value.ends_with('/') || fragments.contains(literal.value.as_str()) {
                    continue;
                }
                let target = model.resolve(&literal.value);
                self.push(
                    path,
                    target,
                    ReferenceKind::PathLiteral,
                    literal.line,
                    format!("\"{}\"", literal.value),
                    Confidence::Inferred,
                );
            }
            for literal in facts.damaged_text_paths() {
                let target = model.resolve(&literal.value);
                self.push(
                    path,
                    target,
                    ReferenceKind::PathLiteral,
                    literal.line,
                    format!("\"{}\" (inside a damaged region)", literal.value),
                    Confidence::Inferred,
                );
            }

            for identifier in facts.identifiers() {
                let declaring = model.class_scripts(identifier);
                let confidence = if declaring.len() == 1 {
                    floor
                } else {
                    Confidence::Inferred
                };
                for target in declaring {
                    if target == path {
                        continue;
                    }
                    self.push(
                        path,
                        Target::File {
                            path: target.clone(),
                            via_uid: None,
                        },
                        ReferenceKind::ClassReference,
                        1,
                        format!("uses class_name {identifier}"),
                        confidence,
                    );
                }
            }
        }
        self.class_reference_lines();
    }

    /// Class references were recorded at line 1; move each to the first line
    /// the class name occurs on in the using script's source.
    fn class_reference_lines(&mut self) {
        let model = self.model;
        for reference in self.references.values_mut() {
            if reference.kind != ReferenceKind::ClassReference {
                continue;
            }
            let Some(name) = reference.detail.strip_prefix("uses class_name ") else {
                continue;
            };
            if let Some(text) = model.sources().text(&reference.from)
                && let Some(line) = first_word_line(text, name)
            {
                reference.line = line;
            }
        }
    }

    // -- other text files ---------------------------------------------------------

    fn text_files(&mut self) {
        let model = self.model;
        for (path, kind) in model.kinds() {
            let Some(text) = model.sources().text(path) else {
                continue;
            };
            match kind {
                FileKind::Shader | FileKind::ShaderInclude => {
                    for (index, line) in text.lines().enumerate() {
                        let Some(rest) = line.trim_start().strip_prefix("#include") else {
                            continue;
                        };
                        let Some(value) = rest
                            .trim()
                            .strip_prefix('"')
                            .and_then(|rest| rest.split('"').next())
                        else {
                            continue;
                        };
                        let target = model.resolve(&absolute_from(path, value, true));
                        self.push(
                            path,
                            target,
                            ReferenceKind::TextReference,
                            u32::try_from(index + 1).unwrap_or(u32::MAX),
                            format!("#include \"{value}\""),
                            Confidence::Exact,
                        );
                    }
                    self.quoted_paths(path, text, |line| line.trim_start().starts_with("#include"));
                }
                FileKind::PluginDescriptor => {
                    if let Some((script, line)) = plugin_script(path, text) {
                        let target = model.resolve(&script);
                        self.push(
                            path,
                            target,
                            ReferenceKind::PluginScript,
                            line,
                            format!("script=\"{script}\""),
                            Confidence::Exact,
                        );
                    }
                }
                FileKind::ImportSidecar => {
                    let Some(owner) = path.strip_suffix(".import") else {
                        continue;
                    };
                    for literal in scan_text_paths(text) {
                        let Some(relative) = literal.value.strip_prefix("res://") else {
                            continue;
                        };
                        if relative.starts_with(".godot/") || relative == owner {
                            continue;
                        }
                        let line_text = text
                            .lines()
                            .nth(literal.line as usize - 1)
                            .unwrap_or_default();
                        if !line_text.trim_start().starts_with("dest_files") {
                            continue;
                        }
                        if model.sources().contains(relative) {
                            self.push(
                                relative,
                                Target::File {
                                    path: owner.to_owned(),
                                    via_uid: None,
                                },
                                ReferenceKind::ImportSource,
                                literal.line,
                                format!("imported from {owner}"),
                                Confidence::Exact,
                            );
                        }
                    }
                }
                FileKind::Text | FileKind::Extension => {
                    self.quoted_paths(path, text, |_| false);
                }
                _ => {}
            }
        }
    }

    fn quoted_paths(&mut self, path: &str, text: &str, skip_line: impl Fn(&str) -> bool) {
        let model = self.model;
        for literal in scan_text_paths(text) {
            let line_text = text
                .lines()
                .nth(literal.line as usize - 1)
                .unwrap_or_default();
            if skip_line(line_text) {
                continue;
            }
            let target = model.resolve(&literal.value);
            self.push(
                path,
                target,
                ReferenceKind::TextReference,
                literal.line,
                format!("\"{}\"", literal.value),
                Confidence::Inferred,
            );
        }
    }

    // -- roots --------------------------------------------------------------------

    fn root(
        &mut self,
        target: Target,
        reason: RootReason,
        detail: String,
        declared_in: &str,
        line: Option<u32>,
    ) {
        match target {
            Target::File { path, .. } => {
                self.roots.insert(Root {
                    path,
                    reason,
                    detail,
                    declared_in: declared_in.to_owned(),
                    line,
                });
            }
            other => {
                self.coverage.note(
                    "unresolved_roots",
                    "these roots name something that is not a project file",
                    format!(
                        "{declared_in}:{} ({detail}: {})",
                        line.unwrap_or(0),
                        describe_target(&other)
                    ),
                );
            }
        }
    }

    fn roots(&mut self) {
        let model = self.model;
        if let Some(settings) = model.settings() {
            if let Some(main) = settings.main_scene() {
                let target = model.resolve_ext_resource(None, main.target());
                self.root(
                    target,
                    RootReason::MainScene,
                    "application/run/main_scene".to_owned(),
                    "project.godot",
                    Some(main.line()),
                );
            }
            for autoload in settings.autoloads() {
                let target = model.resolve_ext_resource(None, autoload.target());
                self.root(
                    target,
                    RootReason::Autoload,
                    autoload.name().to_owned(),
                    "project.godot",
                    Some(autoload.line()),
                );
            }
            for plugin in settings.enabled_plugins() {
                let target = model.resolve_ext_resource(None, plugin.target());
                self.root(
                    target,
                    RootReason::EnabledPlugin,
                    "editor_plugins/enabled".to_owned(),
                    "project.godot",
                    Some(plugin.line()),
                );
            }
            let mut bus_layout_set = false;
            for setting in settings.settings() {
                let section = setting.section().unwrap_or_default();
                let key = setting.key();
                if (section == "application" && key == "run/main_scene")
                    || section == "autoload"
                    || (section == "editor_plugins" && key == "enabled")
                {
                    continue;
                }
                if section == "audio" && key == "buses/default_bus_layout" {
                    bus_layout_set = true;
                }
                let detail = if section.is_empty() {
                    key.to_owned()
                } else {
                    format!("{section}/{key}")
                };
                for literal in scan_text_paths(setting.raw_value()) {
                    let target = model.resolve(&literal.value);
                    self.root(
                        target,
                        RootReason::ProjectSetting,
                        detail.clone(),
                        "project.godot",
                        Some(setting.line() + literal.line - 1),
                    );
                }
            }
            if !bus_layout_set && model.sources().contains("default_bus_layout.tres") {
                self.root(
                    Target::File {
                        path: "default_bus_layout.tres".to_owned(),
                        via_uid: None,
                    },
                    RootReason::DefaultBusLayout,
                    "audio/buses/default_bus_layout (default)".to_owned(),
                    "project.godot",
                    None,
                );
            }
        }

        if let Some(text) = model.sources().text("export_presets.cfg") {
            let presets = parse_export_presets(text);
            for preset in &presets {
                let name = preset
                    .name
                    .clone()
                    .unwrap_or_else(|| preset.section.clone());
                if !preset.include_filters.is_empty() {
                    for path in model.kinds().keys() {
                        if let Some(filter) = preset.includes(path) {
                            self.roots.insert(Root {
                                path: path.clone(),
                                reason: RootReason::ExportIncludeFilter,
                                detail: format!("{name}: {filter}"),
                                declared_in: "export_presets.cfg".to_owned(),
                                line: Some(preset.line),
                            });
                        }
                    }
                }
                if preset.selects_listed_files() {
                    for file in &preset.export_files {
                        let target = model.resolve(&format!("res://{file}"));
                        self.root(
                            target,
                            RootReason::ExportSelected,
                            name.clone(),
                            "export_presets.cfg",
                            Some(preset.line),
                        );
                    }
                }
            }
        }

        for (path, kind) in model.kinds() {
            if *kind == FileKind::Extension {
                self.roots.insert(Root {
                    path: path.clone(),
                    reason: RootReason::Extension,
                    detail: "loaded at startup".to_owned(),
                    declared_in: path.clone(),
                    line: None,
                });
            }
            if *kind == FileKind::Script
                && model.script_chain(path).engine_base.as_deref() == Some("EditorScript")
            {
                self.roots.insert(Root {
                    path: path.clone(),
                    reason: RootReason::EditorScript,
                    detail: "run from the script editor".to_owned(),
                    declared_in: path.clone(),
                    line: None,
                });
            }
        }
    }
}

/// Resolve a script-relative or project-relative literal to a `res://`
/// value. `preload` and `#include` resolve a relative path against the
/// referring file's directory; run-time loads resolve it against the project
/// root.
fn absolute_from(from: &str, value: &str, relative_to_file: bool) -> String {
    if value.starts_with("res://") || value.starts_with("uid://") || value.contains("://") {
        return value.to_owned();
    }
    let value = value.strip_prefix("./").unwrap_or(value);
    if !relative_to_file {
        return format!("res://{value}");
    }
    let directory = from.rsplit_once('/').map_or("", |(directory, _)| directory);
    let mut segments: Vec<&str> = directory
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    for segment in value.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.pop().is_none() {
                    // Escapes the project root: leave `..` in place so the
                    // shared classification rule refuses it.
                    return format!("res://{value}");
                }
            }
            other => segments.push(other),
        }
    }
    format!("res://{}", segments.join("/"))
}

fn first_word_line(text: &str, word: &str) -> Option<u32> {
    for (index, line) in text.lines().enumerate() {
        let code = line.split('#').next().unwrap_or_default();
        let mut start = 0;
        while let Some(found) = code[start..].find(word) {
            let position = start + found;
            let before = code[..position].chars().next_back();
            let after = code[position + word.len()..].chars().next();
            let boundary = |c: Option<char>| c.is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
            if boundary(before) && boundary(after) {
                return u32::try_from(index + 1).ok();
            }
            start = position + word.len();
        }
    }
    None
}

// -- unused files -------------------------------------------------------------------

/// Why an unused file is unused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnusedReason {
    /// Nothing references the file.
    Unreferenced,
    /// Only files that are themselves unused reference it.
    ReferencedOnlyByUnused,
}

/// A load site that may reach an unused file at run time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DynamicSite {
    /// The script making the call.
    pub file: String,
    /// 1-based line.
    pub line: u32,
    /// The literal prefix that covers the file.
    pub prefix: String,
}

/// One file nothing used reaches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnusedFile {
    /// Project-relative path.
    pub path: String,
    /// What it is.
    pub kind: FileKind,
    /// Why it is unused.
    pub reason: UnusedReason,
    /// The unused files that reference it (first 10).
    pub referenced_by: Vec<String>,
    /// How many files reference it.
    pub referenced_by_count: usize,
    /// Run-time loads whose literal prefix covers it (first 10).
    pub possible_dynamic_loads: Vec<DynamicSite>,
    /// How many run-time loads cover it.
    pub possible_dynamic_load_count: usize,
    /// The sidecars that belong to it.
    pub sidecars: Vec<String>,
    /// Always `inferred`: the used set is a lower bound.
    pub confidence: Confidence,
}

/// The unused-files view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnusedReport {
    /// Every unused candidate the selection kept, ordered by path.
    pub unused: Vec<UnusedFile>,
    /// Loadable files examined (after the selection).
    pub candidates: usize,
    /// Loadable files reached from a root (after the selection).
    pub used: usize,
    /// Every root.
    pub roots: Vec<Root>,
    /// Every load whose path is computed at run time.
    pub dynamic_loads: Vec<DynamicLoad>,
    /// Every reference to something that is not a project file.
    pub broken_references: Vec<BrokenReference>,
    /// What the analysis could not see.
    pub coverage: Coverage,
}

/// Find the loadable files nothing used reaches. `selection` narrows which
/// files are reported (the walk always covers the whole project).
#[must_use]
pub fn unused_files(model: &ProjectModel<'_>, selection: &GlobSet) -> UnusedReport {
    let graph = ReferenceGraph::build(model);
    let reached = graph.reachable();
    let mut referrers: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for reference in graph.references() {
        if reference.from != reference.to {
            referrers
                .entry(reference.to.as_str())
                .or_default()
                .insert(reference.from.as_str());
        }
    }
    let mut unused = Vec::new();
    let mut candidates = 0;
    let mut used = 0;
    for (path, kind) in model.kinds() {
        if !kind.is_loadable() || !selection.selects(path) {
            continue;
        }
        candidates += 1;
        if reached.contains(path) {
            used += 1;
            continue;
        }
        let by: Vec<&str> = referrers
            .get(path.as_str())
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default();
        let uri = format!("res://{path}");
        let possible: Vec<DynamicSite> = graph
            .dynamic_loads()
            .iter()
            .flat_map(|load| {
                load.literal_prefixes
                    .iter()
                    .filter(|prefix| {
                        uri.starts_with(prefix.as_str()) && prefix.len() > "res://".len()
                    })
                    .map(move |prefix| DynamicSite {
                        file: load.file.clone(),
                        line: load.line,
                        prefix: prefix.clone(),
                    })
            })
            .collect();
        let possible_count = possible.len();
        let possible: Vec<DynamicSite> = possible.into_iter().take(MAX_LISTED).collect();
        let sidecars: Vec<String> = [format!("{path}.import"), format!("{path}.uid")]
            .into_iter()
            .filter(|sidecar| model.sources().contains(sidecar))
            .collect();
        unused.push(UnusedFile {
            path: path.clone(),
            kind: *kind,
            reason: if by.is_empty() {
                UnusedReason::Unreferenced
            } else {
                UnusedReason::ReferencedOnlyByUnused
            },
            referenced_by_count: by.len(),
            referenced_by: by
                .iter()
                .take(MAX_LISTED)
                .map(|path| (*path).to_owned())
                .collect(),
            possible_dynamic_loads: possible,
            possible_dynamic_load_count: possible_count,
            sidecars,
            confidence: Confidence::Inferred,
        });
    }
    UnusedReport {
        unused,
        candidates,
        used,
        roots: graph.roots().to_vec(),
        dynamic_loads: graph.dynamic_loads().to_vec(),
        broken_references: graph.broken().to_vec(),
        coverage: graph.coverage().clone(),
    }
}

// -- cycles -------------------------------------------------------------------------

/// What Godot does with a cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CycleKind {
    /// Scripts that `extends` each other in a ring: they do not compile.
    Inheritance,
    /// Scenes and resources whose `[ext_resource]`s form a ring: the loader
    /// rejects the reference that closes the ring and loads the rest without
    /// it.
    ResourceLoad,
    /// A ring that goes through `preload`, `load`, a class-name usage or a path
    /// string: Godot loads it.
    Tolerated,
}

impl CycleKind {
    /// Whether Godot reports an error on the cycle and loses part of it.
    #[must_use]
    pub const fn breaks_loading(self) -> bool {
        matches!(self, Self::Inheritance | Self::ResourceLoad)
    }

    const fn explanation(self) -> &'static str {
        match self {
            Self::Inheritance => {
                "scripts extend each other in a ring; GDScript reports \"Could not resolve super class inheritance\" and none of them compiles, and the Godot 4.4.1 and 4.5.1 editors crash while scanning a project that holds one"
            }
            Self::ResourceLoad => {
                "scenes or resources depend on each other through [ext_resource]; the loader rejects the reference that closes the ring (\"[ext_resource] referenced non-existent resource\") and loads the rest without it"
            }
            Self::Tolerated => {
                "the ring passes through preload, load, a class-name usage or a path string, which Godot resolves without failing"
            }
        }
    }
}

/// One edge of a cycle.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct CycleEdge {
    /// Using file.
    pub from: String,
    /// Used file.
    pub to: String,
    /// How it is used.
    pub kind: ReferenceKind,
    /// 1-based line in [`Self::from`].
    pub line: u32,
}

/// One strongly connected set of files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Cycle {
    /// The members, in order (first 100).
    pub members: Vec<String>,
    /// How many files the cycle joins.
    pub member_count: usize,
    /// What Godot does with it.
    pub classification: CycleKind,
    /// Whether Godot fails on it.
    pub breaks_loading: bool,
    /// What the classification means.
    pub explanation: String,
    /// One concrete ring of the classification's edges, starting and ending
    /// at the same member.
    pub witness: Vec<CycleEdge>,
    /// Every edge between members (first 100).
    pub edges: Vec<CycleEdge>,
    /// How many edges join members.
    pub edge_count: usize,
    /// `exact` when every witness edge was parsed from an undamaged
    /// declaration.
    pub confidence: Confidence,
}

/// The dependency-cycles view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CycleReport {
    /// Every cycle, breaking ones first, then by first member.
    pub cycles: Vec<Cycle>,
    /// What the analysis could not see.
    pub coverage: Coverage,
}

/// Find every strongly connected set of files.
#[must_use]
pub fn dependency_cycles(model: &ProjectModel<'_>) -> CycleReport {
    let graph = ReferenceGraph::build(model);
    let references = graph.references();
    let all: Vec<&Reference> = references.iter().collect();
    let mut cycles = Vec::new();
    for component in strongly_connected(&all) {
        let members: BTreeSet<&str> = component.iter().copied().collect();
        let inner: Vec<&Reference> = references
            .iter()
            .filter(|reference| {
                members.contains(reference.from.as_str()) && members.contains(reference.to.as_str())
            })
            .collect();
        let is_cycle =
            members.len() > 1 || inner.iter().any(|reference| reference.from == reference.to);
        if !is_cycle {
            continue;
        }
        let inheritance: Vec<&Reference> = inner
            .iter()
            .copied()
            .filter(|reference| reference.kind == ReferenceKind::Extends)
            .collect();
        let loading: Vec<&Reference> = inner
            .iter()
            .copied()
            .filter(|reference| {
                reference.kind.is_document_load()
                    && matches!(
                        model.kind(&reference.from),
                        Some(FileKind::Scene | FileKind::Resource)
                    )
            })
            .collect();
        let (classification, witness) = if let Some(ring) = find_ring(&inheritance) {
            (CycleKind::Inheritance, ring)
        } else if let Some(ring) = find_ring(&loading) {
            (CycleKind::ResourceLoad, ring)
        } else {
            (CycleKind::Tolerated, find_ring(&inner).unwrap_or_default())
        };
        let confidence = witness.iter().fold(Confidence::Exact, |floor, reference| {
            weakest(floor, reference.confidence)
        });
        let edge = |reference: &Reference| CycleEdge {
            from: reference.from.clone(),
            to: reference.to.clone(),
            kind: reference.kind,
            line: reference.line,
        };
        cycles.push(Cycle {
            member_count: members.len(),
            members: members
                .iter()
                .take(MAX_CYCLE_MEMBERS)
                .map(|member| (*member).to_owned())
                .collect(),
            classification,
            breaks_loading: classification.breaks_loading(),
            explanation: classification.explanation().to_owned(),
            witness: witness.iter().map(|reference| edge(reference)).collect(),
            edge_count: inner.len(),
            edges: inner
                .iter()
                .take(MAX_CYCLE_EDGES)
                .map(|reference| edge(reference))
                .collect(),
            confidence,
        });
    }
    cycles.sort_by(|left, right| {
        (!left.breaks_loading, &left.members).cmp(&(!right.breaks_loading, &right.members))
    });
    CycleReport {
        cycles,
        coverage: graph.coverage().clone(),
    }
}

/// Tarjan's strongly connected components, iteratively.
fn strongly_connected<'r>(references: &[&'r Reference]) -> Vec<Vec<&'r str>> {
    let mut nodes: BTreeSet<&str> = BTreeSet::new();
    let mut adjacency: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for reference in references {
        nodes.insert(&reference.from);
        nodes.insert(&reference.to);
        adjacency
            .entry(&reference.from)
            .or_default()
            .push(&reference.to);
    }
    for targets in adjacency.values_mut() {
        targets.sort_unstable();
        targets.dedup();
    }
    let mut index: BTreeMap<&str, usize> = BTreeMap::new();
    let mut low: BTreeMap<&str, usize> = BTreeMap::new();
    let mut on_stack: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<&str> = Vec::new();
    let mut components = Vec::new();
    let mut counter = 0;
    let empty: Vec<&str> = Vec::new();
    for &start in &nodes {
        if index.contains_key(start) {
            continue;
        }
        let mut work: Vec<(&str, usize)> = vec![(start, 0)];
        index.insert(start, counter);
        low.insert(start, counter);
        counter += 1;
        stack.push(start);
        on_stack.insert(start);
        while let Some((node, next)) = work.pop() {
            let targets = adjacency.get(node).unwrap_or(&empty);
            if next < targets.len() {
                work.push((node, next + 1));
                let target = targets[next];
                if !index.contains_key(target) {
                    index.insert(target, counter);
                    low.insert(target, counter);
                    counter += 1;
                    stack.push(target);
                    on_stack.insert(target);
                    work.push((target, 0));
                } else if on_stack.contains(target) {
                    let candidate = index[target];
                    let entry = low.get_mut(node).expect("visited");
                    *entry = (*entry).min(candidate);
                }
                continue;
            }
            if low[node] == index[node] {
                let mut component = Vec::new();
                while let Some(member) = stack.pop() {
                    on_stack.remove(member);
                    component.push(member);
                    if member == node {
                        break;
                    }
                }
                component.sort_unstable();
                components.push(component);
            }
            if let Some(&(parent, _)) = work.last() {
                let child_low = low[node];
                let entry = low.get_mut(parent).expect("visited");
                *entry = (*entry).min(child_low);
            }
        }
    }
    components
}

/// One ring among `edges`, as a list of edges from a member back to itself,
/// preferring the ring through the smallest member.
fn find_ring<'r>(edges: &[&'r Reference]) -> Option<Vec<&'r Reference>> {
    if edges.is_empty() {
        return None;
    }
    if let Some(self_loop) = edges
        .iter()
        .find(|reference| reference.from == reference.to)
    {
        return Some(vec![*self_loop]);
    }
    let mut outgoing: BTreeMap<&str, Vec<&'r Reference>> = BTreeMap::new();
    for reference in edges {
        outgoing.entry(&reference.from).or_default().push(reference);
    }
    let starts: BTreeSet<&str> = edges
        .iter()
        .map(|reference| reference.from.as_str())
        .collect();
    for start in starts {
        let mut previous: BTreeMap<&str, &'r Reference> = BTreeMap::new();
        let mut queue: VecDeque<&str> = VecDeque::from([start]);
        let mut visited: BTreeSet<&str> = BTreeSet::from([start]);
        while let Some(current) = queue.pop_front() {
            for reference in outgoing.get(current).into_iter().flatten() {
                if reference.to == start {
                    let mut ring = vec![*reference];
                    let mut cursor = current;
                    while cursor != start {
                        let step = previous[cursor];
                        ring.push(step);
                        cursor = &step.from;
                    }
                    ring.reverse();
                    return Some(ring);
                }
                if visited.insert(&reference.to) {
                    previous.insert(&reference.to, reference);
                    queue.push_back(&reference.to);
                }
            }
        }
    }
    None
}

/// The `res://` URI of a project-relative path, or the path's own target when
/// it is already a URI.
#[must_use]
pub fn res_uri(path: &str) -> String {
    match resource_target(path) {
        ResourceTarget::ProjectRelative { res_uri, .. } => res_uri,
        _ => format!("res://{path}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::insight::model::ProjectSources;
    use crate::project::ProjectSettings;

    fn model_of<'s>(
        sources: &'s ProjectSources,
        settings: Option<&'s ProjectSettings>,
    ) -> ProjectModel<'s> {
        ProjectModel::build(sources, settings, 1)
    }

    #[test]
    fn relative_literals_resolve_like_godot() {
        assert_eq!(
            absolute_from("a/b/c.gd", "d.tscn", true),
            "res://a/b/d.tscn"
        );
        assert_eq!(
            absolute_from("a/b/c.gd", "../d.tscn", true),
            "res://a/d.tscn"
        );
        assert_eq!(absolute_from("a/b/c.gd", "d.tscn", false), "res://d.tscn");
        assert_eq!(
            absolute_from("c.gd", "../../x.gd", true),
            "res://../../x.gd"
        );
        assert_eq!(absolute_from("a.gd", "res://x.gd", true), "res://x.gd");
    }

    #[test]
    fn reachability_starts_at_settings_roots() {
        let mut sources = ProjectSources::new();
        let settings_text = "config_version=5\n\n[application]\n\nrun/main_scene=\"res://main.tscn\"\nconfig/icon=\"res://icon.svg\"\n\n[autoload]\n\nState=\"*res://state.gd\"\n";
        sources.insert_text("project.godot", settings_text);
        sources.insert_text(
            "main.tscn",
            "[gd_scene format=3]\n\n[ext_resource type=\"Script\" path=\"res://main.gd\" id=\"1\"]\n\n[node name=\"Main\" type=\"Node\"]\nscript = ExtResource(\"1\")\n",
        );
        sources.insert_text("main.gd", "extends Node\nconst NEXT = preload(\"res://level.tscn\")\nfunc _ready():\n\tload(\"res://levels/\" + name)\n");
        sources.insert_text(
            "level.tscn",
            "[gd_scene format=3]\n\n[node name=\"Level\" type=\"Node\"]\n",
        );
        sources.insert_text("state.gd", "extends Node\n");
        sources.insert_text("icon.svg", "<svg/>");
        sources.insert_text(
            "old.gd",
            "extends Node\nvar x = preload(\"res://older.tscn\")\n",
        );
        sources.insert_text(
            "older.tscn",
            "[gd_scene format=3]\n\n[node name=\"Old\" type=\"Node\"]\n",
        );
        sources.insert_text(
            "levels/two.tscn",
            "[gd_scene format=3]\n\n[node name=\"Two\" type=\"Node\"]\n",
        );
        let settings = ProjectSettings::parse(settings_text, 1).expect("settings");
        let model = model_of(&sources, Some(&settings));
        let report = unused_files(&model, &GlobSet::default());
        let unused: Vec<(&str, UnusedReason)> = report
            .unused
            .iter()
            .map(|entry| (entry.path.as_str(), entry.reason))
            .collect();
        assert_eq!(
            unused,
            vec![
                ("levels/two.tscn", UnusedReason::Unreferenced),
                ("old.gd", UnusedReason::Unreferenced),
                ("older.tscn", UnusedReason::ReferencedOnlyByUnused),
            ]
        );
        assert_eq!(
            report.unused[0].possible_dynamic_loads[0].prefix,
            "res://levels/"
        );
        assert_eq!(report.dynamic_loads.len(), 1);
        assert!(!report.coverage.complete);
        assert_eq!(report.candidates, 8);
        assert_eq!(report.used, 5);
    }

    #[test]
    fn cycles_are_classified_by_their_edges() {
        let mut sources = ProjectSources::new();
        sources.insert_text("a.gd", "extends \"res://b.gd\"\n");
        sources.insert_text("b.gd", "extends \"res://a.gd\"\n");
        sources.insert_text("c.gd", "extends Node\nvar s = preload(\"res://d.gd\")\n");
        sources.insert_text("d.gd", "extends Node\nvar s = preload(\"res://c.gd\")\n");
        sources.insert_text(
            "x.tscn",
            "[gd_scene format=3]\n\n[ext_resource type=\"PackedScene\" path=\"res://y.tscn\" id=\"1\"]\n\n[node name=\"X\" type=\"Node\"]\n\n[node name=\"Y\" parent=\".\" instance=ExtResource(\"1\")]\n",
        );
        sources.insert_text(
            "y.tscn",
            "[gd_scene format=3]\n\n[ext_resource type=\"PackedScene\" path=\"res://x.tscn\" id=\"1\"]\n\n[node name=\"Y\" type=\"Node\"]\n\n[node name=\"X\" parent=\".\" instance=ExtResource(\"1\")]\n",
        );
        let model = model_of(&sources, None);
        let report = dependency_cycles(&model);
        let summary: Vec<(Vec<&str>, CycleKind)> = report
            .cycles
            .iter()
            .map(|cycle| {
                (
                    cycle.members.iter().map(String::as_str).collect(),
                    cycle.classification,
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (vec!["a.gd", "b.gd"], CycleKind::Inheritance),
                (vec!["x.tscn", "y.tscn"], CycleKind::ResourceLoad),
                (vec!["c.gd", "d.gd"], CycleKind::Tolerated),
            ]
        );
        assert_eq!(report.cycles[0].witness.len(), 2);
        assert_eq!(
            report.cycles[0].witness[0].from,
            report.cycles[0].witness[1].to
        );
    }
}
