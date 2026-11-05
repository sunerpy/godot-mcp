//! The project-wide insight views over the committed `insight` fixture.
//!
//! The fixture is a small but complete Godot project: a uid-addressed main
//! scene, an autoload, an enabled and a disabled plugin, an export preset
//! include filter, instanced scenes with overrides, class_name inheritance,
//! signal wiring with one missing handler, an inheritance cycle, an instancing
//! cycle, a tolerated class-reference cycle, a dynamically loaded level, and
//! files nothing uses. Every view is asserted against facts read off those
//! files by hand.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use godot_mcp_analysis::insight::glob::GlobSet;
use godot_mcp_analysis::insight::metrics::{
    FunctionSort, SceneSort, Thresholds, function_complexity, scene_complexity, statistics,
};
use godot_mcp_analysis::insight::references::{
    CycleKind, ReferenceKind, RootReason, UnusedReason, dependency_cycles, unused_files,
};
use godot_mcp_analysis::insight::scenes::{
    NodeQuery, TypeMatch, ValueSource, find_nodes, scene_exports,
};
use godot_mcp_analysis::insight::search::{DEFAULT_MAX_MATCHES, Matcher, SearchQuery, search};
use godot_mcp_analysis::insight::signals::{HandlerStatus, ProblemKind, SignalQuery, signal_flow};
use godot_mcp_analysis::insight::uid::{UidSource, UidStatus};
use godot_mcp_analysis::insight::{FileKind, ProjectModel, ProjectSources};
use godot_mcp_analysis::project::ProjectSettings;
use godot_mcp_domain::Confidence;

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("insight")
        .join("project")
}

fn collect(base: &Path, directory: &Path, sources: &mut ProjectSources) {
    let mut entries: Vec<_> = std::fs::read_dir(directory)
        .expect("read fixture directory")
        .map(|entry| entry.expect("fixture entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect(base, &path, sources);
        } else {
            let relative = path
                .strip_prefix(base)
                .expect("relative")
                .to_string_lossy()
                .replace('\\', "/");
            sources.insert_bytes(relative, &std::fs::read(&path).expect("read fixture file"));
        }
    }
}

fn sources() -> ProjectSources {
    let mut sources = ProjectSources::new();
    let root = fixture_root();
    collect(&root, &root, &mut sources);
    sources
}

fn settings(sources: &ProjectSources) -> ProjectSettings {
    ProjectSettings::parse(sources.text("project.godot").expect("manifest"), 7).expect("settings")
}

#[test]
fn unused_files_are_the_loadable_files_no_root_reaches() {
    let sources = sources();
    let settings = settings(&sources);
    let model = ProjectModel::build(&sources, Some(&settings), 7);
    let report = unused_files(&model, &GlobSet::default());

    let unused: Vec<(&str, UnusedReason)> = report
        .unused
        .iter()
        .map(|entry| (entry.path.as_str(), entry.reason))
        .collect();
    assert_eq!(
        unused,
        vec![
            (
                "addons/disabled/plugin.gd",
                UnusedReason::ReferencedOnlyByUnused
            ),
            ("art/unused.png", UnusedReason::Unreferenced),
            ("levels/level_1.tscn", UnusedReason::Unreferenced),
            ("old/legacy.gd", UnusedReason::ReferencedOnlyByUnused),
            ("old/legacy.tscn", UnusedReason::Unreferenced),
            ("scenes/loop_a.tscn", UnusedReason::ReferencedOnlyByUnused),
            ("scenes/loop_b.tscn", UnusedReason::ReferencedOnlyByUnused),
            ("scripts/cycle_a.gd", UnusedReason::ReferencedOnlyByUnused),
            ("scripts/cycle_b.gd", UnusedReason::ReferencedOnlyByUnused),
            ("scripts/unused_helper.gd", UnusedReason::Unreferenced),
        ]
    );
    assert!(
        report
            .unused
            .iter()
            .all(|entry| entry.confidence == Confidence::Inferred)
    );

    let level = &report.unused[2];
    let prefixes: BTreeSet<&str> = level
        .possible_dynamic_loads
        .iter()
        .map(|site| site.prefix.as_str())
        .collect();
    assert_eq!(prefixes, BTreeSet::from(["res://levels/"]));
    assert_eq!(level.possible_dynamic_loads.len(), 2);
    assert_eq!(level.possible_dynamic_load_count, 2);
    assert_eq!(
        report.unused[1].sidecars,
        vec!["art/unused.png.import".to_owned()]
    );
    assert_eq!(
        report.unused[3].referenced_by,
        vec!["old/legacy.tscn".to_owned()]
    );

    let roots: BTreeSet<(&str, RootReason)> = report
        .roots
        .iter()
        .map(|root| (root.path.as_str(), root.reason))
        .collect();
    for expected in [
        ("main.tscn", RootReason::MainScene),
        ("scripts/game_state.gd", RootReason::Autoload),
        ("icon.svg", RootReason::ProjectSetting),
        ("resources/theme.tres", RootReason::ProjectSetting),
        ("addons/tools/plugin.cfg", RootReason::EnabledPlugin),
        ("default_bus_layout.tres", RootReason::DefaultBusLayout),
        ("data/levels.json", RootReason::ExportIncludeFilter),
    ] {
        assert!(
            roots.contains(&expected),
            "missing root {expected:?} in {roots:?}"
        );
    }
    assert_eq!(report.dynamic_loads.len(), 2);
    assert!(
        report.broken_references.is_empty(),
        "{:?}",
        report.broken_references
    );
    assert!(!report.coverage.complete);
    assert!(
        report
            .coverage
            .gaps
            .iter()
            .any(|gap| gap.code == "dynamic_loads")
    );

    let scoped = unused_files(
        &model,
        &GlobSet::new(["scripts/**"], Vec::<String>::new()).expect("globs"),
    );
    let scoped: Vec<&str> = scoped
        .unused
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    assert_eq!(
        scoped,
        vec![
            "scripts/cycle_a.gd",
            "scripts/cycle_b.gd",
            "scripts/unused_helper.gd"
        ]
    );
}

#[test]
fn cycles_are_found_and_classified() {
    let sources = sources();
    let settings = settings(&sources);
    let model = ProjectModel::build(&sources, Some(&settings), 7);
    let report = dependency_cycles(&model);
    let cycles: Vec<(Vec<&str>, CycleKind, bool)> = report
        .cycles
        .iter()
        .map(|cycle| {
            (
                cycle.members.iter().map(String::as_str).collect(),
                cycle.classification,
                cycle.breaks_loading,
            )
        })
        .collect();
    assert_eq!(
        cycles,
        vec![
            (
                vec!["scenes/loop_a.tscn", "scenes/loop_b.tscn"],
                CycleKind::ResourceLoad,
                true
            ),
            (
                vec!["scripts/cycle_a.gd", "scripts/cycle_b.gd"],
                CycleKind::Inheritance,
                true
            ),
            (
                vec!["scripts/enemy.gd", "scripts/player.gd"],
                CycleKind::Tolerated,
                false
            ),
        ]
    );
    let tolerated = &report.cycles[2];
    assert!(
        tolerated
            .witness
            .iter()
            .all(|edge| edge.kind == ReferenceKind::ClassReference)
    );
    assert_eq!(tolerated.witness.len(), 2);
    assert_eq!(
        report.cycles[0].witness[0].kind,
        ReferenceKind::SceneInstance
    );
}

#[test]
fn signal_flow_attributes_connections_and_reports_problems() {
    let sources = sources();
    let model = ProjectModel::build(&sources, None, 7);
    let report = signal_flow(&model, &SignalQuery::default());
    let declared: Vec<(&str, &str)> = report
        .signals
        .iter()
        .map(|signal| (signal.script.as_str(), signal.name.as_str()))
        .collect();
    assert_eq!(
        declared,
        vec![
            ("scripts/game_state.gd", "score_changed"),
            ("scripts/hud.gd", "start_requested"),
            ("scripts/player.gd", "health_changed"),
            ("scripts/player.gd", "died"),
        ]
    );
    let score = &report.signals[0];
    assert_eq!(score.code_connections.len(), 1);
    assert_eq!(score.code_connections[0].script, "scripts/hud.gd");
    assert_eq!(
        score.code_connections[0].receiver.as_deref(),
        Some("GameState")
    );
    assert_eq!(score.emissions.len(), 1);
    let health = &report.signals[2];
    assert_eq!(health.scene_connections.len(), 1);
    assert_eq!(health.scene_connections[0].handler, HandlerStatus::Found);
    assert_eq!(
        health.scene_connections[0].emitter_script.as_deref(),
        Some("scripts/player.gd")
    );
    let died = &report.signals[3];
    assert_eq!(died.scene_connections[0].handler, HandlerStatus::Missing);

    let problems: Vec<(ProblemKind, &str, &str, u32)> = report
        .problems
        .iter()
        .map(|problem| {
            (
                problem.kind,
                problem.signal.as_deref().unwrap_or_default(),
                problem.file.as_str(),
                problem.line,
            )
        })
        .collect();
    assert_eq!(
        problems,
        vec![
            (ProblemKind::MissingHandler, "died", "main.tscn", 28),
            (
                ProblemKind::EmittedNeverConnected,
                "start_requested",
                "scripts/hud.gd",
                3
            ),
            (
                ProblemKind::ConnectedNeverDeclared,
                "tree_exited",
                "main.tscn",
                29
            ),
        ]
    );
    let undeclared: Vec<&str> = report
        .undeclared_connections
        .iter()
        .filter_map(|connection| connection.signal.as_deref())
        .collect();
    assert_eq!(undeclared, vec!["tree_exited", "pressed"]);

    let filtered = signal_flow(
        &model,
        &SignalQuery {
            signal: Some("died".to_owned()),
            script: None,
        },
    );
    assert_eq!(filtered.signals.len(), 1);
    assert_eq!(filtered.problems.len(), 1);
}

#[test]
fn statistics_and_complexity_describe_the_project() {
    let sources = sources();
    let model = ProjectModel::build(&sources, None, 7);
    let stats = statistics(&model);
    assert_eq!(stats.files, 39);
    assert_eq!(stats.files_without_length, 0);
    assert_eq!(stats.scripts.files, 12);
    assert_eq!(stats.scripts.class_names, 3);
    assert_eq!(stats.scripts.signals, 4);
    assert_eq!(stats.scripts.tool_scripts, 2);
    assert_eq!(stats.scenes.scenes, 8);
    assert_eq!(stats.scenes.resources, 3);
    assert_eq!(stats.scenes.connections, 4);
    let kinds: BTreeSet<FileKind> = stats.by_kind.iter().map(|count| count.kind).collect();
    assert!(kinds.contains(&FileKind::UidSidecar) && kinds.contains(&FileKind::Shader));
    assert!(stats.coverage.complete, "{:?}", stats.coverage);

    let (functions, _) = function_complexity(
        &model,
        &GlobSet::default(),
        FunctionSort::Cyclomatic,
        Thresholds::default(),
    );
    let top = &functions[0];
    assert_eq!(
        (top.script.as_str(), top.name.as_str()),
        ("scripts/player.gd", "take_hit")
    );
    assert_eq!((top.cyclomatic, top.max_nesting, top.lines), (10, 3, 17));
    let (scenes, _) = scene_complexity(
        &model,
        &GlobSet::default(),
        SceneSort::Nodes,
        Thresholds::default(),
    );
    assert_eq!(scenes[0].scene, "main.tscn");
    assert_eq!(
        (
            scenes[0].nodes,
            scenes[0].instances,
            scenes[0].instanced_scenes
        ),
        (6, 4, 3)
    );
}

#[test]
fn search_finds_text_with_context() {
    let sources = sources();
    let query = SearchQuery {
        pattern: "health_changed".to_owned(),
        regex: false,
        case_sensitive: true,
        whole_word: true,
        context: 1,
        max_matches: DEFAULT_MAX_MATCHES,
        selection: GlobSet::default(),
    };
    let matcher = Matcher::new(&query).expect("compiles");
    let report = search(&sources, &query, &matcher, &|| false).expect("searched");
    let found: Vec<(&str, u32)> = report
        .matches
        .iter()
        .map(|found| (found.path.as_str(), found.line))
        .collect();
    assert_eq!(
        found,
        vec![
            ("main.tscn", 27),
            ("scripts/player.gd", 4),
            ("scripts/player.gd", 19)
        ]
    );
    assert_eq!(report.skipped.binary, 1);
    assert_eq!(report.matches[1].before[0].line, 3);
}

#[test]
fn uids_resolve_both_ways_from_headers_and_sidecars() {
    let sources = sources();
    let model = ProjectModel::build(&sources, None, 7);
    let uids = model.uids();
    let script = uids.lookup_uid("uid://cplayerscript1");
    assert_eq!(script.status, UidStatus::Resolved);
    assert_eq!(script.value.as_deref(), Some("scripts/player.gd"));
    assert_eq!(script.records[0].source, UidSource::UidSidecar);
    assert_eq!(script.claims[0].referrer, "scenes/player.tscn");
    let icon = uids.lookup_path("icon.svg");
    assert_eq!(icon.value.as_deref(), Some("uid://cicontexture01"));
    assert_eq!(icon.records[0].source, UidSource::ImportSidecar);
    let main = uids.lookup_uid("uid://bmainscene001");
    assert_eq!(main.value.as_deref(), Some("main.tscn"));
    assert_eq!(main.records[0].source, UidSource::DocumentHeader);
    assert_eq!(
        uids.lookup_path("scripts/hud.gd").status,
        UidStatus::Unknown
    );
    assert!(uids.conflicts().is_empty());
}

#[test]
fn nodes_are_found_across_scenes_through_instancing() {
    let sources = sources();
    let model = ProjectModel::build(&sources, None, 7);
    let enemies = find_nodes(
        &model,
        &NodeQuery {
            type_name: Some("Enemy".to_owned()),
            ..NodeQuery::default()
        },
    );
    let found: Vec<(&str, &str, Option<TypeMatch>)> = enemies
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
            ("main.tscn", "Grunt", Some(TypeMatch::ScriptClass)),
            ("main.tscn", "Boss", Some(TypeMatch::InheritedClass)),
            ("scenes/enemy.tscn", ".", Some(TypeMatch::ScriptClass)),
        ]
    );
    assert_eq!(enemies.scenes_searched, 8);

    let foes = find_nodes(
        &model,
        &NodeQuery {
            group: Some("foes".to_owned()),
            scenes: GlobSet::new(["main.tscn"], Vec::<String>::new()).expect("globs"),
            ..NodeQuery::default()
        },
    );
    let names: Vec<&str> = foes
        .matches
        .iter()
        .map(|found| found.node_path.as_str())
        .collect();
    assert_eq!(names, vec!["Grunt", "Boss"]);
}

#[test]
fn scene_exports_combine_script_defaults_and_stored_values() {
    let sources = sources();
    let model = ProjectModel::build(&sources, None, 7);
    let report = scene_exports(&model, "main.tscn").expect("main.tscn parses");
    let rows: Vec<(&str, &str, ValueSource, Option<&str>)> = report
        .rows
        .iter()
        .map(|row| {
            (
                row.node_path.as_str(),
                row.name.as_str(),
                row.value_source,
                row.stored_value.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            (".", "first_level", ValueSource::ScriptDefault, None),
            ("Player", "max_health", ValueSource::Scene, Some("150")),
            (
                "Player",
                "speed",
                ValueSource::InstancedScene,
                Some("250.0")
            ),
            ("Player", "damage", ValueSource::ScriptDefault, None),
            ("Grunt", "speed", ValueSource::InstancedScene, Some("90.0")),
            ("Grunt", "title", ValueSource::Scene, Some("\"first\"")),
            ("Boss", "phase", ValueSource::ScriptDefault, None),
            ("Boss", "speed", ValueSource::InstancedScene, Some("90.0")),
            ("Boss", "title", ValueSource::ScriptDefault, None),
        ]
    );
    let damage = &report.rows[3];
    assert_eq!(damage.group.as_deref(), Some("Combat"));
    assert_eq!(damage.default_value.as_deref(), Some("10"));
    assert_eq!(report.scripted_nodes, 5);
}
