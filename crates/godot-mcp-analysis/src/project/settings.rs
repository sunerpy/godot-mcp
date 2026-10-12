//! `project.godot` INI semantics: application metadata, main scene, autoloads,
//! input actions and enabled editor plugins, with comments and unknown keys
//! preserved and honest provenance attached to every derived value.
//!
//! ## Scope
//!
//! This module is a *pure text* layer: it is handed the already-read manifest
//! bytes by [`super::ResolvedProject`] (which owns the held-object authority) and
//! turns them into typed records. It never opens a file, never resolves a path
//! against the filesystem, never spawns a process and never consults a Godot
//! install.
//!
//! ## Attribution
//!
//! The recognized-section vocabulary (`[application] run/main_scene`,
//! `[autoload] Name="[*]res://…"`, `[input] action={…}`,
//! `[editor_plugins] enabled=PackedStringArray(…)`), the leading-`*`
//! singleton-marker rule, the `res://` → project-relative mapping and the
//! brace-depth tolerance for multi-line `[input]` dictionary values are adapted
//! from the MIT-licensed `codegraph-rust` project, file
//! `crates/codegraph-resolve/src/frameworks/godot_project.rs`. Copyright (c) 2026
//! sunerpy, MIT. See `THIRD-PARTY-NOTICES.md` at the workspace root. Only
//! behavior is adapted; there is no build-time or runtime dependency on that
//! checkout.
//!
//! ## Honesty rules enforced here
//!
//! * A malformed line is never silently dropped: it becomes a
//!   [`ProjectDiagnostic`], and a manifest with any diagnostic may not claim
//!   [`Confidence::Exact`].
//! * A `uid://` target cannot be resolved without the (Todo 11) index, so it is
//!   reported as [`ResourceTarget::Uid`] with `Unresolved` provenance rather than
//!   guessed.
//! * A `res://` value that would escape the project root is
//!   [`ResourceTarget::Unsupported`], never mapped to an in-root path.
//! * Every retained line keeps its 1-based source line so an agent can point at
//!   it.

use godot_mcp_domain::Confidence;
use godot_mcp_domain::contracts::{ContractViolation, Provenance, ProvenanceSource};
use serde::{Deserialize, Serialize};

/// The single project-manifest basename parsed by this module.
pub const PROJECT_MANIFEST: &str = "project.godot";

/// Whether this workspace assigns a meaning to a retained key, independent of
/// whether the key's raw text was read exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingInterpretation {
    /// The key is one this workspace understands and has modelled.
    Interpreted,
    /// The key was read exactly but carries no modelled meaning here. It stays
    /// queryable so a newer engine's settings are never lost.
    RetainedUnknown,
}

/// One preserved comment line from the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectComment {
    line: u32,
    text: String,
}

impl ProjectComment {
    /// 1-based source line of the comment.
    #[must_use]
    pub const fn line(&self) -> u32 {
        self.line
    }

    /// Verbatim (trimmed) comment text, including its leading marker.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// One retained `key=value` entry, exactly as written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawSetting {
    section: Option<String>,
    key: String,
    raw_value: String,
    line: u32,
    interpretation: SettingInterpretation,
    provenance: Provenance,
}

impl RawSetting {
    /// Section the entry appeared in, or `None` for a top-level key.
    #[must_use]
    pub fn section(&self) -> Option<&str> {
        self.section.as_deref()
    }

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

    /// 1-based source line the key was written on.
    #[must_use]
    pub const fn line(&self) -> u32 {
        self.line
    }

    /// Whether this workspace models the key's meaning.
    #[must_use]
    pub const fn interpretation(&self) -> SettingInterpretation {
        self.interpretation
    }

    /// Evidence for the retained text.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// Class of manifest problem surfaced for a `project.godot` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectDiagnosticKind {
    /// A line opened with `[` but is not a well-formed `[section]` header.
    MalformedSectionHeader,
    /// A non-comment, non-header line has no `=`.
    MissingAssignment,
    /// A line assigns to an empty key.
    EmptyKey,
    /// A brace-opened value never closed before end of file.
    UnterminatedValue,
    /// A recognized key's value could not be interpreted in its expected shape.
    UninterpretableValue,
    /// A key was declared more than once in the same section. The first
    /// declaration wins, matching the engine, and the later one is reported here
    /// rather than silently merged.
    DuplicateKey,
}

impl ProjectDiagnosticKind {
    /// Stable wire code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::MalformedSectionHeader => "malformed_section_header",
            Self::MissingAssignment => "missing_assignment",
            Self::EmptyKey => "empty_key",
            Self::UnterminatedValue => "unterminated_value",
            Self::UninterpretableValue => "uninterpretable_value",
            Self::DuplicateKey => "duplicate_key",
        }
    }
}

/// One manifest problem, always carrying its exact source line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectDiagnostic {
    kind: ProjectDiagnosticKind,
    line: u32,
    message: String,
}

impl ProjectDiagnostic {
    /// Problem class.
    #[must_use]
    pub const fn kind(&self) -> ProjectDiagnosticKind {
        self.kind
    }

    /// 1-based source line of the offending text.
    #[must_use]
    pub const fn line(&self) -> u32 {
        self.line
    }

    /// Human-readable explanation.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// What a manifest value points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceTarget {
    /// A `res://` URI successfully mapped to a project-root-relative path.
    ProjectRelative {
        /// The `res://` URI exactly as written (with any `*` marker stripped).
        res_uri: String,
        /// The project-root-relative path the URI denotes.
        path: String,
    },
    /// A `uid://` reference. Resolving it needs the project-wide UID index, which
    /// this layer deliberately does not own, so it stays unresolved.
    Uid(String),
    /// A value this layer will not map: a non-`res://`/`uid://` scheme, or a
    /// `res://` path that would escape the project root.
    Unsupported(String),
}

impl ResourceTarget {
    /// The project-root-relative path, when one was proven.
    #[must_use]
    pub fn project_relative_path(&self) -> Option<&str> {
        match self {
            Self::ProjectRelative { path, .. } => Some(path),
            Self::Uid(_) | Self::Unsupported(_) => None,
        }
    }

    fn evidence(&self) -> (ProvenanceSource, Confidence) {
        match self {
            Self::ProjectRelative { .. } => (ProvenanceSource::ProjectMetadata, Confidence::Exact),
            Self::Uid(_) | Self::Unsupported(_) => {
                (ProvenanceSource::Unresolved, Confidence::Unresolved)
            }
        }
    }
}

/// A manifest value that names a project resource.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceBinding {
    target: ResourceTarget,
    line: u32,
    provenance: Provenance,
}

impl ResourceBinding {
    /// What the value points at.
    #[must_use]
    pub const fn target(&self) -> &ResourceTarget {
        &self.target
    }

    /// 1-based source line.
    #[must_use]
    pub const fn line(&self) -> u32 {
        self.line
    }

    /// Evidence for the binding.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// One `[autoload]` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoloadEntry {
    name: String,
    is_singleton: bool,
    target: ResourceTarget,
    line: u32,
    provenance: Provenance,
}

impl AutoloadEntry {
    /// Global name the entry binds.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the value carried the leading `*` singleton marker. Without it
    /// the entry is a plain preload rather than a global singleton.
    #[must_use]
    pub const fn is_singleton(&self) -> bool {
        self.is_singleton
    }

    /// What the entry binds the name to.
    #[must_use]
    pub const fn target(&self) -> &ResourceTarget {
        &self.target
    }

    /// 1-based source line.
    #[must_use]
    pub const fn line(&self) -> u32 {
        self.line
    }

    /// Evidence for the entry.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// One `[input]` action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputActionEntry {
    name: String,
    raw_value: String,
    line: u32,
    provenance: Provenance,
}

impl InputActionEntry {
    /// Action name (the key).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The action's dictionary literal, retained verbatim (newlines included).
    #[must_use]
    pub fn raw_value(&self) -> &str {
        &self.raw_value
    }

    /// 1-based source line the action was declared on.
    #[must_use]
    pub const fn line(&self) -> u32 {
        self.line
    }

    /// Evidence for the action.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// `[application]` metadata this workspace models.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ApplicationMetadata {
    name: Option<String>,
    description: Option<String>,
    version: Option<String>,
    icon: Option<String>,
    features: Vec<String>,
}

impl ApplicationMetadata {
    /// `config/name`.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// `config/description`.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// `config/version`.
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// `config/icon`, as written (a `res://` URI in a normal project).
    #[must_use]
    pub fn icon(&self) -> Option<&str> {
        self.icon.as_deref()
    }

    /// `config/features` entries, in source order.
    #[must_use]
    pub fn features(&self) -> &[String] {
        &self.features
    }
}

/// Fully parsed `project.godot` settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectSettings {
    config_version: Option<u64>,
    application: ApplicationMetadata,
    main_scene: Option<ResourceBinding>,
    autoloads: Vec<AutoloadEntry>,
    input_actions: Vec<InputActionEntry>,
    enabled_plugins: Vec<ResourceBinding>,
    comments: Vec<ProjectComment>,
    settings: Vec<RawSetting>,
    diagnostics: Vec<ProjectDiagnostic>,
    provenance: Provenance,
}

impl ProjectSettings {
    /// Top-level `config_version`, when present and numeric.
    #[must_use]
    pub const fn config_version(&self) -> Option<u64> {
        self.config_version
    }

    /// `[application]` metadata.
    #[must_use]
    pub const fn application(&self) -> &ApplicationMetadata {
        &self.application
    }

    /// `[application] run/main_scene`, when configured.
    #[must_use]
    pub const fn main_scene(&self) -> Option<&ResourceBinding> {
        self.main_scene.as_ref()
    }

    /// `[autoload]` entries in source order.
    #[must_use]
    pub fn autoloads(&self) -> &[AutoloadEntry] {
        &self.autoloads
    }

    /// `[input]` actions in source order.
    #[must_use]
    pub fn input_actions(&self) -> &[InputActionEntry] {
        &self.input_actions
    }

    /// `[editor_plugins] enabled` entries in source order.
    #[must_use]
    pub fn enabled_plugins(&self) -> &[ResourceBinding] {
        &self.enabled_plugins
    }

    /// Every preserved comment line.
    #[must_use]
    pub fn comments(&self) -> &[ProjectComment] {
        &self.comments
    }

    /// Every retained `key=value` entry in source order, interpreted or not.
    #[must_use]
    pub fn settings(&self) -> &[RawSetting] {
        &self.settings
    }

    /// Retained entries this workspace assigns no meaning to.
    #[must_use]
    pub fn unknown_settings(&self) -> Vec<&RawSetting> {
        self.settings
            .iter()
            .filter(|setting| setting.interpretation == SettingInterpretation::RetainedUnknown)
            .collect()
    }

    /// Look up one retained entry by section and key. The first occurrence wins,
    /// matching the engine's first-write-wins behavior for duplicate keys.
    #[must_use]
    pub fn setting(&self, section: Option<&str>, key: &str) -> Option<&RawSetting> {
        self.settings
            .iter()
            .find(|setting| setting.section() == section && setting.key == key)
    }

    /// Problems found while parsing. Never silently discarded.
    #[must_use]
    pub fn diagnostics(&self) -> &[ProjectDiagnostic] {
        &self.diagnostics
    }

    /// Whether the manifest parsed with any problem.
    #[must_use]
    pub fn is_partial(&self) -> bool {
        !self.diagnostics.is_empty()
    }

    /// Evidence for the manifest as a whole.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    /// Parse a `project.godot` manifest.
    ///
    /// `generation` is the project generation the read was performed against and
    /// is carried into every emitted [`Provenance`].
    ///
    /// # Errors
    /// Returns [`ContractViolation`] only when a provenance value the parser
    /// wants to emit would be dishonest, which is a parser bug rather than a
    /// property of the input.
    pub fn parse(text: &str, generation: u64) -> Result<Self, ContractViolation> {
        Parser::new(generation).run(text)
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Sections whose keys carry a modelled meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Application,
    Autoload,
    Input,
    EditorPlugins,
    Other,
}

impl Section {
    fn from_name(name: &str) -> Self {
        match name {
            "application" => Self::Application,
            "autoload" => Self::Autoload,
            "input" => Self::Input,
            "editor_plugins" => Self::EditorPlugins,
            _ => Self::Other,
        }
    }
}

struct Parser {
    generation: u64,
    config_version: Option<u64>,
    application: ApplicationMetadata,
    main_scene: Option<ResourceBinding>,
    autoloads: Vec<AutoloadEntry>,
    input_actions: Vec<InputActionEntry>,
    enabled_plugins: Vec<ResourceBinding>,
    comments: Vec<ProjectComment>,
    settings: Vec<RawSetting>,
    diagnostics: Vec<ProjectDiagnostic>,
}

impl Parser {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            config_version: None,
            application: ApplicationMetadata::default(),
            main_scene: None,
            autoloads: Vec::new(),
            input_actions: Vec::new(),
            enabled_plugins: Vec::new(),
            comments: Vec::new(),
            settings: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    fn diagnose(&mut self, kind: ProjectDiagnosticKind, line: u32, message: impl Into<String>) {
        self.diagnostics.push(ProjectDiagnostic {
            kind,
            line,
            message: message.into(),
        });
    }

    fn provenance(
        &self,
        source: ProvenanceSource,
        confidence: Confidence,
        subject: String,
    ) -> Result<Provenance, ContractViolation> {
        Provenance::new(source, confidence, subject, self.generation)
    }

    fn exact(&self, subject: String) -> Result<Provenance, ContractViolation> {
        self.provenance(
            ProvenanceSource::ProjectMetadata,
            Confidence::Exact,
            subject,
        )
    }

    fn for_target(
        &self,
        target: &ResourceTarget,
        subject: String,
    ) -> Result<Provenance, ContractViolation> {
        let (source, confidence) = target.evidence();
        self.provenance(source, confidence, subject)
    }

    fn run(mut self, text: &str) -> Result<ProjectSettings, ContractViolation> {
        let lines: Vec<&str> = text.lines().collect();
        let mut section_name: Option<String> = None;
        let mut section = Section::Other;
        let mut index = 0usize;

        while index < lines.len() {
            let raw = lines[index];
            let line_no = clamp_line(index + 1);
            let trimmed = raw.trim();

            if trimmed.is_empty() {
                index += 1;
                continue;
            }
            if is_comment(trimmed) {
                self.comments.push(ProjectComment {
                    line: line_no,
                    text: trimmed.to_owned(),
                });
                index += 1;
                continue;
            }
            if trimmed.starts_with('[') {
                if let Some(name) = parse_section_header(trimmed) {
                    section = Section::from_name(name);
                    section_name = Some(name.to_owned());
                } else {
                    self.diagnose(
                        ProjectDiagnosticKind::MalformedSectionHeader,
                        line_no,
                        format!("`{trimmed}` is not a well-formed [section] header"),
                    );
                    section = Section::Other;
                    section_name = None;
                }
                index += 1;
                continue;
            }

            let Some((key, first_value)) = split_key_value(trimmed) else {
                if trimmed.contains('=') {
                    self.diagnose(
                        ProjectDiagnosticKind::EmptyKey,
                        line_no,
                        "assignment with an empty key",
                    );
                } else {
                    self.diagnose(
                        ProjectDiagnosticKind::MissingAssignment,
                        line_no,
                        "line is neither a comment, a [section] header nor a key=value assignment",
                    );
                }
                index += 1;
                continue;
            };

            // A brace-opened value may span several lines; consume them verbatim
            // so an inner `"key": value` pair is never mistaken for a new key.
            let mut value = first_value.to_owned();
            let mut depth = brace_delta(first_value);
            let mut consumed = 1usize;
            while depth > 0 && index + consumed < lines.len() {
                let continuation = lines[index + consumed];
                value.push('\n');
                value.push_str(continuation);
                depth += brace_delta(continuation);
                consumed += 1;
            }
            if depth > 0 {
                self.diagnose(
                    ProjectDiagnosticKind::UnterminatedValue,
                    line_no,
                    format!("value of `{key}` opens a brace that is never closed"),
                );
            }
            index += consumed;

            if self
                .settings
                .iter()
                .any(|prior| prior.section() == section_name.as_deref() && prior.key == key)
            {
                self.diagnose(
                    ProjectDiagnosticKind::DuplicateKey,
                    line_no,
                    format!("`{key}` is already declared earlier in this section"),
                );
            }

            let interpretation =
                self.interpret(section, section_name.as_deref(), key, &value, line_no)?;
            let provenance = self.exact(format!(
                "{PROJECT_MANIFEST}#setting:{}:{key}",
                section_name.as_deref().unwrap_or("<top-level>")
            ))?;
            self.settings.push(RawSetting {
                section: section_name.clone(),
                key: key.to_owned(),
                raw_value: value,
                line: line_no,
                interpretation,
                provenance,
            });
        }

        let (source, confidence) = if self.diagnostics.is_empty() {
            (ProvenanceSource::ProjectMetadata, Confidence::Exact)
        } else {
            (ProvenanceSource::Heuristic, Confidence::Inferred)
        };
        let provenance =
            self.provenance(source, confidence, format!("{PROJECT_MANIFEST}#manifest"))?;

        Ok(ProjectSettings {
            config_version: self.config_version,
            application: self.application,
            main_scene: self.main_scene,
            autoloads: self.autoloads,
            input_actions: self.input_actions,
            enabled_plugins: self.enabled_plugins,
            comments: self.comments,
            settings: self.settings,
            diagnostics: self.diagnostics,
            provenance,
        })
    }

    /// Assign a modelled meaning to one entry, returning whether it was
    /// interpreted. An unrecognized key is retained untouched.
    fn interpret(
        &mut self,
        section: Section,
        section_name: Option<&str>,
        key: &str,
        value: &str,
        line: u32,
    ) -> Result<SettingInterpretation, ContractViolation> {
        match (section_name, section) {
            (None, _) if key == "config_version" => {
                match value.trim().parse::<u64>() {
                    Ok(parsed) => self.config_version = Some(parsed),
                    Err(_) => self.diagnose(
                        ProjectDiagnosticKind::UninterpretableValue,
                        line,
                        "config_version is not a non-negative integer",
                    ),
                }
                Ok(SettingInterpretation::Interpreted)
            }
            (_, Section::Application) => match key {
                "config/name" => {
                    self.application.name = Some(strip_quotes(value).to_owned());
                    Ok(SettingInterpretation::Interpreted)
                }
                "config/description" => {
                    self.application.description = Some(strip_quotes(value).to_owned());
                    Ok(SettingInterpretation::Interpreted)
                }
                "config/version" => {
                    self.application.version = Some(strip_quotes(value).to_owned());
                    Ok(SettingInterpretation::Interpreted)
                }
                "config/icon" => {
                    self.application.icon = Some(strip_quotes(value).to_owned());
                    Ok(SettingInterpretation::Interpreted)
                }
                "config/features" => {
                    self.application.features = quoted_strings(value);
                    Ok(SettingInterpretation::Interpreted)
                }
                "run/main_scene" => {
                    // An empty value is how Godot records a cleared main scene.
                    if self.main_scene.is_none() && !strip_quotes(value).is_empty() {
                        let target = resource_target(value);
                        let provenance =
                            self.for_target(&target, format!("{PROJECT_MANIFEST}#main_scene"))?;
                        self.main_scene = Some(ResourceBinding {
                            target,
                            line,
                            provenance,
                        });
                    }
                    Ok(SettingInterpretation::Interpreted)
                }
                _ => Ok(SettingInterpretation::RetainedUnknown),
            },
            (_, Section::Autoload) => {
                if self.autoloads.iter().all(|entry| entry.name != key) {
                    let unquoted = strip_quotes(value);
                    let is_singleton = unquoted.starts_with('*');
                    let target = resource_target(value);
                    let provenance =
                        self.for_target(&target, format!("{PROJECT_MANIFEST}#autoload:{key}"))?;
                    self.autoloads.push(AutoloadEntry {
                        name: key.to_owned(),
                        is_singleton,
                        target,
                        line,
                        provenance,
                    });
                }
                Ok(SettingInterpretation::Interpreted)
            }
            (_, Section::Input) => {
                if self.input_actions.iter().all(|entry| entry.name != key) {
                    let provenance =
                        self.exact(format!("{PROJECT_MANIFEST}#input_action:{key}"))?;
                    self.input_actions.push(InputActionEntry {
                        name: key.to_owned(),
                        raw_value: value.to_owned(),
                        line,
                        provenance,
                    });
                }
                Ok(SettingInterpretation::Interpreted)
            }
            (_, Section::EditorPlugins) if key == "enabled" => {
                for (ordinal, entry) in quoted_strings(value).into_iter().enumerate() {
                    let target = resource_target(&entry);
                    let provenance = self.for_target(
                        &target,
                        format!("{PROJECT_MANIFEST}#enabled_plugin:{line}:{ordinal}"),
                    )?;
                    self.enabled_plugins.push(ResourceBinding {
                        target,
                        line,
                        provenance,
                    });
                }
                Ok(SettingInterpretation::Interpreted)
            }
            _ => Ok(SettingInterpretation::RetainedUnknown),
        }
    }
}

fn clamp_line(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn is_comment(line: &str) -> bool {
    line.starts_with(';') || line.starts_with('#')
}

/// `[name]` → `Some("name")`. Requires a trimmed line that starts `[`, ends `]`
/// and has a non-empty interior with no further `]`.
fn parse_section_header(line: &str) -> Option<&str> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    if inner.is_empty() || inner.contains(']') {
        return None;
    }
    Some(inner)
}

/// Split at the FIRST `=`. `None` when there is no `=` or the key is empty.
fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let equals = line.find('=')?;
    let key = line[..equals].trim();
    if key.is_empty() {
        return None;
    }
    Some((key, line[equals + 1..].trim()))
}

/// Net `{` minus `}` on a line. String-content unaware, which is sufficient for
/// the brace-balanced dictionary values Godot writes into `[input]`.
fn brace_delta(line: &str) -> i32 {
    let mut delta = 0i32;
    for byte in line.bytes() {
        match byte {
            b'{' => delta += 1,
            b'}' => delta -= 1,
            _ => {}
        }
    }
    delta
}

/// Remove one matching pair of surrounding double quotes, if present.
fn strip_quotes(value: &str) -> &str {
    let trimmed = value.trim();
    trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(trimmed)
}

/// Every double-quoted string in `value`, in source order. Used for
/// `PackedStringArray("a", "b")` and for a single quoted scalar.
fn quoted_strings(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = value;
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else {
            break;
        };
        out.push(after[..close].to_owned());
        rest = &after[close + 1..];
    }
    out
}

/// Classify a value that names a project resource.
///
/// A leading `*` (the autoload singleton marker) is stripped first. A `res://`
/// URI maps to a project-root-relative path; the mapping refuses any result that
/// is absolute or contains a `..` component, so a value can never escape the
/// project root. A `uid://` reference stays unresolved. Anything else is
/// unsupported.
///
/// This is the single classification rule for the whole analysis layer: the
/// scene/resource parser calls it rather than reimplementing `res://` mapping.
#[must_use]
pub fn resource_target(value: &str) -> ResourceTarget {
    let unquoted = strip_quotes(value);
    let uri = unquoted.strip_prefix('*').unwrap_or(unquoted);
    if let Some(rest) = uri.strip_prefix("res://") {
        let relative = rest.trim_start_matches('/');
        let escapes = relative.is_empty()
            || relative
                .split(['/', '\\'])
                .any(|segment| segment == ".." || segment == ".");
        if escapes {
            return ResourceTarget::Unsupported(uri.to_owned());
        }
        return ResourceTarget::ProjectRelative {
            res_uri: uri.to_owned(),
            path: relative.to_owned(),
        };
    }
    if uri.starts_with("uid://") {
        return ResourceTarget::Uid(uri.to_owned());
    }
    ResourceTarget::Unsupported(uri.to_owned())
}
