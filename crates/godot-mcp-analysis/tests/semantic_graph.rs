//! Todo 10 acceptance tests: the Godot semantic graph, references, impact and
//! the honesty model.
//!
//! Exactly two test cases, matching the `qa/tasks/10.json` descriptor:
//! `semantic_graph_happy_path` and `semantic_graph_failure_paths`.
//!
//! Both cases run against a *copy* of the committed fixture project, so the
//! repository tree is never mutated and the read-only guarantee can be asserted
//! by comparing fixture bytes before and after every build.
//!
//! The reads flow through Todo 8's [`ResolvedProject`], which is built on Todo
//! 2's held-object ownership layer (Unix object identity on this host); the
//! tests therefore assert the real behaviour on Unix and are compiled out
//! elsewhere rather than silently weakened.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use godot_mcp_analysis::gdscript::GdScriptParser;
use godot_mcp_analysis::graph::{
    DanglingKind, EdgeKind, EdgeResolution, GRAPH_SCHEMA, GraphNodeId, GraphNodeKind,
    LimitationKind, SemanticGraph, SemanticGraphBuilder,
};
use godot_mcp_analysis::project::{ProjectResolver, ResolvedProject};
use godot_mcp_analysis::scene::read_document;
use godot_mcp_domain::Confidence;
use godot_mcp_domain::contracts::ProvenanceSource;

/// Generation every fixture read is attributed to.
const GENERATION: u64 = 10;

/// Every project-relative path the fixture project contains, in the order the
/// golden graph expects to see them.
const DOCUMENTS: &[&str] = &[
    "main.tscn",
    "scenes/hud.tscn",
    "scenes/enemy.tscn",
    "resources/buff.tres",
];

const SCRIPTS: &[&str] = &[
    "scripts/base_actor.gd",
    "scripts/player.gd",
    "scripts/hud.gd",
    "scripts/game_state.gd",
    "scripts/dynamic.gd",
];

const INVENTORY: &[&str] = &[
    "project.godot",
    "main.tscn",
    "scenes/hud.tscn",
    "scenes/enemy.tscn",
    "resources/buff.tres",
    "scripts/base_actor.gd",
    "scripts/player.gd",
    "scripts/hud.gd",
    "scripts/game_state.gd",
    "scripts/dynamic.gd",
    "art/icon.png",
];

fn fixture_project() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("graph")
        .join("sample")
}

fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("graph")
        .join("golden")
        .join("semantic-graph.json")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create destination directory");
    for entry in fs::read_dir(from).expect("read fixture directory") {
        let entry = entry.expect("fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("fixture entry type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canonicalize temp base");
    let root = base.join("game");
    let state = base.join("state");
    copy_tree(&fixture_project(), &root);
    Fixture {
        _tmp: tmp,
        root,
        state,
    }
}

fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    collect(dir, dir, &mut out);
    out
}

fn collect(base: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
    for entry in fs::read_dir(dir).expect("read directory") {
        let entry = entry.expect("entry");
        let path = entry.path();
        if entry.file_type().expect("entry type").is_dir() {
            collect(base, &path, out);
        } else {
            let relative = path
                .strip_prefix(base)
                .expect("relative path")
                .to_path_buf();
            out.insert(relative, fs::read(&path).expect("read file"));
        }
    }
}

/// Build the graph from the fixture project, reading every document and script
/// through the held lease.
fn build_graph(project: &ResolvedProject, with_inventory: bool) -> SemanticGraph {
    let mut parser = GdScriptParser::new().expect("load the GDScript grammar");
    let mut builder =
        SemanticGraphBuilder::new(GENERATION).with_settings(project.settings().clone());
    for relative in SCRIPTS {
        let bytes = project
            .read_source_file(Path::new(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        let text = String::from_utf8(bytes).expect("fixture script is UTF-8");
        let script = parser
            .parse(relative, &text, GENERATION)
            .unwrap_or_else(|error| panic!("parse {relative}: {error}"));
        builder = builder.with_script(script);
    }
    for relative in DOCUMENTS {
        let document = read_document(project, Path::new(relative), GENERATION)
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        builder = builder.with_document(document);
    }
    if with_inventory {
        builder = builder.with_file_inventory(INVENTORY.iter().copied());
    }
    builder.build().expect("derive the semantic graph")
}

fn edge<'graph>(
    graph: &'graph SemanticGraph,
    kind: EdgeKind,
    source: &GraphNodeId,
    detail_contains: &str,
) -> &'graph godot_mcp_analysis::graph::GraphEdge {
    graph
        .edges_from(source)
        .into_iter()
        .find(|edge| edge.kind() == kind && edge.detail().contains(detail_contains))
        .unwrap_or_else(|| {
            panic!(
                "no {} edge from {source} whose detail contains {detail_contains:?}; edges: {:?}",
                kind.code(),
                graph
                    .edges_from(source)
                    .iter()
                    .map(|edge| (edge.kind().code(), edge.detail().to_owned()))
                    .collect::<Vec<_>>()
            )
        })
}

/// The happy path: a real Godot project's scene → script → autoload / signal /
/// resource chain is traced end to end, and the golden graph is byte-stable.
#[test]
#[allow(clippy::too_many_lines)]
fn semantic_graph_happy_path() {
    let fixture = fixture();
    let before = snapshot(&fixture.root);
    let resolver = ProjectResolver::new(&fixture.state);
    let project = resolver
        .resolve(&fixture.root, GENERATION)
        .expect("resolve the fixture project");

    let graph = build_graph(&project, true);

    assert_eq!(graph.schema(), GRAPH_SCHEMA);
    assert_eq!(graph.project_generation(), GENERATION);

    // Every input parsed cleanly, so the graph as a whole is exact evidence.
    assert_eq!(
        graph.provenance().source(),
        ProvenanceSource::StaticIndex,
        "a clean build is index evidence"
    );
    assert_eq!(graph.provenance().confidence(), Confidence::Exact);

    // -- 1. the boot scene is reachable from the project ---------------------
    let project_node = GraphNodeId::project();
    let main_scene = edge(
        &graph,
        EdgeKind::MainScene,
        &project_node,
        "application/run/main_scene",
    );
    assert_eq!(main_scene.resolution(), EdgeResolution::Exact);
    assert_eq!(
        main_scene.exact_target(),
        Some(&GraphNodeId::scene("main.tscn"))
    );

    // -- 2. scene -> node -> script attachment ------------------------------
    let player_node = GraphNodeId::scene_node("main.tscn", "Player");
    assert_eq!(
        graph
            .node(&player_node)
            .expect("the Player node is indexed")
            .kind(),
        GraphNodeKind::SceneNode
    );
    let attachment = edge(
        &graph,
        EdgeKind::ScriptAttachment,
        &player_node,
        "script = ExtResource",
    );
    assert_eq!(attachment.resolution(), EdgeResolution::Exact);
    assert_eq!(
        attachment.exact_target(),
        Some(&GraphNodeId::script("scripts/player.gd"))
    );

    // the node's place in the tree is exact too
    let parent = edge(&graph, EdgeKind::SceneParent, &player_node, "parent=\".\"");
    assert_eq!(parent.resolution(), EdgeResolution::Exact);
    assert_eq!(
        parent.exact_target(),
        Some(&GraphNodeId::scene_node("main.tscn", "."))
    );

    // -- 3. script -> autoload -----------------------------------------------
    let autoload = GraphNodeId::autoload("GameState");
    let registration = edge(
        &graph,
        EdgeKind::AutoloadRegistration,
        &project_node,
        "[autoload] GameState",
    );
    assert_eq!(registration.resolution(), EdgeResolution::Exact);
    assert_eq!(registration.exact_target(), Some(&autoload));
    let backing = edge(&graph, EdgeKind::AutoloadScript, &autoload, "[autoload]");
    assert_eq!(backing.resolution(), EdgeResolution::Exact);
    assert_eq!(
        backing.exact_target(),
        Some(&GraphNodeId::script("scripts/game_state.gd"))
    );

    // A call through the singleton name resolves into that singleton's script
    // only — and stays inferred, because dispatch is a runtime fact.
    let register_hit = GraphNodeId::function("scripts/game_state.gd", None, "register_hit");
    let through_autoload = graph
        .callers(&register_hit)
        .into_iter()
        .find(|edge| edge.detail() == "GameState.register_hit()")
        .expect("the autoload call is recorded");
    assert_eq!(through_autoload.resolution(), EdgeResolution::Inferred);
    assert_eq!(
        through_autoload.candidates().len(),
        1,
        "only the singleton's own script declares it"
    );
    assert_eq!(
        through_autoload.provenance().source(),
        ProvenanceSource::DynamicReference,
        "an autoload member access is a dynamic reference"
    );
    assert_eq!(
        through_autoload.provenance().confidence(),
        Confidence::Inferred,
        "one visible candidate is never promoted to exact"
    );
    assert!(
        through_autoload.exact_target().is_none(),
        "an inferred edge has no single proven target"
    );

    // -- 4. signal declaration and its connection to a handler ---------------
    let player_script = GraphNodeId::script("scripts/player.gd");
    let health_changed = GraphNodeId::signal("scripts/player.gd", None, "health_changed");
    let declaration = edge(
        &graph,
        EdgeKind::SignalDeclaration,
        &player_script,
        "signal health_changed",
    );
    assert_eq!(declaration.resolution(), EdgeResolution::Exact);
    assert_eq!(declaration.exact_target(), Some(&health_changed));

    let handler = GraphNodeId::function("scripts/hud.gd", None, "_on_player_health_changed");
    let connection = graph
        .references(&handler)
        .into_iter()
        .find(|edge| edge.kind() == EdgeKind::SignalConnection)
        .expect("the connection reaches the handler");
    assert_eq!(connection.resolution(), EdgeResolution::Exact);
    assert_eq!(connection.exact_target(), Some(&handler));
    assert!(connection.detail().contains("health_changed"));
    assert_eq!(connection.file(), "main.tscn");

    // -- 5. scene instance and resource references --------------------------
    let hud_node = GraphNodeId::scene_node("main.tscn", "Hud");
    let instance = edge(&graph, EdgeKind::SceneInstance, &hud_node, "instance=");
    assert_eq!(instance.resolution(), EdgeResolution::Exact);
    assert_eq!(
        instance.exact_target(),
        Some(&GraphNodeId::scene("scenes/hud.tscn"))
    );

    // A `uid://`-only reference resolves once the declaring document is indexed:
    // this is the first layer able to connect a uid to a file.
    let enemy_scene = GraphNodeId::scene("scenes/enemy.tscn");
    let by_uid = graph
        .references(&enemy_scene)
        .into_iter()
        .find(|edge| edge.detail().contains("spawn_scene"))
        .expect("the uid-only property reference is recorded");
    assert_eq!(by_uid.resolution(), EdgeResolution::Exact);
    assert_eq!(by_uid.exact_target(), Some(&enemy_scene));

    // A preload of a text resource is exact.
    let buff = GraphNodeId::text_resource("resources/buff.tres");
    let preload = edge(
        &graph,
        EdgeKind::Preloads,
        &player_script,
        "preload(\"res://resources/buff.tres\")",
    );
    assert_eq!(preload.resolution(), EdgeResolution::Exact);
    assert_eq!(preload.exact_target(), Some(&buff));

    // The .tres body binds its own script, closing the resource -> script leg.
    let buff_script = edge(&graph, EdgeKind::ScriptAttachment, &buff, "script =");
    assert_eq!(buff_script.resolution(), EdgeResolution::Exact);
    assert_eq!(
        buff_script.exact_target(),
        Some(&GraphNodeId::script("scripts/base_actor.gd"))
    );

    // -- 6. references and impact --------------------------------------------
    let hud_script = GraphNodeId::script("scripts/hud.gd");
    let hud_references = graph.references(&hud_script);
    assert!(
        hud_references
            .iter()
            .any(|edge| edge.kind() == EdgeKind::ScriptAttachment),
        "the hud script is attached by a scene node"
    );

    let impact = graph.impact(&GraphNodeId::script("scripts/base_actor.gd"));
    assert!(
        impact.contains(&player_script),
        "player.gd extends base_actor.gd, so it is impacted: {impact:?}"
    );
    assert!(
        impact.contains(&GraphNodeId::scene_node("main.tscn", "Player")),
        "the node attaching player.gd is transitively impacted: {impact:?}"
    );
    assert!(
        impact.contains(&project_node),
        "the boot scene chain reaches the project: {impact:?}"
    );
    assert!(
        !impact.contains(&GraphNodeId::script("scripts/base_actor.gd")),
        "impact excludes the subject itself"
    );
    let mut sorted = impact.clone();
    sorted.sort();
    assert_eq!(
        impact, sorted,
        "impact is returned in a deterministic order"
    );

    // -- 7. byte stability ---------------------------------------------------
    let first = graph.to_canonical_json().expect("render the graph");
    let second = graph.to_canonical_json().expect("render the graph again");
    assert_eq!(
        first, second,
        "repeated serialization of one graph is byte-identical"
    );

    let rebuilt = build_graph(&project, true);
    assert_eq!(
        rebuilt
            .to_canonical_json()
            .expect("render the rebuilt graph"),
        first,
        "two builds over the same bytes are byte-identical"
    );

    let golden = fs::read_to_string(golden_path()).expect("read the committed golden graph");
    assert_eq!(
        first, golden,
        "the derived graph matches the committed golden fixture byte for byte"
    );

    let round_tripped: SemanticGraph =
        serde_json::from_str(&first).expect("the golden graph deserializes");
    assert_eq!(
        round_tripped.to_canonical_json().expect("re-render"),
        first,
        "serialization is stable across a round trip"
    );

    // -- 8. the layer is read-only and never invokes Godot ------------------
    assert_eq!(
        snapshot(&fixture.root),
        before,
        "deriving the graph rewrote no byte of the project"
    );
    for module in ["mod.rs", "model.rs", "builder.rs"] {
        let source = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src")
                .join("graph")
                .join(module),
        )
        .expect("read the graph module source");
        for forbidden in [
            "std::process",
            "Command::new",
            "std::fs::write",
            "std::fs::read",
            "tree_sitter",
        ] {
            assert!(
                !source.contains(forbidden),
                "{module} must not reference {forbidden}"
            );
        }
    }
}

/// The failure paths: an ambiguous dynamic target keeps every candidate plus its
/// uncertainty, an unsupported relation stays unresolved rather than absent or
/// exact, and the dangling audit separates "absent" from "undecidable".
#[test]
#[allow(clippy::too_many_lines)]
fn semantic_graph_failure_paths() {
    let fixture = fixture();
    let before = snapshot(&fixture.root);
    let resolver = ProjectResolver::new(&fixture.state);
    let project = resolver
        .resolve(&fixture.root, GENERATION)
        .expect("resolve the fixture project");

    let graph = build_graph(&project, true);

    // -- 1. an ambiguous dynamic call keeps EVERY candidate ------------------
    let dynamic_script = GraphNodeId::script("scripts/dynamic.gd");
    let ambiguous = edge(
        &graph,
        EdgeKind::Calls,
        &GraphNodeId::function("scripts/dynamic.gd", None, "dispatch"),
        "target.take_damage()",
    );
    assert_eq!(
        ambiguous.resolution(),
        EdgeResolution::Inferred,
        "a qualified call is never exact"
    );
    assert!(
        ambiguous.exact_target().is_none(),
        "an inferred edge must not expose a single proven target"
    );
    let hints: Vec<&str> = ambiguous
        .candidates()
        .iter()
        .map(godot_mcp_analysis::graph::EdgeCandidate::hint)
        .collect();
    assert_eq!(
        hints.len(),
        3,
        "all three declarations of take_damage are candidates: {hints:?}"
    );
    for expected in [
        "scripts/base_actor.gd::take_damage",
        "scripts/player.gd::take_damage",
        "scripts/game_state.gd::take_damage",
    ] {
        assert!(
            hints.contains(&expected),
            "{expected} is missing from {hints:?}"
        );
    }
    for candidate in ambiguous.candidates() {
        assert_eq!(
            candidate.provenance().confidence(),
            Confidence::Inferred,
            "every candidate carries its own uncertainty"
        );
        assert_eq!(
            candidate.provenance().source(),
            ProvenanceSource::DynamicReference
        );
    }
    let ranks: Vec<u32> = ambiguous
        .candidates()
        .iter()
        .map(godot_mcp_analysis::graph::EdgeCandidate::rank)
        .collect();
    let mut sorted_ranks = ranks.clone();
    sorted_ranks.sort_unstable();
    assert_eq!(ranks, sorted_ranks, "candidates are returned ranked");

    // A *single*-candidate dynamic call is still inferred: one visible target is
    // not a proof that no other exists at run time.
    let unique = edge(
        &graph,
        EdgeKind::Calls,
        &GraphNodeId::function("scripts/dynamic.gd", None, "dispatch"),
        "target.only_here()",
    );
    assert_eq!(unique.candidates().len(), 1);
    assert_eq!(
        unique.resolution(),
        EdgeResolution::Inferred,
        "a lone candidate is never collapsed into an exact target"
    );
    assert!(unique.exact_target().is_none());

    // A callee nothing declares is unresolved, not silently dropped.
    let engine_call = edge(
        &graph,
        EdgeKind::Calls,
        &GraphNodeId::function("scripts/dynamic.gd", None, "dispatch"),
        "queue_redraw()",
    );
    assert_eq!(engine_call.resolution(), EdgeResolution::Unresolved);
    assert_eq!(
        engine_call.provenance().source(),
        ProvenanceSource::Unresolved
    );
    assert_eq!(
        engine_call.provenance().confidence(),
        Confidence::Unresolved
    );

    // -- 2. an unsupported sub-resource relation is unresolved, not absent ----
    let body_node = GraphNodeId::scene_node("main.tscn", "Player/Body");
    let sub_relation = edge(
        &graph,
        EdgeKind::SubResourceRelation,
        &body_node,
        "shape = SubResource",
    );
    assert_eq!(
        sub_relation.resolution(),
        EdgeResolution::Unresolved,
        "sub-resource semantics are not modelled, so the relation is unresolved"
    );
    assert_eq!(
        sub_relation.provenance().confidence(),
        Confidence::Unresolved,
        "and never labeled exact"
    );
    assert!(
        sub_relation
            .candidates()
            .iter()
            .any(|candidate| candidate.target()
                == Some(&GraphNodeId::sub_resource(
                    "main.tscn",
                    "CircleShape2D_body"
                ))),
        "the declaration it points at stays queryable"
    );
    assert!(
        graph.has_limitation(LimitationKind::SubResourceSemanticsUnsupported),
        "the limitation is disclosed rather than hidden"
    );

    // -- 3. dangling audit separates absent from undecidable -----------------
    let absent = graph.dangling_of(DanglingKind::TargetAbsent);
    assert!(
        absent.iter().any(|entry| entry.hint() == "art/missing.png"),
        "a named file that provably is not in the project is absent: {:?}",
        absent.iter().map(|entry| entry.hint()).collect::<Vec<_>>()
    );
    assert!(
        absent
            .iter()
            .all(|entry| entry.edge_kind() != EdgeKind::SignalConnection),
        "a handler is never claimed absent: the base chain always ends at an \
         engine class this layer cannot inspect"
    );

    let undecidable = graph.dangling_of(DanglingKind::TargetUndecidable);
    assert!(
        undecidable
            .iter()
            .any(|entry| entry.hint() == "uid://cneverdeclared1"),
        "an unindexed uid:// cannot be decided statically: {:?}",
        undecidable
            .iter()
            .map(|entry| entry.hint())
            .collect::<Vec<_>>()
    );
    assert!(
        undecidable
            .iter()
            .any(|entry| entry.hint() == "res://../outside/escape.tres"),
        "a path escaping the project root cannot be decided"
    );
    assert!(
        undecidable
            .iter()
            .any(|entry| entry.edge_kind() == EdgeKind::SignalConnection
                && entry.hint() == "_on_missing_pressed"),
        "a handler nothing in the indexed chain declares stays undecidable"
    );
    assert!(
        undecidable
            .iter()
            .any(|entry| entry.edge_kind() == EdgeKind::Inherits && entry.hint() == "Node"),
        "an engine base type is undecidable, not absent"
    );
    assert!(
        !absent.iter().any(|entry| entry.hint() == "Node"),
        "an engine base type must never be reported as a missing file"
    );

    // The undecidable uid edge is still present in the graph.
    let hud_node = GraphNodeId::scene_node("scenes/hud.tscn", "Overlay");
    let unresolved_uid = edge(&graph, EdgeKind::ScriptAttachment, &hud_node, "script =");
    assert_eq!(unresolved_uid.resolution(), EdgeResolution::Unresolved);
    assert_eq!(
        unresolved_uid.candidates().len(),
        1,
        "the hint survives even with no mapped node"
    );
    assert!(unresolved_uid.candidates()[0].target().is_none());
    assert!(graph.has_limitation(LimitationKind::UidIndexIncomplete));

    // -- 4. without an inventory, absence cannot be claimed ------------------
    let unvouched = build_graph(&project, false);
    assert!(
        unvouched
            .dangling_of(DanglingKind::TargetAbsent)
            .iter()
            .all(|entry| entry.hint() != "art/missing.png"),
        "with no file inventory, a missing file is undecidable rather than absent"
    );
    assert!(
        unvouched
            .dangling_of(DanglingKind::TargetUndecidable)
            .iter()
            .any(|entry| entry.hint() == "art/missing.png"),
        "and the fact is still reported"
    );

    // -- 5. grammar limitations are never hidden -----------------------------
    assert!(
        graph.has_limitation(LimitationKind::DynamicResourcePathNotRecorded),
        "a computed load() path is disclosed as a missing edge"
    );
    assert!(
        graph.has_limitation(LimitationKind::CallArgumentsNotRecorded),
        "string-keyed relationships are disclosed as unavailable"
    );
    assert!(
        graph.has_limitation(LimitationKind::EngineClassRegistryUnavailable),
        "the absent engine class registry is disclosed"
    );
    let dynamic_limitation = graph
        .limitations()
        .iter()
        .find(|limitation| limitation.kind() == LimitationKind::DynamicResourcePathNotRecorded)
        .expect("the dynamic-path limitation is present");
    assert!(
        dynamic_limitation
            .subjects()
            .iter()
            .any(|subject| subject.starts_with("scripts/dynamic.gd:")),
        "the limitation names the exact site: {:?}",
        dynamic_limitation.subjects()
    );
    assert!(
        !dynamic_limitation.message().is_empty(),
        "each limitation explains itself"
    );

    // -- 6. a damaged document downgrades the graph, never fabricates --------
    let broken = read_document(&project, Path::new("scenes/broken.tscn"), GENERATION)
        .expect("a damaged document still parses");
    assert!(broken.is_partial(), "the fixture really is damaged");
    let degraded = SemanticGraphBuilder::new(GENERATION)
        .with_settings(project.settings().clone())
        .with_document(broken)
        .build()
        .expect("a damaged document still yields a graph");
    assert_eq!(
        degraded.provenance().confidence(),
        Confidence::Inferred,
        "a partial input can never produce an exact graph"
    );
    assert!(
        degraded.has_limitation(LimitationKind::DamagedRegionNotTraversed),
        "the damage is disclosed"
    );
    assert!(
        degraded
            .edges()
            .iter()
            .all(|edge| edge.resolution() != EdgeResolution::Exact
                || edge.file() != "scenes/broken.tscn"
                || edge.kind() != EdgeKind::SceneParent),
        "no relationship inside the damaged region is claimed exact"
    );

    // A build with nothing supplied is empty rather than invented.
    let empty = SemanticGraphBuilder::new(GENERATION)
        .build()
        .expect("an empty build succeeds");
    assert!(empty.nodes().is_empty());
    assert!(empty.edges().is_empty());
    assert!(empty.dangling().is_empty());

    // -- 7. an empty source path is rejected, not guessed at -----------------
    let mut parser = GdScriptParser::new().expect("load the GDScript grammar");
    let nameless = parser
        .parse("placeholder.gd", "extends Node\n", GENERATION)
        .expect("parse a trivial script");
    let round_trip = serde_json::to_string(&nameless).expect("serialize the script record");
    let blanked = round_trip.replacen("\"placeholder.gd\"", "\"   \"", 1);
    let blank_path: godot_mcp_analysis::gdscript::GdScriptFile =
        serde_json::from_str(&blanked).expect("a blank path deserializes as a record");
    let rejected = SemanticGraphBuilder::new(GENERATION)
        .with_script(blank_path)
        .build()
        .expect_err("a record with no source path is rejected");
    assert_eq!(rejected.kind(), godot_mcp_domain::ErrorKind::InvalidInput);

    // -- 8. still read-only --------------------------------------------------
    assert_eq!(
        snapshot(&fixture.root),
        before,
        "the failure paths rewrote no byte of the project"
    );
    assert!(
        !dynamic_script.as_str().is_empty(),
        "graph identities are non-empty spellings"
    );
}
