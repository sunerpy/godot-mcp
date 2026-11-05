//! Project statistics and code/scene complexity.
//!
//! Statistics count what the inventory holds. Complexity ranks GDScript
//! functions by McCabe complexity, length and nesting (see
//! [`FunctionMetrics`](super::script_facts::FunctionMetrics) for the exact
//! counting rule) and scenes by node count and tree depth.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::glob::GlobSet;
use super::model::{Coverage, CoverageBuilder, FileContent, FileKind, ProjectModel};
use super::script_facts::LineCounts;

/// Files and bytes of one kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KindCount {
    /// The kind.
    pub kind: FileKind,
    /// Files of the kind.
    pub files: usize,
    /// Bytes of the files whose length is known.
    pub bytes: u64,
}

/// Files and bytes with one extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExtensionCount {
    /// The extension, lower-cased, without the dot; empty for none.
    pub extension: String,
    /// Files with it.
    pub files: usize,
    /// Bytes of the files whose length is known.
    pub bytes: u64,
}

/// What the project's GDScript holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ScriptStatistics {
    /// Scripts parsed.
    pub files: usize,
    /// Their lines.
    pub lines: LineCounts,
    /// Scripts registering a `class_name`.
    pub class_names: usize,
    /// Inner `class` declarations.
    pub inner_classes: usize,
    /// Functions and constructors.
    pub functions: usize,
    /// Signals.
    pub signals: usize,
    /// Member variables and constants.
    pub members: usize,
    /// `@export`ed variables.
    pub exports: usize,
    /// Scripts annotated `@tool`.
    pub tool_scripts: usize,
    /// Scripts with parse errors.
    pub damaged: usize,
}

/// What the project's text scenes and resources hold.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SceneStatistics {
    /// Text scenes parsed.
    pub scenes: usize,
    /// Nodes they declare.
    pub nodes: usize,
    /// Nodes that instance another scene.
    pub instances: usize,
    /// Nodes that attach a script.
    pub scripted_nodes: usize,
    /// `[connection]` entries.
    pub connections: usize,
    /// Text resources parsed.
    pub resources: usize,
    /// Scenes and resources with syntax problems.
    pub damaged: usize,
}

/// The statistics view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Statistics {
    /// Inventoried files.
    pub files: usize,
    /// Bytes of the files whose length is known.
    pub bytes: u64,
    /// Files whose length is not known (not read).
    pub files_without_length: usize,
    /// Counts by kind, in kind order.
    pub by_kind: Vec<KindCount>,
    /// Counts by extension, most files first, then by extension.
    pub by_extension: Vec<ExtensionCount>,
    /// GDScript totals.
    pub scripts: ScriptStatistics,
    /// Scene and resource totals.
    pub scenes: SceneStatistics,
    /// What the analysis could not see.
    pub coverage: Coverage,
}

/// Count everything the inventory holds.
#[must_use]
pub fn statistics(model: &ProjectModel<'_>) -> Statistics {
    let mut by_kind: BTreeMap<FileKind, (usize, u64)> = BTreeMap::new();
    let mut by_extension: BTreeMap<String, (usize, u64)> = BTreeMap::new();
    let mut bytes = 0u64;
    let mut without_length = 0;
    for (path, file) in model.sources().files() {
        let kind = model.kind(path).unwrap_or(FileKind::Other);
        let length = file.bytes();
        if length.is_none() {
            without_length += 1;
        }
        let length = length.unwrap_or(0);
        bytes = bytes.saturating_add(length);
        let entry = by_kind.entry(kind).or_default();
        entry.0 += 1;
        entry.1 = entry.1.saturating_add(length);
        let name = path.rsplit('/').next().unwrap_or(path);
        let extension = name
            .rsplit_once('.')
            .map(|(_, extension)| extension.to_ascii_lowercase())
            .unwrap_or_default();
        let entry = by_extension.entry(extension).or_default();
        entry.0 += 1;
        entry.1 = entry.1.saturating_add(length);
    }
    let mut extensions: Vec<ExtensionCount> = by_extension
        .into_iter()
        .map(|(extension, (files, bytes))| ExtensionCount {
            extension,
            files,
            bytes,
        })
        .collect();
    extensions.sort_by(|left, right| {
        right
            .files
            .cmp(&left.files)
            .then_with(|| left.extension.cmp(&right.extension))
    });

    let mut scripts = ScriptStatistics::default();
    for script in model.scripts().values() {
        scripts.files += 1;
        if script.is_damaged() {
            scripts.damaged += 1;
        }
        if let Some(facts) = script.facts() {
            let lines = facts.lines();
            scripts.lines.total += lines.total;
            scripts.lines.code += lines.code;
            scripts.lines.comment += lines.comment;
            scripts.lines.blank += lines.blank;
            scripts.exports += facts.exports().len();
            if facts.is_tool() {
                scripts.tool_scripts += 1;
            }
        }
        if let Some(symbols) = script.symbols() {
            if symbols.class_name_declaration().is_some() {
                scripts.class_names += 1;
            }
            scripts.inner_classes += symbols.classes().len();
            scripts.functions += symbols.functions().len();
            scripts.signals += symbols.signals().len();
            scripts.members += symbols.variables().len();
        }
    }

    let mut scenes = SceneStatistics::default();
    for (path, document) in model.documents() {
        let Ok(document) = document else {
            scenes.damaged += 1;
            continue;
        };
        if document.is_partial() {
            scenes.damaged += 1;
        }
        match model.kind(path) {
            Some(FileKind::Scene) => {
                scenes.scenes += 1;
                scenes.nodes += document.nodes().len();
                scenes.instances += document.scene_instances().len();
                scenes.scripted_nodes += document.script_attachments().len();
                scenes.connections += document.connections().len();
            }
            Some(FileKind::Resource) => scenes.resources += 1,
            _ => {}
        }
    }

    let mut coverage = CoverageBuilder::default();
    for (path, file) in model.sources().files() {
        if let FileContent::Unread(_) = file.content() {
            coverage.note(
                "files_not_read",
                "these files were not read, so their length and content are not counted",
                path.to_owned(),
            );
        }
    }
    model.note_read_gaps(
        &mut coverage,
        &[
            FileKind::Script,
            FileKind::Scene,
            FileKind::Resource,
            FileKind::BinaryScene,
            FileKind::BinaryResource,
        ],
    );
    Statistics {
        files: model.sources().len(),
        bytes,
        files_without_length: without_length,
        by_kind: by_kind
            .into_iter()
            .map(|(kind, (files, bytes))| KindCount { kind, files, bytes })
            .collect(),
        by_extension: extensions,
        scripts,
        scenes,
        coverage: coverage.finish(),
    }
}

/// What a function-complexity ranking orders by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionSort {
    /// McCabe complexity.
    Cyclomatic,
    /// Lines the declaration spans.
    Lines,
    /// Deepest nesting.
    Nesting,
}

/// What a scene-complexity ranking orders by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SceneSort {
    /// Nodes declared.
    Nodes,
    /// Deepest node.
    Depth,
    /// Instanced scenes.
    Instances,
}

/// The thresholds a complexity ranking applies: an entry is kept when it
/// reaches every one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Thresholds {
    /// Minimum McCabe complexity.
    pub min_cyclomatic: u32,
    /// Minimum lines.
    pub min_lines: u32,
    /// Minimum nesting.
    pub min_nesting: u32,
    /// Minimum nodes.
    pub min_nodes: usize,
    /// Minimum depth.
    pub min_depth: usize,
}

/// One ranked function.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FunctionComplexity {
    /// The script.
    pub script: String,
    /// The inner class, when nested.
    pub owner: Option<String>,
    /// The function name.
    pub name: String,
    /// First line.
    pub line: u32,
    /// Last line.
    pub end_line: u32,
    /// Lines spanned.
    pub lines: u32,
    /// McCabe complexity.
    pub cyclomatic: u32,
    /// Deepest nesting.
    pub max_nesting: u32,
    /// Whether the function overlaps a parse error, which makes the counts
    /// lower bounds.
    pub damaged: bool,
}

/// One ranked scene.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SceneComplexity {
    /// The scene.
    pub scene: String,
    /// Nodes declared.
    pub nodes: usize,
    /// Deepest node (the root is depth 0).
    pub max_depth: usize,
    /// Nodes that instance another scene.
    pub instances: usize,
    /// Distinct scenes instanced.
    pub instanced_scenes: usize,
    /// Nodes that attach a script.
    pub scripted_nodes: usize,
    /// `[connection]` entries.
    pub connections: usize,
    /// Whether the scene has syntax problems, which makes the counts lower
    /// bounds.
    pub damaged: bool,
}

/// Rank functions of the selected scripts.
#[must_use]
pub fn function_complexity(
    model: &ProjectModel<'_>,
    selection: &GlobSet,
    sort: FunctionSort,
    thresholds: Thresholds,
) -> (Vec<FunctionComplexity>, Coverage) {
    let mut out = Vec::new();
    for (path, script) in model.scripts() {
        if !selection.selects(path) {
            continue;
        }
        let Some(facts) = script.facts() else {
            continue;
        };
        for function in facts.functions() {
            if function.cyclomatic < thresholds.min_cyclomatic
                || function.lines < thresholds.min_lines
                || function.max_nesting < thresholds.min_nesting
            {
                continue;
            }
            out.push(FunctionComplexity {
                script: path.clone(),
                owner: function.owner.clone(),
                name: function.name.clone(),
                line: function.line,
                end_line: function.end_line,
                lines: function.lines,
                cyclomatic: function.cyclomatic,
                max_nesting: function.max_nesting,
                damaged: function.damaged,
            });
        }
    }
    out.sort_by(|left, right| {
        let key = |entry: &FunctionComplexity| match sort {
            FunctionSort::Cyclomatic => (entry.cyclomatic, entry.lines, entry.max_nesting),
            FunctionSort::Lines => (entry.lines, entry.cyclomatic, entry.max_nesting),
            FunctionSort::Nesting => (entry.max_nesting, entry.cyclomatic, entry.lines),
        };
        key(right)
            .cmp(&key(left))
            .then_with(|| (&left.script, left.line).cmp(&(&right.script, right.line)))
    });
    let mut coverage = CoverageBuilder::default();
    model.note_read_gaps(&mut coverage, &[FileKind::Script]);
    (out, coverage.finish())
}

/// Rank the selected scenes.
#[must_use]
pub fn scene_complexity(
    model: &ProjectModel<'_>,
    selection: &GlobSet,
    sort: SceneSort,
    thresholds: Thresholds,
) -> (Vec<SceneComplexity>, Coverage) {
    let mut out = Vec::new();
    for (path, document) in model.documents() {
        if model.kind(path) != Some(FileKind::Scene) || !selection.selects(path) {
            continue;
        }
        let Ok(document) = document else {
            continue;
        };
        let max_depth = document
            .nodes()
            .iter()
            .filter_map(|node| node.node_path())
            .map(|node_path| {
                if node_path == "." {
                    0
                } else {
                    node_path.split('/').count()
                }
            })
            .max()
            .unwrap_or(0);
        let instances = document.scene_instances();
        let instanced: BTreeSet<&str> = instances.iter().map(|(_, handle)| handle.id()).collect();
        let entry = SceneComplexity {
            scene: path.clone(),
            nodes: document.nodes().len(),
            max_depth,
            instances: instances.len(),
            instanced_scenes: instanced.len(),
            scripted_nodes: document.script_attachments().len(),
            connections: document.connections().len(),
            damaged: document.is_partial(),
        };
        if entry.nodes < thresholds.min_nodes || entry.max_depth < thresholds.min_depth {
            continue;
        }
        out.push(entry);
    }
    out.sort_by(|left, right| {
        let key = |entry: &SceneComplexity| match sort {
            SceneSort::Nodes => (entry.nodes, entry.max_depth, entry.instances),
            SceneSort::Depth => (entry.max_depth, entry.nodes, entry.instances),
            SceneSort::Instances => (entry.instances, entry.nodes, entry.max_depth),
        };
        key(right)
            .cmp(&key(left))
            .then_with(|| left.scene.cmp(&right.scene))
    });
    let mut coverage = CoverageBuilder::default();
    model.note_read_gaps(&mut coverage, &[FileKind::Scene, FileKind::BinaryScene]);
    (out, coverage.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::insight::model::ProjectSources;

    fn sources() -> ProjectSources {
        let mut sources = ProjectSources::new();
        sources.insert_text(
            "a.gd",
            "class_name A\nextends Node\nsignal s\n@export var x := 1\n# note\n\nfunc f(v):\n\tif v and v > 2:\n\t\treturn 1\n\treturn 0\nfunc g():\n\tpass\n",
        );
        sources.insert_text("b.gd", "@tool\nextends Node\nclass Inner:\n\tfunc h():\n\t\tfor i in 3:\n\t\t\twhile i:\n\t\t\t\tbreak\n");
        sources.insert_text(
            "s.tscn",
            "[gd_scene format=3]\n\n[ext_resource type=\"Script\" path=\"res://a.gd\" id=\"1\"]\n\n[node name=\"R\" type=\"Node\"]\nscript = ExtResource(\"1\")\n\n[node name=\"C\" type=\"Node\" parent=\".\"]\n\n[node name=\"D\" type=\"Node\" parent=\"C\"]\n",
        );
        sources.insert_text(
            "r.tres",
            "[gd_resource type=\"Resource\" format=3]\n\n[resource]\n",
        );
        sources.insert_bytes("i.png", b"\x89PNG\0\0\0");
        sources.insert_unread("big.ogg", "budget");
        sources
    }

    #[test]
    fn statistics_count_kinds_lines_and_declarations() {
        let sources = sources();
        let model = ProjectModel::build(&sources, None, 1);
        let stats = statistics(&model);
        assert_eq!(stats.files, 6);
        assert_eq!(stats.files_without_length, 1);
        assert_eq!(stats.scripts.files, 2);
        assert_eq!(stats.scripts.class_names, 1);
        assert_eq!(stats.scripts.inner_classes, 1);
        assert_eq!(stats.scripts.functions, 3);
        assert_eq!(stats.scripts.signals, 1);
        assert_eq!(stats.scripts.exports, 1);
        assert_eq!(stats.scripts.tool_scripts, 1);
        assert_eq!(stats.scripts.lines.comment, 1);
        assert_eq!(stats.scripts.lines.blank, 1);
        assert_eq!(stats.scenes.scenes, 1);
        assert_eq!(stats.scenes.nodes, 3);
        assert_eq!(stats.scenes.resources, 1);
        assert_eq!(stats.by_extension[0].extension, "gd");
        assert!(!stats.coverage.complete);
    }

    #[test]
    fn complexity_ranks_and_filters() {
        let sources = sources();
        let model = ProjectModel::build(&sources, None, 1);
        let (functions, _) = function_complexity(
            &model,
            &GlobSet::default(),
            FunctionSort::Cyclomatic,
            Thresholds::default(),
        );
        let ranked: Vec<(&str, u32)> = functions
            .iter()
            .map(|entry| (entry.name.as_str(), entry.cyclomatic))
            .collect();
        // `h` ties `f` on complexity and length and nests deeper.
        assert_eq!(ranked, vec![("h", 3), ("f", 3), ("g", 1)]);
        assert_eq!(functions[0].owner.as_deref(), Some("Inner"));
        let (nested, _) = function_complexity(
            &model,
            &GlobSet::default(),
            FunctionSort::Nesting,
            Thresholds {
                min_nesting: 2,
                ..Thresholds::default()
            },
        );
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].max_nesting, 2);
        let (scenes, _) = scene_complexity(
            &model,
            &GlobSet::default(),
            SceneSort::Depth,
            Thresholds::default(),
        );
        assert_eq!(scenes.len(), 1);
        assert_eq!(scenes[0].max_depth, 2);
        assert_eq!(scenes[0].scripted_nodes, 1);
    }
}
