//! The project-wide static views of the `core` profile.
//!
//! Seven tools answer questions about the whole project rather than one
//! document: `project.dependencies.audit` (unused files and dependency
//! cycles), `project.signals.get`, `project.metrics.get` (statistics and
//! complexity), `project.search`, `project.uid.get`, `scene.nodes.find` and
//! `scene.exports.get`.
//!
//! Each one reads the files it needs through the pinned session — so every
//! byte goes through the held lease, and [`PinnedSession::is_consistent`]
//! proves the answer was computed from the snapshot it names — hands them to
//! the analysis crate's `insight` views, and renders the result into the
//! common envelope with one paged collection. The envelope's confidence is
//! the weakest confidence anywhere in the rendered data, lowered to
//! `inferred` whenever the view reports incomplete coverage.

use std::collections::BTreeSet;

use godot_mcp_analysis::insight::glob::{GlobSet, MAX_GLOB_LENGTH};
use godot_mcp_analysis::insight::metrics::{
    FunctionSort, SceneSort, Thresholds, function_complexity, scene_complexity, statistics,
};
use godot_mcp_analysis::insight::references::{dependency_cycles, unused_files};
use godot_mcp_analysis::insight::scenes::{NodeQuery, find_nodes, scene_exports};
use godot_mcp_analysis::insight::search::{
    DEFAULT_MAX_MATCHES, MAX_CONTEXT_LINES, MAX_MATCHES, MAX_PATTERN_LENGTH, Matcher, SearchQuery,
    search,
};
use godot_mcp_analysis::insight::signals::{SignalQuery, signal_flow};
use godot_mcp_analysis::insight::uid::{MAX_UID_LENGTH, UidLookup, UidStatus, is_uid};
use godot_mcp_analysis::insight::{
    Coverage, CoverageGap, Exclusion, FileKind, GodotFileView, ProjectModel, ProjectSources,
    is_binary_by_extension,
};
use godot_mcp_domain::Confidence;
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::catalog::CoreOperation;
use godot_mcp_domain::contracts::OutcomeError;
use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::answer::{ConfidenceFloor, answer, floor_for, outcome_error};
use super::handlers::{Arguments, guard_cancelled};
use super::session::{CoreLimits, DocumentExpectation, PinnedSession};

/// Bytes one request may read before the remaining files are left unread
/// (and the view says so): 512 MiB.
const MAX_READ_BYTES: u64 = 512 * 1024 * 1024;

/// Most globs in one `include` or `exclude` list.
const MAX_GLOBS: usize = 32;

/// Longest name, pattern or value argument, in characters: Unicode scalar
/// values, as a schema's `maxLength` counts them.
const MAX_NAME_LENGTH: usize = 1024;

/// Most roots, dynamic loads, broken references or UID claims listed beside
/// the paged collection; their counts are always exact.
const MAX_LISTED: usize = 100;

/// The file kinds the analysis views read.
const ANALYSIS_KINDS: &[FileKind] = &[
    FileKind::ProjectSettings,
    FileKind::ExportPresets,
    FileKind::Script,
    FileKind::CSharpScript,
    FileKind::Scene,
    FileKind::Resource,
    FileKind::Shader,
    FileKind::ShaderInclude,
    FileKind::PluginDescriptor,
    FileKind::Extension,
    FileKind::Text,
    FileKind::UidSidecar,
    FileKind::ImportSidecar,
];

const AUDIT_VIEWS: &[&str] = &["unused", "cycles"];
const METRIC_VIEWS: &[&str] = &["statistics", "complexity"];
const SIGNAL_VIEWS: &[&str] = &["signals", "problems", "undeclared_connections"];
const SUBJECTS: &[&str] = &["functions", "scenes"];
const SORTS: &[&str] = &[
    "cyclomatic",
    "lines",
    "nesting",
    "nodes",
    "depth",
    "instances",
];

/// The arguments each project-wide view accepts.
pub(super) fn accepted_keys(operation: CoreOperation) -> &'static [&'static str] {
    match operation {
        CoreOperation::ProjectAudit => &["view", "include", "exclude", "offset", "limit"],
        CoreOperation::ProjectMetrics => &[
            "view",
            "subject",
            "sort",
            "min_cyclomatic",
            "min_lines",
            "min_nesting",
            "min_nodes",
            "min_depth",
            "include",
            "exclude",
            "offset",
            "limit",
        ],
        CoreOperation::ProjectSearch => &[
            "query",
            "regex",
            "case_sensitive",
            "whole_word",
            "context",
            "max_matches",
            "include",
            "exclude",
            "offset",
            "limit",
        ],
        CoreOperation::ProjectSignals => &["view", "signal", "script", "offset", "limit"],
        CoreOperation::ProjectUid => &["uid", "path", "offset", "limit"],
        CoreOperation::SceneExports => &["scene", "node", "offset", "limit"],
        CoreOperation::SceneNodes => &[
            "type", "group", "name", "script", "property", "value", "include", "exclude", "offset",
            "limit",
        ],
        _ => &[],
    }
}

fn glob_list(description: &str) -> Value {
    json!({
        "type": "array",
        "maxItems": MAX_GLOBS,
        "items": {
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_GLOB_LENGTH,
            "description": "A glob: `*` and `?` stay within one path segment, `**` crosses \
                segments, `[...]` is a class and `{a,b}` an alternation; a glob without `/` \
                matches the file name at any depth."
        },
        "description": description,
    })
}

fn threshold(description: &str) -> Value {
    json!({
        "type": "integer",
        "minimum": 0,
        "maximum": u32::MAX,
        "description": description,
    })
}

fn name(description: &str) -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": MAX_NAME_LENGTH,
        "description": description,
    })
}

/// The schema of every argument key only the project-wide views use. The
/// same key has the same type and bounds in every tool; a view-specific
/// enumeration is added by [`operation_argument_schema`].
pub(super) fn argument_schema(key: &str) -> Option<Value> {
    let schema = match key {
        "view" => json!({
            "type": "string",
            "description": "Which view to return."
        }),
        "include" => {
            glob_list("Only consider project-relative paths matching at least one of these globs.")
        }
        "exclude" => glob_list("Leave out project-relative paths matching any of these globs."),
        "signal" => name("Only this signal name."),
        "subject" => json!({
            "type": "string",
            "enum": SUBJECTS,
            "default": "functions",
            "description": "With view `complexity`: rank GDScript `functions` or text `scenes`."
        }),
        "sort" => json!({
            "type": "string",
            "enum": SORTS,
            "description": "With view `complexity`: the ranking key, `cyclomatic` (default), `lines` \
                or `nesting` for functions, `nodes` (default), `depth` or `instances` for scenes."
        }),
        "min_cyclomatic" => {
            threshold("With view `complexity`: only functions of at least this McCabe complexity.")
        }
        "min_lines" => {
            threshold("With view `complexity`: only functions spanning at least this many lines.")
        }
        "min_nesting" => {
            threshold("With view `complexity`: only functions nesting at least this deep.")
        }
        "min_nodes" => {
            threshold("With view `complexity`: only scenes declaring at least this many nodes.")
        }
        "min_depth" => {
            threshold("With view `complexity`: only scenes whose tree is at least this deep.")
        }
        "query" => json!({
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_PATTERN_LENGTH,
            "description": "The text, or with `regex` the regular expression, to search for."
        }),
        "regex" => json!({
            "type": "boolean",
            "default": false,
            "description": "Treat `query` as a regular expression in Rust `regex` syntax, matched \
                within one line."
        }),
        "case_sensitive" => json!({
            "type": "boolean",
            "default": false,
            "description": "Match letter case exactly."
        }),
        "whole_word" => json!({
            "type": "boolean",
            "default": false,
            "description": "Only match at word boundaries."
        }),
        "context" => json!({
            "type": "integer",
            "minimum": 0,
            "maximum": MAX_CONTEXT_LINES,
            "default": 0,
            "description": "Lines of context returned on each side of a match."
        }),
        "max_matches" => json!({
            "type": "integer",
            "minimum": 1,
            "maximum": MAX_MATCHES,
            "default": DEFAULT_MAX_MATCHES,
            "description": "Stop collecting matches after this many."
        }),
        "uid" => json!({
            "type": "string",
            "minLength": 7,
            "maxLength": MAX_UID_LENGTH,
            "pattern": "^uid://[a-z0-9]+$",
            "description": "A `uid://` handle to look up as a project path."
        }),
        "path" => name("A project-relative path (or `res://` URI) to look up the UID of."),
        "type" => name(
            "Node type: an engine class, matched exactly, or a project class_name, matched \
             through script inheritance.",
        ),
        "group" => name("Only nodes in this group."),
        "name" => {
            name("A node-name pattern; `*` and `?` are wildcards, matching is case-sensitive.")
        }
        "property" => name("Only nodes for which the scene stores this property."),
        "value" => json!({
            "type": "string",
            "maxLength": MAX_NAME_LENGTH,
            "description": "With `property`: only nodes storing exactly this value, as the scene \
                file writes it (`\"text\"` keeps its quotes)."
        }),
        "node" => name("Only this node path (`.` is the scene root)."),
        _ => return None,
    };
    Some(schema)
}

/// A view-specific refinement of one argument's schema, when it has one.
pub(super) fn operation_argument_schema(
    operation: CoreOperation,
    key: &str,
    _limits: CoreLimits,
) -> Option<Value> {
    let view = |views: &[&str], description: &str| {
        Some(json!({
            "type": "string",
            "enum": views,
            "default": views[0],
            "description": description,
        }))
    };
    match (operation, key) {
        (CoreOperation::ProjectAudit, "view") => view(
            AUDIT_VIEWS,
            "`unused` lists files no root reaches; `cycles` lists dependency cycles.",
        ),
        (CoreOperation::ProjectMetrics, "view") => view(
            METRIC_VIEWS,
            "`statistics` counts the project; `complexity` ranks functions or scenes.",
        ),
        (CoreOperation::ProjectSignals, "view") => view(
            SIGNAL_VIEWS,
            "`signals` lists declared signals with their wiring; `problems` lists wiring \
             problems; `undeclared_connections` lists connections to signals no project script \
             declares.",
        ),
        (CoreOperation::ProjectSignals, "script") => Some(json!({
            "type": "string",
            "minLength": 1,
            "description": "Only signals declared by this project-relative GDScript path."
        })),
        (CoreOperation::SceneNodes, "script") => Some(json!({
            "type": "string",
            "minLength": 1,
            "description": "Only nodes running this project-relative GDScript, directly or \
                through inheritance."
        })),
        (CoreOperation::SceneExports, "scene") => Some(json!({
            "type": "string",
            "minLength": 1,
            "description": "Project-relative scene path."
        })),
        _ => None,
    }
}

/// Answer one project-wide view.
pub(super) fn serve(
    operation: CoreOperation,
    session: &PinnedSession<'_>,
    arguments: &Arguments<'_>,
    cancellation: &CancellationToken,
) -> Result<Value, OutcomeError> {
    let values = arguments.values();
    let view = Request {
        operation,
        session,
        arguments,
        values,
        cancellation,
    };
    match operation {
        CoreOperation::ProjectAudit => view.audit(),
        CoreOperation::ProjectMetrics => view.metrics(),
        CoreOperation::ProjectSearch => view.search(),
        CoreOperation::ProjectSignals => view.signals(),
        CoreOperation::ProjectUid => view.uid(),
        CoreOperation::SceneExports => view.exports(),
        CoreOperation::SceneNodes => view.nodes(),
        _ => Err(outcome_error(
            ErrorKind::Internal,
            "a core operation was routed to the project-wide views",
        )),
    }
}

/// Which files a view reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadSet {
    /// Scripts, scenes, resources, configuration and sidecars.
    Analysis,
    /// Every file that is not binary by its extension.
    Text,
    /// Every file, for its length.
    Everything,
}

struct Request<'r, 's> {
    operation: CoreOperation,
    session: &'r PinnedSession<'s>,
    arguments: &'r Arguments<'r>,
    values: &'r Map<String, Value>,
    cancellation: &'r CancellationToken,
}

impl Request<'_, '_> {
    // -- reading ----------------------------------------------------------------

    fn load(&self, set: ReadSet) -> Result<ProjectSources, OutcomeError> {
        guard_cancelled(self.cancellation)?;
        let inventory: BTreeSet<&str> = self.session.declared_files().collect();
        let view = GodotFileView::new(inventory.iter().copied());
        let mut sources = ProjectSources::new();
        let mut spent: u64 = 0;
        for path in &inventory {
            guard_cancelled(self.cancellation)?;
            if let Some(exclusion) = view.exclusion(path) {
                sources.insert_excluded(*path, exclusion);
                continue;
            }
            let imported = inventory.contains(format!("{path}.import").as_str());
            let kind = FileKind::classify(path, imported);
            let wanted = match set {
                ReadSet::Analysis => ANALYSIS_KINDS.contains(&kind),
                ReadSet::Text => !is_binary_by_extension(path),
                ReadSet::Everything => true,
            };
            if !wanted {
                if set == ReadSet::Text {
                    sources.insert_binary_unread(*path);
                } else {
                    sources.insert_unread(*path, "not needed by this view");
                }
                continue;
            }
            if spent >= MAX_READ_BYTES {
                sources.insert_unread(*path, "the 512 MiB read budget of one request was spent");
                continue;
            }
            match self.session.read_file(path) {
                Ok(bytes) => {
                    spent = spent.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
                    sources.insert_bytes(*path, &bytes);
                }
                Err(error) if error.kind() == ErrorKind::Conflict => return Err(error),
                Err(error) => sources.insert_unread(*path, error.message()),
            }
        }
        guard_cancelled(self.cancellation)?;
        Ok(sources)
    }

    fn model<'s>(&self, sources: &'s ProjectSources) -> ProjectModel<'s>
    where
        Self: 's,
    {
        ProjectModel::build(
            sources,
            Some(self.session.settings()),
            self.session.generation(),
        )
    }

    // -- arguments ----------------------------------------------------------------

    fn text(&self, key: &str, max: usize) -> Result<Option<&str>, OutcomeError> {
        let Some(value) = self.values.get(key).and_then(Value::as_str) else {
            return Ok(None);
        };
        if value.chars().count() > max {
            return Err(invalid(format!("`{key}` must not exceed {max} characters")));
        }
        Ok(Some(value))
    }

    fn name(&self, key: &str) -> Result<Option<&str>, OutcomeError> {
        match self.text(key, MAX_NAME_LENGTH)? {
            Some(value) if value.trim().is_empty() => {
                Err(invalid(format!("`{key}` must not be empty")))
            }
            other => Ok(other),
        }
    }

    fn flag(&self, key: &str) -> bool {
        self.values
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    fn integer(&self, key: &str) -> Option<u64> {
        self.values
            .get(key)
            .and_then(crate::arguments::non_negative_whole_u64)
    }

    fn threshold(&self, key: &str) -> u32 {
        self.integer(key)
            .map_or(0, |value| u32::try_from(value).unwrap_or(u32::MAX))
    }

    fn choice(
        &self,
        key: &str,
        allowed: &'static [&'static str],
    ) -> Result<Option<&'static str>, OutcomeError> {
        let Some(value) = self.values.get(key).and_then(Value::as_str) else {
            return Ok(None);
        };
        allowed
            .iter()
            .copied()
            .find(|candidate| *candidate == value)
            .map(Some)
            .ok_or_else(|| {
                let names: Vec<String> = allowed.iter().map(|name| format!("`{name}`")).collect();
                invalid(format!("`{key}` must be one of {}", names.join(", ")))
            })
    }

    fn globs(&self) -> Result<GlobSet, OutcomeError> {
        let list = |key: &str| -> Result<Vec<String>, OutcomeError> {
            let Some(items) = self.values.get(key).and_then(Value::as_array) else {
                return Ok(Vec::new());
            };
            if items.len() > MAX_GLOBS {
                return Err(invalid(format!(
                    "`{key}` must not list more than {MAX_GLOBS} globs"
                )));
            }
            items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| invalid(format!("every `{key}` entry must be a string")))
                })
                .collect()
        };
        let include = list("include")?;
        let exclude = list("exclude")?;
        GlobSet::new(&include, &exclude)
            .map_err(|error| invalid(format!("a glob is invalid: {error}")))
    }

    // -- rendering ----------------------------------------------------------------

    fn render(
        &self,
        sources: &ProjectSources,
        coverage: &Coverage,
        mut data: Value,
    ) -> Result<Value, OutcomeError> {
        if let Value::Object(fields) = &mut data {
            fields.insert("ignored_files".to_owned(), to_value(&sources.ignored())?);
        }
        let snapshot = self.session.snapshot()?;
        let mut floor: ConfidenceFloor = floor_for(&snapshot);
        floor.include(weakest_in(&data));
        if !coverage.complete {
            floor.include(Confidence::Inferred);
        }
        answer(
            self.session.project_identity(),
            self.operation.tool_name(),
            floor,
            data,
        )
    }

    /// Refuse a named file Godot's editor file system ignores: it is not part
    /// of the project Godot sees, so no view can describe it.
    fn visible(sources: &ProjectSources, key: &str, path: &str) -> Result<(), OutcomeError> {
        let why = match sources.exclusion(path) {
            None => return Ok(()),
            Some(Exclusion::Hidden) => "its name or a directory above it starts with `.`",
            Some(Exclusion::GdIgnore) => "a directory above it holds a `.gdignore`",
        };
        Err(outcome_error(
            ErrorKind::NotFound,
            format!("`{key}` names {path}, which Godot's editor file system ignores: {why}"),
        ))
    }

    fn page<T: Serialize>(&self, items: Vec<T>) -> Result<(Value, Value), OutcomeError> {
        let (items, page) = self.arguments.page().apply(items);
        Ok((to_value(&items)?, page.to_value()))
    }

    // -- views --------------------------------------------------------------------

    fn audit(&self) -> Result<Value, OutcomeError> {
        let view = self.choice("view", AUDIT_VIEWS)?.unwrap_or("unused");
        let selection = self.globs()?;
        let sources = self.load(ReadSet::Analysis)?;
        let model = self.model(&sources);
        guard_cancelled(self.cancellation)?;
        if view == "cycles" {
            let report = dependency_cycles(&model);
            let total = report.cycles.len();
            let breaking = report
                .cycles
                .iter()
                .filter(|cycle| cycle.breaks_loading)
                .count();
            let cycles: Vec<_> = report
                .cycles
                .into_iter()
                .filter(|cycle| {
                    selection.is_empty()
                        || cycle.members.iter().any(|member| selection.selects(member))
                })
                .collect();
            let (cycles, page) = self.page(cycles)?;
            let data = json!({
                "view": view,
                "cycles": cycles,
                "page": page,
                "summary": { "cycles": total, "breaking": breaking },
                "coverage": to_value(&report.coverage)?,
            });
            return self.render(&sources, &report.coverage, data);
        }
        let report = unused_files(&model, &selection);
        let unused_count = report.unused.len();
        let (unused, page) = self.page(report.unused)?;
        let data = json!({
            "view": view,
            "unused": unused,
            "page": page,
            "summary": {
                "candidates": report.candidates,
                "used": report.used,
                "unused": unused_count,
            },
            "roots": to_value(&first(&report.roots))?,
            "root_count": report.roots.len(),
            "dynamic_loads": to_value(&first(&report.dynamic_loads))?,
            "dynamic_load_count": report.dynamic_loads.len(),
            "broken_references": to_value(&first(&report.broken_references))?,
            "broken_reference_count": report.broken_references.len(),
            "coverage": to_value(&report.coverage)?,
        });
        self.render(&sources, &report.coverage, data)
    }

    fn metrics(&self) -> Result<Value, OutcomeError> {
        let view = self.choice("view", METRIC_VIEWS)?.unwrap_or("statistics");
        let selection = self.globs()?;
        if view == "statistics" {
            for key in [
                "subject",
                "sort",
                "min_cyclomatic",
                "min_lines",
                "min_nesting",
                "min_nodes",
                "min_depth",
                "include",
                "exclude",
            ] {
                if self.values.contains_key(key) {
                    return Err(invalid(format!(
                        "`{key}` applies to view `complexity` only"
                    )));
                }
            }
            let sources = self.load(ReadSet::Everything)?;
            let model = self.model(&sources);
            let mut report = statistics(&model);
            let extensions = std::mem::take(&mut report.by_extension);
            let (extensions, page) = self.page(extensions)?;
            let mut rendered = to_value(&report)?;
            if let Value::Object(fields) = &mut rendered {
                fields.insert("by_extension".to_owned(), extensions);
            }
            let data = json!({
                "view": view,
                "statistics": rendered,
                "page": page,
                "coverage": to_value(&report.coverage)?,
            });
            return self.render(&sources, &report.coverage, data);
        }
        let subject = self.choice("subject", SUBJECTS)?.unwrap_or("functions");
        let sort = self.choice("sort", SORTS)?;
        let thresholds = Thresholds {
            min_cyclomatic: self.threshold("min_cyclomatic"),
            min_lines: self.threshold("min_lines"),
            min_nesting: self.threshold("min_nesting"),
            min_nodes: usize::try_from(self.threshold("min_nodes")).unwrap_or(usize::MAX),
            min_depth: usize::try_from(self.threshold("min_depth")).unwrap_or(usize::MAX),
        };
        let sources = self.load(ReadSet::Analysis)?;
        let model = self.model(&sources);
        guard_cancelled(self.cancellation)?;
        if subject == "functions" {
            for key in ["min_nodes", "min_depth"] {
                if self.values.contains_key(key) {
                    return Err(invalid(format!("`{key}` applies to subject `scenes` only")));
                }
            }
            let sort = match sort.unwrap_or("cyclomatic") {
                "cyclomatic" => FunctionSort::Cyclomatic,
                "lines" => FunctionSort::Lines,
                "nesting" => FunctionSort::Nesting,
                other => {
                    return Err(invalid(format!(
                        "`sort` `{other}` ranks scenes; functions sort by `cyclomatic`, `lines` or `nesting`"
                    )));
                }
            };
            let (functions, coverage) = function_complexity(&model, &selection, sort, thresholds);
            let examined = functions.len();
            let (functions, page) = self.page(functions)?;
            let data = json!({
                "view": view,
                "subject": subject,
                "sort": sort,
                "thresholds": to_value(&thresholds)?,
                "functions": functions,
                "page": page,
                "ranked": examined,
                "coverage": to_value(&coverage)?,
            });
            return self.render(&sources, &coverage, data);
        }
        for key in ["min_cyclomatic", "min_lines", "min_nesting"] {
            if self.values.contains_key(key) {
                return Err(invalid(format!(
                    "`{key}` applies to subject `functions` only"
                )));
            }
        }
        let sort = match sort.unwrap_or("nodes") {
            "nodes" => SceneSort::Nodes,
            "depth" => SceneSort::Depth,
            "instances" => SceneSort::Instances,
            other => {
                return Err(invalid(format!(
                    "`sort` `{other}` ranks functions; scenes sort by `nodes`, `depth` or `instances`"
                )));
            }
        };
        let (scenes, coverage) = scene_complexity(&model, &selection, sort, thresholds);
        let examined = scenes.len();
        let (scenes, page) = self.page(scenes)?;
        let data = json!({
            "view": view,
            "subject": subject,
            "sort": sort,
            "thresholds": to_value(&thresholds)?,
            "scenes": scenes,
            "page": page,
            "ranked": examined,
            "coverage": to_value(&coverage)?,
        });
        self.render(&sources, &coverage, data)
    }

    fn search(&self) -> Result<Value, OutcomeError> {
        let pattern = self
            .text("query", MAX_PATTERN_LENGTH)?
            .ok_or_else(|| invalid("`query` is required"))?;
        let query = SearchQuery {
            pattern: pattern.to_owned(),
            regex: self.flag("regex"),
            case_sensitive: self.flag("case_sensitive"),
            whole_word: self.flag("whole_word"),
            context: self.integer("context").map_or(0, |value| {
                usize::try_from(value).unwrap_or(MAX_CONTEXT_LINES)
            }),
            max_matches: self
                .integer("max_matches")
                .map_or(DEFAULT_MAX_MATCHES, |value| {
                    usize::try_from(value).unwrap_or(MAX_MATCHES)
                }),
            selection: self.globs()?,
        };
        let matcher = Matcher::new(&query).map_err(invalid)?;
        let sources = self.load(ReadSet::Text)?;
        let cancellation = self.cancellation;
        let report = search(&sources, &query, &matcher, &|| cancellation.is_cancelled())
            .ok_or_else(|| {
                OutcomeError::new(
                    ErrorKind::Cancelled,
                    "the request was cancelled before the answer was produced",
                    false,
                    std::collections::BTreeMap::new(),
                )
            })?;
        let mut report = report;
        let matches = std::mem::take(&mut report.matches);
        let (matches, page) = self.page(matches)?;
        let data = json!({
            "query": {
                "query": query.pattern,
                "regex": query.regex,
                "case_sensitive": query.case_sensitive,
                "whole_word": query.whole_word,
                "context": query.context,
            },
            "matches": matches,
            "page": page,
            "files_searched": report.files_searched,
            "files_matched": report.files_matched,
            "match_limit_reached": report.match_limit_reached,
            "max_matches": report.max_matches,
            "skipped": to_value(&report.skipped)?,
            "coverage": to_value(&report.coverage)?,
        });
        self.render(&sources, &report.coverage, data)
    }

    fn signals(&self) -> Result<Value, OutcomeError> {
        let view = self.choice("view", SIGNAL_VIEWS)?.unwrap_or("signals");
        let signal = self.name("signal")?.map(str::to_owned);
        let script = match self.values.get("script").and_then(Value::as_str) {
            Some(raw) => Some(
                self.session
                    .resolve_document(raw, DocumentExpectation::Script)?,
            ),
            None => None,
        };
        let sources = self.load(ReadSet::Analysis)?;
        if let Some(script) = &script {
            Self::visible(&sources, "script", script)?;
        }
        let model = self.model(&sources);
        guard_cancelled(self.cancellation)?;
        let report = signal_flow(&model, &SignalQuery { signal, script });
        let summary = json!({
            "signals": report.signals.len(),
            "problems": report.problems.len(),
            "undeclared_connections": report.undeclared_connections.len(),
        });
        let coverage = report.coverage.clone();
        let (items, page) = match view {
            "problems" => self.page(report.problems)?,
            "undeclared_connections" => self.page(report.undeclared_connections)?,
            _ => self.page(report.signals)?,
        };
        let mut data = Map::new();
        data.insert("view".to_owned(), json!(view));
        data.insert(view.to_owned(), items);
        data.insert("page".to_owned(), page);
        data.insert("summary".to_owned(), summary);
        data.insert("coverage".to_owned(), to_value(&coverage)?);
        self.render(&sources, &coverage, Value::Object(data))
    }

    fn uid(&self) -> Result<Value, OutcomeError> {
        let uid = self.text("uid", MAX_UID_LENGTH)?;
        let path = self.values.get("path").and_then(Value::as_str);
        if uid.is_some() && path.is_some() {
            return Err(invalid("pass `uid` or `path`, not both"));
        }
        if let Some(uid) = uid
            && !is_uid(uid)
        {
            return Err(invalid(
                "`uid` must be a uid:// handle: `uid://` followed by lowercase letters and digits",
            ));
        }
        let path = match path {
            Some(raw) => Some(self.session.resolve_inventory_path(raw)?),
            None => None,
        };
        let sources = self.load(ReadSet::Analysis)?;
        if let Some(path) = &path {
            Self::visible(&sources, "path", path)?;
        }
        let model = self.model(&sources);
        let table = model.uids();
        let limitation = "Godot's own UID cache (.godot/uid_cache.bin) is not read; a UID recorded \
             only there is reported unknown.";
        let mut coverage = Coverage {
            complete: true,
            gaps: Vec::new(),
        };
        let data = match (uid, path) {
            (Some(uid), None) => self.lookup("uid", uid, table.lookup_uid(uid), &mut coverage)?,
            (None, Some(path)) => {
                self.lookup("path", &path, table.lookup_path(&path), &mut coverage)?
            }
            _ => {
                let records = table.records().to_vec();
                let total = records.len();
                let conflicts = table.conflicts();
                let (records, page) = self.page(records)?;
                json!({
                    "mode": "table",
                    "records": records,
                    "page": page,
                    "record_count": total,
                    "conflicts": to_value(&conflicts)?,
                    "claim_count": table.claims().len(),
                })
            }
        };
        let mut data = data;
        if let Value::Object(fields) = &mut data {
            fields.insert("limitations".to_owned(), json!([limitation]));
            fields.insert("coverage".to_owned(), to_value(&coverage)?);
        }
        self.render(&sources, &coverage, data)
    }

    fn lookup(
        &self,
        mode: &str,
        subject: &str,
        lookup: UidLookup,
        coverage: &mut Coverage,
    ) -> Result<Value, OutcomeError> {
        let confidence = match lookup.status {
            UidStatus::Resolved => Confidence::Exact,
            UidStatus::Conflict | UidStatus::Orphaned => Confidence::Inferred,
            UidStatus::Unknown => Confidence::Unresolved,
        };
        if lookup.status == UidStatus::Unknown {
            coverage.complete = false;
            coverage.gaps.push(CoverageGap {
                code: "uid_cache_not_read",
                message: "nothing in the project records this; Godot's own UID cache \
                          (.godot/uid_cache.bin), which is not read, may"
                    .to_owned(),
                count: 1,
                subjects: vec![subject.to_owned()],
            });
        }
        let claim_count = lookup.claims.len();
        let (records, page) = self.page(lookup.records)?;
        Ok(json!({
            "mode": mode,
            "subject": subject,
            "status": lookup.status,
            "value": lookup.value,
            "confidence": confidence,
            "records": records,
            "page": page,
            "claims": to_value(&first(&lookup.claims))?,
            "claim_count": claim_count,
        }))
    }

    fn exports(&self) -> Result<Value, OutcomeError> {
        let scene = self
            .arguments
            .document("scene", DocumentExpectation::Scene, self.session)?;
        let node = self.name("node")?.map(str::to_owned);
        let sources = self.load(ReadSet::Analysis)?;
        Self::visible(&sources, "scene", &scene)?;
        let model = self.model(&sources);
        guard_cancelled(self.cancellation)?;
        let Some(mut report) = scene_exports(&model, &scene) else {
            // Surface the classified reason the document could not be modelled.
            self.session.read_document(&scene)?;
            return Err(outcome_error(
                ErrorKind::Internal,
                format!("{scene} was read but could not be modelled"),
            ));
        };
        if let Some(node) = &node {
            let declared = model
                .document(&scene)
                .is_some_and(|document| document.node_at(node).is_some());
            if !declared {
                return Err(outcome_error(
                    ErrorKind::NotFound,
                    format!("{scene} declares no node at `{node}`"),
                ));
            }
            report.rows.retain(|row| &row.node_path == node);
        }
        let rows = std::mem::take(&mut report.rows);
        let (rows, page) = self.page(rows)?;
        let data = json!({
            "scene": scene,
            "exports": rows,
            "page": page,
            "scripted_nodes": report.scripted_nodes,
            "built_in_scripts": report.built_in_scripts,
            "unresolved_scripts": report.unresolved_scripts,
            "coverage": to_value(&report.coverage)?,
        });
        self.render(&sources, &report.coverage, data)
    }

    fn nodes(&self) -> Result<Value, OutcomeError> {
        let script = match self.values.get("script").and_then(Value::as_str) {
            Some(raw) => Some(
                self.session
                    .resolve_document(raw, DocumentExpectation::Script)?,
            ),
            None => None,
        };
        let value = self.text("value", MAX_NAME_LENGTH)?.map(str::to_owned);
        let property = self.name("property")?.map(str::to_owned);
        if value.is_some() && property.is_none() {
            return Err(invalid("`value` needs `property`"));
        }
        let query = NodeQuery {
            type_name: self.name("type")?.map(str::to_owned),
            group: self.name("group")?.map(str::to_owned),
            name: self.name("name")?.map(str::to_owned),
            script,
            property,
            value,
            scenes: self.globs()?,
        };
        let sources = self.load(ReadSet::Analysis)?;
        if let Some(script) = &query.script {
            Self::visible(&sources, "script", script)?;
        }
        let model = self.model(&sources);
        guard_cancelled(self.cancellation)?;
        let mut report = find_nodes(&model, &query);
        let matches = std::mem::take(&mut report.matches);
        let (matches, page) = self.page(matches)?;
        let data = json!({
            "nodes": matches,
            "page": page,
            "scenes_searched": report.scenes_searched,
            "nodes_examined": report.nodes_examined,
            "limitations": report.limitations,
            "coverage": to_value(&report.coverage)?,
        });
        self.render(&sources, &report.coverage, data)
    }
}

fn first<T: Clone>(items: &[T]) -> Vec<T> {
    items.iter().take(MAX_LISTED).cloned().collect()
}

fn to_value<T: Serialize + ?Sized>(value: &T) -> Result<Value, OutcomeError> {
    serde_json::to_value(value).map_err(|error| {
        outcome_error(
            ErrorKind::Internal,
            format!("a view could not be rendered: {error}"),
        )
    })
}

fn invalid(message: impl Into<String>) -> OutcomeError {
    outcome_error(ErrorKind::InvalidInput, message)
}

/// The weakest `confidence` anywhere inside `value`.
fn weakest_in(value: &Value) -> Confidence {
    fn rank(confidence: &str) -> u8 {
        match confidence {
            "exact" => 0,
            "inferred" => 1,
            _ => 2,
        }
    }
    fn walk(value: &Value, worst: &mut u8) {
        match value {
            Value::Object(fields) => {
                for (key, child) in fields {
                    if key == "confidence"
                        && let Some(found) = child.as_str()
                    {
                        *worst = (*worst).max(rank(found));
                    } else {
                        walk(child, worst);
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, worst);
                }
            }
            _ => {}
        }
    }
    let mut worst = 0;
    walk(value, &mut worst);
    match worst {
        0 => Confidence::Exact,
        1 => Confidence::Inferred,
        _ => Confidence::Unresolved,
    }
}
