//! Todo 9 acceptance tests: text scene/resource parsing and the structural
//! relationship model.
//!
//! Exactly two test cases, matching the `qa/tasks/9.json` descriptor:
//! `scene_resource_model_happy_path` and `scene_resource_model_failure_paths`.
//!
//! Both cases run against a *copy* of the committed fixture project, so the
//! repository tree is never mutated and the read-only guarantee can be asserted
//! by comparing fixture bytes before and after every parse.
//!
//! The reads flow through Todo 8's [`ResolvedProject`], which is built on Todo
//! 2's held-object ownership layer (Unix object identity on this host); the
//! tests therefore assert the real behaviour on Unix and are compiled out
//! elsewhere rather than silently weakened.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use godot_mcp_analysis::project::{ProjectResolver, ResolvedProject, ResourceTarget};
use godot_mcp_analysis::scene::{
    DocumentKind, PropertyValue, ResourceRefKind, SceneDiagnosticKind, SceneError, parse_document,
    read_document,
};
use godot_mcp_domain::contracts::ProvenanceSource;
use godot_mcp_domain::{Confidence, ErrorKind};

/// Generation every fixture read is attributed to.
const GENERATION: u64 = 9;

/// Absolute path of the committed fixture project.
fn fixture_project() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("scene")
        .join("sample")
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

/// Every regular file under `dir`, relative path → bytes. Used to prove the
/// parser never rewrites a byte of what it reads.
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

fn open(fx: &Fixture) -> ResolvedProject {
    ProjectResolver::new(&fx.state)
        .resolve(&fx.root, GENERATION)
        .expect("the fixture project resolves")
}

// ===========================================================================
// Descriptor-owned happy case
// ===========================================================================

#[test]
fn scene_resource_model_happy_path() {
    let fx = fixture();
    let before = snapshot(&fx.root);
    let project = open(&fx);

    // -- (1) the scene's own header -----------------------------------------
    let scene = read_document(&project, Path::new("main.tscn"), GENERATION)
        .expect("the committed scene parses");
    assert_eq!(scene.kind(), DocumentKind::Scene);
    assert_eq!(scene.source_path(), "main.tscn");
    assert_eq!(scene.format_version(), Some(3));
    assert_eq!(scene.self_uid(), Some("uid://cscenefixture001"));
    assert!(
        scene.diagnostics().is_empty(),
        "a well-formed scene has no diagnostics: {:?}",
        scene.diagnostics()
    );
    assert!(!scene.is_partial());
    assert_eq!(
        scene.provenance().source(),
        ProvenanceSource::ProjectMetadata
    );
    assert_eq!(scene.provenance().confidence(), Confidence::Exact);
    assert_eq!(scene.provenance().project_generation(), GENERATION);

    // -- (2) external resources, with spans and honest targets --------------
    let ext = scene.ext_resources();
    assert_eq!(
        ext.iter().map(|e| e.id()).collect::<Vec<_>>(),
        vec![
            Some("1_player"),
            Some("2_hud"),
            Some("3_icon"),
            Some("4_escape"),
        ]
    );
    assert_eq!(ext[0].type_name(), Some("Script"));
    assert_eq!(ext[0].uid(), Some("uid://cplayerscript01"));
    assert_eq!(
        ext[0].target(),
        &ResourceTarget::ProjectRelative {
            res_uri: "res://scripts/player.gd".to_owned(),
            path: "scripts/player.gd".to_owned(),
        }
    );
    assert_eq!(ext[0].span().start_line(), 3);
    assert_eq!(ext[0].span().end_line(), 3);
    assert_eq!(ext[0].span().start_column(), 1);
    assert_eq!(
        ext[0].provenance().source(),
        ProvenanceSource::ProjectMetadata
    );
    assert_eq!(ext[0].provenance().confidence(), Confidence::Exact);
    assert_eq!(ext[1].type_name(), Some("PackedScene"));
    assert_eq!(
        ext[1].target().project_relative_path(),
        Some("scenes/hud.tscn")
    );
    assert_eq!(ext[1].span().start_line(), 4);
    assert_eq!(ext[2].uid(), None, "a path-only ext_resource has no uid");
    assert_eq!(
        ext[2].target().project_relative_path(),
        Some("art/icon.png")
    );
    // A `res://` path escaping the project root is never mapped in-root: the
    // frozen Todo 8 classification is reused verbatim.
    assert!(
        matches!(ext[3].target(), ResourceTarget::Unsupported(_)),
        "an escaping res:// path must stay unsupported: {:?}",
        ext[3].target()
    );
    assert_eq!(ext[3].provenance().source(), ProvenanceSource::Unresolved);
    assert_eq!(ext[3].provenance().confidence(), Confidence::Unresolved);
    assert_eq!(ext[3].span().start_line(), 6);

    // -- (3) internal (sub) resources, including a nested ext handle --------
    let subs = scene.sub_resources();
    assert_eq!(
        subs.iter().map(|s| s.id()).collect::<Vec<_>>(),
        vec![Some("CircleShape2D_body"), Some("StandardMaterial3D_skin")]
    );
    assert_eq!(subs[0].type_name(), Some("CircleShape2D"));
    assert_eq!(subs[0].span().start_line(), 8);
    let radius = subs[0]
        .properties()
        .iter()
        .find(|p| p.key() == "radius")
        .expect("the sub-resource keeps its properties");
    assert_eq!(radius.raw_value(), "12.0");
    assert_eq!(radius.span().start_line(), 9);
    assert_eq!(radius.value(), &PropertyValue::RetainedUnknown);
    let albedo = subs[1]
        .properties()
        .iter()
        .find(|p| p.key() == "albedo_texture")
        .expect("a sub-resource may reference an ext_resource");
    let PropertyValue::Resource(albedo_ref) = albedo.value() else {
        panic!("albedo_texture must model its ExtResource handle: {albedo:?}");
    };
    assert_eq!(albedo_ref.kind(), ResourceRefKind::ExtResource);
    assert_eq!(albedo_ref.id(), "3_icon");
    assert_eq!(
        albedo_ref
            .target()
            .and_then(ResourceTarget::project_relative_path),
        Some("art/icon.png")
    );
    assert_eq!(
        albedo_ref.provenance().source(),
        ProvenanceSource::StaticIndex
    );
    assert_eq!(albedo_ref.provenance().confidence(), Confidence::Exact);
    assert_eq!(albedo.span().start_line(), 12);

    // -- (4) the scene tree: parents, ownership and instance relationships --
    let paths: Vec<&str> = scene.nodes().iter().filter_map(|n| n.node_path()).collect();
    assert_eq!(
        paths,
        vec![
            ".",
            "Player",
            "Player/Body",
            "Player/Body/Skin",
            "Hud",
            "Ghost",
            "Hud/Overlay",
        ]
    );
    let root = scene.root().expect("the scene has exactly one root");
    assert_eq!(root.name(), Some("Main"));
    assert_eq!(root.type_name(), Some("Node2D"));
    assert_eq!(root.parent_path(), None);
    assert!(root.is_root());
    assert_eq!(root.span().start_line(), 14);
    assert_eq!(
        scene
            .children_of(".")
            .iter()
            .filter_map(|n| n.name())
            .collect::<Vec<_>>(),
        vec!["Player", "Hud", "Ghost"]
    );
    assert_eq!(
        scene
            .children_of("Player")
            .iter()
            .filter_map(|n| n.name())
            .collect::<Vec<_>>(),
        vec!["Body"]
    );
    let skin = scene
        .node_at("Player/Body/Skin")
        .expect("a grandchild is addressable by its absolute in-scene path");
    assert_eq!(skin.parent_path(), Some("Player/Body"));
    assert_eq!(skin.span().start_line(), 23);
    assert_eq!(skin.provenance().confidence(), Confidence::Exact);
    let follow = skin
        .properties()
        .iter()
        .find(|p| p.key() == "follow_node")
        .expect("a NodePath property is modelled");
    assert_eq!(
        follow.value(),
        &PropertyValue::NodePath("../..".to_owned()),
        "a NodePath value is recognized, not just retained"
    );
    assert_eq!(follow.span().start_line(), 25);

    // Ownership-relevant attributes are retained verbatim.
    let overlay = scene.node_at("Hud/Overlay").expect("owner-bearing node");
    assert_eq!(overlay.owner_path(), Some("."));
    assert_eq!(overlay.index(), Some("1"));
    assert_eq!(overlay.span().start_line(), 32);

    // -- (5) script attachment ----------------------------------------------
    let player = scene.node_at("Player").expect("the player node");
    let script = player.script().expect("the player has a script attached");
    assert_eq!(script.kind(), ResourceRefKind::ExtResource);
    assert_eq!(script.id(), "1_player");
    assert_eq!(
        script
            .target()
            .and_then(ResourceTarget::project_relative_path),
        Some("scripts/player.gd")
    );
    assert_eq!(script.span().start_line(), 17);
    assert_eq!(script.provenance().confidence(), Confidence::Exact);
    assert_eq!(
        scene
            .script_attachments()
            .iter()
            .map(|(node, reference)| (
                node.node_path().unwrap_or_default(),
                reference.id(),
                reference.span().start_line()
            ))
            .collect::<Vec<_>>(),
        vec![("Player", "1_player", 17)]
    );

    // -- (6) group membership ------------------------------------------------
    assert_eq!(
        player
            .groups()
            .iter()
            .map(|g| (g.name(), g.span().start_line()))
            .collect::<Vec<_>>(),
        vec![("actors", 18), ("damageable", 18)]
    );
    assert_eq!(
        player.groups()[0].provenance().confidence(),
        Confidence::Exact
    );
    assert_eq!(
        scene
            .group_memberships()
            .iter()
            .map(|(node, group)| (node.node_path().unwrap_or_default(), group.name()))
            .collect::<Vec<_>>(),
        vec![("Player", "actors"), ("Player", "damageable")]
    );

    // -- (7) sub-resource use from a node ------------------------------------
    let body = scene.node_at("Player/Body").expect("the body node");
    let shape = body
        .properties()
        .iter()
        .find(|p| p.key() == "shape")
        .expect("the collision shape property");
    let PropertyValue::Resource(shape_ref) = shape.value() else {
        panic!("shape must model its SubResource handle: {shape:?}");
    };
    assert_eq!(shape_ref.kind(), ResourceRefKind::SubResource);
    assert_eq!(shape_ref.id(), "CircleShape2D_body");
    assert_eq!(
        shape_ref.target(),
        None,
        "an internal resource is in-file, so it names no project path"
    );
    assert_eq!(
        shape_ref.provenance().source(),
        ProvenanceSource::StaticIndex
    );
    assert_eq!(shape_ref.provenance().confidence(), Confidence::Exact);
    assert_eq!(shape_ref.span().start_line(), 21);

    // -- (8) scene instances (the nested scene) ------------------------------
    let hud = scene.node_at("Hud").expect("the instanced node");
    assert_eq!(
        hud.type_name(),
        None,
        "an instanced node inherits its type from the instanced scene"
    );
    let instance = hud.instance().expect("the Hud node instances a scene");
    assert_eq!(instance.id(), "2_hud");
    assert_eq!(
        instance
            .target()
            .and_then(ResourceTarget::project_relative_path),
        Some("scenes/hud.tscn")
    );
    assert_eq!(instance.span().start_line(), 27);
    assert_eq!(
        scene
            .scene_instances()
            .iter()
            .map(|(node, reference)| (
                node.node_path().unwrap_or_default(),
                reference
                    .target()
                    .and_then(ResourceTarget::project_relative_path)
                    .unwrap_or_default()
            ))
            .collect::<Vec<_>>(),
        vec![("Hud", "scenes/hud.tscn")]
    );
    let ghost = scene.node_at("Ghost").expect("the placeholder node");
    assert_eq!(
        ghost.instance_placeholder(),
        Some(&ResourceTarget::ProjectRelative {
            res_uri: "res://scenes/hud.tscn".to_owned(),
            path: "scenes/hud.tscn".to_owned(),
        }),
        "an instance_placeholder names the deferred scene"
    );
    assert!(
        ghost.instance().is_none(),
        "a placeholder is not a loaded instance"
    );

    // -- (9) signal connections ---------------------------------------------
    let connections = scene.connections();
    assert_eq!(connections.len(), 2);
    assert_eq!(connections[0].signal(), Some("health_changed"));
    assert_eq!(connections[0].from_path(), Some("Player"));
    assert_eq!(connections[0].to_path(), Some("Hud"));
    assert_eq!(connections[0].method(), Some("_on_player_health_changed"));
    assert!(connections[0].from_resolved() && connections[0].to_resolved());
    assert_eq!(connections[0].span().start_line(), 34);
    assert_eq!(
        connections[0].provenance().source(),
        ProvenanceSource::ProjectMetadata
    );
    assert_eq!(connections[0].provenance().confidence(), Confidence::Exact);
    assert_eq!(connections[1].from_path(), Some("Hud/Overlay"));
    assert_eq!(
        connections[1].to_path(),
        Some("."),
        "`.` is the scene root marker and must resolve to the root node"
    );
    assert!(connections[1].to_resolved());
    assert_eq!(connections[1].method(), Some("_on_overlay_pressed"));
    assert_eq!(
        connections[1]
            .extra_attributes()
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect::<Vec<_>>(),
        vec![("binds", "[3]")],
        "an attribute this layer assigns no meaning to is still retained"
    );

    // -- (10) the nested scene, and the uid relationship between them -------
    let nested = read_document(&project, Path::new("scenes/hud.tscn"), GENERATION)
        .expect("the nested scene parses");
    assert_eq!(nested.self_uid(), Some("uid://chudscene00001"));
    assert_eq!(
        nested.self_uid(),
        ext[1].uid(),
        "the parent's ext_resource uid names the nested scene's own uid"
    );
    assert_eq!(
        nested
            .nodes()
            .iter()
            .filter_map(|n| n.node_path())
            .collect::<Vec<_>>(),
        vec![".", "Overlay"]
    );
    assert_eq!(
        nested.root().and_then(|n| n.type_name()),
        Some("CanvasLayer")
    );
    assert!(nested.diagnostics().is_empty());
    // Resolving a uid to a file is the Todo 11 index's job, so the uid handle
    // stays an unresolved target here rather than being guessed.
    let uid_target = ResourceTarget::Uid("uid://chudscene00001".to_owned());
    assert_eq!(uid_target.project_relative_path(), None);

    // -- (11) the text resource (.tres) -------------------------------------
    let resource = read_document(&project, Path::new("resources/buff.tres"), GENERATION)
        .expect("the committed resource parses");
    assert_eq!(resource.kind(), DocumentKind::Resource);
    assert_eq!(resource.root_type(), Some("Buff"));
    assert_eq!(resource.self_uid(), Some("uid://cbufffixture001"));
    assert_eq!(resource.format_version(), Some(3));
    assert!(resource.nodes().is_empty(), "a .tres has no scene tree");
    assert!(resource.diagnostics().is_empty());
    assert_eq!(resource.provenance().confidence(), Confidence::Exact);
    assert_eq!(
        resource
            .ext_resources()
            .iter()
            .map(|e| (
                e.id().unwrap_or_default(),
                e.target().project_relative_path().unwrap_or_default(),
                e.span().start_line()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("1_buff", "scripts/buff.gd", 4),
            ("2_effect", "resources/effect.tres", 5),
        ]
    );
    assert_eq!(
        resource.ext_resources()[1].uid(),
        Some("uid://ceffectfixture1")
    );
    let body = resource
        .resource_body()
        .expect("the [resource] section is modelled");
    assert_eq!(body.span().start_line(), 10);
    let body_script = body.script().expect("the resource is backed by a script");
    assert_eq!(
        body_script
            .target()
            .and_then(ResourceTarget::project_relative_path),
        Some("scripts/buff.gd")
    );
    assert_eq!(body_script.span().start_line(), 11);
    let effect = body
        .properties()
        .iter()
        .find(|p| p.key() == "effect")
        .expect("a resource→resource property");
    let PropertyValue::Resource(effect_ref) = effect.value() else {
        panic!("effect must model its ExtResource handle: {effect:?}");
    };
    assert_eq!(
        effect_ref
            .target()
            .and_then(ResourceTarget::project_relative_path),
        Some("resources/effect.tres")
    );
    assert_eq!(effect.span().start_line(), 12);
    let ramp = body
        .properties()
        .iter()
        .find(|p| p.key() == "ramp")
        .expect("a resource→sub-resource property");
    let PropertyValue::Resource(ramp_ref) = ramp.value() else {
        panic!("ramp must model its SubResource handle: {ramp:?}");
    };
    assert_eq!(ramp_ref.kind(), ResourceRefKind::SubResource);
    assert_eq!(ramp_ref.id(), "Curve_ramp");
    assert_eq!(
        body.properties()
            .iter()
            .find(|p| p.key() == "label")
            .expect("a plain scalar property is retained")
            .raw_value(),
        "\"Regeneration\""
    );
    assert_eq!(
        resource.sub_resources()[0].properties()[0].key(),
        "_data",
        "a bracketed multi-line value stays one property"
    );
    // Every retained record points at a 1-based source line.
    assert!(
        resource
            .resource_references()
            .iter()
            .all(|r| r.span().start_line() > 0)
    );

    // -- (12) the parse is read-only ----------------------------------------
    assert_eq!(
        snapshot(&fx.root),
        before,
        "parsing must never rewrite a byte of the project"
    );

    // -- (13) the parser never invokes Godot --------------------------------
    let module = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("scene");
    let mut sources = Vec::new();
    for entry in fs::read_dir(&module).expect("scene module directory") {
        let path = entry.expect("module entry").path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            sources.push(fs::read_to_string(&path).expect("module source"));
        }
    }
    assert!(!sources.is_empty(), "the scene module must have sources");
    for source in &sources {
        for forbidden in ["std::process", "Command::new", "std::fs::write"] {
            assert!(
                !source.contains(forbidden),
                "the scene parser must not reference {forbidden}"
            );
        }
    }
}

// ===========================================================================
// Descriptor-owned failure case
// ===========================================================================

#[test]
fn scene_resource_model_failure_paths() {
    let fx = fixture();
    let before = snapshot(&fx.root);
    let project = open(&fx);

    // --- (1) a malformed scene stays queryable, with exact locations --------
    let broken = read_document(&project, Path::new("broken.tscn"), GENERATION)
        .expect("a malformed scene still yields a document");
    assert!(broken.is_partial());
    assert_eq!(
        broken.provenance().source(),
        ProvenanceSource::Heuristic,
        "a partial parse is heuristic evidence"
    );
    assert_eq!(broken.provenance().confidence(), Confidence::Inferred);
    assert!(
        broken
            .diagnostics()
            .iter()
            .all(|d| d.span().start_line() > 0),
        "every diagnostic must point at a 1-based source line"
    );
    let at = |kind: SceneDiagnosticKind| -> Vec<u32> {
        broken
            .diagnostics()
            .iter()
            .filter(|d| d.kind() == kind)
            .map(|d| d.span().start_line())
            .collect()
    };
    // Each defect is separately classified, never lumped into one bucket.
    assert_eq!(
        at(SceneDiagnosticKind::MalformedSectionHeader),
        vec![1],
        "the unterminated file descriptor is reported at its own line: {:?}",
        broken.diagnostics()
    );
    assert_eq!(
        broken.self_uid(),
        None,
        "a malformed header supplies no uid"
    );
    assert!(
        at(SceneDiagnosticKind::MissingRequiredAttribute).contains(&3),
        "an ext_resource without an id is reported: {:?}",
        broken.diagnostics()
    );
    assert!(
        at(SceneDiagnosticKind::MissingRequiredAttribute).contains(&5),
        "a node without a name is reported"
    );
    assert!(
        at(SceneDiagnosticKind::MissingRequiredAttribute).contains(&12),
        "a sub_resource without a type is reported"
    );
    assert!(
        at(SceneDiagnosticKind::MissingRequiredAttribute).contains(&23),
        "a connection without a method is reported"
    );
    assert_eq!(at(SceneDiagnosticKind::MissingSceneRoot), vec![1]);
    assert_eq!(at(SceneDiagnosticKind::MissingParentNode), vec![7]);
    assert_eq!(at(SceneDiagnosticKind::UnknownResourceId), vec![8, 9]);
    assert_eq!(at(SceneDiagnosticKind::UnterminatedValue), vec![10]);
    assert_eq!(at(SceneDiagnosticKind::UnknownSection), vec![14]);
    assert_eq!(at(SceneDiagnosticKind::EmptyKey), vec![18]);
    assert_eq!(at(SceneDiagnosticKind::MissingAssignment), vec![19]);
    assert_eq!(at(SceneDiagnosticKind::DuplicateNodePath), vec![21]);
    assert_eq!(
        at(SceneDiagnosticKind::DanglingConnectionEndpoint),
        vec![23]
    );
    // Stable wire codes for the classifications above.
    assert_eq!(
        SceneDiagnosticKind::UnknownResourceId.code(),
        "unknown_resource_id"
    );
    assert_eq!(
        SceneDiagnosticKind::MalformedSectionHeader.code(),
        "malformed_section_header"
    );

    // The damaged file is still a queryable model, not a dropped result.
    let unnamed = broken
        .nodes()
        .iter()
        .find(|n| n.name().is_none())
        .expect("an unnamed node is retained rather than discarded");
    assert_eq!(
        unnamed.node_path(),
        None,
        "an unnamed node is unaddressable"
    );
    assert_eq!(unnamed.span().start_line(), 5);
    assert_eq!(unnamed.provenance().confidence(), Confidence::Inferred);
    let orphan = broken
        .node_at("Missing/Ancestor/Orphan")
        .expect("a node whose parent is absent is still addressable by its path");
    assert_eq!(orphan.parent_path(), Some("Missing/Ancestor"));
    assert_eq!(
        orphan.provenance().source(),
        ProvenanceSource::Heuristic,
        "a node inside a damaged region may not claim exactness"
    );
    assert_eq!(orphan.provenance().confidence(), Confidence::Inferred);

    // An unknown resource id is queryable WITH its uncertainty and location.
    let script = orphan.script().expect("the script handle is retained");
    assert_eq!(script.id(), "9_absent");
    assert_eq!(
        script.target(),
        None,
        "an unknown id must not be resolved to a guessed path"
    );
    assert_eq!(script.provenance().source(), ProvenanceSource::Unresolved);
    assert_eq!(script.provenance().confidence(), Confidence::Unresolved);
    assert_eq!(script.span().start_line(), 8);
    let dangling_sub = orphan
        .properties()
        .iter()
        .find(|p| p.key() == "shape")
        .expect("the dangling sub-resource property is retained");
    let PropertyValue::Resource(dangling_ref) = dangling_sub.value() else {
        panic!("an unknown SubResource handle is still modelled: {dangling_sub:?}");
    };
    assert_eq!(dangling_ref.kind(), ResourceRefKind::SubResource);
    assert_eq!(dangling_ref.id(), "Absent_sub");
    assert_eq!(dangling_ref.target(), None);
    assert_eq!(
        dangling_ref.provenance().confidence(),
        Confidence::Unresolved
    );
    assert_eq!(dangling_sub.span().start_line(), 9);
    // An ext_resource that lost its id keeps its path and its exact location.
    let anonymous = broken
        .ext_resources()
        .iter()
        .find(|e| e.id().is_none())
        .expect("an id-less ext_resource is retained");
    assert_eq!(
        anonymous.target().project_relative_path(),
        Some("scripts/player.gd")
    );
    assert_eq!(anonymous.span().start_line(), 3);
    // An unknown section from a newer engine build stays queryable verbatim.
    let unknown = broken
        .unknown_sections()
        .iter()
        .find(|s| s.name() == "future_section")
        .expect("an unknown section is retained, never silently dropped");
    assert_eq!(unknown.span().start_line(), 14);
    assert_eq!(
        unknown
            .attributes()
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect::<Vec<_>>(),
        vec![("thing", "value")]
    );
    assert_eq!(
        unknown
            .properties()
            .iter()
            .map(|p| (p.key(), p.raw_value(), p.span().start_line()))
            .collect::<Vec<_>>(),
        vec![("mystery", "1", 15)]
    );
    // The unterminated array swallowed no later section: recovery stops at the
    // next well-formed header rather than eating the rest of the file.
    assert!(
        broken
            .sub_resources()
            .iter()
            .any(|s| s.id() == Some("NoType_sub")),
        "recovery must reach the section after the unterminated value"
    );
    // A duplicate node path keeps both declarations queryable.
    assert_eq!(
        broken
            .nodes()
            .iter()
            .filter(|n| n.node_path() == Some("Dup"))
            .count(),
        2,
        "both duplicate declarations stay queryable"
    );
    assert_eq!(
        broken
            .node_at("Dup")
            .expect("first declaration wins")
            .span()
            .start_line(),
        17
    );

    // --- (2) binary formats are refused, never half-parsed -----------------
    fs::write(fx.root.join("packed.res"), b"RSRC\x00binary").expect("write a binary resource");
    for (relative, extension) in [("binary.scn", "scn"), ("packed.res", "res")] {
        let error = read_document(&project, Path::new(relative), GENERATION)
            .expect_err("a binary format must be refused");
        assert!(
            matches!(error, SceneError::UnsupportedBinaryFormat { .. }),
            "{relative} => unsupported binary format, got {error:?}"
        );
        assert_eq!(error.error_kind(), ErrorKind::Unsupported);
        assert!(
            error.to_string().contains(extension),
            "the refusal must name the offending format: {error}"
        );
    }
    // A binary payload wearing a text extension is refused by its magic, so a
    // `.scn` renamed to `.tscn` is never parsed into a fabricated model.
    let disguised = parse_document("disguised.tscn", "RSRC\u{0}not text at all", GENERATION)
        .expect_err("binary magic is refused regardless of extension");
    assert!(matches!(
        disguised,
        SceneError::UnsupportedBinaryFormat { .. }
    ));
    assert_eq!(disguised.error_kind(), ErrorKind::Unsupported);

    // --- (3) a non-text-resource extension is refused, not guessed ---------
    let wrong = read_document(&project, Path::new("scripts/player.gd"), GENERATION)
        .expect_err("a GDScript file is not a text scene or resource");
    assert!(matches!(wrong, SceneError::UnsupportedExtension { .. }));
    assert_eq!(wrong.error_kind(), ErrorKind::Unsupported);

    // --- (4) non-UTF-8 bytes are rejected rather than lossily reread --------
    fs::write(fx.root.join("invalid.tscn"), [0x5b, 0xff, 0xfe, 0x5d]).expect("write invalid UTF-8");
    let not_utf8 = read_document(&project, Path::new("invalid.tscn"), GENERATION)
        .expect_err("a non-UTF-8 scene is rejected");
    assert!(matches!(not_utf8, SceneError::NotUtf8 { .. }));
    assert_eq!(not_utf8.error_kind(), ErrorKind::InvalidInput);

    // --- (5) path authority stays Todo 8's: no second implementation -------
    for outside in ["../escape.tscn", "/etc/hostname", "scenes/../main.tscn"] {
        let denied = read_document(&project, Path::new(outside), GENERATION)
            .expect_err("an outside-root path must be refused");
        assert_eq!(
            denied.error_kind(),
            ErrorKind::PermissionDenied,
            "{outside} => permission_denied, got {denied:?}"
        );
    }
    fs::create_dir_all(fx.root.join(".godot")).expect("mkdir .godot");
    fs::write(
        fx.root.join(".godot").join("cached.tscn"),
        b"[gd_scene format=3]\n",
    )
    .expect("write an engine cache scene");
    let cache = read_document(&project, Path::new(".godot/cached.tscn"), GENERATION)
        .expect_err("the engine cache is not project source");
    assert_eq!(cache.error_kind(), ErrorKind::PermissionDenied);

    // --- (6) more than one scene root is a reported defect, not a silent pick
    let two_roots = parse_document(
        "two_roots.tscn",
        "[gd_scene format=3]\n\n[node name=\"A\" type=\"Node\"]\n\n[node name=\"B\" type=\"Node\"]\n",
        GENERATION,
    )
    .expect("a scene with two roots still parses");
    assert_eq!(
        two_roots
            .diagnostics()
            .iter()
            .filter(|d| d.kind() == SceneDiagnosticKind::MultipleSceneRoots)
            .map(|d| d.span().start_line())
            .collect::<Vec<_>>(),
        vec![5]
    );
    assert_eq!(
        two_roots.root().and_then(|n| n.name()),
        Some("A"),
        "the first root wins, matching the engine, and the second is reported"
    );
    assert_eq!(two_roots.provenance().confidence(), Confidence::Inferred);

    // --- (7) a duplicate resource id is reported; the first wins ------------
    let duplicated = parse_document(
        "dup.tres",
        concat!(
            "[gd_resource type=\"Buff\" format=3]\n\n",
            "[ext_resource type=\"Script\" path=\"res://a.gd\" id=\"1\"]\n",
            "[ext_resource type=\"Script\" path=\"res://b.gd\" id=\"1\"]\n\n",
            "[resource]\nscript = ExtResource(\"1\")\n",
        ),
        GENERATION,
    )
    .expect("a duplicated id still parses");
    assert_eq!(
        duplicated
            .diagnostics()
            .iter()
            .filter(|d| d.kind() == SceneDiagnosticKind::DuplicateResourceId)
            .map(|d| d.span().start_line())
            .collect::<Vec<_>>(),
        vec![4]
    );
    assert_eq!(
        duplicated.ext_resources().len(),
        2,
        "both declarations stay queryable"
    );
    assert_eq!(
        duplicated
            .resource_body()
            .and_then(|b| b.script())
            .and_then(|s| s.target())
            .and_then(ResourceTarget::project_relative_path),
        Some("a.gd"),
        "the first declaration wins"
    );

    // --- (8) an empty document is honest, not an invented scene ------------
    let empty = parse_document("empty.tscn", "", GENERATION).expect("an empty scene parses");
    assert!(empty.nodes().is_empty());
    assert!(empty.root().is_none());
    assert_eq!(
        empty
            .diagnostics()
            .iter()
            .map(|d| d.kind())
            .collect::<Vec<_>>(),
        vec![SceneDiagnosticKind::MissingSceneRoot]
    );

    // --- (9) every failure path left the fixture bytes untouched -----------
    let after = snapshot(&fx.root);
    for (path, bytes) in &before {
        assert_eq!(
            after.get(path),
            Some(bytes),
            "{} must not be rewritten by any parse",
            path.display()
        );
    }
}
