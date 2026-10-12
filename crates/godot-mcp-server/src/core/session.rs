//! Bounded, project-lease-owned session shared by every `core` outcome handler.
//!
//! One [`CoreSession`] owns exactly one Todo 8 `ResolvedProject` plus its Todo 11
//! `ProjectIndex`, and is the only thing the handlers in
//! [`super::handlers`](crate::core::handlers) may touch. That concentration is the
//! point:
//!
//! * **No second path authority.** Every project byte is reached through the held
//!   lease, so `..`, absolute spellings, in-root symlinks and the `.godot/`
//!   engine cache are refused by Todos 2/8 rather than re-checked with string
//!   matching here.
//! * **No re-implemented analysis.** Symbols, references, impact, trees,
//!   dependencies, the dangling audit and diagnostics are all read off the Todo 10
//!   semantic graph that Todo 11 persisted. This layer derives nothing.
//! * **No leaked internals.** The snapshot is accessible, but the *answer*
//!   projection in [`super::answer`](crate::core::answer) is the only way a
//!   handler renders it, so internal graph identity, SQL and local filesystem
//!   paths cannot reach a caller.
//! * **No Godot process.** Nothing here starts, connects to, or waits on an
//!   engine process. The one engine-adjacent fact this session carries is
//!   [`CoreSession::engine_binary`], the operator's explicitly configured
//!   executable path, which exists only so `health.get` can report the `godot`
//!   component from the same resolution input the engine plane uses instead of
//!   guessing. It is read by a filesystem lookup, never executed, and never part
//!   of an answer.

use std::cell::RefCell;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use godot_mcp_analysis::gdscript::{GdScriptFile, GdScriptParser};
use godot_mcp_analysis::index::{FileFingerprint, FileInventory, IndexSnapshot};
use godot_mcp_analysis::project::{ProjectError, ResolvedProject};
use godot_mcp_analysis::scene::{SceneDocument, SceneError, parse_document};
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{OutcomeError, ProjectIdentity};

use super::answer::{outcome_error, remediation_error};
use crate::workspace::{WorkspaceCoordinator, WorkspaceSnapshot};

/// Bounds every `core` answer is rendered under.
///
/// A limit is not a suggestion: a page larger than [`Self::max_page`] is refused
/// as invalid input rather than silently clamped, and a section longer than its
/// page declares itself truncated instead of appearing complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoreLimits {
    default_page: usize,
    max_page: usize,
}

impl CoreLimits {
    /// Construct explicit limits.
    ///
    /// # Errors
    /// Returns [`ErrorKind::InvalidInput`] when either bound is zero or the
    /// default exceeds the maximum, because such limits could not bound anything.
    pub fn new(default_page: usize, max_page: usize) -> Result<Self, OutcomeError> {
        if default_page == 0 || max_page == 0 {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                "core page limits must be positive",
            ));
        }
        if default_page > max_page {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                "the default core page must not exceed the maximum",
            ));
        }
        Ok(Self {
            default_page,
            max_page,
        })
    }

    /// Page size applied when a caller names none.
    #[must_use]
    pub const fn default_page(self) -> usize {
        self.default_page
    }

    /// Largest page a caller may ask for.
    #[must_use]
    pub const fn max_page(self) -> usize {
        self.max_page
    }
}

impl Default for CoreLimits {
    fn default() -> Self {
        Self {
            default_page: 200,
            max_page: 1_000,
        }
    }
}

/// Which text document grammar a project-relative path must name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentExpectation {
    /// A `.gd` GDScript source file.
    Script,
    /// A `.tscn` text scene.
    Scene,
    /// A `.tres` text resource.
    Resource,
}

impl DocumentExpectation {
    const fn extension(self) -> &'static str {
        match self {
            Self::Script => "gd",
            Self::Scene => "tscn",
            Self::Resource => "tres",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Script => "GDScript source",
            Self::Scene => "text scene",
            Self::Resource => "text resource",
        }
    }
}

/// The one shared, bounded analysis session behind the `core` profile.
///
/// The session owns no snapshot of its own: the [`WorkspaceCoordinator`] holds
/// the live, coherent workspace snapshot, and every request answers from one
/// [`PinnedSession`] — a view pinned to exactly one snapshot for the whole
/// answer.
pub struct CoreSession {
    project: ResolvedProject,
    workspace: Arc<WorkspaceCoordinator>,
    limits: CoreLimits,
    engine_binary: Option<PathBuf>,
}

impl std::fmt::Debug for CoreSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately path-free: neither the project root nor the cache
        // directory may appear in a diagnostic rendering of this session.
        formatter
            .debug_struct("CoreSession")
            .field("project", self.project.identity())
            .field("revision", &self.workspace.current().revision())
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl CoreSession {
    /// Build a session over a fixed declared inventory, with no watcher.
    ///
    /// `state_dir` is the private per-user location the index cache lives in.
    /// It is never part of an answer.
    #[must_use]
    pub fn new(
        project: ResolvedProject,
        state_dir: PathBuf,
        inventory: FileInventory,
        limits: CoreLimits,
    ) -> Self {
        let workspace = WorkspaceCoordinator::open_declared(
            project.lease().clone(),
            project.identity().generation(),
            state_dir,
            inventory,
        );
        Self::with_workspace(project, workspace, limits)
    }

    /// Build a session over the production workspace coordinator.
    #[must_use]
    pub fn with_workspace(
        project: ResolvedProject,
        workspace: Arc<WorkspaceCoordinator>,
        limits: CoreLimits,
    ) -> Self {
        Self {
            project,
            workspace,
            limits,
            engine_binary: None,
        }
    }

    /// Record the operator's explicitly configured Godot executable.
    ///
    /// The path is never executed here. `health.get` resolves the `godot`
    /// component through the same discovery order the engine plane uses, and this
    /// is the explicit first entry of that order; without it the component would
    /// have to guess whether an operator override exists.
    #[must_use]
    pub fn with_engine_binary(mut self, engine_binary: Option<PathBuf>) -> Self {
        self.engine_binary = engine_binary;
        self
    }

    /// The explicitly configured Godot executable, when one was recorded.
    #[must_use]
    pub fn engine_binary(&self) -> Option<&Path> {
        self.engine_binary.as_deref()
    }

    /// Path-free public identity every answer is attributed to.
    #[must_use]
    pub const fn project_identity(&self) -> &ProjectIdentity {
        self.project.identity()
    }

    /// Bounds every answer is rendered under.
    #[must_use]
    pub const fn limits(&self) -> CoreLimits {
        self.limits
    }

    /// Project generation every provenance record is attributed to.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.project.identity().generation()
    }

    /// The workspace coordinator this session answers from.
    #[must_use]
    pub const fn workspace(&self) -> &Arc<WorkspaceCoordinator> {
        &self.workspace
    }

    /// The current index snapshot, for health reporting.
    ///
    /// # Errors
    /// Never fails today; the `Result` keeps the health projection's contract.
    pub fn snapshot(&self) -> Result<Arc<IndexSnapshot>, OutcomeError> {
        Ok(Arc::clone(self.workspace.snapshot().index()))
    }

    /// Pin one workspace snapshot for one complete answer.
    #[must_use]
    pub fn pin(&self) -> PinnedSession<'_> {
        PinnedSession {
            session: self,
            workspace: self.workspace.snapshot(),
            reads: RefCell::new(Vec::new()),
        }
    }
}

/// One request's view of the session, pinned to exactly one workspace snapshot.
///
/// Every document the request reads live is recorded with the fingerprint of
/// the bytes it saw. [`Self::is_consistent`] then proves the answer did not mix
/// those bytes with an older graph: when a read disagrees with the pinned
/// snapshot, the caller refreshes the workspace and answers again.
pub struct PinnedSession<'session> {
    session: &'session CoreSession,
    workspace: Arc<WorkspaceSnapshot>,
    reads: RefCell<Vec<(String, String)>>,
}

impl PinnedSession<'_> {
    /// The session this view was pinned from.
    #[must_use]
    pub const fn session(&self) -> &CoreSession {
        self.session
    }

    /// The pinned workspace snapshot.
    #[must_use]
    pub fn workspace(&self) -> &Arc<WorkspaceSnapshot> {
        &self.workspace
    }

    /// Path-free public identity every answer is attributed to.
    #[must_use]
    pub const fn project_identity(&self) -> &ProjectIdentity {
        self.session.project_identity()
    }

    /// Bounds every answer is rendered under.
    #[must_use]
    pub const fn limits(&self) -> CoreLimits {
        self.session.limits()
    }

    /// Project generation every provenance record is attributed to.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.session.generation()
    }

    /// The explicitly configured Godot executable, when one was recorded.
    #[must_use]
    pub fn engine_binary(&self) -> Option<&Path> {
        self.session.engine_binary()
    }

    /// Every inventoried project-relative path, in deterministic order.
    pub fn declared_files(&self) -> impl Iterator<Item = &str> {
        self.workspace.inventory().paths()
    }

    /// The parsed `project.godot` of the pinned snapshot.
    #[must_use]
    pub fn settings(&self) -> &godot_mcp_analysis::project::ProjectSettings {
        self.workspace.settings()
    }

    /// The pinned snapshot's semantic index.
    ///
    /// # Errors
    /// Never fails; the `Result` keeps the handlers' error plumbing uniform.
    pub fn snapshot(&self) -> Result<Arc<IndexSnapshot>, OutcomeError> {
        Ok(Arc::clone(self.workspace.index()))
    }

    /// Whether every live read agreed with the pinned snapshot.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        self.reads
            .borrow()
            .iter()
            .all(|(path, fingerprint)| self.workspace.fingerprint_of(path) == Some(fingerprint))
    }

    /// The project-relative paths this request read live.
    #[must_use]
    pub fn read_paths(&self) -> Vec<String> {
        self.reads
            .borrow()
            .iter()
            .map(|(path, _)| path.clone())
            .collect()
    }

    fn record_read(&self, relative: &str, bytes: &[u8]) {
        self.reads.borrow_mut().push((
            relative.to_owned(),
            FileFingerprint::of_bytes(bytes).as_str().to_owned(),
        ));
    }

    /// Validate a caller-supplied project-relative document path.
    ///
    /// The check is intentionally narrow: it normalizes the spelling, refuses a
    /// binary Godot format with actionable remediation, refuses a wrong text
    /// extension as invalid input, and then proves the file exists *by reading it
    /// through the held lease*. Path authority stays with Todos 2/8 — the reason
    /// an escaping or engine-cache path is refused is the lease, not a substring
    /// test here.
    ///
    /// # Errors
    /// Returns [`ErrorKind::Unsupported`] with remediation for `.scn`/`.res`,
    /// [`ErrorKind::InvalidInput`] for an empty, absolute, escaping or
    /// wrong-extension spelling, [`ErrorKind::PermissionDenied`] for a path the
    /// lease refuses, and [`ErrorKind::NotFound`] for a path that is not in the
    /// project.
    pub fn resolve_document(
        &self,
        raw: &str,
        expectation: DocumentExpectation,
    ) -> Result<String, OutcomeError> {
        let relative = normalize_relative(raw)?;
        let extension = relative
            .rsplit('/')
            .next()
            .unwrap_or(&relative)
            .rsplit_once('.')
            .map(|(_, extension)| extension.to_ascii_lowercase())
            .unwrap_or_default();

        if matches!(extension.as_str(), "scn" | "res") {
            return Err(binary_format_error(&relative, &extension));
        }
        if extension != expectation.extension() {
            let expected = expectation.extension();
            let label = expectation.label();
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                format!("a {label} path must end in .{expected}, not .{extension}"),
            ));
        }

        // The lease decides. A `..`, an absolute spelling, an in-root symlink or a
        // `.godot/` path is refused by Todo 2/8 here, and absence is proven by the
        // same read rather than assumed from the inventory.
        let bytes = self
            .session
            .project
            .read_source_file(Path::new(&relative))
            .map_err(|error| map_project_error(error, &relative))?;
        self.record_read(&relative, &bytes);

        if !self.workspace.inventory().contains(&relative) {
            return Err(outcome_error(
                ErrorKind::NotFound,
                format!("{relative} is not part of the indexed project"),
            ));
        }
        Ok(relative)
    }

    /// Re-read and model one already-validated GDScript file.
    ///
    /// Todo 7 owns the grammar; this only forwards the bytes the lease returned.
    /// The per-file diagnostics live on the returned record, which the graph
    /// deliberately does not retain.
    ///
    /// # Errors
    /// Returns a classified [`OutcomeError`] when the read is refused or the
    /// parser cannot produce an honest record.
    pub fn read_script(&self, relative: &str) -> Result<GdScriptFile, OutcomeError> {
        let bytes = self
            .session
            .project
            .read_source_file(Path::new(relative))
            .map_err(|error| map_project_error(error, relative))?;
        self.record_read(relative, &bytes);
        let text = std::str::from_utf8(&bytes).map_err(|_| {
            outcome_error(
                ErrorKind::InvalidInput,
                format!("{relative} is not valid UTF-8 GDScript source"),
            )
        })?;
        let mut parser = GdScriptParser::new().map_err(|error| {
            outcome_error(
                error.kind(),
                format!("the GDScript parser is unavailable: {}", error.message()),
            )
        })?;
        parser
            .parse(relative, text, self.generation())
            .map_err(|error| outcome_error(error.kind(), error.message().to_owned()))
    }

    /// Re-read and model one already-validated `.tscn` / `.tres` document.
    ///
    /// # Errors
    /// Returns a classified [`OutcomeError`]; a binary container is
    /// [`ErrorKind::Unsupported`] with remediation rather than an empty document.
    pub fn read_document(&self, relative: &str) -> Result<SceneDocument, OutcomeError> {
        self.read_document_with_digest(relative)
            .map(|(document, _)| document)
    }

    /// [`Self::read_document`], also returning the SHA-256 of exactly the bytes
    /// the document was parsed from.
    pub fn read_document_with_digest(
        &self,
        relative: &str,
    ) -> Result<(SceneDocument, String), OutcomeError> {
        let bytes = self
            .session
            .project
            .read_source_file(Path::new(relative))
            .map_err(|error| map_project_error(error, relative))?;
        self.record_read(relative, &bytes);
        let digest = godot_mcp_engine::supervision::journal::hex_sha256(&bytes);
        // Parse exactly the bytes that were recorded, so the consistency check
        // speaks for the document the answer was built from.
        let text = String::from_utf8(bytes).map_err(|error| {
            map_scene_error(
                SceneError::NotUtf8 {
                    path: relative.to_owned(),
                    offset: error.utf8_error().valid_up_to(),
                },
                relative,
            )
        })?;
        parse_document(relative, &text, self.generation())
            .map(|document| (document, digest))
            .map_err(|error| map_scene_error(error, relative))
    }
}

/// The remediation-bearing refusal for a binary Godot document.
///
/// This is deliberately an error and not an empty success: an empty tree would
/// read as "this scene has no nodes", which is a lie about a document this layer
/// simply will not decode.
fn binary_format_error(relative: &str, extension: &str) -> OutcomeError {
    let (binary, text) = if extension == "scn" {
        (".scn binary scene", ".tscn")
    } else {
        (".res binary resource", ".tres")
    };
    remediation_error(
        ErrorKind::Unsupported,
        format!("{relative} is a {binary}, which this static profile does not decode"),
        format!(
            "re-save the document in Godot as a text {text} file (Project Settings -> \
             `editor/export/convert_text_resources_to_binary` off, or `Save As` with a {text} \
             extension), then retry; binary Godot containers are never parsed here"
        ),
    )
}

/// Normalize a caller-supplied project-relative spelling.
///
/// `res://` is accepted as a convenience because that is how a Godot document
/// spells its own references, and Windows separators are folded to `/`. Anything
/// that is not a plain in-project relative path is refused here so the caller
/// learns about the mistake before a read is attempted.
fn normalize_relative(raw: &str) -> Result<String, OutcomeError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            "a project-relative path must not be empty",
        ));
    }
    let stripped = trimmed.strip_prefix("res://").unwrap_or(trimmed);
    let forward = stripped.replace('\\', "/");
    let candidate = Path::new(&forward);
    if candidate.is_absolute() {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            "a project-relative path must not be absolute",
        ));
    }
    let mut parts = Vec::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(outcome_error(
                    ErrorKind::InvalidInput,
                    "a project-relative path must stay inside the project",
                ));
            }
        }
    }
    if parts.is_empty() {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            "a project-relative path must name a file",
        ));
    }
    Ok(parts.join("/"))
}

/// Map a Todo 8 refusal onto the stable public taxonomy without leaking the
/// local path spelling the error carries.
fn map_project_error(error: ProjectError, relative: &str) -> OutcomeError {
    map_project_error_ref(&error, relative)
}

/// Map a Todo 9 failure onto the stable public taxonomy, preserving the
/// remediation a binary container needs.
fn map_scene_error(error: SceneError, relative: &str) -> OutcomeError {
    match &error {
        SceneError::UnsupportedBinaryFormat { .. } => {
            let extension = if relative.ends_with(".scn") {
                "scn"
            } else {
                "res"
            };
            binary_format_error(relative, extension)
        }
        SceneError::Project(inner) => map_project_error_ref(inner, relative),
        _ => outcome_error(
            error.error_kind(),
            format!("{relative} could not be modelled: {error}"),
        ),
    }
}

fn map_project_error_ref(error: &ProjectError, relative: &str) -> OutcomeError {
    let kind = error.error_kind();
    outcome_error(kind, project_error_message(kind, relative))
}

fn project_error_message(kind: ErrorKind, relative: &str) -> String {
    match kind {
        ErrorKind::PermissionDenied => format!("{relative} is not readable project source"),
        ErrorKind::NotFound => format!("{relative} is not part of the project"),
        ErrorKind::Conflict => {
            "the project changed identity while the answer was being prepared".to_owned()
        }
        _ => format!("{relative} could not be read as project source"),
    }
}
