//! The `core` profile's project-wide views, served through the kernel.
//!
//! Every case runs against a copy of the analysis crate's committed `insight`
//! fixture project and reaches the tools through [`ServiceKernel::call_tool`],
//! the same authorization and dispatch path every transport uses. No Godot
//! process is started. The committed fixture bytes are compared before and
//! after, so the repository tree is proven unmodified.
//!
//! The suite pins, per tool: the answer's shape, deterministic paging with an
//! honest continuation, argument refusals as `invalid_input` (or `not_found`
//! for a named file that does not exist), and that the envelope's confidence is
//! never stronger than the weakest confidence inside it.

#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use godot_mcp_analysis::index::FileInventory;
use godot_mcp_analysis::project::ProjectResolver;
use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::contracts::{ErrorKind, OutcomeError, PrincipalId, SessionId};
use godot_mcp_server::core::{CoreLimits, CoreSession, register_core_tools};
use godot_mcp_server::kernel::{AccessContext, ProgressReporter, ServiceKernel};
use godot_mcp_server::test_support::{NameRule, conforms, name_cases};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

const GENERATION: u64 = 31;
const OBSERVED_AT_UNIX_MS: u64 = 1_700_000_000_000;

fn committed_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("godot-mcp-analysis")
        .join("tests")
        .join("fixtures")
        .join("insight")
        .join("project")
}

fn files(base: &Path, directory: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    for entry in fs::read_dir(directory).expect("read fixture directory") {
        let path = entry.expect("fixture entry").path();
        if path.is_dir() {
            files(base, &path, out);
        } else {
            let relative = path
                .strip_prefix(base)
                .expect("relative")
                .to_string_lossy()
                .replace('\\', "/");
            out.insert(relative, fs::read(&path).expect("read fixture file"));
        }
    }
}

fn fixture_files() -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let root = committed_fixture();
    files(&root, &root, &mut out);
    out
}

struct Harness {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    kernel: ServiceKernel,
    access: AccessContext,
}

fn harness_with(extra: &[(&str, &[u8])]) -> Harness {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canonicalize temp base");
    let root = base.join("game");
    let mut contents = fixture_files();
    for (path, bytes) in extra {
        contents.insert((*path).to_owned(), bytes.to_vec());
    }
    for (relative, bytes) in &contents {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(&path, bytes).expect("write fixture copy");
    }
    let resolver = ProjectResolver::new(&base.join("lease-state"));
    let project = resolver
        .resolve(&root, GENERATION)
        .expect("resolve the fixture project");
    let inventory =
        FileInventory::from_paths(contents.keys().map(String::as_str)).expect("fixture inventory");
    let session = Arc::new(CoreSession::new(
        project,
        base.join("index-state"),
        inventory,
        CoreLimits::default(),
    ));
    let mut kernel = ServiceKernel::new(CapabilityProfile::Core);
    register_core_tools(&mut kernel, Arc::clone(&session)).expect("register core handlers");
    let access = AccessContext::new(
        session.project_identity().clone(),
        PrincipalId::parse("principal_0123456789abcdef").expect("principal"),
        Some(SessionId::parse("session_0123456789abcdef").expect("session")),
        OBSERVED_AT_UNIX_MS,
    );
    Harness {
        _tmp: tmp,
        root,
        kernel,
        access,
    }
}

fn harness() -> Harness {
    harness_with(&[])
}

impl Harness {
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        token: CancellationToken,
    ) -> Result<Value, OutcomeError> {
        let Value::Object(arguments) = arguments else {
            panic!("arguments must be an object");
        };
        self.kernel
            .call_tool(
                name,
                arguments,
                self.access.clone(),
                token,
                ProgressReporter::noop(),
            )
            .await
    }

    async fn ok(&self, name: &str, arguments: Value) -> Value {
        let answer = self
            .call(name, arguments.clone(), CancellationToken::new())
            .await
            .unwrap_or_else(|error| panic!("{name} {arguments} must succeed: {error}"));
        assert_honest(name, &answer);
        assert!(
            !answer.to_string().contains(&*self.root.to_string_lossy()),
            "{name} leaked the local project path: {answer}"
        );
        answer
    }

    async fn refused(&self, name: &str, arguments: Value, kind: ErrorKind) -> OutcomeError {
        match self
            .call(name, arguments.clone(), CancellationToken::new())
            .await
        {
            Ok(value) => panic!("{name} {arguments} must be refused, got {value}"),
            Err(error) => {
                assert_eq!(error.kind(), kind, "{name} {arguments}: {error}");
                error
            }
        }
    }
}

fn rank(confidence: &str) -> u8 {
    match confidence {
        "exact" => 0,
        "inferred" => 1,
        "unresolved" => 2,
        other => panic!("unknown confidence {other}"),
    }
}

fn weakest(value: &Value, worst: &mut u8) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == "confidence"
                    && let Some(found) = child.as_str()
                {
                    *worst = (*worst).max(rank(found));
                } else {
                    weakest(child, worst);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| weakest(item, worst)),
        _ => {}
    }
}

/// The envelope is never stronger than any confidence inside the data, and is
/// at most `inferred` when the view reports incomplete coverage.
fn assert_honest(name: &str, answer: &Value) {
    let data = &answer["data"];
    let mut worst = 0;
    weakest(data, &mut worst);
    if data["coverage"]["complete"] == json!(false) {
        worst = worst.max(1);
    }
    let claimed = answer["provenance"]["confidence"]
        .as_str()
        .unwrap_or_else(|| panic!("{name} carries provenance: {answer}"));
    assert!(
        rank(claimed) >= worst,
        "{name} claims {claimed} over weaker data: {answer}"
    );
    assert!(
        answer["workspace"]["revision"].is_u64(),
        "{name} names its snapshot: {answer}"
    );
}

fn strings<'a>(items: &'a Value, key: &str) -> Vec<&'a str> {
    items
        .as_array()
        .unwrap_or_else(|| panic!("expected an array: {items}"))
        .iter()
        .map(|item| {
            item[key]
                .as_str()
                .unwrap_or_else(|| panic!("{key} in {item}"))
        })
        .collect()
}

#[tokio::test]
async fn the_reference_audit_reports_unused_files_and_cycles() {
    let before = fixture_files();
    let harness = harness();

    let unused = harness.ok("project.dependencies.audit", json!({})).await;
    let data = &unused["data"];
    assert_eq!(data["view"], json!("unused"));
    assert_eq!(
        strings(&data["unused"], "path"),
        vec![
            "addons/disabled/plugin.gd",
            "art/unused.png",
            "levels/level_1.tscn",
            "old/legacy.gd",
            "old/legacy.tscn",
            "scenes/loop_a.tscn",
            "scenes/loop_b.tscn",
            "scripts/cycle_a.gd",
            "scripts/cycle_b.gd",
            "scripts/unused_helper.gd",
        ]
    );
    assert_eq!(data["summary"]["unused"], json!(10));
    assert_eq!(data["dynamic_load_count"], json!(2));
    assert_eq!(
        data["unused"][2]["possible_dynamic_loads"][0]["prefix"],
        json!("res://levels/")
    );
    assert_eq!(
        data["unused"][3]["reason"],
        json!("referenced_only_by_unused")
    );
    assert_eq!(unused["provenance"]["confidence"], json!("inferred"));
    let reasons: BTreeSet<&str> = strings(&data["roots"], "reason").into_iter().collect();
    for reason in [
        "main_scene",
        "autoload",
        "project_setting",
        "enabled_plugin",
        "default_bus_layout",
        "export_include_filter",
    ] {
        assert!(
            reasons.contains(reason),
            "missing root reason {reason}: {data}"
        );
    }

    let page = harness
        .ok("project.dependencies.audit", json!({ "limit": 3 }))
        .await;
    assert_eq!(page["data"]["page"]["truncated"], json!(true));
    assert_eq!(page["data"]["page"]["next_offset"], json!(3));
    let next = harness
        .ok(
            "project.dependencies.audit",
            json!({ "offset": 3, "limit": 3 }),
        )
        .await;
    assert_eq!(strings(&next["data"]["unused"], "path")[0], "old/legacy.gd");

    let scoped = harness
        .ok(
            "project.dependencies.audit",
            json!({ "include": ["scripts/**"], "exclude": ["**/cycle_*.gd"] }),
        )
        .await;
    assert_eq!(
        strings(&scoped["data"]["unused"], "path"),
        vec!["scripts/unused_helper.gd"]
    );

    let cycles = harness
        .ok("project.dependencies.audit", json!({ "view": "cycles" }))
        .await;
    let data = &cycles["data"];
    assert_eq!(data["summary"], json!({ "cycles": 3, "breaking": 2 }));
    assert_eq!(
        strings(&data["cycles"], "classification"),
        vec!["resource_load", "inheritance", "tolerated"]
    );
    assert_eq!(
        data["cycles"][1]["members"],
        json!(["scripts/cycle_a.gd", "scripts/cycle_b.gd"])
    );
    assert_eq!(data["cycles"][1]["witness"][0]["kind"], json!("extends"));

    harness
        .refused(
            "project.dependencies.audit",
            json!({ "view": "orphans" }),
            ErrorKind::InvalidInput,
        )
        .await;
    harness
        .refused(
            "project.dependencies.audit",
            json!({ "include": ["{a,b"] }),
            ErrorKind::InvalidInput,
        )
        .await;
    harness
        .refused(
            "project.dependencies.audit",
            json!({ "include": "*.gd" }),
            ErrorKind::InvalidInput,
        )
        .await;
    harness
        .refused(
            "project.dependencies.audit",
            json!({ "include": [1] }),
            ErrorKind::InvalidInput,
        )
        .await;
    harness
        .refused(
            "project.dependencies.audit",
            json!({ "limit": 1001 }),
            ErrorKind::InvalidInput,
        )
        .await;
    assert_eq!(
        fixture_files(),
        before,
        "the committed fixture must stay unmodified"
    );
}

#[tokio::test]
async fn signal_flow_lists_signals_problems_and_undeclared_connections() {
    let harness = harness();
    let signals = harness.ok("project.signals.get", json!({})).await;
    let data = &signals["data"];
    assert_eq!(
        strings(&data["signals"], "name"),
        vec!["score_changed", "start_requested", "health_changed", "died"]
    );
    assert_eq!(
        data["summary"],
        json!({ "signals": 4, "problems": 3, "undeclared_connections": 2 })
    );
    let died = &data["signals"][3];
    assert_eq!(died["scene_connections"][0]["handler"], json!("missing"));
    assert_eq!(died["scene_connection_count"], json!(1));

    let problems = harness
        .ok("project.signals.get", json!({ "view": "problems" }))
        .await;
    assert_eq!(
        strings(&problems["data"]["problems"], "kind"),
        vec![
            "missing_handler",
            "emitted_never_connected",
            "connected_never_declared"
        ]
    );
    assert_eq!(
        problems["data"]["problems"][2]["engine_class"],
        json!("CharacterBody2D")
    );
    assert_eq!(problems["provenance"]["confidence"], json!("unresolved"));

    let undeclared = harness
        .ok(
            "project.signals.get",
            json!({ "view": "undeclared_connections" }),
        )
        .await;
    assert_eq!(
        strings(&undeclared["data"]["undeclared_connections"], "signal"),
        vec!["tree_exited", "pressed"]
    );

    let narrowed = harness
        .ok(
            "project.signals.get",
            json!({ "signal": "health_changed", "script": "res://scripts/player.gd" }),
        )
        .await;
    assert_eq!(
        strings(&narrowed["data"]["signals"], "name"),
        vec!["health_changed"]
    );
    harness
        .refused(
            "project.signals.get",
            json!({ "script": "scripts/missing.gd" }),
            ErrorKind::NotFound,
        )
        .await;
    harness
        .refused(
            "project.signals.get",
            json!({ "script": "main.tscn" }),
            ErrorKind::InvalidInput,
        )
        .await;
    harness
        .refused(
            "project.signals.get",
            json!({ "signal": "" }),
            ErrorKind::InvalidInput,
        )
        .await;
}

#[tokio::test]
async fn metrics_count_the_project_and_rank_complexity() {
    let harness = harness();
    let stats = harness.ok("project.metrics.get", json!({})).await;
    let data = &stats["data"]["statistics"];
    assert_eq!(data["files"], json!(39));
    assert_eq!(data["scripts"]["files"], json!(12));
    assert_eq!(data["scripts"]["class_names"], json!(3));
    // 19 `[node]` sections across the eight text scenes.
    assert_eq!(data["scenes"]["nodes"], json!(19));
    assert_eq!(
        data["by_extension"][0],
        json!({ "extension": "gd", "files": 12, "bytes": data["by_extension"][0]["bytes"] })
    );
    assert!(data["bytes"].as_u64().is_some_and(|bytes| bytes > 0));

    let functions = harness
        .ok(
            "project.metrics.get",
            json!({ "view": "complexity", "limit": 1 }),
        )
        .await;
    let top = &functions["data"]["functions"][0];
    assert_eq!(
        (top["name"].as_str(), top["cyclomatic"].as_u64()),
        (Some("take_hit"), Some(10))
    );
    assert_eq!(functions["data"]["page"]["truncated"], json!(true));

    let nested = harness
        .ok(
            "project.metrics.get",
            json!({ "view": "complexity", "sort": "nesting", "min_nesting": 3 }),
        )
        .await;
    assert_eq!(
        strings(&nested["data"]["functions"], "name"),
        vec!["take_hit"]
    );

    let scenes = harness
        .ok(
            "project.metrics.get",
            json!({ "view": "complexity", "subject": "scenes", "sort": "instances", "min_nodes": 2 }),
        )
        .await;
    assert_eq!(strings(&scenes["data"]["scenes"], "scene")[0], "main.tscn");

    for (arguments, kind) in [
        (
            json!({ "view": "complexity", "sort": "depth" }),
            ErrorKind::InvalidInput,
        ),
        (
            json!({ "view": "complexity", "subject": "scenes", "sort": "lines" }),
            ErrorKind::InvalidInput,
        ),
        (
            json!({ "view": "complexity", "min_nodes": 3 }),
            ErrorKind::InvalidInput,
        ),
        (json!({ "subject": "scenes" }), ErrorKind::InvalidInput),
        (
            json!({ "view": "complexity", "subject": "modules" }),
            ErrorKind::InvalidInput,
        ),
        (
            json!({ "view": "complexity", "min_lines": -1 }),
            ErrorKind::InvalidInput,
        ),
    ] {
        harness
            .refused("project.metrics.get", arguments, kind)
            .await;
    }
}

#[tokio::test]
async fn search_matches_text_and_declares_its_bounds() {
    let harness = harness();
    let found = harness
        .ok(
            "project.search",
            json!({ "query": "health_changed", "whole_word": true, "case_sensitive": true, "context": 1 }),
        )
        .await;
    let data = &found["data"];
    let hits: Vec<(String, u64)> = data["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .map(|hit| {
            (
                hit["path"].as_str().unwrap().to_owned(),
                hit["line"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        hits,
        vec![
            ("main.tscn".to_owned(), 27),
            ("scripts/player.gd".to_owned(), 4),
            ("scripts/player.gd".to_owned(), 19),
        ]
    );
    assert_eq!(data["matches"][1]["before"][0]["line"], json!(3));
    assert_eq!(data["skipped"]["binary"], json!(1));
    assert_eq!(found["provenance"]["confidence"], json!("exact"));

    let bounded = harness
        .ok(
            "project.search",
            json!({ "query": "func", "max_matches": 2 }),
        )
        .await;
    assert_eq!(bounded["data"]["match_limit_reached"], json!(true));
    assert_eq!(bounded["data"]["matches"].as_array().map(Vec::len), Some(2));
    assert_eq!(bounded["provenance"]["confidence"], json!("inferred"));

    let regex = harness
        .ok(
            "project.search",
            json!({ "query": r"^signal \w+", "regex": true, "include": ["scripts/*.gd"], "limit": 2 }),
        )
        .await;
    assert_eq!(regex["data"]["page"]["total"], json!(4));
    assert_eq!(regex["data"]["page"]["next_offset"], json!(2));

    for arguments in [
        json!({}),
        json!({ "query": "" }),
        json!({ "query": "(", "regex": true }),
        json!({ "query": "a", "context": 11 }),
        json!({ "query": "a", "max_matches": 0 }),
        json!({ "query": "a", "max_matches": 10_001 }),
        json!({ "query": "a", "regex": "yes" }),
    ] {
        harness
            .refused("project.search", arguments, ErrorKind::InvalidInput)
            .await;
    }
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error = harness
        .call("project.search", json!({ "query": "a" }), cancelled)
        .await
        .expect_err("a cancelled search is refused");
    assert_eq!(error.kind(), ErrorKind::Cancelled);
}

#[tokio::test]
async fn uid_lookups_resolve_both_ways() {
    let harness = harness();
    let by_uid = harness
        .ok("project.uid.get", json!({ "uid": "uid://cplayerscript1" }))
        .await;
    let data = &by_uid["data"];
    assert_eq!(data["status"], json!("resolved"));
    assert_eq!(data["value"], json!("scripts/player.gd"));
    assert_eq!(data["records"][0]["source"], json!("uid_sidecar"));
    assert_eq!(data["claims"][0]["referrer"], json!("scenes/player.tscn"));

    let by_path = harness
        .ok("project.uid.get", json!({ "path": "res://icon.svg" }))
        .await;
    assert_eq!(by_path["data"]["value"], json!("uid://cicontexture01"));
    assert_eq!(
        by_path["data"]["records"][0]["source"],
        json!("import_sidecar")
    );

    let unknown = harness
        .ok("project.uid.get", json!({ "uid": "uid://bnosuchthing" }))
        .await;
    assert_eq!(unknown["data"]["status"], json!("unknown"));
    assert_eq!(unknown["provenance"]["confidence"], json!("unresolved"));

    let table = harness.ok("project.uid.get", json!({ "limit": 5 })).await;
    assert_eq!(table["data"]["mode"], json!("table"));
    // Eight document headers, five `.uid` sidecars and two `.import` files.
    assert_eq!(table["data"]["record_count"], json!(15));
    assert_eq!(table["data"]["page"]["truncated"], json!(true));

    harness
        .refused(
            "project.uid.get",
            json!({ "uid": "uid://a", "path": "main.tscn" }),
            ErrorKind::InvalidInput,
        )
        .await;
    harness
        .refused(
            "project.uid.get",
            json!({ "uid": "res://main.tscn" }),
            ErrorKind::InvalidInput,
        )
        .await;
    harness
        .refused(
            "project.uid.get",
            json!({ "path": "../outside.gd" }),
            ErrorKind::InvalidInput,
        )
        .await;
    harness
        .refused(
            "project.uid.get",
            json!({ "path": "missing.gd" }),
            ErrorKind::NotFound,
        )
        .await;
}

#[tokio::test]
async fn node_queries_cross_scenes_through_instancing() {
    let harness = harness();
    let enemies = harness
        .ok("scene.nodes.find", json!({ "type": "Enemy" }))
        .await;
    let data = &enemies["data"];
    assert_eq!(
        strings(&data["nodes"], "node_path"),
        vec!["Grunt", "Boss", "."]
    );
    assert_eq!(
        strings(&data["nodes"], "type_match"),
        vec!["script_class", "inherited_class", "script_class"]
    );
    assert_eq!(data["scenes_searched"], json!(8));

    let sprites = harness
        .ok(
            "scene.nodes.find",
            json!({ "type": "Sprite2D", "include": ["scenes/*.tscn"] }),
        )
        .await;
    assert_eq!(sprites["data"]["page"]["total"], json!(2));
    assert!(
        sprites["data"]["limitations"]
            .as_array()
            .is_some_and(|limits| limits.iter().any(|limit| limit
                .as_str()
                .unwrap_or_default()
                .contains("engine class inheritance")))
    );

    let stored = harness
        .ok(
            "scene.nodes.find",
            json!({ "property": "title", "value": "\"first\"", "name": "G*" }),
        )
        .await;
    assert_eq!(
        strings(&stored["data"]["nodes"], "node_path"),
        vec!["Grunt"]
    );
    let scripted = harness
        .ok(
            "scene.nodes.find",
            json!({ "script": "scripts/enemy.gd", "group": "foes" }),
        )
        .await;
    assert_eq!(scripted["data"]["page"]["total"], json!(3));

    harness
        .refused(
            "scene.nodes.find",
            json!({ "value": "1" }),
            ErrorKind::InvalidInput,
        )
        .await;
    harness
        .refused(
            "scene.nodes.find",
            json!({ "script": "scripts/nothing.gd" }),
            ErrorKind::NotFound,
        )
        .await;
    harness
        .refused(
            "scene.nodes.find",
            json!({ "type": "" }),
            ErrorKind::InvalidInput,
        )
        .await;
}

#[tokio::test]
async fn scene_exports_join_script_defaults_and_stored_values() {
    let harness = harness_with(&[("scenes/legacy.scn", b"RSCC\0\0\0\x01binary")]);
    let exports = harness
        .ok("scene.exports.get", json!({ "scene": "main.tscn" }))
        .await;
    let data = &exports["data"];
    assert_eq!(data["scripted_nodes"], json!(5));
    assert_eq!(data["page"]["total"], json!(9));
    let player_health = &data["exports"][1];
    assert_eq!(player_health["name"], json!("max_health"));
    assert_eq!(player_health["value_source"], json!("scene"));
    assert_eq!(player_health["stored_value"], json!("150"));
    assert_eq!(player_health["default_value"], json!("100"));

    let one = harness
        .ok(
            "scene.exports.get",
            json!({ "scene": "main.tscn", "node": "Boss" }),
        )
        .await;
    assert_eq!(
        strings(&one["data"]["exports"], "name"),
        vec!["phase", "speed", "title"]
    );
    assert_eq!(
        one["data"]["exports"][1]["stored_in"],
        json!("scenes/enemy.tscn")
    );

    harness
        .refused("scene.exports.get", json!({}), ErrorKind::InvalidInput)
        .await;
    harness
        .refused(
            "scene.exports.get",
            json!({ "scene": "main.tscn", "node": "Nope" }),
            ErrorKind::NotFound,
        )
        .await;
    harness
        .refused(
            "scene.exports.get",
            json!({ "scene": "scripts/player.gd" }),
            ErrorKind::InvalidInput,
        )
        .await;
    let binary = harness
        .refused(
            "scene.exports.get",
            json!({ "scene": "scenes/legacy.scn" }),
            ErrorKind::Unsupported,
        )
        .await;
    assert!(binary.details().contains_key("remediation"), "{binary}");
}

/// Every project-wide view answers a call with no arguments (or the one it
/// requires) instead of reporting an unimplemented handler.
/// Hold `tool`'s advertised schema and its handler to the same verdict on
/// whether each case's arguments are well formed. A handler that finds a
/// well-formed request names nothing the project has (`not_found`) has not
/// refused its arguments.
async fn agree(harness: &Harness, tool: &str, cases: &[(Value, bool)]) {
    let schema = harness
        .kernel
        .tools()
        .into_iter()
        .find(|listed| listed.name == tool)
        .map(|listed| Value::Object(listed.input_schema))
        .unwrap_or_else(|| panic!("the core profile lists {tool}"));
    for (arguments, expected) in cases {
        let shown = arguments.to_string();
        let shown: String = shown.chars().take(160).collect();
        assert_eq!(
            conforms(&schema, arguments),
            *expected,
            "the {tool} schema's verdict on {shown}"
        );
        let accepted = match harness
            .call(tool, arguments.clone(), CancellationToken::new())
            .await
        {
            Ok(_) => true,
            Err(error) if error.kind() == ErrorKind::InvalidInput => false,
            Err(error) if error.kind() == ErrorKind::NotFound => true,
            Err(error) => panic!("{tool} {shown}: {error}"),
        };
        assert_eq!(
            accepted, *expected,
            "the {tool} handler's verdict on {shown}"
        );
    }
}

/// The name cases of `rule`, each as the arguments `wrap` makes of it.
fn named(rule: NameRule, wrap: impl Fn(String) -> Value) -> Vec<(Value, bool)> {
    name_cases(rule, "", "")
        .into_iter()
        .map(|(name, expected)| (wrap(name), expected))
        .collect()
}

/// Every text argument of the project-wide views is bounded in characters,
/// whatever the bytes each takes, as its schema's `maxLength` counts them.
/// None refuses a control character: a search's text and a stored value are
/// free text, and a name, a glob or a node path is matched against what the
/// project's files hold.
#[tokio::test]
async fn every_text_argument_follows_one_rule_in_the_schema_and_the_check() {
    use godot_mcp_analysis::insight::glob::MAX_GLOB_LENGTH;
    use godot_mcp_analysis::insight::search::MAX_PATTERN_LENGTH;
    use godot_mcp_analysis::insight::uid::MAX_UID_LENGTH;

    let harness = harness();
    let name = NameRule {
        limit: 1024,
        empty: false,
        control: false,
    };
    agree(
        &harness,
        "project.search",
        &named(
            NameRule {
                limit: MAX_PATTERN_LENGTH,
                ..name
            },
            |query| json!({ "query": query }),
        ),
    )
    .await;
    agree(
        &harness,
        "project.search",
        &named(
            NameRule {
                limit: MAX_GLOB_LENGTH,
                ..name
            },
            |glob| json!({ "query": "func", "include": [glob] }),
        ),
    )
    .await;
    agree(
        &harness,
        "project.signals.get",
        &named(name, |signal| json!({ "signal": signal })),
    )
    .await;
    agree(
        &harness,
        "scene.exports.get",
        &named(name, |node| json!({ "scene": "main.tscn", "node": node })),
    )
    .await;
    for field in ["type", "group", "name", "property"] {
        agree(
            &harness,
            "scene.nodes.find",
            &named(name, |text| {
                let mut arguments = Map::new();
                arguments.insert(field.to_owned(), Value::String(text));
                Value::Object(arguments)
            }),
        )
        .await;
    }
    agree(
        &harness,
        "scene.nodes.find",
        &named(
            NameRule {
                empty: true,
                ..name
            },
            |value| json!({ "property": "position", "value": value }),
        ),
    )
    .await;
    let longest = format!("uid://{}", "a".repeat(MAX_UID_LENGTH - 6));
    let uids = [
        ("uid://abc123", true),
        (longest.as_str(), true),
        (&format!("{longest}a"), false),
        ("uid://", false),
        ("uid://ABC", false),
        ("uid://a-b", false),
        ("uid://\u{1F600}", false),
        ("uid://\u{E9}", false),
        ("abcdefg", false),
    ]
    .map(|(uid, expected)| (json!({ "uid": uid }), expected));
    agree(&harness, "project.uid.get", &uids).await;
}

#[tokio::test]
async fn every_project_wide_view_has_a_real_handler() {
    let harness = harness();
    for (tool, arguments) in [
        ("project.dependencies.audit", json!({})),
        ("project.metrics.get", json!({})),
        ("project.search", json!({ "query": "extends" })),
        ("project.signals.get", json!({})),
        ("project.uid.get", json!({})),
        ("scene.exports.get", json!({ "scene": "scenes/hud.tscn" })),
        ("scene.nodes.find", json!({})),
    ] {
        let answer = harness.ok(tool, arguments).await;
        let page = &answer["data"]["page"];
        for key in [
            "offset",
            "limit",
            "returned",
            "total",
            "truncated",
            "next_offset",
        ] {
            assert!(page.get(key).is_some(), "{tool} page lacks {key}: {answer}");
        }
        let _: &Map<String, Value> = answer["data"].as_object().expect("data object");
    }
}

/// Godot's editor never scans a hidden file, a file under a hidden directory
/// or one under a directory holding a `.gdignore`; the views leave them out
/// too, and say how many they left out.
#[tokio::test]
async fn views_leave_out_what_godot_ignores() {
    let harness = harness_with(&[
        (".git/config", b"[core]\n\tsignal = health_changed\n"),
        (
            "scripts/.draft.gd",
            b"class_name Ghost\nextends Node\nsignal health_changed\n",
        ),
        ("tools/.gdignore", b""),
        (
            "tools/raw.tscn",
            b"[gd_scene format=3]\n\n[node name=\"Raw\" type=\"Node\"]\n",
        ),
        (
            "tools/helper.gd",
            b"class_name Helper\nextends Node\nconst H = preload(\"res://scripts/unused_helper.gd\")\n",
        ),
    ]);
    let ignored = json!({ "hidden": 3, "gdignore": 2 });

    let stats = harness.ok("project.metrics.get", json!({})).await;
    assert_eq!(stats["data"]["ignored_files"], ignored, "{stats}");
    assert_eq!(stats["data"]["statistics"]["files"], json!(39), "{stats}");
    assert_eq!(
        stats["data"]["statistics"]["scripts"]["class_names"],
        json!(3),
        "Ghost and Helper are not registered: {stats}"
    );

    let unused = harness.ok("project.dependencies.audit", json!({})).await;
    assert_eq!(unused["data"]["ignored_files"], ignored, "{unused}");
    let paths = strings(&unused["data"]["unused"], "path");
    assert!(
        paths
            .iter()
            .all(|path| !path.starts_with("tools/") && !path.contains("/.")),
        "{paths:?}"
    );
    assert!(
        paths.contains(&"scripts/unused_helper.gd"),
        "a file only an ignored file preloads stays unused: {paths:?}"
    );

    let found = harness
        .ok(
            "project.search",
            json!({ "query": "health_changed", "whole_word": true }),
        )
        .await;
    assert_eq!(found["data"]["ignored_files"], ignored, "{found}");
    assert_eq!(found["data"]["page"]["total"], json!(3), "{found}");

    let ghost = harness
        .ok("scene.nodes.find", json!({ "type": "Ghost" }))
        .await;
    assert_eq!(ghost["data"]["page"]["total"], json!(0), "{ghost}");
    assert!(
        ghost["data"]["limitations"]
            .to_string()
            .contains("not a project class_name"),
        "{ghost}"
    );

    let refused = harness
        .refused(
            "scene.exports.get",
            json!({ "scene": "tools/raw.tscn" }),
            ErrorKind::NotFound,
        )
        .await;
    assert!(refused.message().contains(".gdignore"), "{refused}");
}
