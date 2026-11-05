//! Cross-scene node queries, and the script variables a scene exports.
//!
//! Both views resolve a node the way the editor does: a node that instances
//! another scene takes its type and script from that scene's root unless it
//! overrides them, and a node with no declared type (an editable child of an
//! instance, or a node of an inherited scene) is described by the instanced
//! scene it belongs to. Scripts are followed up their `extends` chain through
//! project scripts. Engine classes are opaque: which engine class inherits
//! which is not known statically, so a query by engine type matches that type
//! exactly.

use std::collections::BTreeSet;

use godot_mcp_domain::Confidence;
use serde::Serialize;

use super::glob::{GlobSet, godot_wildcard_match};
use super::model::{
    Coverage, CoverageBuilder, FileKind, NodeOrigin, ProjectModel, ScriptOrigin, weakest,
};
use super::script_facts::TypeSource;

/// Deepest instance chain followed when looking up a stored property value.
const MAX_STORED_DEPTH: usize = 32;

/// Longest default or stored value returned, in characters.
const MAX_VALUE_CHARS: usize = 1_000;

/// What a node query asks for. Every criterion given must hold.
#[derive(Debug, Clone, Default)]
pub struct NodeQuery {
    /// A node type: an engine class, matched exactly, or a project
    /// `class_name`, matched through script inheritance.
    pub type_name: Option<String>,
    /// A group the node belongs to.
    pub group: Option<String>,
    /// A node-name pattern (`*` and `?`, case-sensitive).
    pub name: Option<String>,
    /// A project-relative script the node runs, directly or through
    /// inheritance.
    pub script: Option<String>,
    /// A property the scene stores on the node.
    pub property: Option<String>,
    /// The stored property's value exactly as written (requires
    /// [`Self::property`]).
    pub value: Option<String>,
    /// Which scenes to search.
    pub scenes: GlobSet,
}

/// How a node satisfied the type criterion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeMatch {
    /// The node's declared type.
    Declared,
    /// The type of the scene the node instances or belongs to.
    Instanced,
    /// The `class_name` of the node's own script.
    ScriptClass,
    /// The `class_name` of a script the node's script inherits from.
    InheritedClass,
    /// The engine class the node's script chain extends.
    ScriptBase,
}

/// One node satisfying a query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NodeMatch {
    /// The scene the node is declared in.
    pub scene: String,
    /// The node's path in that scene (`.` is the root).
    pub node_path: String,
    /// The node's name.
    pub name: Option<String>,
    /// Declared or inherited engine type.
    pub type_name: Option<String>,
    /// Where the type came from.
    pub type_origin: NodeOrigin,
    /// Effective script.
    pub script: Option<String>,
    /// How the script was established.
    pub script_origin: ScriptOrigin,
    /// The `class_name` of the effective script, when it registers one.
    pub script_class: Option<String>,
    /// The scene this node instances.
    pub instance: Option<String>,
    /// Groups the node belongs to: its own, then those of the scene root it
    /// instances.
    pub groups: Vec<String>,
    /// How the type criterion was met, when one was given.
    pub type_match: Option<TypeMatch>,
    /// 1-based line of the node's header.
    pub line: u32,
    /// The weakest confidence of any step that decided the match.
    pub confidence: Confidence,
}

/// The node-query view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NodeReport {
    /// Every matching node, ordered by scene, then source order.
    pub matches: Vec<NodeMatch>,
    /// Scenes searched.
    pub scenes_searched: usize,
    /// Nodes examined.
    pub nodes_examined: usize,
    /// Statements about what the query cannot match.
    pub limitations: Vec<String>,
    /// What the analysis could not see.
    pub coverage: Coverage,
}

/// Find the nodes of every selected scene that satisfy `query`.
#[must_use]
pub fn find_nodes(model: &ProjectModel<'_>, query: &NodeQuery) -> NodeReport {
    let mut matches = Vec::new();
    let mut scenes_searched = 0;
    let mut nodes_examined = 0;
    let wanted_script = query
        .script
        .as_deref()
        .map(|script| script.strip_prefix("res://").unwrap_or(script).to_owned());

    for (path, document) in model.documents() {
        if model.kind(path) != Some(FileKind::Scene) || !query.scenes.selects(path) {
            continue;
        }
        let Ok(document) = document else {
            continue;
        };
        scenes_searched += 1;
        for node in document.nodes() {
            let Some(node_path) = node.node_path() else {
                continue;
            };
            nodes_examined += 1;
            if let Some(pattern) = &query.name
                && !node
                    .name()
                    .is_some_and(|name| godot_wildcard_match(pattern, name, true))
            {
                continue;
            }
            if let Some(key) = &query.property {
                let stored = node
                    .properties()
                    .iter()
                    .find(|property| property.key() == key);
                let Some(stored) = stored else {
                    continue;
                };
                if let Some(value) = &query.value
                    && stored.raw_value().trim() != value.trim()
                {
                    continue;
                }
            }
            let resolved = model.resolve_node(path, node_path);
            let mut groups: Vec<String> = node
                .groups()
                .iter()
                .map(|group| group.name().to_owned())
                .collect();
            if let Some(instance) = &resolved.instance
                && let Some(root) = model
                    .document(instance)
                    .and_then(|document| document.root())
            {
                for group in root.groups() {
                    if !groups.iter().any(|known| known == group.name()) {
                        groups.push(group.name().to_owned());
                    }
                }
            }
            if let Some(group) = &query.group
                && !groups.iter().any(|known| known == group)
            {
                continue;
            }
            let chain = resolved
                .script
                .as_deref()
                .map(|script| model.script_chain(script));
            let mut confidence = resolved.confidence;
            if let Some(script) = &wanted_script {
                let Some(chain) = &chain else {
                    continue;
                };
                if !chain.scripts.iter().any(|link| link == script) {
                    continue;
                }
                if chain.scripts.first() != Some(script) && !chain.complete {
                    confidence = weakest(confidence, Confidence::Inferred);
                }
            }
            let script_class = resolved
                .script
                .as_deref()
                .and_then(|script| class_of(model, script));
            let type_match = match &query.type_name {
                None => None,
                Some(wanted) => {
                    let matched = if resolved.type_name.as_deref() == Some(wanted) {
                        Some(match resolved.type_origin {
                            NodeOrigin::Declared => TypeMatch::Declared,
                            _ => TypeMatch::Instanced,
                        })
                    } else if let Some(chain) = &chain {
                        if script_class.as_deref() == Some(wanted) {
                            Some(TypeMatch::ScriptClass)
                        } else if chain
                            .scripts
                            .iter()
                            .skip(1)
                            .any(|link| class_of(model, link).as_deref() == Some(wanted))
                        {
                            Some(TypeMatch::InheritedClass)
                        } else if chain.engine_base.as_deref() == Some(wanted) {
                            Some(TypeMatch::ScriptBase)
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    let Some(matched) = matched else {
                        continue;
                    };
                    if matches!(matched, TypeMatch::InheritedClass | TypeMatch::ScriptBase)
                        && chain.as_ref().is_some_and(|chain| !chain.complete)
                    {
                        confidence = weakest(confidence, Confidence::Inferred);
                    }
                    Some(matched)
                }
            };
            matches.push(NodeMatch {
                scene: path.clone(),
                node_path: node_path.to_owned(),
                name: node.name().map(str::to_owned),
                type_name: resolved.type_name,
                type_origin: resolved.type_origin,
                script: resolved.script,
                script_origin: resolved.script_origin,
                script_class,
                instance: resolved.instance,
                groups,
                type_match,
                line: node.span().start_line(),
                confidence,
            });
        }
    }

    let mut limitations = vec![
        "Only nodes a scene file declares are searched; the nodes of an instanced scene are found by searching that scene.".to_owned(),
    ];
    if let Some(wanted) = &query.type_name
        && model.class_scripts(wanted).is_empty()
    {
        limitations.push(format!(
            "{wanted} is not a project class_name, so it is matched as an engine class name exactly; engine class inheritance is not known statically, so nodes of a type that merely inherits {wanted} are not matched."
        ));
    }
    let mut coverage = CoverageBuilder::default();
    model.note_read_gaps(
        &mut coverage,
        &[FileKind::Scene, FileKind::BinaryScene, FileKind::Script],
    );
    NodeReport {
        matches,
        scenes_searched,
        nodes_examined,
        limitations,
        coverage: coverage.finish(),
    }
}

fn class_of(model: &ProjectModel<'_>, script: &str) -> Option<String> {
    model
        .script(script)
        .and_then(|model| model.symbols())
        .and_then(|symbols| symbols.class_name_declaration())
        .map(str::to_owned)
}

/// Where an exported variable's effective value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueSource {
    /// The queried scene stores a value for the node.
    Scene,
    /// The scene the node instances (or belongs to) stores it.
    InstancedScene,
    /// Nothing stores it; the script's initializer applies.
    ScriptDefault,
    /// Nothing stores it and the script writes no initializer.
    None,
}

/// One exported variable of one node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportRow {
    /// The node's path in the scene (`.` is the root).
    pub node_path: String,
    /// The node's name.
    pub node_name: Option<String>,
    /// The node's effective script.
    pub script: String,
    /// The script in the chain that declares the variable.
    pub declared_in: String,
    /// The variable's name.
    pub name: String,
    /// Every export annotation exactly as written.
    pub annotation: String,
    /// The declared or literal-inferred type.
    pub type_name: Option<String>,
    /// Where the type came from.
    pub type_source: TypeSource,
    /// The script's initializer exactly as written (first 1,000 characters).
    pub default_value: Option<String>,
    /// Whether [`Self::default_value`] was cut.
    pub default_value_truncated: bool,
    /// Active `@export_category`.
    pub category: Option<String>,
    /// Active `@export_group`.
    pub group: Option<String>,
    /// Active `@export_subgroup`.
    pub subgroup: Option<String>,
    /// 1-based line of the declaration in [`Self::declared_in`].
    pub line: u32,
    /// The value a scene stores for the variable, exactly as written (first
    /// 1,000 characters).
    pub stored_value: Option<String>,
    /// Whether [`Self::stored_value`] was cut.
    pub stored_value_truncated: bool,
    /// The scene that stores it.
    pub stored_in: Option<String>,
    /// 1-based line of the stored value in [`Self::stored_in`].
    pub stored_line: Option<u32>,
    /// Where the effective value comes from.
    pub value_source: ValueSource,
    /// `exact` when the node, its script chain and the stored value were all
    /// read from undamaged documents.
    pub confidence: Confidence,
}

/// The scene-exports view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportReport {
    /// One row per exported variable per scripted node, in node order, then
    /// script-chain order (the node's own script first), then declaration
    /// order.
    pub rows: Vec<ExportRow>,
    /// Nodes of the scene that run a script file.
    pub scripted_nodes: usize,
    /// Nodes whose built-in script was not analysed.
    pub built_in_scripts: Vec<String>,
    /// Nodes whose script could not be identified.
    pub unresolved_scripts: Vec<String>,
    /// What the analysis could not see.
    pub coverage: Coverage,
}

/// The exported variables of every scripted node of `scene`, with the values
/// the scene (or an instanced scene) stores for them. `None` when `scene` was
/// not parsed.
#[must_use]
pub fn scene_exports(model: &ProjectModel<'_>, scene: &str) -> Option<ExportReport> {
    let document = model.document(scene)?;
    let mut rows = Vec::new();
    let mut scripted_nodes = 0;
    let mut built_in_scripts = Vec::new();
    let mut unresolved_scripts = Vec::new();
    let mut coverage = CoverageBuilder::default();
    for node in document.nodes() {
        let Some(node_path) = node.node_path() else {
            continue;
        };
        let resolved = model.resolve_node(scene, node_path);
        match resolved.script_origin {
            ScriptOrigin::BuiltIn => {
                built_in_scripts.push(node_path.to_owned());
                continue;
            }
            ScriptOrigin::Unresolved => {
                unresolved_scripts.push(node_path.to_owned());
                continue;
            }
            ScriptOrigin::None => continue,
            ScriptOrigin::Attached | ScriptOrigin::InstancedScene => {}
        }
        let Some(script) = resolved.script.clone() else {
            continue;
        };
        scripted_nodes += 1;
        let chain = model.script_chain(&script);
        let mut declared: BTreeSet<String> = BTreeSet::new();
        for link in &chain.scripts {
            let Some(facts) = model.script(link).and_then(|script| script.facts()) else {
                continue;
            };
            for export in facts.exports() {
                if !declared.insert(export.name.clone()) {
                    continue;
                }
                let stored = stored_property(model, scene, node_path, &export.name, 0);
                let mut confidence = weakest(
                    resolved.confidence,
                    if chain.complete {
                        Confidence::Exact
                    } else {
                        Confidence::Inferred
                    },
                );
                let (stored_value, stored_in, stored_line, value_source) = match stored {
                    Some(stored) => {
                        confidence = weakest(confidence, stored.confidence);
                        let source = if stored.scene == scene {
                            ValueSource::Scene
                        } else {
                            ValueSource::InstancedScene
                        };
                        (
                            Some(stored.value),
                            Some(stored.scene),
                            Some(stored.line),
                            source,
                        )
                    }
                    None if export.default_value.is_some() => {
                        (None, None, None, ValueSource::ScriptDefault)
                    }
                    None => (None, None, None, ValueSource::None),
                };
                let (default_value, default_value_truncated) = cut(export.default_value.as_deref());
                let (stored_value, stored_value_truncated) = cut(stored_value.as_deref());
                rows.push(ExportRow {
                    node_path: node_path.to_owned(),
                    node_name: node.name().map(str::to_owned),
                    script: script.clone(),
                    declared_in: link.clone(),
                    name: export.name.clone(),
                    annotation: export.annotation.clone(),
                    type_name: export.type_name.clone(),
                    type_source: export.type_source,
                    default_value,
                    default_value_truncated,
                    category: export.category.clone(),
                    group: export.group.clone(),
                    subgroup: export.subgroup.clone(),
                    line: export.line,
                    stored_value,
                    stored_value_truncated,
                    stored_in,
                    stored_line,
                    value_source,
                    confidence,
                });
            }
        }
        if !chain.complete {
            coverage.note(
                "incomplete_script_chains",
                "these nodes' script inheritance could not be followed to the end; exports of the unresolved bases are missing",
                format!("{scene}:{node_path}"),
            );
        }
    }
    if !built_in_scripts.is_empty() {
        coverage.note(
            "built_in_scripts_not_analysed",
            "these nodes run built-in scripts, whose exports are not analysed",
            scene.to_owned(),
        );
    }
    model.note_read_gaps(&mut coverage, &[FileKind::Script]);
    if document.is_partial() {
        coverage.note(
            "damaged_documents",
            "these documents have syntax problems; damaged sections may be missing",
            scene.to_owned(),
        );
    }
    Some(ExportReport {
        rows,
        scripted_nodes,
        built_in_scripts,
        unresolved_scripts,
        coverage: coverage.finish(),
    })
}

/// `value` cut to 1,000 characters, and whether it was cut.
fn cut(value: Option<&str>) -> (Option<String>, bool) {
    match value {
        Some(value) if value.chars().count() > MAX_VALUE_CHARS => {
            (Some(value.chars().take(MAX_VALUE_CHARS).collect()), true)
        }
        Some(value) => (Some(value.to_owned()), false),
        None => (None, false),
    }
}

struct StoredValue {
    scene: String,
    value: String,
    line: u32,
    confidence: Confidence,
}

/// The value the nearest scene in the node's instance chain stores for `key`.
fn stored_property(
    model: &ProjectModel<'_>,
    scene: &str,
    node_path: &str,
    key: &str,
    depth: usize,
) -> Option<StoredValue> {
    if depth >= MAX_STORED_DEPTH {
        return None;
    }
    let document = model.document(scene)?;
    if let Some(node) = document.node_at(node_path)
        && let Some(property) = node
            .properties()
            .iter()
            .find(|property| property.key() == key)
    {
        return Some(StoredValue {
            scene: scene.to_owned(),
            value: property.raw_value().trim().to_owned(),
            line: property.span().start_line(),
            confidence: if document.is_partial() {
                Confidence::Inferred
            } else {
                Confidence::Exact
            },
        });
    }
    let (base, relative) = model.node_base(scene, node_path)?;
    stored_property(model, &base, &relative, key, depth + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::insight::model::ProjectSources;

    fn sources() -> ProjectSources {
        let mut sources = ProjectSources::new();
        sources.insert_text(
            "actors/enemy.gd",
            "class_name Enemy\nextends CharacterBody2D\n@export var speed := 4.0\n@export var title: String = \"grunt\"\n",
        );
        sources.insert_text(
            "actors/boss.gd",
            "class_name Boss\nextends Enemy\n@export_range(1, 9) var phase: int = 1\n",
        );
        sources.insert_text(
            "actors/enemy.tscn",
            "[gd_scene format=3]\n\n[ext_resource type=\"Script\" path=\"res://actors/enemy.gd\" id=\"1\"]\n\n[node name=\"Enemy\" type=\"CharacterBody2D\" groups=[\"foes\"]]\nscript = ExtResource(\"1\")\nspeed = 6.0\n\n[node name=\"Sprite\" type=\"Sprite2D\" parent=\".\"]\n",
        );
        sources.insert_text(
            "levels/one.tscn",
            "[gd_scene format=3]\n\n[ext_resource type=\"PackedScene\" path=\"res://actors/enemy.tscn\" id=\"1\"]\n[ext_resource type=\"Script\" path=\"res://actors/boss.gd\" id=\"2\"]\n\n[node name=\"Level\" type=\"Node2D\"]\n\n[node name=\"Grunt\" parent=\".\" instance=ExtResource(\"1\")]\ntitle = \"first\"\n\n[node name=\"Boss\" parent=\".\" instance=ExtResource(\"1\")]\nscript = ExtResource(\"2\")\n\n[node name=\"Sprite\" parent=\"Grunt\"]\nmodulate = Color(1, 0, 0, 1)\n",
        );
        sources
    }

    #[test]
    fn nodes_match_by_type_through_instances_and_script_classes() {
        let sources = sources();
        let model = ProjectModel::build(&sources, None, 1);
        let query = NodeQuery {
            type_name: Some("Enemy".to_owned()),
            ..NodeQuery::default()
        };
        let report = find_nodes(&model, &query);
        let found: Vec<(&str, &str, Option<TypeMatch>)> = report
            .matches
            .iter()
            .map(|found| {
                (
                    found.scene.as_str(),
                    found.node_path.as_str(),
                    found.type_match,
                )
            })
            .collect();
        assert_eq!(
            found,
            vec![
                ("actors/enemy.tscn", ".", Some(TypeMatch::ScriptClass)),
                ("levels/one.tscn", "Grunt", Some(TypeMatch::ScriptClass)),
                ("levels/one.tscn", "Boss", Some(TypeMatch::InheritedClass)),
            ]
        );
        assert_eq!(
            report.matches[1].type_name.as_deref(),
            Some("CharacterBody2D")
        );
        assert_eq!(report.matches[1].type_origin, NodeOrigin::Instance);
        assert_eq!(report.matches[1].groups, vec!["foes".to_owned()]);

        let sprites = find_nodes(
            &model,
            &NodeQuery {
                type_name: Some("Sprite2D".to_owned()),
                ..NodeQuery::default()
            },
        );
        let found: Vec<(&str, NodeOrigin)> = sprites
            .matches
            .iter()
            .map(|found| (found.node_path.as_str(), found.type_origin))
            .collect();
        assert_eq!(
            found,
            vec![
                ("Sprite", NodeOrigin::Declared),
                ("Grunt/Sprite", NodeOrigin::InstancedScene)
            ]
        );
        assert_eq!(sprites.limitations.len(), 2);

        let by_group_and_property = find_nodes(
            &model,
            &NodeQuery {
                group: Some("foes".to_owned()),
                property: Some("title".to_owned()),
                value: Some("\"first\"".to_owned()),
                ..NodeQuery::default()
            },
        );
        assert_eq!(by_group_and_property.matches.len(), 1);
        assert_eq!(by_group_and_property.matches[0].node_path, "Grunt");

        let by_name = find_nodes(
            &model,
            &NodeQuery {
                name: Some("B*".to_owned()),
                script: Some("res://actors/enemy.gd".to_owned()),
                ..NodeQuery::default()
            },
        );
        assert_eq!(by_name.matches.len(), 1);
        assert_eq!(by_name.matches[0].script.as_deref(), Some("actors/boss.gd"));
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn exports_carry_script_defaults_and_stored_overrides() {
        let sources = sources();
        let model = ProjectModel::build(&sources, None, 1);
        let report = scene_exports(&model, "levels/one.tscn").expect("parsed");
        let rows: Vec<(&str, &str, Option<&str>, ValueSource, Option<&str>)> = report
            .rows
            .iter()
            .map(|row| {
                (
                    row.node_path.as_str(),
                    row.name.as_str(),
                    row.stored_value.as_deref(),
                    row.value_source,
                    row.stored_in.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (
                    "Grunt",
                    "speed",
                    Some("6.0"),
                    ValueSource::InstancedScene,
                    Some("actors/enemy.tscn")
                ),
                (
                    "Grunt",
                    "title",
                    Some("\"first\""),
                    ValueSource::Scene,
                    Some("levels/one.tscn")
                ),
                ("Boss", "phase", None, ValueSource::ScriptDefault, None),
                (
                    "Boss",
                    "speed",
                    Some("6.0"),
                    ValueSource::InstancedScene,
                    Some("actors/enemy.tscn")
                ),
                ("Boss", "title", None, ValueSource::ScriptDefault, None),
            ]
        );
        assert_eq!(report.rows[2].declared_in, "actors/boss.gd");
        assert_eq!(report.rows[3].declared_in, "actors/enemy.gd");
        assert_eq!(report.scripted_nodes, 2);
        assert!(scene_exports(&model, "missing.tscn").is_none());
    }
}
