//! The fifteen `core` outcome handlers.
//!
//! Each handler is a thin, cancellable projection over the shared
//! [`CoreSession`]: it validates its arguments, takes one coherent index
//! snapshot, reads the answer off the Todo 10 graph, and renders it through
//! [`super::answer`] so provenance, pagination and redaction are applied in one
//! place rather than fifteen.
//!
//! Three rules are enforced here rather than left to convention:
//!
//! * **Unknown arguments are refused.** Silently ignoring `{"scene": ...}` on a
//!   script tool would answer a question the caller did not ask.
//! * **Cancellation is checked before the expensive step.** The index scan and the
//!   graph walk are the two costs; both are preceded by a token check, so a
//!   cancelled request returns [`ErrorKind::Cancelled`] instead of finishing work
//!   nobody is waiting for.
//! * **Absence is never an empty answer.** A missing file is `not_found`, an
//!   unknown symbol is `not_found`, a binary container is `unsupported` with
//!   remediation. An empty list is reserved for "this really is empty".

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use godot_mcp_analysis::gdscript::GdScriptFile;
use godot_mcp_analysis::graph::{
    DanglingKind, EdgeKind, GraphEdge, GraphNode, GraphNodeId, GraphNodeKind, GraphSpan,
    SemanticGraph,
};
use godot_mcp_analysis::index::IndexSnapshot;
use godot_mcp_analysis::project::{ProjectDiagnostic, ResourceTarget};
use godot_mcp_analysis::scene::{Property, ResourceRef, SceneDiagnostic, SceneNode, SceneSpan};
use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::Confidence;
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::catalog::{CoreOperation, KernelOperation};
use godot_mcp_domain::contracts::{OutcomeError, ProvenanceSource};
use godot_mcp_domain::manifests::canonical_manifest;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::answer::{
    ConfidenceFloor, PageRequest, answer, coherence_value, edge_value, floor_for,
    limitations_value, node_value, outcome_error, public_hint,
};
use super::session::{CoreLimits, CoreSession, DocumentExpectation, PinnedSession};
use crate::diagnostics::{DiagnosticsContext, legacy_health};
use crate::kernel::{KernelToolCall, KernelToolHandler, ServiceKernel};
use crate::workspace::CommitScope;

/// Register a real handler for every canonical `core` tool.
///
/// Registration goes through [`ServiceKernel::register_tool_handler`], so a name
/// this module got wrong is rejected by the kernel instead of quietly shadowing
/// nothing. The returned set is what was actually registered, which the
/// acceptance suite compares against the canonical manifest.
///
/// `capabilities.get` is answered by the kernel itself (it is profile metadata,
/// not project analysis); it is included in the returned set because the kernel
/// does serve it, and registering a second implementation for it would be a
/// [`ErrorKind::Conflict`].
///
/// # Errors
/// Returns [`ErrorKind::NotFound`] when a name is absent from the selected
/// profile and [`ErrorKind::Conflict`] when a handler is already registered.
pub fn register_core_tools(
    kernel: &mut ServiceKernel,
    session: Arc<CoreSession>,
) -> Result<BTreeSet<String>, OutcomeError> {
    let canonical = canonical_manifest(CapabilityProfile::Core).tools;
    if !canonical.is_subset(&canonical_manifest(kernel.profile()).tools) {
        return Err(outcome_error(
            ErrorKind::Unsupported,
            "the selected capability profile does not include every core outcome tool",
        ));
    }

    let profile = kernel.profile();
    kernel.set_core_limits(session.limits());
    let mut registered = BTreeSet::new();
    for &operation in CoreOperation::ALL {
        kernel.register_tool_handler(
            operation.key(),
            Arc::new(CoreHandler {
                operation,
                profile,
                session: Arc::clone(&session),
            }),
        )?;
        registered.insert(operation.tool_name().to_owned());
    }
    // Served by the kernel from the canonical manifest; see the note above.
    registered.insert(KernelOperation::Capabilities.tool_name().to_owned());

    if registered != canonical {
        return Err(outcome_error(
            ErrorKind::Internal,
            "the core profile has a tool without a registered handler",
        ));
    }
    Ok(registered)
}

/// The arguments each core operation takes, beside the catalog descriptor
/// its operation serves.
trait CoreArguments: Copy {
    /// Argument keys this operation accepts. Anything else is invalid input.
    fn accepted_keys(self) -> &'static [&'static str];
    fn requires_argument(self, key: &str) -> bool;
    fn input_schema(self, limits: CoreLimits) -> Map<String, Value>;
}

impl CoreArguments for CoreOperation {
    fn accepted_keys(self) -> &'static [&'static str] {
        match self {
            Self::ProjectInfo | Self::Health => &[],
            Self::ProjectDiscover | Self::ProjectDependencies | Self::ProjectValidate => {
                &["offset", "limit"]
            }
            Self::ScriptDiagnostics => &["script", "offset", "limit"],
            Self::ScriptValidate => &["script"],
            Self::ScriptSymbols => &["script", "offset", "limit"],
            Self::ScriptReferences | Self::ScriptImpact => &["script", "symbol", "offset", "limit"],
            Self::SceneTree | Self::SceneDependencies => &["scene", "offset", "limit"],
            Self::ResourceTree | Self::ResourceDependencies => &["resource", "offset", "limit"],
        }
    }

    fn requires_argument(self, key: &str) -> bool {
        !matches!(self, Self::ScriptDiagnostics) && matches!(key, "script" | "scene" | "resource")
    }

    fn input_schema(self, limits: CoreLimits) -> Map<String, Value> {
        let accepted = self.accepted_keys();
        let properties = accepted
            .iter()
            .copied()
            .map(|key| (key.to_owned(), core_argument_schema(key, limits)))
            .collect::<Map<_, _>>();
        let required = accepted
            .iter()
            .copied()
            .filter(|key| self.requires_argument(key))
            .collect::<Vec<_>>();
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": properties,
            "required": required,
        })
        .as_object()
        .expect("core tool schemas are JSON objects")
        .clone()
    }
}

/// The strict input schema of one core operation.
pub(crate) fn input_schema(operation: CoreOperation, limits: CoreLimits) -> Map<String, Value> {
    operation.input_schema(limits)
}

fn core_argument_schema(key: &str, limits: CoreLimits) -> Value {
    match key {
        "script" => json!({
            "type": "string",
            "minLength": 1,
            "description": "Project-relative GDScript path."
        }),
        "scene" => json!({
            "type": "string",
            "minLength": 1,
            "description": "Project-relative scene path."
        }),
        "resource" => json!({
            "type": "string",
            "minLength": 1,
            "description": "Project-relative resource path."
        }),
        "symbol" => json!({
            "type": "string",
            "description": "Optional function, signal, or script-class name declared by the script."
        }),
        "offset" => json!({
            "type": "integer",
            "minimum": 0,
            "maximum": usize::MAX,
            "description": "Zero-based result offset."
        }),
        "limit" => json!({
            "type": "integer",
            "minimum": 1,
            "maximum": limits.max_page(),
            "description": "Maximum number of entries returned in each result set."
        }),
        _ => unreachable!("accepted core argument keys have schemas"),
    }
}

fn validate_core_argument_type(
    key: &str,
    value: &Value,
    limits: CoreLimits,
) -> Result<(), OutcomeError> {
    let schema = core_argument_schema(key, limits);
    let declared_type = schema["type"]
        .as_str()
        .expect("core arguments declare one JSON type");
    let matches = match declared_type {
        "string" => value.is_string(),
        "integer" => crate::arguments::non_negative_whole_u64(value).is_some(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => unreachable!("core argument schemas use standard JSON types"),
    };
    if matches {
        return Ok(());
    }

    let expected = match declared_type {
        "string" => "a string",
        "integer" => "a non-negative whole number",
        "number" => "a number",
        "boolean" => "a boolean",
        "array" => "an array",
        "object" => "an object",
        "null" => "null",
        _ => unreachable!("core argument schemas use standard JSON types"),
    };
    Err(outcome_error(
        ErrorKind::InvalidInput,
        format!("`{key}` must be {expected}"),
    ))
}

struct CoreHandler {
    operation: CoreOperation,
    profile: CapabilityProfile,
    session: Arc<CoreSession>,
}

/// How many times one answer is recomputed when the project changes under it.
const CONSISTENT_ANSWER_ATTEMPTS: usize = 3;

/// One request, answered from exactly one pinned workspace snapshot.
struct ServedRequest<'session> {
    operation: CoreOperation,
    profile: CapabilityProfile,
    session: PinnedSession<'session>,
}

#[async_trait]
impl KernelToolHandler for CoreHandler {
    async fn call(&self, call: KernelToolCall) -> Result<Value, OutcomeError> {
        let arguments = Arguments::new(
            &call.arguments,
            self.operation.accepted_keys(),
            self.session.limits(),
        )?;
        // Checked before the scan: a cancelled request must not pay for it.
        guard_cancelled(&call.cancellation)?;
        call.progress
            .report(0.0, Some(1.0), Some("preparing the static answer"))
            .await;

        let mut attempt = 0;
        let result = loop {
            attempt += 1;
            let request = ServedRequest {
                operation: self.operation,
                profile: self.profile,
                session: self.session.pin(),
            };
            let result = request.serve(&arguments, &call.cancellation, call.diagnostics.as_deref());
            // An answer that read bytes the pinned snapshot does not describe
            // would mix a live read with an older graph. Observe those paths and
            // answer again from the snapshot that contains them.
            if !request.session.is_consistent() && attempt < CONSISTENT_ANSWER_ATTEMPTS {
                let _ = self
                    .session
                    .workspace()
                    .observe_commit(&CommitScope::Paths(request.session.read_paths()));
                continue;
            }
            let consistent = request.session.is_consistent();
            let workspace = request.session.workspace().to_value();
            break result.and_then(|value| {
                if !consistent {
                    return Err(outcome_error(
                        ErrorKind::Uncertain,
                        "the project kept changing while the answer was prepared; retry",
                    ));
                }
                // `health.get` keeps its frozen diagnostics shape; every other
                // core answer envelope names the snapshot it was computed from.
                if self.operation == CoreOperation::Health {
                    return Ok(value);
                }
                Ok(with_workspace(value, workspace))
            });
        };
        call.progress.report(1.0, Some(1.0), None).await;
        result
    }
}

/// Attach the pinned snapshot's revision and coherence to an answer envelope.
fn with_workspace(value: Value, workspace: Value) -> Value {
    match value {
        Value::Object(mut envelope) => {
            envelope.insert("workspace".to_owned(), workspace);
            Value::Object(envelope)
        }
        other => other,
    }
}

impl ServedRequest<'_> {
    fn serve(
        &self,
        arguments: &Arguments<'_>,
        cancellation: &CancellationToken,
        diagnostics: Option<&DiagnosticsContext>,
    ) -> Result<Value, OutcomeError> {
        match self.operation {
            CoreOperation::ProjectInfo => self.project_info(),
            CoreOperation::ProjectDiscover => self.project_discover(arguments, cancellation),
            CoreOperation::ProjectDependencies => {
                self.project_dependencies(arguments, cancellation)
            }
            CoreOperation::ProjectValidate => self.project_validate(arguments, cancellation),
            CoreOperation::ScriptValidate => self.script_validate(arguments),
            CoreOperation::ScriptSymbols => self.script_symbols(arguments),
            CoreOperation::ScriptReferences => self.script_references(arguments, cancellation),
            CoreOperation::ScriptImpact => self.script_impact(arguments, cancellation),
            CoreOperation::ScriptDiagnostics => self.script_diagnostics(arguments, cancellation),
            CoreOperation::SceneTree => self.scene_tree(arguments),
            CoreOperation::SceneDependencies => self.document_dependencies(
                arguments,
                cancellation,
                DocumentExpectation::Scene,
                "scene",
            ),
            CoreOperation::ResourceTree => self.resource_tree(arguments),
            CoreOperation::ResourceDependencies => self.document_dependencies(
                arguments,
                cancellation,
                DocumentExpectation::Resource,
                "resource",
            ),
            CoreOperation::Health => match diagnostics {
                Some(diagnostics) => diagnostics.health_value(),
                None => legacy_health(self.profile, self.session.session()),
            },
        }
    }

    // -- project ------------------------------------------------------------

    fn project_info(&self) -> Result<Value, OutcomeError> {
        let settings = self.session.settings();
        let application = settings.application();
        let mut floor = ConfidenceFloor::exact(ProvenanceSource::ProjectMetadata);
        if settings.is_partial() {
            floor.include(Confidence::Inferred);
        }
        let main_scene = settings.main_scene().and_then(|binding| {
            floor.include(binding.provenance().confidence());
            binding.target().project_relative_path().map(str::to_owned)
        });
        let data = json!({
            "name": application.name(),
            "description": application.description(),
            "version": application.version(),
            "icon": application.icon(),
            "features": application.features(),
            "config_version": settings.config_version(),
            "main_scene": main_scene,
            "autoload_count": settings.autoloads().len(),
            "input_action_count": settings.input_actions().len(),
            "enabled_plugin_count": settings.enabled_plugins().len(),
            "manifest_diagnostics": settings
                .diagnostics()
                .iter()
                .map(project_diagnostic_value)
                .collect::<Vec<_>>(),
        });
        answer(self.session.project_identity(), "project.info", floor, data)
    }

    fn project_discover(
        &self,
        arguments: &Arguments<'_>,
        cancellation: &CancellationToken,
    ) -> Result<Value, OutcomeError> {
        let snapshot = self.snapshot(cancellation)?;
        let mut floor = floor_for(&snapshot);
        let graph = snapshot.graph();

        let mut files: Vec<Value> = Vec::new();
        for file in self.session.declared_files() {
            guard_cancelled(cancellation)?;
            let slice = snapshot.slice(file);
            let declared: Vec<&GraphNode> = graph
                .nodes()
                .values()
                .filter(|node| node.file() == Some(file))
                .collect();
            for node in &declared {
                floor.include(node.provenance().confidence());
            }
            files.push(json!({
                "file": file,
                "role": file_role(file),
                "declared_symbols": slice.map_or(0, |slice| slice.node_count()),
                "relationships": slice.map_or(0, |slice| slice.edge_count()),
            }));
        }
        let (files, page) = arguments.page().apply(files);
        let data = json!({
            "files": files,
            "page": page.to_value(),
            "coherence": coherence_value(snapshot.coherence()),
        });
        answer(
            self.session.project_identity(),
            CoreOperation::ProjectDiscover.tool_name(),
            floor,
            data,
        )
    }

    fn project_dependencies(
        &self,
        arguments: &Arguments<'_>,
        cancellation: &CancellationToken,
    ) -> Result<Value, OutcomeError> {
        let snapshot = self.snapshot(cancellation)?;
        let mut floor = floor_for(&snapshot);
        let graph = snapshot.graph();
        let settings = self.session.settings();

        let autoloads: Vec<Value> = settings
            .autoloads()
            .iter()
            .map(|autoload| {
                floor.include(autoload.provenance().confidence());
                json!({
                    "name": autoload.name(),
                    "singleton": autoload.is_singleton(),
                    "target": resource_target_value(autoload.target()),
                    "line": autoload.line(),
                    "confidence": autoload.provenance().confidence(),
                })
            })
            .collect();
        let plugins: Vec<Value> = settings
            .enabled_plugins()
            .iter()
            .map(|plugin| {
                floor.include(plugin.provenance().confidence());
                json!({
                    "target": resource_target_value(plugin.target()),
                    "line": plugin.line(),
                    "confidence": plugin.provenance().confidence(),
                })
            })
            .collect();
        let input_actions: Vec<Value> = settings
            .input_actions()
            .iter()
            .map(|action| {
                floor.include(action.provenance().confidence());
                json!({ "name": action.name(), "line": action.line() })
            })
            .collect();

        guard_cancelled(cancellation)?;
        let manifest_edges: Vec<&GraphEdge> = graph
            .edges_from(&GraphNodeId::project())
            .into_iter()
            .collect();
        let mut edges = Vec::with_capacity(manifest_edges.len());
        for edge in manifest_edges {
            floor.include_resolution(edge.resolution());
            edges.push(edge_value(graph, edge));
        }
        let (edges, page) = arguments.page().apply(edges);

        let data = json!({
            "main_scene": settings
                .main_scene()
                .map(|binding| resource_target_value(binding.target())),
            "autoloads": autoloads,
            "enabled_plugins": plugins,
            "input_actions": input_actions,
            "manifest_relationships": edges,
            "page": page.to_value(),
            "limitations": limitations_value(graph),
        });
        answer(
            self.session.project_identity(),
            "project.dependencies",
            floor,
            data,
        )
    }

    /// Static project validation.
    ///
    /// This deliberately does **not** claim the project would open in Godot: the
    /// `core` profile starts no engine. It reports the dangling-reference audit,
    /// the manifest diagnostics and the analyzer's own limitations, and states as
    /// a limitation that engine validation was not performed.
    fn project_validate(
        &self,
        arguments: &Arguments<'_>,
        cancellation: &CancellationToken,
    ) -> Result<Value, OutcomeError> {
        let snapshot = self.snapshot(cancellation)?;
        let mut floor = floor_for(&snapshot);
        let graph = snapshot.graph();

        let mut dangling = Vec::new();
        for entry in graph.dangling() {
            guard_cancelled(cancellation)?;
            floor.include(match entry.kind() {
                DanglingKind::TargetAbsent => Confidence::Exact,
                DanglingKind::TargetUndecidable => Confidence::Unresolved,
            });
            dangling.push(json!({
                "kind": entry.kind().code(),
                "relation": entry.edge_kind().code(),
                "from": super::answer::node_ref_value(graph, entry.source()),
                "hint": public_hint(entry.hint()),
                "file": entry.file(),
                "line": entry.span().map(GraphSpan::start_line),
                "reason": entry.reason(),
            }));
        }
        let (dangling, page) = arguments.page().apply(dangling);

        let mut limitations = limitations_value(graph);
        if let Value::Array(items) = &mut limitations {
            items.push(engine_validation_limitation());
        }

        let absent = graph.dangling_of(DanglingKind::TargetAbsent).len();
        let undecidable = graph.dangling_of(DanglingKind::TargetUndecidable).len();
        let data = json!({
            "verdict": if absent == 0 { "no_absent_targets" } else { "absent_targets_found" },
            "absent_target_count": absent,
            "undecidable_target_count": undecidable,
            "dangling": dangling,
            "page": page.to_value(),
            "manifest_diagnostics": self
                .session
                .settings()
                .diagnostics()
                .iter()
                .map(project_diagnostic_value)
                .collect::<Vec<_>>(),
            "coherence": coherence_value(snapshot.coherence()),
            "limitations": limitations,
        });
        answer(
            self.session.project_identity(),
            CoreOperation::ProjectValidate.tool_name(),
            floor,
            data,
        )
    }

    // -- GDScript -----------------------------------------------------------

    /// Static script validation: the parser's own verdict, never the engine's.
    fn script_validate(&self, arguments: &Arguments<'_>) -> Result<Value, OutcomeError> {
        let relative = arguments.document("script", DocumentExpectation::Script, &self.session)?;
        let script = self.session.read_script(&relative)?;
        let mut floor = ConfidenceFloor::exact(ProvenanceSource::StaticSyntax);
        if script.is_partial() {
            floor.include(Confidence::Inferred);
        }
        let diagnostics: Vec<Value> = script
            .diagnostics()
            .iter()
            .map(|diagnostic| {
                json!({
                    "code": diagnostic.kind().code(),
                    "message": diagnostic.message(),
                    "line": diagnostic.span().start_line(),
                    "column": diagnostic.span().start_column(),
                })
            })
            .collect();
        let data = json!({
            "file": relative,
            "verdict": if diagnostics.is_empty() { "valid_static" } else { "static_errors" },
            "diagnostics": diagnostics,
            "limitations": Value::Array(vec![engine_validation_limitation()]),
        });
        answer(
            self.session.project_identity(),
            CoreOperation::ScriptValidate.tool_name(),
            floor,
            data,
        )
    }

    fn script_symbols(&self, arguments: &Arguments<'_>) -> Result<Value, OutcomeError> {
        let relative = arguments.document("script", DocumentExpectation::Script, &self.session)?;
        let script = self.session.read_script(&relative)?;
        let mut floor = ConfidenceFloor::exact(ProvenanceSource::StaticSyntax);
        if script.is_partial() {
            floor.include(Confidence::Inferred);
        }

        let mut symbols: Vec<(u32, u32, String, Value)> = Vec::new();
        if let Some(class_name) = script.class_name_declaration() {
            symbols.push((
                0,
                0,
                class_name.to_owned(),
                json!({
                    "kind": "script_class",
                    "name": class_name,
                    "owner": Value::Null,
                    "line": 1,
                    "confidence": Confidence::Exact,
                }),
            ));
        }
        for function in script.functions() {
            floor.include(function.provenance().confidence());
            symbols.push((
                function.span().start_line(),
                function.span().start_column(),
                function.name().to_owned(),
                json!({
                    "kind": "function",
                    "name": function.name(),
                    "owner": function.owner(),
                    "line": function.span().start_line(),
                    "static": function.is_static(),
                    "constructor": function.is_constructor(),
                    "signature": function.signature(),
                    "return_type": function.return_type(),
                    "confidence": function.provenance().confidence(),
                }),
            ));
        }
        for signal in script.signals() {
            floor.include(signal.provenance().confidence());
            symbols.push((
                signal.span().start_line(),
                signal.span().start_column(),
                signal.name().to_owned(),
                json!({
                    "kind": "signal",
                    "name": signal.name(),
                    "owner": signal.owner(),
                    "line": signal.span().start_line(),
                    "confidence": signal.provenance().confidence(),
                }),
            ));
        }
        for variable in script.variables() {
            floor.include(variable.provenance().confidence());
            symbols.push((
                variable.span().start_line(),
                variable.span().start_column(),
                variable.name().to_owned(),
                json!({
                    "kind": variable.kind().code(),
                    "name": variable.name(),
                    "owner": variable.owner(),
                    "line": variable.span().start_line(),
                    "type_hint": variable.type_hint(),
                    "confidence": variable.provenance().confidence(),
                }),
            ));
        }
        for class in script.classes() {
            floor.include(class.provenance().confidence());
            symbols.push((
                class.span().start_line(),
                class.span().start_column(),
                class.name().to_owned(),
                json!({
                    "kind": "inner_class",
                    "name": class.name(),
                    "owner": class.owner(),
                    "line": class.span().start_line(),
                    "extends": class.extends(),
                    "confidence": class.provenance().confidence(),
                }),
            ));
        }
        for enumeration in script.enums() {
            floor.include(enumeration.provenance().confidence());
            symbols.push((
                enumeration.span().start_line(),
                enumeration.span().start_column(),
                enumeration.name().to_owned(),
                json!({
                    "kind": "enum",
                    "name": enumeration.name(),
                    "owner": enumeration.owner(),
                    "line": enumeration.span().start_line(),
                    "members": enumeration
                        .members()
                        .iter()
                        .map(|member| json!({ "name": member.name(), "value": member.value() }))
                        .collect::<Vec<_>>(),
                    "confidence": enumeration.provenance().confidence(),
                }),
            ));
        }

        // Stable total order: source position, then name, so paging is repeatable.
        symbols.sort_by(|left, right| (left.0, left.1, &left.2).cmp(&(right.0, right.1, &right.2)));
        let ordered: Vec<Value> = symbols.into_iter().map(|(_, _, _, value)| value).collect();
        let (symbols, page) = arguments.page().apply(ordered);

        let data = json!({
            "file": relative,
            "extends": script.extends(),
            "class_name": script.class_name_declaration(),
            "symbols": symbols,
            "page": page.to_value(),
            "partial": script.is_partial(),
        });
        answer(
            self.session.project_identity(),
            "gdscript.symbols",
            floor,
            data,
        )
    }

    fn script_references(
        &self,
        arguments: &Arguments<'_>,
        cancellation: &CancellationToken,
    ) -> Result<Value, OutcomeError> {
        let relative = arguments.document("script", DocumentExpectation::Script, &self.session)?;
        let snapshot = self.snapshot(cancellation)?;
        let mut floor = floor_for(&snapshot);
        let graph = snapshot.graph();
        let subject = self.subject(graph, &relative, arguments.symbol())?;

        guard_cancelled(cancellation)?;
        let mut references = Vec::new();
        for edge in graph.references(&subject) {
            floor.include_resolution(edge.resolution());
            references.push(edge_value(graph, edge));
        }
        let mut callers = Vec::new();
        for edge in graph.callers(&subject) {
            floor.include_resolution(edge.resolution());
            callers.push(edge_value(graph, edge));
        }
        let caller_total = callers.len();
        let (references, page) = arguments.page().apply(references);
        let (callers, caller_page) = arguments.page().apply(callers);

        let data = json!({
            "subject": super::answer::node_ref_value(graph, &subject),
            "references": references,
            "page": page.to_value(),
            "callers": callers,
            "caller_page": caller_page.to_value(),
            "caller_total": caller_total,
            "limitations": limitations_value(graph),
        });
        answer(
            self.session.project_identity(),
            "gdscript.references",
            floor,
            data,
        )
    }

    /// The bounded impact of one script (or one symbol inside it).
    ///
    /// The impacted set is Todo 10's transitive referrer closure, which is
    /// deterministic and already sorted. Every impacted node is rendered with its
    /// own confidence, and the explaining edges are included so the answer can be
    /// checked rather than merely believed.
    fn script_impact(
        &self,
        arguments: &Arguments<'_>,
        cancellation: &CancellationToken,
    ) -> Result<Value, OutcomeError> {
        let relative = arguments.document("script", DocumentExpectation::Script, &self.session)?;
        let snapshot = self.snapshot(cancellation)?;
        let mut floor = floor_for(&snapshot);
        let graph = snapshot.graph();
        let subject = self.subject(graph, &relative, arguments.symbol())?;

        guard_cancelled(cancellation)?;
        let impacted_ids = graph.impact(&subject);
        let mut impacted = Vec::with_capacity(impacted_ids.len());
        for id in &impacted_ids {
            guard_cancelled(cancellation)?;
            match graph.node(id) {
                Some(node) => {
                    floor.include(node.provenance().confidence());
                    impacted.push(node_value(node));
                }
                None => {
                    floor.include(Confidence::Unresolved);
                    impacted.push(super::answer::node_ref_value(graph, id));
                }
            }
        }

        let reachable: BTreeSet<&GraphNodeId> = impacted_ids.iter().collect();
        let mut edges = Vec::new();
        for edge in graph.edges() {
            guard_cancelled(cancellation)?;
            let relevant = edge.targets(&subject)
                || (reachable.contains(edge.source())
                    && edge
                        .candidates()
                        .iter()
                        .filter_map(|candidate| candidate.target())
                        .any(|target| reachable.contains(target) || target == &subject));
            if relevant {
                floor.include_resolution(edge.resolution());
                edges.push(edge_value(graph, edge));
            }
        }

        let impacted_total = impacted.len();
        let (impacted, page) = arguments.page().apply(impacted);
        let (edges, edge_page) = arguments.page().apply(edges);

        let data = json!({
            "subject": super::answer::node_ref_value(graph, &subject),
            "impacted": impacted,
            "impacted_total": impacted_total,
            "page": page.to_value(),
            "referencing_edges": edges,
            "edge_page": edge_page.to_value(),
            "coherence": coherence_value(snapshot.coherence()),
            "limitations": limitations_value(graph),
        });
        answer(
            self.session.project_identity(),
            "gdscript.impact",
            floor,
            data,
        )
    }

    fn script_diagnostics(
        &self,
        arguments: &Arguments<'_>,
        cancellation: &CancellationToken,
    ) -> Result<Value, OutcomeError> {
        let mut floor = ConfidenceFloor::exact(ProvenanceSource::StaticSyntax);
        let scripts: Vec<String> = match arguments.raw_document("script") {
            Some(_) => {
                vec![arguments.document("script", DocumentExpectation::Script, &self.session)?]
            }
            None => self
                .session
                .declared_files()
                .filter(|file| file.ends_with(".gd"))
                .map(str::to_owned)
                .collect(),
        };

        let mut diagnostics = Vec::new();
        for file in &scripts {
            guard_cancelled(cancellation)?;
            let script = self.session.read_script(file)?;
            if script.is_partial() {
                floor.include(Confidence::Inferred);
            }
            for diagnostic in script.diagnostics() {
                diagnostics.push(json!({
                    "file": file,
                    "code": diagnostic.kind().code(),
                    "message": diagnostic.message(),
                    "line": diagnostic.span().start_line(),
                    "column": diagnostic.span().start_column(),
                }));
            }
        }
        let (diagnostics, page) = arguments.page().apply(diagnostics);
        let data = json!({
            "scripts_examined": scripts.len(),
            "diagnostics": diagnostics,
            "page": page.to_value(),
            "limitations": Value::Array(vec![engine_validation_limitation()]),
        });
        answer(
            self.session.project_identity(),
            "gdscript.diagnostics",
            floor,
            data,
        )
    }

    // -- documents ----------------------------------------------------------

    fn scene_tree(&self, arguments: &Arguments<'_>) -> Result<Value, OutcomeError> {
        let relative = arguments.document("scene", DocumentExpectation::Scene, &self.session)?;
        let (document, sha256) = self.session.read_document_with_digest(&relative)?;
        let mut floor = ConfidenceFloor::exact(ProvenanceSource::ProjectMetadata);
        if document.is_partial() {
            floor.include(Confidence::Inferred);
        }
        // Every addressable node carries a handle bound to exactly these bytes;
        // a batch naming a node by it refuses once the scene has changed.
        let with_handle = |node: &SceneNode| {
            let mut value = scene_node_value(node);
            value["handle"] = node.node_path().map_or(Value::Null, |path| {
                Value::String(crate::node_handle::mint(&relative, path, &sha256))
            });
            value
        };

        let nodes: Vec<Value> = document
            .nodes()
            .iter()
            .map(|node| {
                floor.include(node.provenance().confidence());
                with_handle(node)
            })
            .collect();
        let (nodes, page) = arguments.page().apply(nodes);
        let connections: Vec<Value> = document
            .connections()
            .iter()
            .map(|connection| {
                floor.include(connection.provenance().confidence());
                json!({
                    "signal": connection.signal(),
                    "from": connection.from_path(),
                    "to": connection.to_path(),
                    "method": connection.method(),
                    "from_resolved": connection.from_resolved(),
                    "to_resolved": connection.to_resolved(),
                    "line": connection.span().start_line(),
                    "confidence": connection.provenance().confidence(),
                })
            })
            .collect();

        let data = json!({
            "file": relative,
            "sha256": sha256,
            "format_version": document.format_version(),
            "root_type": document.root_type(),
            "root": document.root().map(with_handle),
            "nodes": nodes,
            "page": page.to_value(),
            "connections": connections,
            "diagnostics": document
                .diagnostics()
                .iter()
                .map(scene_diagnostic_value)
                .collect::<Vec<_>>(),
        });
        answer(self.session.project_identity(), "scene.tree", floor, data)
    }

    fn resource_tree(&self, arguments: &Arguments<'_>) -> Result<Value, OutcomeError> {
        let relative =
            arguments.document("resource", DocumentExpectation::Resource, &self.session)?;
        let document = self.session.read_document(&relative)?;
        let mut floor = ConfidenceFloor::exact(ProvenanceSource::ProjectMetadata);
        if document.is_partial() {
            floor.include(Confidence::Inferred);
        }

        let body = document.resource_body();
        let properties: Vec<Value> = body
            .map(|body| {
                floor.include(body.provenance().confidence());
                body.properties().iter().map(property_value).collect()
            })
            .unwrap_or_default();
        let (properties, page) = arguments.page().apply(properties);

        let sub_resources: Vec<Value> = document
            .sub_resources()
            .iter()
            .map(|sub| {
                floor.include(sub.provenance().confidence());
                json!({
                    "id": sub.id(),
                    "type": sub.type_name(),
                    "line": sub.span().start_line(),
                    "properties": sub.properties().iter().map(property_value).collect::<Vec<_>>(),
                })
            })
            .collect();

        let data = json!({
            "file": relative,
            "format_version": document.format_version(),
            "root_type": document.root_type(),
            "script": body
                .and_then(|body| body.script())
                .map(resource_ref_value),
            "properties": properties,
            "page": page.to_value(),
            "sub_resources": sub_resources,
            "diagnostics": document
                .diagnostics()
                .iter()
                .map(scene_diagnostic_value)
                .collect::<Vec<_>>(),
        });
        answer(
            self.session.project_identity(),
            "resource.tree",
            floor,
            data,
        )
    }

    fn document_dependencies(
        &self,
        arguments: &Arguments<'_>,
        cancellation: &CancellationToken,
        expectation: DocumentExpectation,
        key: &str,
    ) -> Result<Value, OutcomeError> {
        let relative = arguments.document(key, expectation, &self.session)?;
        let snapshot = self.snapshot(cancellation)?;
        let mut floor = floor_for(&snapshot);
        let graph = snapshot.graph();
        let subject = match expectation {
            DocumentExpectation::Scene => GraphNodeId::scene(&relative),
            DocumentExpectation::Resource => GraphNodeId::text_resource(&relative),
            DocumentExpectation::Script => GraphNodeId::script(&relative),
        };
        if graph.node(&subject).is_none() {
            return Err(outcome_error(
                ErrorKind::NotFound,
                format!("{relative} is not present in the analysis index"),
            ));
        }

        guard_cancelled(cancellation)?;
        // Everything the document itself declares, plus everything its own nodes
        // declare: a scene's dependencies live on its `[node]` entries too.
        let mut dependencies = Vec::new();
        for edge in graph.edges() {
            guard_cancelled(cancellation)?;
            if edge.file() != relative {
                continue;
            }
            if matches!(edge.kind(), EdgeKind::SceneParent) {
                continue;
            }
            floor.include_resolution(edge.resolution());
            dependencies.push(edge_value(graph, edge));
        }
        let dependency_total = dependencies.len();
        let (dependencies, page) = arguments.page().apply(dependencies);

        let mut dependents = Vec::new();
        for edge in graph.references(&subject) {
            floor.include_resolution(edge.resolution());
            dependents.push(edge_value(graph, edge));
        }
        let (dependents, dependent_page) = arguments.page().apply(dependents);

        let data = json!({
            "file": relative,
            "dependencies": dependencies,
            "dependency_total": dependency_total,
            "page": page.to_value(),
            "dependents": dependents,
            "dependent_page": dependent_page.to_value(),
            "limitations": limitations_value(graph),
        });
        answer(
            self.session.project_identity(),
            self.operation.tool_name(),
            floor,
            data,
        )
    }

    // -- shared -------------------------------------------------------------

    fn snapshot(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Arc<IndexSnapshot>, OutcomeError> {
        guard_cancelled(cancellation)?;
        let snapshot = self.session.snapshot()?;
        guard_cancelled(cancellation)?;
        Ok(snapshot)
    }

    /// Resolve the subject a reference/impact query is about.
    ///
    /// Without `symbol` the subject is the script file itself. With `symbol` it is
    /// a named function, signal or script class. Other declaration kinds are
    /// invalid input, while a genuinely absent name is [`ErrorKind::NotFound`].
    fn subject(
        &self,
        graph: &SemanticGraph,
        relative: &str,
        symbol: Option<&str>,
    ) -> Result<GraphNodeId, OutcomeError> {
        let Some(symbol) = symbol else {
            let id = GraphNodeId::script(relative);
            if graph.node(&id).is_none() {
                return Err(outcome_error(
                    ErrorKind::NotFound,
                    format!("{relative} is not present in the analysis index"),
                ));
            }
            return Ok(id);
        };
        let found = graph.nodes().values().find(|node| {
            node.file() == Some(relative)
                && node.name() == symbol
                && matches!(
                    node.kind(),
                    GraphNodeKind::Function | GraphNodeKind::Signal | GraphNodeKind::ScriptClass
                )
        });
        if let Some(node) = found {
            return Ok(node.id().clone());
        }

        let script = self.session.read_script(relative)?;
        if let Some(kind) = non_resolvable_declaration_kind(&script, symbol) {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                format!(
                    "{relative} declares `{symbol}` as `{kind}`, but reference and impact queries resolve only functions, signals, and script classes"
                ),
            ));
        }

        Err(outcome_error(
            ErrorKind::NotFound,
            format!(
                "{relative} declares no resolvable function, signal, or script class named `{symbol}`"
            ),
        ))
    }
}

fn non_resolvable_declaration_kind(script: &GdScriptFile, symbol: &str) -> Option<&'static str> {
    script
        .variables()
        .iter()
        .find(|variable| variable.name() == symbol)
        .map(|variable| variable.kind().code())
        .or_else(|| {
            script
                .classes()
                .iter()
                .any(|class| class.name() == symbol)
                .then_some("inner_class")
        })
        .or_else(|| {
            script
                .enums()
                .iter()
                .any(|enumeration| enumeration.name() == symbol)
                .then_some("enum")
        })
}

/// Validated tool arguments.
struct Arguments<'call> {
    values: &'call Map<String, Value>,
    page: PageRequest,
}

impl<'call> Arguments<'call> {
    fn new(
        values: &'call Map<String, Value>,
        accepted: &[&str],
        limits: CoreLimits,
    ) -> Result<Self, OutcomeError> {
        for (key, value) in values {
            if !accepted.contains(&key.as_str()) {
                return Err(outcome_error(
                    ErrorKind::InvalidInput,
                    format!("`{key}` is not an argument of this tool"),
                ));
            }
            if value.is_null() {
                return Err(outcome_error(
                    ErrorKind::InvalidInput,
                    format!("`{key}` must not be null"),
                ));
            }
            validate_core_argument_type(key, value, limits)?;
        }
        let offset = read_index(values, "offset")?.unwrap_or(0);
        let limit = match read_index(values, "limit")? {
            Some(0) => {
                return Err(outcome_error(
                    ErrorKind::InvalidInput,
                    "`limit` must be at least 1",
                ));
            }
            Some(limit) if limit > limits.max_page() => {
                let max = limits.max_page();
                return Err(outcome_error(
                    ErrorKind::InvalidInput,
                    format!("`limit` must not exceed {max}"),
                ));
            }
            Some(limit) => limit,
            None => limits.default_page(),
        };
        Ok(Self {
            values,
            page: PageRequest::new(offset, limit),
        })
    }

    const fn page(&self) -> PageRequest {
        self.page
    }

    fn raw_document(&self, key: &str) -> Option<&'call Value> {
        self.values.get(key)
    }

    fn document(
        &self,
        key: &str,
        expectation: DocumentExpectation,
        session: &PinnedSession<'_>,
    ) -> Result<String, OutcomeError> {
        let value = self.values.get(key).ok_or_else(|| {
            outcome_error(
                ErrorKind::InvalidInput,
                format!("`{key}` is required and must be a project-relative path"),
            )
        })?;
        let raw = value.as_str().ok_or_else(|| {
            outcome_error(
                ErrorKind::InvalidInput,
                format!("`{key}` must be a project-relative path string"),
            )
        })?;
        session.resolve_document(raw, expectation)
    }

    fn symbol(&self) -> Option<&'call str> {
        self.values.get("symbol").and_then(Value::as_str)
    }
}

fn read_index(values: &Map<String, Value>, key: &str) -> Result<Option<usize>, OutcomeError> {
    match values.get(key) {
        None => Ok(None),
        Some(value) => {
            let value = crate::arguments::non_negative_whole_u64(value)
                .and_then(|value| usize::try_from(value).ok());
            value.map(Some).ok_or_else(|| {
                outcome_error(
                    ErrorKind::InvalidInput,
                    format!("`{key}` must be a non-negative whole number"),
                )
            })
        }
    }
}

fn guard_cancelled(cancellation: &CancellationToken) -> Result<(), OutcomeError> {
    if cancellation.is_cancelled() {
        return Err(OutcomeError::new(
            ErrorKind::Cancelled,
            "the request was cancelled before the answer was produced",
            false,
            BTreeMap::new(),
        ));
    }
    Ok(())
}

/// The limitation every static validation answer must carry: no engine ran.
///
/// The reason is a property of *this validator*, not of the selected profile:
/// `project.validate` and `script.validate` are static in every profile, so the
/// message must not name one.
fn engine_validation_limitation() -> Value {
    json!({
        "code": "engine_validation_not_performed",
        "message": "this static validator never starts a Godot process, so this verdict covers \
                    static analysis only; engine-level script compilation and scene instantiation \
                    were not checked",
        "subjects": Vec::<String>::new(),
    })
}

fn file_role(file: &str) -> &'static str {
    if file == "project.godot" {
        return "manifest";
    }
    match file.rsplit_once('.').map(|(_, extension)| extension) {
        Some("gd") => "script",
        Some("tscn") => "text_scene",
        Some("tres") => "text_resource",
        Some("scn") => "binary_scene_unsupported",
        Some("res") => "binary_resource_unsupported",
        _ => "asset",
    }
}

fn resource_target_value(target: &ResourceTarget) -> Value {
    match target {
        ResourceTarget::ProjectRelative { res_uri, path } => json!({
            "resolution": "project_relative",
            "res_uri": res_uri,
            "path": path,
            "confidence": Confidence::Exact,
        }),
        ResourceTarget::Uid(uid) => json!({
            "resolution": "uid",
            "res_uri": uid,
            "path": Value::Null,
            "confidence": Confidence::Unresolved,
        }),
        ResourceTarget::Unsupported(value) => json!({
            "resolution": "unsupported",
            "res_uri": value,
            "path": Value::Null,
            "confidence": Confidence::Unresolved,
        }),
    }
}

fn resource_ref_value(reference: &ResourceRef) -> Value {
    json!({
        "handle": reference.kind().code(),
        "id": reference.id(),
        "target": reference.target().map(resource_target_value),
        "line": reference.span().start_line(),
        "confidence": reference.provenance().confidence(),
    })
}

fn property_value(property: &Property) -> Value {
    json!({
        "key": property.key(),
        "raw_value": property.raw_value(),
        "line": property.span().start_line(),
        "confidence": property.provenance().confidence(),
    })
}

fn scene_node_value(node: &SceneNode) -> Value {
    json!({
        "name": node.name(),
        "type": node.type_name(),
        "path": node.node_path(),
        "parent": node.parent_path(),
        "root": node.is_root(),
        "instance": node.instance().map(resource_ref_value),
        "instance_placeholder": node.instance_placeholder().map(resource_target_value),
        "script": node.script().map(resource_ref_value),
        "groups": node
            .groups()
            .iter()
            .map(|group| group.name().to_owned())
            .collect::<Vec<_>>(),
        "line": node.span().start_line(),
        "confidence": node.provenance().confidence(),
    })
}

fn scene_diagnostic_value(diagnostic: &SceneDiagnostic) -> Value {
    json!({
        "code": diagnostic.kind().code(),
        "message": diagnostic.message(),
        "line": SceneSpan::start_line(diagnostic.span()),
    })
}

fn project_diagnostic_value(diagnostic: &ProjectDiagnostic) -> Value {
    json!({
        "code": diagnostic.kind().code(),
        "message": diagnostic.message(),
        "line": diagnostic.line(),
    })
}

#[cfg(test)]
mod schema_tests {
    use super::*;

    fn advertised_schema(operation: CoreOperation) -> Map<String, Value> {
        advertised_schema_with_limits(operation, CoreLimits::default())
    }

    fn advertised_schema_with_limits(
        operation: CoreOperation,
        limits: CoreLimits,
    ) -> Map<String, Value> {
        let mut kernel = ServiceKernel::new(CapabilityProfile::Core);
        kernel.set_core_limits(limits);
        kernel
            .tools()
            .into_iter()
            .find(|tool| tool.name == operation.tool_name())
            .unwrap_or_else(|| panic!("{} must be advertised", operation.tool_name()))
            .input_schema
    }

    fn string_set(values: impl IntoIterator<Item = impl AsRef<str>>) -> BTreeSet<String> {
        values
            .into_iter()
            .map(|value| value.as_ref().to_owned())
            .collect()
    }

    /// The core and kernel operations are exactly the catalog's contract 0.1
    /// project-file reads, and every other tool that needs only the project
    /// files is exactly one authoring or bootstrap operation.
    #[test]
    fn core_bindings_are_exactly_the_catalog_project_file_reads() {
        use godot_mcp_domain::catalog::{
            AuthoringOperation, BootstrapOperation, ServerState, TOOL_CATALOG, ToolNeeds,
        };
        use godot_mcp_domain::contracts::ContractVersion;

        let project_files: Vec<_> = TOOL_CATALOG
            .iter()
            .filter(|tool| tool.needs == ToolNeeds::ProjectFiles)
            .collect();
        let bound: BTreeSet<&str> = CoreOperation::ALL
            .iter()
            .map(|operation| operation.tool_name())
            .chain(
                KernelOperation::ALL
                    .iter()
                    .map(|operation| operation.tool_name()),
            )
            .collect();
        let first_release_reads: BTreeSet<&str> = project_files
            .iter()
            .filter(|tool| tool.introduced_in == ContractVersion::V0_1_0 && !tool.is_mutation())
            .map(|tool| tool.name)
            .collect();
        assert_eq!(bound, first_release_reads);

        let authoring: BTreeSet<&str> = AuthoringOperation::ALL
            .iter()
            .map(|operation| operation.tool_name())
            .collect();
        let bootstrap_only: BTreeSet<&str> = BootstrapOperation::ALL
            .iter()
            .map(|operation| operation.tool_name())
            .collect();
        let unbound: BTreeSet<&str> = project_files
            .iter()
            .filter(|tool| !tool.in_state(ServerState::ProjectBound))
            .map(|tool| tool.name)
            .collect();
        assert_eq!(bootstrap_only, unbound);
        assert!(bound.is_disjoint(&authoring));
        assert!(bound.is_disjoint(&bootstrap_only));
        assert!(authoring.is_disjoint(&bootstrap_only));
        let owned: BTreeSet<&str> = bound
            .iter()
            .chain(&authoring)
            .chain(&bootstrap_only)
            .copied()
            .collect();
        let catalog: BTreeSet<&str> = project_files.iter().map(|tool| tool.name).collect();
        assert_eq!(owned, catalog);
    }

    #[test]
    fn advertised_properties_match_every_core_operations_accepted_keys() {
        for &operation in CoreOperation::ALL {
            let schema = advertised_schema(operation);
            let properties = schema["properties"].as_object().unwrap_or_else(|| {
                panic!("{} properties must be an object", operation.tool_name())
            });
            let advertised = string_set(properties.keys());
            let mut accepted = string_set(operation.accepted_keys());
            accepted.insert("idempotencyId".to_owned());
            assert_eq!(
                advertised,
                accepted,
                "advertised properties for `{}` drifted from its local and kernel arguments",
                operation.tool_name()
            );
        }
    }

    fn schema_integer(value: &Value, context: &str) -> i128 {
        value
            .as_i64()
            .map(i128::from)
            .or_else(|| value.as_u64().map(i128::from))
            .unwrap_or_else(|| panic!("{context} must be an integer, got {value}"))
    }

    fn json_integer(value: i128) -> Option<Value> {
        i64::try_from(value)
            .ok()
            .map(Value::from)
            .or_else(|| u64::try_from(value).ok().map(Value::from))
    }

    #[test]
    fn every_advertised_numeric_constraint_matches_runtime_validation() {
        let limits = CoreLimits::new(2, 3).expect("valid non-default limits");

        for &operation in CoreOperation::ALL {
            let schema = advertised_schema_with_limits(operation, limits);
            let properties = schema["properties"].as_object().unwrap_or_else(|| {
                panic!("{} properties must be an object", operation.tool_name())
            });
            for (key, property) in properties {
                if !matches!(property["type"].as_str(), Some("integer" | "number")) {
                    continue;
                }
                let property = property.as_object().unwrap_or_else(|| {
                    panic!("{}.{key} schema must be an object", operation.tool_name())
                });
                for (constraint, declared) in property {
                    let outside_delta = match constraint.as_str() {
                        "type" | "description" | "default" => continue,
                        "minimum" => -1_i128,
                        "maximum" => 1_i128,
                        unsupported => panic!(
                            "unhandled numeric schema keyword `{unsupported}` for {}.{key}",
                            operation.tool_name()
                        ),
                    };
                    let context = format!("{}.{key}.{constraint}", operation.tool_name());
                    let boundary = schema_integer(declared, &context);
                    let boundary_value = json_integer(boundary)
                        .unwrap_or_else(|| panic!("{context} boundary is outside JSON integers"));
                    let at_boundary = Map::from_iter([(key.clone(), boundary_value)]);
                    assert!(
                        Arguments::new(&at_boundary, operation.accepted_keys(), limits).is_ok(),
                        "runtime rejected advertised {context}={boundary}"
                    );

                    let Some(outside_value) =
                        boundary.checked_add(outside_delta).and_then(json_integer)
                    else {
                        continue;
                    };
                    let outside = Map::from_iter([(key.clone(), outside_value)]);
                    assert!(
                        Arguments::new(&outside, operation.accepted_keys(), limits).is_err(),
                        "runtime accepted a value outside advertised {context}={boundary}"
                    );
                }
            }
        }
    }

    #[test]
    fn json_schema_integer_values_with_zero_fraction_are_accepted() {
        let limits = CoreLimits::default();
        let integral: Value =
            serde_json::from_str(r#"{"limit":1.0}"#).expect("literal JSON number");
        let integral = integral.as_object().expect("arguments object");
        assert!(
            integral["limit"]
                .as_number()
                .is_some_and(|number| number.is_f64()),
            "the probe must exercise serde_json's floating-point number tag"
        );
        Arguments::new(
            integral,
            CoreOperation::ProjectDiscover.accepted_keys(),
            limits,
        )
        .expect("JSON Schema integer 1.0 must be accepted by runtime validation");

        let fractional: Value =
            serde_json::from_str(r#"{"limit":1.5}"#).expect("literal JSON number");
        assert!(
            Arguments::new(
                fractional.as_object().expect("arguments object"),
                CoreOperation::ProjectDiscover.accepted_keys(),
                limits,
            )
            .is_err(),
            "a fractional number is not a JSON Schema integer"
        );
    }

    #[test]
    fn advertised_path_symbol_and_limit_descriptions_match_runtime_semantics() {
        let limits = CoreLimits::default();
        for key in ["script", "scene", "resource"] {
            let property = core_argument_schema(key, limits);
            assert_eq!(
                property["minLength"],
                json!(1),
                "{key} must advertise the runtime's non-empty path constraint"
            );
        }
        assert_eq!(
            core_argument_schema("symbol", limits)["description"],
            json!("Optional function, signal, or script-class name declared by the script.")
        );
        assert_eq!(
            core_argument_schema("limit", limits)["description"],
            json!("Maximum number of entries returned in each result set.")
        );
    }

    #[test]
    fn non_nullable_core_arguments_reject_explicit_null() {
        let limits = CoreLimits::default();

        for &operation in CoreOperation::ALL {
            let schema = advertised_schema(operation);
            for key in operation.accepted_keys() {
                assert!(
                    schema["properties"][key]["type"].is_string(),
                    "{key} for `{}` must advertise one non-null type",
                    operation.tool_name()
                );
                let arguments = Map::from_iter([((*key).to_owned(), Value::Null)]);
                let error = match Arguments::new(&arguments, operation.accepted_keys(), limits) {
                    Ok(_) => panic!(
                        "{} treated explicit null `{key}` as absence",
                        operation.tool_name()
                    ),
                    Err(error) => error,
                };
                assert_eq!(
                    error.message(),
                    format!("`{key}` must not be null"),
                    "runtime null handling for `{}.{key}` drifted from its schema",
                    operation.tool_name()
                );
            }
        }
    }

    #[test]
    fn core_arguments_reject_values_with_the_wrong_declared_type() {
        let limits = CoreLimits::default();

        for &operation in CoreOperation::ALL {
            let schema = operation.input_schema(limits);
            let properties = schema["properties"].as_object().unwrap_or_else(|| {
                panic!("{} properties must be an object", operation.tool_name())
            });
            for (key, property) in properties {
                let declared_type = property["type"].as_str().unwrap_or_else(|| {
                    panic!(
                        "{key} for `{}` must declare one type",
                        operation.tool_name()
                    )
                });
                let wrong_value = match declared_type {
                    "string" => json!(123),
                    "integer" => json!("not an integer"),
                    "boolean" => json!("not a boolean"),
                    "array" => json!("not an array"),
                    "object" => json!("not an object"),
                    "number" => json!("not a number"),
                    other => panic!(
                        "{key} for `{}` declares unsupported type `{other}`",
                        operation.tool_name()
                    ),
                };
                let arguments = Map::from_iter([(key.clone(), wrong_value)]);
                let error = match Arguments::new(&arguments, operation.accepted_keys(), limits) {
                    Ok(_) => panic!(
                        "{} accepted `{key}` with a non-{declared_type} value",
                        operation.tool_name()
                    ),
                    Err(error) => error,
                };
                let expected = match declared_type {
                    "string" => "a string",
                    "integer" => "a non-negative whole number",
                    "boolean" => "a boolean",
                    "array" => "an array",
                    "object" => "an object",
                    "number" => "a number",
                    _ => unreachable!("supported schema types were checked above"),
                };
                assert_eq!(error.kind(), ErrorKind::InvalidInput);
                assert_eq!(
                    error.message(),
                    format!("`{key}` must be {expected}"),
                    "runtime type handling for `{}.{key}` drifted from declared type `{declared_type}`",
                    operation.tool_name()
                );
            }
        }
    }

    #[test]
    fn advertised_required_keys_match_path_requirements() {
        for (operation, expected) in [
            (CoreOperation::ScriptValidate, &["script"][..]),
            (CoreOperation::SceneTree, &["scene"][..]),
            (CoreOperation::ResourceTree, &["resource"][..]),
            (CoreOperation::ProjectDiscover, &[][..]),
            (CoreOperation::ScriptDiagnostics, &[][..]),
        ] {
            let schema = advertised_schema(operation);
            let required = schema["required"]
                .as_array()
                .unwrap_or_else(|| panic!("{} required must be an array", operation.tool_name()));
            let required = string_set(required.iter().map(|value| {
                value.as_str().unwrap_or_else(|| {
                    panic!("{} required keys must be strings", operation.tool_name())
                })
            }));
            assert_eq!(
                required,
                string_set(expected),
                "advertised required keys are wrong for `{}`",
                operation.tool_name()
            );
        }
    }
}
