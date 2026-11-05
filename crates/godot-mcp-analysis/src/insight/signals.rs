//! Signal flow: declarations, scene and code connections, emissions, and the
//! wiring problems that are statically visible.
//!
//! A scene `[connection]` is resolved through the emitting and receiving
//! nodes' effective scripts (see [`ProjectModel::resolve_node`]), so its
//! signal is attributed to the script that declares it and its handler is
//! checked against the receiver's script chain. A code use (`emit`,
//! `emit_signal`, `connect`) on `self` is attributed through the using
//! script's own chain; one on another object is matched by name only, and
//! stays `inferred`.
//!
//! Engine classes are opaque: a signal or method an engine class provides
//! cannot be distinguished from a missing one, so such a finding says which
//! engine class it may belong to instead of claiming a defect.

use std::collections::{BTreeMap, BTreeSet};

use godot_mcp_domain::Confidence;
use serde::Serialize;

use super::model::{Coverage, CoverageBuilder, FileKind, ProjectModel, ScriptChain, weakest};
use super::script_facts::{SignalUse, SignalUseKind};

/// Most connections or emissions listed on one signal; the counts are always
/// exact.
const MAX_LISTED_USES: usize = 50;

/// Which signals a query covers.
#[derive(Debug, Clone, Default)]
pub struct SignalQuery {
    /// Only signals with this name.
    pub signal: Option<String>,
    /// Only signals declared by this project-relative script.
    pub script: Option<String>,
}

/// Whether a connection's handler exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HandlerStatus {
    /// A script of the receiver's chain declares the method.
    Found,
    /// No script of the receiver's fully resolved chain declares it; only the
    /// engine base class could still provide it.
    Missing,
    /// The receiver's script chain could not be fully resolved, or the
    /// receiver runs no script, so the method may be an engine method.
    Unverified,
}

/// One `[connection]` of a scene.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SceneConnection {
    /// The scene.
    pub scene: String,
    /// 1-based line of the `[connection]` header.
    pub line: u32,
    /// The signal name.
    pub signal: Option<String>,
    /// Emitting node path (`.` is the root).
    pub from: Option<String>,
    /// Receiving node path.
    pub to: Option<String>,
    /// Handler method.
    pub method: Option<String>,
    /// `flags`, exactly as written.
    pub flags: Option<String>,
    /// Other header attributes (`binds`, `unbinds`), exactly as written.
    pub attributes: Vec<(String, String)>,
    /// The emitting node's effective script.
    pub emitter_script: Option<String>,
    /// The emitting node's engine type.
    pub emitter_type: Option<String>,
    /// The script in the emitter's chain that declares the signal.
    pub declared_in: Option<String>,
    /// The receiving node's effective script.
    pub receiver_script: Option<String>,
    /// Whether the handler exists.
    pub handler: HandlerStatus,
    /// The script declaring the handler, when found.
    pub handler_script: Option<String>,
    /// 1-based line of the handler, when found.
    pub handler_line: Option<u32>,
    /// The weakest confidence of the resolution steps.
    pub confidence: Confidence,
}

/// One emission or code connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodeSite {
    /// The script.
    pub script: String,
    /// 1-based line.
    pub line: u32,
    /// 1-based column.
    pub column: u32,
    /// What the use does.
    pub kind: SignalUseKind,
    /// The signal name, when written literally.
    pub signal: Option<String>,
    /// The object the signal is used on, `None` for the script itself.
    pub receiver: Option<String>,
    /// The handler a connection names.
    pub handler: Option<String>,
    /// The enclosing function.
    pub function: Option<String>,
    /// `exact` when the use is on the declaring script itself, `inferred`
    /// when it was matched by name on another object.
    pub confidence: Confidence,
}

/// One declared signal and everything that uses it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SignalFlow {
    /// The declaring script.
    pub script: String,
    /// The inner class declaring it, when nested.
    pub owner: Option<String>,
    /// The signal name.
    pub name: String,
    /// 1-based line of the declaration.
    pub line: u32,
    /// Declared parameters (`name: Type`).
    pub parameters: Vec<String>,
    /// Scene connections whose emitter's chain declares the signal here
    /// (first 50).
    pub scene_connections: Vec<SceneConnection>,
    /// How many scene connections there are.
    pub scene_connection_count: usize,
    /// Code connections attributed to it (first 50).
    pub code_connections: Vec<CodeSite>,
    /// How many code connections there are.
    pub code_connection_count: usize,
    /// Emissions attributed to it (first 50).
    pub emissions: Vec<CodeSite>,
    /// How many emissions there are.
    pub emission_count: usize,
}

/// What kind of wiring problem a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemKind {
    /// A scene connection names a handler no script of the receiver's chain
    /// declares.
    MissingHandler,
    /// A signal is emitted but nothing connects to it.
    EmittedNeverConnected,
    /// A connection names a signal the emitter's script chain does not
    /// declare.
    ConnectedNeverDeclared,
}

/// One statically visible wiring problem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SignalProblem {
    /// What kind of problem.
    pub kind: ProblemKind,
    /// The signal.
    pub signal: Option<String>,
    /// The file the finding is about.
    pub file: String,
    /// 1-based line.
    pub line: u32,
    /// What was found.
    pub detail: String,
    /// The engine class that could still provide the signal or method, when
    /// that is the open question.
    pub engine_class: Option<String>,
    /// `inferred` for a probable defect, `unresolved` when an engine class
    /// may make it correct.
    pub confidence: Confidence,
}

/// The signal-flow view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SignalReport {
    /// Every declared signal the query selects, ordered by script, owner and
    /// line.
    pub signals: Vec<SignalFlow>,
    /// Scene connections whose signal no script of the emitter's chain
    /// declares (engine signals, among others).
    pub undeclared_connections: Vec<SceneConnection>,
    /// Every problem, ordered by kind, file and line.
    pub problems: Vec<SignalProblem>,
    /// What the analysis could not see.
    pub coverage: Coverage,
}

struct Declaration {
    script: String,
    owner: Option<String>,
    name: String,
    line: u32,
    parameters: Vec<String>,
}

/// Trace every signal of the project.
#[must_use]
pub fn signal_flow(model: &ProjectModel<'_>, query: &SignalQuery) -> SignalReport {
    let mut coverage = CoverageBuilder::default();
    let mut declarations: Vec<Declaration> = Vec::new();
    for (path, script) in model.scripts() {
        let Some(symbols) = script.symbols() else {
            continue;
        };
        for signal in symbols.signals() {
            declarations.push(Declaration {
                script: path.clone(),
                owner: signal.owner().map(str::to_owned),
                name: signal.name().to_owned(),
                line: signal.span().start_line(),
                parameters: signal
                    .parameters()
                    .iter()
                    .map(|parameter| match parameter.type_hint() {
                        Some(hint) => format!("{}: {hint}", parameter.name()),
                        None => parameter.name().to_owned(),
                    })
                    .collect(),
            });
        }
    }
    // Lookup: (script, owner, name) -> declaration index.
    let index: BTreeMap<(String, Option<String>, String), usize> = declarations
        .iter()
        .enumerate()
        .map(|(position, declaration)| {
            (
                (
                    declaration.script.clone(),
                    declaration.owner.clone(),
                    declaration.name.clone(),
                ),
                position,
            )
        })
        .collect();
    let by_name: BTreeMap<&str, Vec<usize>> = declarations.iter().enumerate().fold(
        BTreeMap::new(),
        |mut map, (position, declaration)| {
            map.entry(declaration.name.as_str())
                .or_insert_with(Vec::new)
                .push(position);
            map
        },
    );

    let mut scene_connections: Vec<Vec<SceneConnection>> = vec![Vec::new(); declarations.len()];
    let mut code_connections: Vec<Vec<CodeSite>> = vec![Vec::new(); declarations.len()];
    let mut emissions: Vec<Vec<CodeSite>> = vec![Vec::new(); declarations.len()];
    let mut undeclared_connections = Vec::new();
    let mut problems = Vec::new();
    let mut connected_names: BTreeSet<String> = BTreeSet::new();

    // Scene connections.
    for (scene, document) in model.documents() {
        if model.kind(scene) != Some(FileKind::Scene) {
            continue;
        }
        let Ok(document) = document else {
            continue;
        };
        for connection in document.connections() {
            if let Some(signal) = connection.signal() {
                connected_names.insert(signal.to_owned());
            }
            let emitter = connection
                .from_path()
                .map(|from| model.resolve_node(scene, from));
            let receiver = connection.to_path().map(|to| model.resolve_node(scene, to));
            let emitter_chain = emitter
                .as_ref()
                .and_then(|node| node.script.as_deref())
                .map(|script| model.script_chain(script));
            let receiver_chain = receiver
                .as_ref()
                .and_then(|node| node.script.as_deref())
                .map(|script| model.script_chain(script));
            let mut confidence = if document.is_partial() {
                Confidence::Inferred
            } else {
                Confidence::Exact
            };
            for node in emitter.iter().chain(receiver.iter()) {
                confidence = weakest(confidence, node.confidence);
            }
            let declared = connection.signal().and_then(|signal| {
                emitter_chain
                    .as_ref()
                    .and_then(|chain| declaring_script(&index, chain, signal))
            });
            let (handler, handler_script, handler_line) =
                match (connection.method(), &receiver_chain) {
                    (Some(method), Some(chain)) => match find_method(model, chain, method) {
                        Some((script, line)) => (HandlerStatus::Found, Some(script), Some(line)),
                        None if chain.complete => (HandlerStatus::Missing, None, None),
                        None => (HandlerStatus::Unverified, None, None),
                    },
                    _ => (HandlerStatus::Unverified, None, None),
                };
            let flags = connection
                .extra_attributes()
                .iter()
                .find(|(key, _)| key == "flags")
                .map(|(_, value)| value.clone());
            let attributes: Vec<(String, String)> = connection
                .extra_attributes()
                .iter()
                .filter(|(key, _)| key != "flags")
                .cloned()
                .collect();
            let line = connection.span().start_line();
            let record = SceneConnection {
                scene: scene.clone(),
                line,
                signal: connection.signal().map(str::to_owned),
                from: connection.from_path().map(str::to_owned),
                to: connection.to_path().map(str::to_owned),
                method: connection.method().map(str::to_owned),
                flags,
                attributes,
                emitter_script: emitter.as_ref().and_then(|node| node.script.clone()),
                emitter_type: emitter.as_ref().and_then(|node| node.type_name.clone()),
                declared_in: declared.map(|position| declarations[position].script.clone()),
                receiver_script: receiver.as_ref().and_then(|node| node.script.clone()),
                handler,
                handler_script,
                handler_line,
                confidence,
            };
            if handler == HandlerStatus::Missing {
                let chain = receiver_chain
                    .as_ref()
                    .expect("a missing handler has a resolved chain");
                problems.push(SignalProblem {
                    kind: ProblemKind::MissingHandler,
                    signal: record.signal.clone(),
                    file: scene.clone(),
                    line,
                    detail: format!(
                        "`{}` is not declared by {}; methods of engine class {} are not checked",
                        record.method.as_deref().unwrap_or_default(),
                        chain.scripts.join(" -> "),
                        chain.engine_base.as_deref().unwrap_or("?"),
                    ),
                    engine_class: chain.engine_base.clone(),
                    confidence: weakest(Confidence::Inferred, confidence),
                });
            }
            match declared {
                Some(position) => scene_connections[position].push(record),
                None => {
                    if let (Some(chain), Some(signal)) = (&emitter_chain, connection.signal())
                        && chain.complete
                    {
                        problems.push(SignalProblem {
                            kind: ProblemKind::ConnectedNeverDeclared,
                            signal: Some(signal.to_owned()),
                            file: scene.clone(),
                            line,
                            detail: format!(
                                "`{signal}` is not declared by {}; it may be a signal of engine class {}",
                                chain.scripts.join(" -> "),
                                chain.engine_base.as_deref().unwrap_or("?")
                            ),
                            engine_class: chain.engine_base.clone(),
                            confidence: Confidence::Unresolved,
                        });
                    }
                    undeclared_connections.push(record);
                }
            }
        }
    }

    // Code uses.
    for (path, script) in model.scripts() {
        let Some(facts) = script.facts() else {
            continue;
        };
        let chain = model.script_chain(path);
        for site in facts.signal_uses() {
            let Some(signal) = site.signal.as_deref() else {
                coverage.note(
                    "dynamic_signal_names",
                    "these signal uses name the signal at run time and are not attributed",
                    format!("{path}:{}", site.line),
                );
                continue;
            };
            if !site.kind.is_emission() {
                connected_names.insert(signal.to_owned());
            }
            let targets: Vec<(usize, Confidence)> = match &site.receiver {
                None => {
                    let own = site
                        .owner
                        .as_ref()
                        .and_then(|owner| {
                            index.get(&(path.clone(), Some(owner.clone()), signal.to_owned()))
                        })
                        .copied()
                        .or_else(|| {
                            if site.owner.is_some() {
                                None
                            } else {
                                declaring_script(&index, &chain, signal)
                            }
                        });
                    match own {
                        Some(position) => vec![(
                            position,
                            if chain.complete {
                                Confidence::Exact
                            } else {
                                Confidence::Inferred
                            },
                        )],
                        None => {
                            if site.owner.is_none() && chain.complete && !site.kind.is_emission() {
                                problems.push(SignalProblem {
                                    kind: ProblemKind::ConnectedNeverDeclared,
                                    signal: Some(signal.to_owned()),
                                    file: path.clone(),
                                    line: site.line,
                                    detail: format!(
                                        "`{signal}` is not declared by {}; it may be a signal of engine class {}",
                                        chain.scripts.join(" -> "),
                                        chain.engine_base.as_deref().unwrap_or("?")
                                    ),
                                    engine_class: chain.engine_base.clone(),
                                    confidence: Confidence::Unresolved,
                                });
                            }
                            Vec::new()
                        }
                    }
                }
                Some(_) => by_name
                    .get(signal)
                    .map(|positions| {
                        positions
                            .iter()
                            .map(|position| (*position, Confidence::Inferred))
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            for (position, confidence) in targets {
                let record = code_site(path, site, confidence);
                if site.kind.is_emission() {
                    emissions[position].push(record);
                } else {
                    code_connections[position].push(record);
                }
            }
        }
    }

    let mut signals = Vec::new();
    for (position, declaration) in declarations.into_iter().enumerate() {
        let scenes = std::mem::take(&mut scene_connections[position]);
        let codes = std::mem::take(&mut code_connections[position]);
        let emits = std::mem::take(&mut emissions[position]);
        if !emits.is_empty() && !connected_names.contains(&declaration.name) {
            problems.push(SignalProblem {
                kind: ProblemKind::EmittedNeverConnected,
                signal: Some(declaration.name.clone()),
                file: declaration.script.clone(),
                line: declaration.line,
                detail: format!(
                    "`{}` is emitted {} time(s) but no scene or script connects a signal of that name",
                    declaration.name,
                    emits.len()
                ),
                engine_class: None,
                confidence: Confidence::Inferred,
            });
        }
        let selected = query
            .signal
            .as_deref()
            .is_none_or(|name| name == declaration.name)
            && query.script.as_deref().is_none_or(|script| {
                script.strip_prefix("res://").unwrap_or(script) == declaration.script
            });
        if selected {
            signals.push(SignalFlow {
                script: declaration.script,
                owner: declaration.owner,
                name: declaration.name,
                line: declaration.line,
                parameters: declaration.parameters,
                scene_connection_count: scenes.len(),
                scene_connections: scenes.into_iter().take(MAX_LISTED_USES).collect(),
                code_connection_count: codes.len(),
                code_connections: codes.into_iter().take(MAX_LISTED_USES).collect(),
                emission_count: emits.len(),
                emissions: emits.into_iter().take(MAX_LISTED_USES).collect(),
            });
        }
    }
    signals.sort_by(|left, right| {
        (&left.script, &left.owner, left.line).cmp(&(&right.script, &right.owner, right.line))
    });
    if let Some(name) = &query.signal {
        undeclared_connections.retain(|connection| connection.signal.as_deref() == Some(name));
        problems.retain(|problem| problem.signal.as_deref() == Some(name));
    }
    if let Some(script) = &query.script {
        let script = script.strip_prefix("res://").unwrap_or(script);
        problems.retain(|problem| {
            problem.file == script
                || signals
                    .iter()
                    .any(|signal| Some(&signal.name) == problem.signal.as_ref())
        });
    }
    problems.sort_by(|left, right| {
        (left.kind, &left.file, left.line).cmp(&(right.kind, &right.file, right.line))
    });
    model.note_read_gaps(
        &mut coverage,
        &[
            FileKind::Script,
            FileKind::Scene,
            FileKind::BinaryScene,
            FileKind::CSharpScript,
        ],
    );
    SignalReport {
        signals,
        undeclared_connections,
        problems,
        coverage: coverage.finish(),
    }
}

fn code_site(path: &str, site: &SignalUse, confidence: Confidence) -> CodeSite {
    CodeSite {
        script: path.to_owned(),
        line: site.line,
        column: site.column,
        kind: site.kind,
        signal: site.signal.clone(),
        receiver: site.receiver.clone(),
        handler: site.handler.clone(),
        function: site.enclosing_function.clone(),
        confidence,
    }
}

/// The declaration of `signal` by the first script of `chain` that declares
/// it at top level.
fn declaring_script(
    index: &BTreeMap<(String, Option<String>, String), usize>,
    chain: &ScriptChain,
    signal: &str,
) -> Option<usize> {
    chain.scripts.iter().find_map(|script| {
        index
            .get(&(script.clone(), None, signal.to_owned()))
            .copied()
    })
}

/// The first script of `chain` declaring a top-level function `method`.
fn find_method(
    model: &ProjectModel<'_>,
    chain: &ScriptChain,
    method: &str,
) -> Option<(String, u32)> {
    chain.scripts.iter().find_map(|script| {
        model
            .script(script)
            .and_then(|model| model.symbols())
            .and_then(|symbols| {
                symbols
                    .functions()
                    .iter()
                    .find(|function| function.owner().is_none() && function.name() == method)
                    .map(|function| (script.clone(), function.span().start_line()))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::insight::model::ProjectSources;

    #[test]
    fn connections_emissions_and_problems_are_traced() {
        let mut sources = ProjectSources::new();
        sources.insert_text(
            "player.gd",
            "extends CharacterBody2D\nsignal health_changed(value: int)\nsignal died\nsignal never_heard\nfunc hurt():\n\thealth_changed.emit(1)\n\temit_signal(\"died\")\n\tnever_heard.emit()\n",
        );
        sources.insert_text(
            "hud.gd",
            "extends Control\nfunc _ready():\n\t$\"../Player\".died.connect(_on_died)\nfunc _on_player_health_changed(value):\n\tpass\nfunc _on_died():\n\tpass\n",
        );
        sources.insert_text(
            "main.tscn",
            "[gd_scene format=3]\n\n[ext_resource type=\"Script\" path=\"res://player.gd\" id=\"1\"]\n[ext_resource type=\"Script\" path=\"res://hud.gd\" id=\"2\"]\n\n[node name=\"Main\" type=\"Node\"]\n\n[node name=\"Player\" type=\"CharacterBody2D\" parent=\".\"]\nscript = ExtResource(\"1\")\n\n[node name=\"Hud\" type=\"Control\" parent=\".\"]\nscript = ExtResource(\"2\")\n\n[node name=\"Button\" type=\"Button\" parent=\"Hud\"]\n\n[connection signal=\"health_changed\" from=\"Player\" to=\"Hud\" method=\"_on_player_health_changed\"]\n[connection signal=\"died\" from=\"Player\" to=\"Hud\" method=\"_on_gone\" flags=3]\n[connection signal=\"pressed\" from=\"Hud/Button\" to=\"Hud\" method=\"_on_died\"]\n[connection signal=\"tree_exited\" from=\"Player\" to=\"Hud\" method=\"_on_died\"]\n",
        );
        let model = ProjectModel::build(&sources, None, 1);
        let report = signal_flow(&model, &SignalQuery::default());
        let names: Vec<&str> = report
            .signals
            .iter()
            .map(|signal| signal.name.as_str())
            .collect();
        assert_eq!(names, vec!["health_changed", "died", "never_heard"]);
        let health = &report.signals[0];
        assert_eq!(health.parameters, vec!["value: int".to_owned()]);
        assert_eq!(health.scene_connections.len(), 1);
        assert_eq!(health.scene_connections[0].handler, HandlerStatus::Found);
        assert_eq!(
            health.scene_connections[0].handler_script.as_deref(),
            Some("hud.gd")
        );
        assert_eq!(health.emissions.len(), 1);
        let died = &report.signals[1];
        assert_eq!(died.scene_connections[0].handler, HandlerStatus::Missing);
        assert_eq!(died.scene_connections[0].flags.as_deref(), Some("3"));
        assert_eq!(died.code_connections.len(), 1);
        assert_eq!(died.code_connections[0].confidence, Confidence::Inferred);
        assert_eq!(died.emissions[0].kind, SignalUseKind::EmitSignal);
        let kinds: Vec<(ProblemKind, Option<&str>)> = report
            .problems
            .iter()
            .map(|problem| (problem.kind, problem.signal.as_deref()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (ProblemKind::MissingHandler, Some("died")),
                (ProblemKind::EmittedNeverConnected, Some("never_heard")),
                (ProblemKind::ConnectedNeverDeclared, Some("tree_exited")),
            ]
        );
        assert_eq!(
            report.problems[2].engine_class.as_deref(),
            Some("CharacterBody2D")
        );
        assert_eq!(report.problems[2].confidence, Confidence::Unresolved);
        let undeclared: Vec<&str> = report
            .undeclared_connections
            .iter()
            .filter_map(|connection| connection.signal.as_deref())
            .collect();
        assert_eq!(undeclared, vec!["pressed", "tree_exited"]);
    }
}
