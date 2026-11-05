//! The inputs every view shares and the per-request parsed project.

use std::collections::{BTreeMap, BTreeSet};

use godot_mcp_domain::Confidence;
use serde::Serialize;

use super::script_facts::ScriptFacts;
use super::uid::UidTable;
use crate::gdscript::{GdScriptFile, GdScriptParser};
use crate::project::{ProjectSettings, ResourceTarget, resource_target};
use crate::scene::{ResourceRef, ResourceRefKind, SceneDocument, parse_document};

/// Largest file whose text the views analyse, in bytes. A larger file is
/// counted but not read as text, and the view's coverage says so.
pub const MAX_TEXT_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// How many leading bytes are inspected for a NUL when deciding whether a file
/// is binary (the heuristic Git uses).
const BINARY_SNIFF_BYTES: usize = 8_000;

/// Deepest chain of instanced or inherited scenes followed when resolving one
/// node. Godot itself refuses cyclic instancing; this bound keeps a cyclic
/// project from looping here.
const MAX_INSTANCE_DEPTH: usize = 32;

/// Longest script inheritance chain followed.
const MAX_CHAIN_LENGTH: usize = 64;

/// Most subjects one coverage gap lists by name; the count is always exact.
const MAX_GAP_SUBJECTS: usize = 20;

/// What a caller supplied for one inventoried file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileContent {
    /// The bytes decoded as UTF-8 text (a leading byte-order mark removed).
    Text(String),
    /// The bytes hold a NUL in their first 8,000 bytes or are not UTF-8.
    Binary,
    /// The file is larger than [`MAX_TEXT_FILE_BYTES`] and was not decoded.
    TooLarge,
    /// The caller did not read the file; the reason says why.
    Unread(String),
}

/// One inventoried file: what was read, and how long it is when known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    content: FileContent,
    bytes: Option<u64>,
}

impl SourceFile {
    /// What was read.
    #[must_use]
    pub const fn content(&self) -> &FileContent {
        &self.content
    }

    /// The file's length, when it was read.
    #[must_use]
    pub const fn bytes(&self) -> Option<u64> {
        self.bytes
    }

    /// The file's text, when it was read and decoded.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match &self.content {
            FileContent::Text(text) => Some(text),
            _ => None,
        }
    }
}

/// Why Godot's editor file system leaves an inventoried file out.
///
/// Godot never scans such a file: it writes no `.uid` sidecar or import for
/// it and registers no `class_name` it declares (observed on Godot 4.4.1 and
/// 4.7.1). The views follow Godot and leave it out as well; a reference to it
/// still resolves, because the file exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Exclusion {
    /// The file's name, or a directory's name above it, starts with `.`.
    Hidden,
    /// A directory above the file holds a `.gdignore` file. Godot does not
    /// honour one at the project root, and neither does this.
    GdIgnore,
}

/// Which inventoried files Godot's editor file system ignores.
#[derive(Debug, Clone, Default)]
pub struct GodotFileView {
    ignored_directories: BTreeSet<String>,
}

impl GodotFileView {
    /// The view of a project whose inventory is `paths`.
    pub fn new<'a>(paths: impl IntoIterator<Item = &'a str>) -> Self {
        let ignored_directories = paths
            .into_iter()
            .filter_map(|path| path.strip_suffix("/.gdignore"))
            .map(str::to_owned)
            .collect();
        Self {
            ignored_directories,
        }
    }

    /// Why Godot ignores `path`, or `None` when it scans it.
    #[must_use]
    pub fn exclusion(&self, path: &str) -> Option<Exclusion> {
        if path.split('/').any(|component| component.starts_with('.')) {
            return Some(Exclusion::Hidden);
        }
        let ignored = path
            .match_indices('/')
            .any(|(end, _)| self.ignored_directories.contains(&path[..end]));
        ignored.then_some(Exclusion::GdIgnore)
    }
}

/// How many inventoried files the views left out because Godot ignores them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct IgnoredFiles {
    /// Files with a hidden name or under a hidden directory.
    pub hidden: usize,
    /// Files under a directory that holds a `.gdignore`.
    pub gdignore: usize,
}

/// Every inventoried file of one project, with whatever the caller read for it.
///
/// The set of paths is the project inventory: a path absent here is not part
/// of the project as far as the views are concerned, while a path present with
/// [`FileContent::Unread`] exists but was not analysed. A file Godot ignores
/// ([`Exclusion`]) exists, so references to it resolve, but it is never listed,
/// read or analysed.
#[derive(Debug, Clone, Default)]
pub struct ProjectSources {
    files: BTreeMap<String, SourceFile>,
    excluded: BTreeMap<String, Exclusion>,
}

impl ProjectSources {
    /// An empty inventory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the bytes read for `path`, classifying them as text or binary.
    pub fn insert_bytes(&mut self, path: impl Into<String>, bytes: &[u8]) {
        let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let sniff = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
        let content = if sniff.contains(&0) {
            FileContent::Binary
        } else if length > MAX_TEXT_FILE_BYTES {
            FileContent::TooLarge
        } else {
            match std::str::from_utf8(bytes) {
                Ok(text) => {
                    FileContent::Text(text.strip_prefix('\u{feff}').unwrap_or(text).to_owned())
                }
                Err(_) => FileContent::Binary,
            }
        };
        self.files.insert(
            path.into(),
            SourceFile {
                content,
                bytes: Some(length),
            },
        );
    }

    /// Record text for `path` (a convenience for callers that already decoded
    /// it).
    pub fn insert_text(&mut self, path: impl Into<String>, text: &str) {
        self.insert_bytes(path, text.as_bytes());
    }

    /// Record that `path` is binary by its extension (see
    /// [`is_binary_by_extension`]) and was deliberately not read.
    pub fn insert_binary_unread(&mut self, path: impl Into<String>) {
        self.files.insert(
            path.into(),
            SourceFile {
                content: FileContent::Binary,
                bytes: None,
            },
        );
    }

    /// Record that `path` exists but was not read.
    pub fn insert_unread(&mut self, path: impl Into<String>, reason: impl Into<String>) {
        self.files.insert(
            path.into(),
            SourceFile {
                content: FileContent::Unread(reason.into()),
                bytes: None,
            },
        );
    }

    /// Record that `path` exists but Godot ignores it, so no view lists it.
    pub fn insert_excluded(&mut self, path: impl Into<String>, exclusion: Exclusion) {
        let path = path.into();
        self.files.remove(&path);
        self.excluded.insert(path, exclusion);
    }

    /// Why Godot ignores `path`, when it does.
    #[must_use]
    pub fn exclusion(&self, path: &str) -> Option<Exclusion> {
        self.excluded.get(path).copied()
    }

    /// How many inventoried files Godot ignores, by reason.
    #[must_use]
    pub fn ignored(&self) -> IgnoredFiles {
        let mut ignored = IgnoredFiles::default();
        for exclusion in self.excluded.values() {
            match exclusion {
                Exclusion::Hidden => ignored.hidden += 1,
                Exclusion::GdIgnore => ignored.gdignore += 1,
            }
        }
        ignored
    }

    /// One file.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&SourceFile> {
        self.files.get(path)
    }

    /// One file's text, when it was read and decoded.
    #[must_use]
    pub fn text(&self, path: &str) -> Option<&str> {
        self.files.get(path).and_then(SourceFile::text)
    }

    /// Whether `path` is part of the inventory, ignored by Godot or not.
    #[must_use]
    pub fn contains(&self, path: &str) -> bool {
        self.files.contains_key(path) || self.excluded.contains_key(path)
    }

    /// Every inventoried path Godot does not ignore, in order.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// Every inventoried file Godot does not ignore, in path order.
    pub fn files(&self) -> impl Iterator<Item = (&str, &SourceFile)> {
        self.files.iter().map(|(path, file)| (path.as_str(), file))
    }

    /// Number of inventoried files Godot does not ignore.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether no inventoried file is left once Godot's ignored files are
    /// set aside.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// What a project file is, decided from its path (and whether Godot keeps an
/// import sidecar for it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    /// `project.godot`.
    ProjectSettings,
    /// `export_presets.cfg`.
    ExportPresets,
    /// A `.gd` GDScript source.
    Script,
    /// A `.cs` C# source, which this layer does not parse.
    CSharpScript,
    /// A `.tscn` text scene.
    Scene,
    /// A `.scn` binary scene, which this layer does not decode.
    BinaryScene,
    /// A `.tres` text resource.
    Resource,
    /// A `.res` binary resource, which this layer does not decode.
    BinaryResource,
    /// A `.gdshader` (or legacy `.shader`) shader.
    Shader,
    /// A `.gdshaderinc` shader include.
    ShaderInclude,
    /// An editor plugin's `plugin.cfg`.
    PluginDescriptor,
    /// A `.gdextension` native-extension descriptor.
    Extension,
    /// A `.uid` sidecar recording a script's or shader's UID (Godot 4.4+).
    UidSidecar,
    /// A `.import` sidecar recording how an asset is imported.
    ImportSidecar,
    /// An imported asset: a file with an `.import` sidecar, or an extension
    /// Godot imports (images, audio, fonts, models).
    Asset,
    /// Other text: JSON, configuration, documentation.
    Text,
    /// Anything else.
    Other,
}

const ASSET_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "svg", "svgz", "bmp", "tga", "exr", "hdr", "dds", "ktx", "ktx2",
    "wav", "ogg", "mp3", "ttf", "otf", "woff", "woff2", "fnt", "font", "pfb", "pfm", "glb", "gltf",
    "obj", "fbx", "blend", "dae", "escn", "ogv", "webm", "mtl",
];

/// Extensions whose files are binary by format, so a text view need not read
/// them.
const BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "bmp", "tga", "exr", "hdr", "dds", "ktx", "ktx2", "wav", "ogg",
    "mp3", "ttf", "otf", "woff", "woff2", "glb", "fbx", "blend", "ogv", "webm", "scn", "res",
    "ctex", "pck", "zip", "so", "dll", "dylib", "exe", "wasm", "a", "lib",
];

/// Whether `path` names a file that is binary by its format.
#[must_use]
pub fn is_binary_by_extension(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').is_some_and(|(_, extension)| {
        BINARY_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
    })
}

const TEXT_EXTENSIONS: &[&str] = &[
    "json",
    "cfg",
    "ini",
    "txt",
    "md",
    "csv",
    "tsv",
    "xml",
    "yaml",
    "yml",
    "toml",
    "html",
    "htm",
    "css",
    "js",
    "po",
    "pot",
    "gitignore",
    "gitattributes",
    "editorconfig",
    "translation",
];

impl FileKind {
    /// Stable wire code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ProjectSettings => "project_settings",
            Self::ExportPresets => "export_presets",
            Self::Script => "script",
            Self::CSharpScript => "c_sharp_script",
            Self::Scene => "scene",
            Self::BinaryScene => "binary_scene",
            Self::Resource => "resource",
            Self::BinaryResource => "binary_resource",
            Self::Shader => "shader",
            Self::ShaderInclude => "shader_include",
            Self::PluginDescriptor => "plugin_descriptor",
            Self::Extension => "extension",
            Self::UidSidecar => "uid_sidecar",
            Self::ImportSidecar => "import_sidecar",
            Self::Asset => "asset",
            Self::Text => "text",
            Self::Other => "other",
        }
    }

    /// Classify `path`. `imported` says whether `<path>.import` exists.
    #[must_use]
    pub fn classify(path: &str, imported: bool) -> Self {
        if path == "project.godot" {
            return Self::ProjectSettings;
        }
        if path == "export_presets.cfg" {
            return Self::ExportPresets;
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        if name == "plugin.cfg" {
            return Self::PluginDescriptor;
        }
        let extension = name
            .rsplit_once('.')
            .map(|(_, extension)| extension.to_ascii_lowercase())
            .unwrap_or_default();
        match extension.as_str() {
            "gd" => return Self::Script,
            "cs" => return Self::CSharpScript,
            "tscn" => return Self::Scene,
            "scn" => return Self::BinaryScene,
            "tres" => return Self::Resource,
            "res" => return Self::BinaryResource,
            "gdshader" | "shader" => return Self::Shader,
            "gdshaderinc" => return Self::ShaderInclude,
            "gdextension" => return Self::Extension,
            "uid" => return Self::UidSidecar,
            "import" => return Self::ImportSidecar,
            _ => {}
        }
        if imported || ASSET_EXTENSIONS.contains(&extension.as_str()) {
            return Self::Asset;
        }
        if TEXT_EXTENSIONS.contains(&extension.as_str()) {
            return Self::Text;
        }
        Self::Other
    }

    /// Whether a file of this kind is something the game loads, and so can be
    /// reported as unused.
    #[must_use]
    pub const fn is_loadable(self) -> bool {
        matches!(
            self,
            Self::Script
                | Self::Scene
                | Self::BinaryScene
                | Self::Resource
                | Self::BinaryResource
                | Self::Shader
                | Self::ShaderInclude
                | Self::Asset
        )
    }

    /// Whether the file is a sidecar Godot keeps beside another file.
    #[must_use]
    pub const fn is_sidecar(self) -> bool {
        matches!(self, Self::UidSidecar | Self::ImportSidecar)
    }
}

/// Where a coverage gap leaves a view's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CoverageGap {
    /// Stable code naming what was not seen.
    pub code: &'static str,
    /// What the gap means for this answer.
    pub message: String,
    /// How many files or sites the gap covers.
    pub count: usize,
    /// The first 20 of them, in order.
    pub subjects: Vec<String>,
}

/// Whether a view saw everything its answer depends on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Coverage {
    /// `true` exactly when no gap was recorded.
    pub complete: bool,
    /// Every gap, ordered by code.
    pub gaps: Vec<CoverageGap>,
}

/// Accumulates [`CoverageGap`]s while a view runs.
#[derive(Debug, Default)]
pub(crate) struct CoverageBuilder {
    gaps: BTreeMap<&'static str, (String, BTreeSet<String>)>,
}

impl CoverageBuilder {
    pub(crate) fn note(&mut self, code: &'static str, message: &str, subject: impl Into<String>) {
        self.gaps
            .entry(code)
            .or_insert_with(|| (message.to_owned(), BTreeSet::new()))
            .1
            .insert(subject.into());
    }

    pub(crate) fn finish(self) -> Coverage {
        let gaps: Vec<CoverageGap> = self
            .gaps
            .into_iter()
            .map(|(code, (message, subjects))| CoverageGap {
                code,
                message,
                count: subjects.len(),
                subjects: subjects.into_iter().take(MAX_GAP_SUBJECTS).collect(),
            })
            .collect();
        Coverage {
            complete: gaps.is_empty(),
            gaps,
        }
    }
}

/// What one reference value names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Target {
    /// An inventoried project file.
    File {
        /// Project-relative path.
        path: String,
        /// The `uid://` handle the file was found through, when it was.
        via_uid: Option<String>,
    },
    /// A well-formed `res://` path that names no inventoried file.
    Missing {
        /// Project-relative path.
        path: String,
    },
    /// A `uid://` handle no recorded file carries.
    UnknownUid {
        /// The handle.
        uid: String,
    },
    /// A `uid://` handle more than one file claims.
    AmbiguousUid {
        /// The handle.
        uid: String,
        /// Every claiming file, in order.
        paths: Vec<String>,
    },
    /// Not a project reference: another scheme, or a path escaping the root.
    Unsupported {
        /// The value exactly as written.
        value: String,
    },
}

impl Target {
    /// The project file, when one was found.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::File { path, .. } => Some(path),
            _ => None,
        }
    }
}

/// Where a scene node's type or script came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeOrigin {
    /// Declared on the node itself.
    Declared,
    /// Taken from the root of the scene the node instances.
    Instance,
    /// The node belongs to an instanced or inherited scene and is described
    /// there.
    InstancedScene,
    /// Could not be established.
    Unknown,
}

/// How a scene node's script was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptOrigin {
    /// `script = ExtResource(...)` on the node itself.
    Attached,
    /// Inherited from the scene the node instances or belongs to.
    InstancedScene,
    /// A built-in script embedded in the scene, which this layer does not
    /// analyse.
    BuiltIn,
    /// The node has no script.
    None,
    /// A script is attached but its file could not be identified.
    Unresolved,
}

/// One scene node with its type and script resolved through instancing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedNode {
    /// Declared or inherited node type (an engine class name).
    pub type_name: Option<String>,
    /// Where the type came from.
    pub type_origin: NodeOrigin,
    /// Effective script file.
    pub script: Option<String>,
    /// How the script was established.
    pub script_origin: ScriptOrigin,
    /// The scene this node instances directly.
    pub instance: Option<String>,
    /// The scene the node is described in, when that is an instanced or
    /// inherited scene rather than the queried one.
    pub described_in: Option<String>,
    /// `exact` when every step was proven, `inferred` when a step relied on a
    /// damaged document, `unresolved` when a step could not be decided.
    pub confidence: Confidence,
}

impl ResolvedNode {
    fn unknown() -> Self {
        Self {
            type_name: None,
            type_origin: NodeOrigin::Unknown,
            script: None,
            script_origin: ScriptOrigin::Unresolved,
            instance: None,
            described_in: None,
            confidence: Confidence::Unresolved,
        }
    }
}

/// A script and the project scripts it inherits from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScriptChain {
    /// The script itself first, then each base script in order.
    pub scripts: Vec<String>,
    /// The engine class the chain ends in, when it ends in one.
    pub engine_base: Option<String>,
    /// Whether every link was resolved from an undamaged script.
    pub complete: bool,
    /// Why the chain stopped early, when it did.
    pub unresolved: Option<String>,
}

/// One parsed script: the symbol layer's record and this module's facts.
#[derive(Debug, Clone)]
pub struct ScriptModel {
    symbols: Option<GdScriptFile>,
    facts: Option<ScriptFacts>,
    failure: Option<String>,
}

impl ScriptModel {
    /// Classes, functions, signals and variables, as the symbol layer parsed
    /// them.
    #[must_use]
    pub const fn symbols(&self) -> Option<&GdScriptFile> {
        self.symbols.as_ref()
    }

    /// Load calls, literals, signal uses, metrics and exports.
    #[must_use]
    pub const fn facts(&self) -> Option<&ScriptFacts> {
        self.facts.as_ref()
    }

    /// Why the script could not be parsed, when it could not.
    #[must_use]
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Whether any part of the script failed to parse.
    #[must_use]
    pub fn is_damaged(&self) -> bool {
        self.failure.is_some()
            || self.symbols.as_ref().is_some_and(GdScriptFile::is_partial)
            || self.facts.as_ref().is_some_and(ScriptFacts::is_damaged)
    }
}

/// The whole project parsed once for one request.
pub struct ProjectModel<'s> {
    sources: &'s ProjectSources,
    settings: Option<&'s ProjectSettings>,
    generation: u64,
    kinds: BTreeMap<String, FileKind>,
    scripts: BTreeMap<String, ScriptModel>,
    documents: BTreeMap<String, Result<SceneDocument, String>>,
    class_names: BTreeMap<String, Vec<String>>,
    uids: UidTable,
}

impl<'s> ProjectModel<'s> {
    /// Parse every script, scene and resource `sources` holds text for.
    #[must_use]
    pub fn build(
        sources: &'s ProjectSources,
        settings: Option<&'s ProjectSettings>,
        generation: u64,
    ) -> Self {
        let kinds: BTreeMap<String, FileKind> = sources
            .paths()
            .map(|path| {
                let imported = sources.contains(&format!("{path}.import"));
                (path.to_owned(), FileKind::classify(path, imported))
            })
            .collect();

        let mut parser = GdScriptParser::new().ok();
        let mut scripts = BTreeMap::new();
        let mut documents = BTreeMap::new();
        for (path, kind) in &kinds {
            match kind {
                FileKind::Script => {
                    if let Some(text) = sources.text(path) {
                        scripts.insert(
                            path.clone(),
                            parse_script(parser.as_mut(), path, text, generation),
                        );
                    }
                }
                FileKind::Scene | FileKind::Resource => {
                    if let Some(text) = sources.text(path) {
                        documents.insert(
                            path.clone(),
                            parse_document(path, text, generation)
                                .map_err(|error| error.to_string()),
                        );
                    }
                }
                _ => {}
            }
        }

        let mut class_names: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (path, script) in &scripts {
            if let Some(name) = script
                .symbols
                .as_ref()
                .and_then(GdScriptFile::class_name_declaration)
            {
                class_names
                    .entry(name.to_owned())
                    .or_default()
                    .push(path.clone());
            }
        }

        let uids = UidTable::collect(
            sources,
            documents.iter().filter_map(|(path, document)| {
                document
                    .as_ref()
                    .ok()
                    .map(|document| (path.as_str(), document))
            }),
        );

        Self {
            sources,
            settings,
            generation,
            kinds,
            scripts,
            documents,
            class_names,
            uids,
        }
    }

    /// The inventory and its contents.
    #[must_use]
    pub const fn sources(&self) -> &'s ProjectSources {
        self.sources
    }

    /// The parsed `project.godot`, when the caller supplied it.
    #[must_use]
    pub const fn settings(&self) -> Option<&'s ProjectSettings> {
        self.settings
    }

    /// The project generation every parse was attributed to.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Every inventoried path with its kind, in path order.
    #[must_use]
    pub const fn kinds(&self) -> &BTreeMap<String, FileKind> {
        &self.kinds
    }

    /// One path's kind.
    #[must_use]
    pub fn kind(&self, path: &str) -> Option<FileKind> {
        self.kinds.get(path).copied()
    }

    /// Every script read as text, in path order.
    #[must_use]
    pub const fn scripts(&self) -> &BTreeMap<String, ScriptModel> {
        &self.scripts
    }

    /// One script.
    #[must_use]
    pub fn script(&self, path: &str) -> Option<&ScriptModel> {
        self.scripts.get(path)
    }

    /// Every scene and resource read as text, in path order, with the parse
    /// failure for one that could not be modelled.
    #[must_use]
    pub const fn documents(&self) -> &BTreeMap<String, Result<SceneDocument, String>> {
        &self.documents
    }

    /// One parsed scene or resource.
    #[must_use]
    pub fn document(&self, path: &str) -> Option<&SceneDocument> {
        self.documents
            .get(path)
            .and_then(|document| document.as_ref().ok())
    }

    /// Scripts registering `name` with `class_name`.
    #[must_use]
    pub fn class_scripts(&self, name: &str) -> &[String] {
        self.class_names.get(name).map_or(&[], Vec::as_slice)
    }

    /// Every `class_name` registration, by name.
    #[must_use]
    pub const fn class_names(&self) -> &BTreeMap<String, Vec<String>> {
        &self.class_names
    }

    /// The UID table reconstructed from headers and sidecars.
    #[must_use]
    pub const fn uids(&self) -> &UidTable {
        &self.uids
    }

    /// Resolve a `res://` or `uid://` value to a project file.
    #[must_use]
    pub fn resolve(&self, value: &str) -> Target {
        match resource_target(value) {
            ResourceTarget::ProjectRelative { path, .. } => self.file_or_missing(path),
            ResourceTarget::Uid(uid) => self.resolve_uid(&uid),
            ResourceTarget::Unsupported(value) => Target::Unsupported { value },
        }
    }

    fn file_or_missing(&self, path: String) -> Target {
        if self.sources.contains(&path) {
            Target::File {
                path,
                via_uid: None,
            }
        } else {
            Target::Missing { path }
        }
    }

    fn resolve_uid(&self, uid: &str) -> Target {
        let paths = self.uids.paths_of(uid);
        match paths.as_slice() {
            [] => Target::UnknownUid {
                uid: uid.to_owned(),
            },
            [only] => Target::File {
                path: (*only).to_owned(),
                via_uid: Some(uid.to_owned()),
            },
            many => Target::AmbiguousUid {
                uid: uid.to_owned(),
                paths: many.iter().map(|path| (*path).to_owned()).collect(),
            },
        }
    }

    /// Resolve an `[ext_resource]` the way Godot loads it: a known `uid` wins
    /// and the `path` is the fallback.
    #[must_use]
    pub fn resolve_ext_resource(&self, uid: Option<&str>, target: &ResourceTarget) -> Target {
        if let Some(uid) = uid {
            let by_uid = self.resolve_uid(uid);
            if matches!(by_uid, Target::File { .. } | Target::AmbiguousUid { .. }) {
                return by_uid;
            }
        }
        match target {
            ResourceTarget::ProjectRelative { path, .. } => self.file_or_missing(path.clone()),
            ResourceTarget::Uid(uid) => self.resolve_uid(uid),
            ResourceTarget::Unsupported(value) => Target::Unsupported {
                value: value.clone(),
            },
        }
    }

    /// Resolve an `ExtResource("id")` handle of `document`.
    #[must_use]
    pub fn resolve_handle(&self, document: &SceneDocument, handle: &ResourceRef) -> Option<Target> {
        if handle.kind() != ResourceRefKind::ExtResource {
            return None;
        }
        let declaration = document
            .ext_resources()
            .iter()
            .find(|declaration| declaration.id() == Some(handle.id()))?;
        Some(self.resolve_ext_resource(declaration.uid(), declaration.target()))
    }

    /// The script `name` (an `extends` operand) denotes, relative to `from`.
    pub(crate) fn base_script(&self, from: &str, base: &str) -> Result<Option<String>, String> {
        let looks_like_path =
            base.starts_with("res://") || base.starts_with("uid://") || base.ends_with(".gd");
        if looks_like_path {
            let value = if base.starts_with("res://") || base.starts_with("uid://") {
                base.to_owned()
            } else {
                let directory = from.rsplit_once('/').map_or("", |(directory, _)| directory);
                if directory.is_empty() {
                    format!("res://{base}")
                } else {
                    format!("res://{directory}/{base}")
                }
            };
            return match self.resolve(&value) {
                Target::File { path, .. } => Ok(Some(path)),
                other => Err(format!(
                    "`extends {base}` names {}",
                    describe_target(&other)
                )),
            };
        }
        let head = base.split('.').next().unwrap_or(base);
        match self.class_scripts(head) {
            [] if head == base => Ok(None),
            [] => Err(format!(
                "`extends {base}` names an inner class of an engine type"
            )),
            [only] if head == base => Ok(Some(only.clone())),
            [_] => Err(format!(
                "`extends {base}` names an inner class, which is not followed"
            )),
            many => Err(format!(
                "`extends {base}` is ambiguous: {} scripts register class_name {head}",
                many.len()
            )),
        }
    }

    /// Follow `script`'s `extends` chain through project scripts.
    #[must_use]
    pub fn script_chain(&self, script: &str) -> ScriptChain {
        let mut chain = ScriptChain {
            scripts: vec![script.to_owned()],
            engine_base: None,
            complete: true,
            unresolved: None,
        };
        let mut seen = BTreeSet::from([script.to_owned()]);
        let mut current = script.to_owned();
        loop {
            let Some(model) = self.scripts.get(&current) else {
                chain.complete = false;
                chain.unresolved = Some(format!("{current} was not read as GDScript"));
                return chain;
            };
            if model.is_damaged() {
                chain.complete = false;
            }
            let Some(symbols) = model.symbols() else {
                chain.complete = false;
                chain.unresolved = Some(format!("{current} could not be parsed"));
                return chain;
            };
            let Some(base) = symbols.extends() else {
                // A script without `extends` inherits RefCounted.
                chain.engine_base = Some("RefCounted".to_owned());
                return chain;
            };
            match self.base_script(&current, base) {
                Ok(None) => {
                    chain.engine_base = Some(base.to_owned());
                    return chain;
                }
                Ok(Some(next)) => {
                    if !seen.insert(next.clone()) || chain.scripts.len() >= MAX_CHAIN_LENGTH {
                        chain.complete = false;
                        chain.unresolved = Some(format!("the inheritance of {script} is cyclic"));
                        return chain;
                    }
                    chain.scripts.push(next.clone());
                    current = next;
                }
                Err(reason) => {
                    chain.complete = false;
                    chain.unresolved = Some(reason);
                    return chain;
                }
            }
        }
    }

    /// Resolve the node at `node_path` (`.` is the root) of `scene` through
    /// instanced and inherited scenes.
    #[must_use]
    pub fn resolve_node(&self, scene: &str, node_path: &str) -> ResolvedNode {
        self.resolve_node_at(scene, node_path, 0)
    }

    fn resolve_node_at(&self, scene: &str, node_path: &str, depth: usize) -> ResolvedNode {
        if depth >= MAX_INSTANCE_DEPTH {
            return ResolvedNode::unknown();
        }
        let Some(document) = self.document(scene) else {
            return ResolvedNode::unknown();
        };
        let damaged = document.is_partial();
        let node = document.node_at(node_path);

        let mut resolved = ResolvedNode {
            type_name: node.and_then(|node| node.type_name()).map(str::to_owned),
            type_origin: NodeOrigin::Declared,
            script: None,
            script_origin: ScriptOrigin::None,
            instance: None,
            described_in: None,
            confidence: if damaged {
                Confidence::Inferred
            } else {
                Confidence::Exact
            },
        };

        // The node's own script, when it attaches one.
        if let Some(handle) = node.and_then(|node| node.script()) {
            match handle.kind() {
                ResourceRefKind::SubResource => resolved.script_origin = ScriptOrigin::BuiltIn,
                ResourceRefKind::ExtResource => match self.resolve_handle(document, handle) {
                    Some(Target::File { path, .. }) => {
                        resolved.script = Some(path);
                        resolved.script_origin = ScriptOrigin::Attached;
                    }
                    _ => {
                        resolved.script_origin = ScriptOrigin::Unresolved;
                        resolved.confidence = Confidence::Unresolved;
                    }
                },
            }
        }

        // The scene the node itself instances, or the instanced ancestor the
        // node belongs to.
        let base = match node.and_then(|node| node.instance()) {
            Some(handle) => match self.resolve_handle(document, handle) {
                Some(Target::File { path, .. }) => {
                    resolved.instance = Some(path.clone());
                    Some((path, ".".to_owned(), NodeOrigin::Instance))
                }
                _ => {
                    resolved.confidence = Confidence::Unresolved;
                    None
                }
            },
            None if resolved.type_name.is_none() => self
                .instanced_ancestor(document, node_path)
                .map(|(path, relative)| (path, relative, NodeOrigin::InstancedScene)),
            None => None,
        };

        if let Some((base_scene, relative, origin)) = base {
            let inner = self.resolve_node_at(&base_scene, &relative, depth + 1);
            if resolved.type_name.is_none() {
                resolved.type_name = inner.type_name.clone();
                resolved.type_origin = if inner.type_name.is_some() {
                    origin
                } else {
                    NodeOrigin::Unknown
                };
            }
            if resolved.script_origin == ScriptOrigin::None {
                resolved.script = inner.script.clone();
                resolved.script_origin = match inner.script_origin {
                    ScriptOrigin::Attached | ScriptOrigin::InstancedScene => {
                        ScriptOrigin::InstancedScene
                    }
                    other => other,
                };
            }
            if origin == NodeOrigin::InstancedScene {
                resolved.described_in = Some(base_scene);
            }
            resolved.confidence = weakest(resolved.confidence, inner.confidence);
        } else if node.is_none() {
            return ResolvedNode::unknown();
        } else if resolved.type_name.is_none() {
            resolved.type_origin = NodeOrigin::Unknown;
            resolved.confidence = weakest(resolved.confidence, Confidence::Unresolved);
        }
        resolved
    }

    /// The scene a node of `scene` is described in further, with its path
    /// there: the scene the node instances (at `.`), or, for a node without a
    /// declared type, the scene its nearest instanced ancestor instances.
    #[must_use]
    pub fn node_base(&self, scene: &str, node_path: &str) -> Option<(String, String)> {
        let document = self.document(scene)?;
        let node = document.node_at(node_path);
        if let Some(handle) = node.and_then(|node| node.instance()) {
            return match self.resolve_handle(document, handle) {
                Some(Target::File { path, .. }) => Some((path, ".".to_owned())),
                _ => None,
            };
        }
        if node.is_some_and(|node| node.type_name().is_some()) {
            return None;
        }
        self.instanced_ancestor(document, node_path)
    }

    /// The nearest ancestor of `node_path` (the root included) that instances
    /// a scene, with `node_path` expressed relative to that ancestor.
    fn instanced_ancestor(
        &self,
        document: &SceneDocument,
        node_path: &str,
    ) -> Option<(String, String)> {
        let mut ancestors: Vec<String> = Vec::new();
        if node_path != "." {
            let segments: Vec<&str> = node_path.split('/').collect();
            for end in (1..segments.len()).rev() {
                ancestors.push(segments[..end].join("/"));
            }
            ancestors.push(".".to_owned());
        }
        for ancestor in ancestors {
            let Some(node) = document.node_at(&ancestor) else {
                continue;
            };
            let Some(handle) = node.instance() else {
                continue;
            };
            let Some(Target::File { path, .. }) = self.resolve_handle(document, handle) else {
                return None;
            };
            let relative = if ancestor == "." {
                node_path.to_owned()
            } else {
                node_path
                    .strip_prefix(&ancestor)
                    .and_then(|rest| rest.strip_prefix('/'))
                    .unwrap_or(".")
                    .to_owned()
            };
            return Some((path, relative));
        }
        None
    }

    /// Record the gaps the parse itself leaves for files of `kinds`.
    pub(crate) fn note_read_gaps(&self, coverage: &mut CoverageBuilder, kinds: &[FileKind]) {
        for (path, kind) in &self.kinds {
            if !kinds.contains(kind) {
                continue;
            }
            let Some(file) = self.sources.get(path) else {
                continue;
            };
            match file.content() {
                FileContent::Unread(_) => coverage.note(
                    "files_not_read",
                    "these files were not read, so nothing they reference or declare is known",
                    path.clone(),
                ),
                FileContent::TooLarge => coverage.note(
                    "files_too_large",
                    "these files exceed the 8 MiB analysis bound and were not parsed",
                    path.clone(),
                ),
                FileContent::Binary => match kind {
                    FileKind::BinaryScene | FileKind::BinaryResource => coverage.note(
                        "binary_documents_not_decoded",
                        "binary .scn / .res documents are not decoded, so their references are unknown",
                        path.clone(),
                    ),
                    FileKind::Script | FileKind::Scene | FileKind::Resource => coverage.note(
                        "documents_not_utf8",
                        "these text documents are not valid UTF-8 and were not parsed",
                        path.clone(),
                    ),
                    _ => {}
                },
                FileContent::Text(_) => match kind {
                    FileKind::Script => {
                        if let Some(script) = self.scripts.get(path)
                            && script.is_damaged()
                        {
                            coverage.note(
                                "damaged_scripts",
                                "these scripts have parse errors; facts inside damaged regions are missing",
                                path.clone(),
                            );
                        }
                    }
                    FileKind::Scene | FileKind::Resource => match self.documents.get(path) {
                        Some(Err(_)) => coverage.note(
                            "documents_not_parsed",
                            "these documents could not be modelled",
                            path.clone(),
                        ),
                        Some(Ok(document)) if document.is_partial() => coverage.note(
                            "damaged_documents",
                            "these documents have syntax problems; damaged sections may be missing",
                            path.clone(),
                        ),
                        _ => {}
                    },
                    FileKind::CSharpScript => coverage.note(
                        "c_sharp_not_analysed",
                        "C# scripts are not parsed, so what they load or declare is unknown",
                        path.clone(),
                    ),
                    _ => {}
                },
            }
            if matches!(kind, FileKind::BinaryScene | FileKind::BinaryResource)
                && matches!(file.content(), FileContent::Text(_))
            {
                coverage.note(
                    "binary_documents_not_decoded",
                    "binary .scn / .res documents are not decoded, so their references are unknown",
                    path.clone(),
                );
            }
        }
    }
}

fn parse_script(
    parser: Option<&mut GdScriptParser>,
    path: &str,
    text: &str,
    generation: u64,
) -> ScriptModel {
    let symbols = match parser {
        Some(parser) => parser
            .parse(path, text, generation)
            .map_err(|error| error.message().to_owned()),
        None => Err("the GDScript grammar cannot be loaded".to_owned()),
    };
    let facts = ScriptFacts::parse(path, text);
    let failure = symbols
        .as_ref()
        .err()
        .cloned()
        .or_else(|| facts.as_ref().err().cloned());
    ScriptModel {
        symbols: symbols.ok(),
        facts: facts.ok(),
        failure,
    }
}

/// The weaker of two confidences.
#[must_use]
pub(crate) const fn weakest(left: Confidence, right: Confidence) -> Confidence {
    match (left, right) {
        (Confidence::Unresolved, _) | (_, Confidence::Unresolved) => Confidence::Unresolved,
        (Confidence::Inferred, _) | (_, Confidence::Inferred) => Confidence::Inferred,
        (Confidence::Exact, Confidence::Exact) => Confidence::Exact,
    }
}

/// A short description of an unresolvable target, for messages.
#[must_use]
pub(crate) fn describe_target(target: &Target) -> String {
    match target {
        Target::File { path, .. } => path.clone(),
        Target::Missing { path } => format!("{path}, which is not in the project"),
        Target::UnknownUid { uid } => format!("{uid}, which no file records"),
        Target::AmbiguousUid { uid, paths } => format!("{uid}, which {} files record", paths.len()),
        Target::Unsupported { value } => format!("{value}, which is not a project path"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_are_classified_as_text_binary_or_too_large() {
        let mut sources = ProjectSources::new();
        sources.insert_bytes("a.gd", b"\xef\xbb\xbfextends Node\n");
        sources.insert_bytes("b.png", b"\x89PNG\r\n\x1a\n\0\0");
        sources.insert_bytes("c.txt", &[0xff, 0xfe, 0x41]);
        sources.insert_unread("d.ogg", "budget");
        assert_eq!(sources.text("a.gd"), Some("extends Node\n"));
        assert_eq!(
            sources.get("b.png").map(SourceFile::content),
            Some(&FileContent::Binary)
        );
        assert_eq!(
            sources.get("c.txt").map(SourceFile::content),
            Some(&FileContent::Binary)
        );
        assert_eq!(sources.get("d.ogg").and_then(SourceFile::bytes), None);
        assert_eq!(sources.get("a.gd").and_then(SourceFile::bytes), Some(16));
    }

    #[test]
    fn godot_ignores_hidden_paths_and_gdignore_directories() {
        let view = GodotFileView::new([
            ".gdignore",
            "project.godot",
            "tools/.gdignore",
            "tools/raw/a.gd",
            "tools/b.gd",
            "toolsmith/c.gd",
            ".git/config",
            "art/.cache/x.png",
            "scripts/.draft.gd",
            "scripts/ok.gd",
        ]);
        // A `.gdignore` at the project root is not honoured by Godot.
        assert_eq!(view.exclusion("project.godot"), None);
        assert_eq!(view.exclusion("scripts/ok.gd"), None);
        assert_eq!(view.exclusion("toolsmith/c.gd"), None);
        assert_eq!(view.exclusion("tools/b.gd"), Some(Exclusion::GdIgnore));
        assert_eq!(view.exclusion("tools/raw/a.gd"), Some(Exclusion::GdIgnore));
        assert_eq!(view.exclusion("tools/.gdignore"), Some(Exclusion::Hidden));
        assert_eq!(view.exclusion(".git/config"), Some(Exclusion::Hidden));
        assert_eq!(view.exclusion("art/.cache/x.png"), Some(Exclusion::Hidden));
        assert_eq!(view.exclusion("scripts/.draft.gd"), Some(Exclusion::Hidden));
    }

    #[test]
    fn ignored_files_exist_but_are_never_listed() {
        let mut sources = ProjectSources::new();
        sources.insert_text("a.gd", "extends Node\n");
        sources.insert_excluded(".git/config", Exclusion::Hidden);
        sources.insert_excluded("tools/b.gd", Exclusion::GdIgnore);
        assert!(
            sources.contains("tools/b.gd"),
            "an ignored file still exists"
        );
        assert_eq!(sources.exclusion("tools/b.gd"), Some(Exclusion::GdIgnore));
        assert_eq!(sources.exclusion("a.gd"), None);
        assert_eq!(sources.paths().collect::<Vec<_>>(), vec!["a.gd"]);
        assert_eq!(sources.len(), 1);
        assert_eq!(
            sources.ignored(),
            IgnoredFiles {
                hidden: 1,
                gdignore: 1
            }
        );
        let model = ProjectModel::build(&sources, None, 0);
        assert!(
            model.kind("tools/b.gd").is_none(),
            "an ignored file is not classified"
        );
        assert!(matches!(
            model.resolve("res://tools/b.gd"),
            Target::File { .. }
        ));
    }

    #[test]
    fn kinds_follow_extensions_and_import_sidecars() {
        assert_eq!(
            FileKind::classify("project.godot", false),
            FileKind::ProjectSettings
        );
        assert_eq!(
            FileKind::classify("addons/x/plugin.cfg", false),
            FileKind::PluginDescriptor
        );
        assert_eq!(FileKind::classify("a/b.GD", false), FileKind::Script);
        assert_eq!(FileKind::classify("data/levels.csv", true), FileKind::Asset);
        assert_eq!(FileKind::classify("data/levels.csv", false), FileKind::Text);
        assert_eq!(FileKind::classify("icon.svg", false), FileKind::Asset);
        assert_eq!(
            FileKind::classify("icon.svg.import", false),
            FileKind::ImportSidecar
        );
        assert_eq!(FileKind::classify("a.gd.uid", false), FileKind::UidSidecar);
        assert_eq!(FileKind::classify("LICENSE", false), FileKind::Other);
        assert!(FileKind::Asset.is_loadable() && !FileKind::Text.is_loadable());
    }
}
