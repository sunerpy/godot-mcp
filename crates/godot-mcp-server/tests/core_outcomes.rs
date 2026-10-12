//! Todo 12 acceptance tests: the `core` capability profile's outcome tools.
//!
//! Exactly two test cases, matching the `qa/tasks/12.json` descriptor:
//! `core_outcomes_happy_path` and `core_outcomes_failure_paths`.
//!
//! Both cases run against a *copy* of the committed fixture project under
//! `tests/fixtures/core/project`, and the committed bytes are compared before and
//! after every case so the repository tree itself is proven unmodified.
//!
//! No Godot process is started, and none is available: every fact asserted here
//! is derived by the Todo 7–11 static layers through the Todo 5 kernel. The tests
//! deliberately reach the tools through [`ServiceKernel::call_tool`], i.e. the
//! same authorization and dispatch path every transport uses, rather than calling
//! a handler directly.
//!
//! Every project byte is read through Todo 8's `ResolvedProject`, which is built
//! on Todo 2's held-object ownership layer (Unix object identity on this host), so
//! the suite asserts the real behaviour on Unix and is compiled out elsewhere
//! rather than silently weakened.

#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use godot_mcp_analysis::index::FileInventory;
use godot_mcp_analysis::project::ProjectResolver;
use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::contracts::{ErrorKind, PrincipalId, SessionId};
use godot_mcp_domain::manifests::canonical_manifest;
use godot_mcp_server::core::{CoreLimits, CoreSession, register_core_tools};
use godot_mcp_server::kernel::{AccessContext, ProgressReporter, ServiceKernel};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

/// Generation every fixture read is attributed to.
const GENERATION: u64 = 12;

/// Trusted observation time supplied by the simulated transport boundary.
const OBSERVED_AT_UNIX_MS: u64 = 1_700_000_000_000;

/// Every project-relative path the fixture project contains.
const INVENTORY: &[&str] = &[
    "project.godot",
    "resources/buff.tres",
    "resources/legacy.res",
    "scenes/hud.tscn",
    "scenes/legacy.scn",
    "scenes/main.tscn",
    "scripts/dynamic.gd",
    "scripts/game_state.gd",
    "scripts/hud.gd",
    "scripts/player.gd",
];

fn committed_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("core")
        .join("project")
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

fn snapshot_tree(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
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

struct Harness {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    kernel: ServiceKernel,
    session: Arc<CoreSession>,
    access: AccessContext,
    registered: BTreeSet<String>,
}

fn harness() -> Harness {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canonicalize temp base");
    let root = base.join("game");
    copy_tree(&committed_fixture(), &root);

    let resolver = ProjectResolver::new(&base.join("lease-state"));
    let project = resolver
        .resolve(&root, GENERATION)
        .expect("resolve the fixture project");
    let inventory = FileInventory::from_paths(INVENTORY).expect("fixture inventory");
    let session = Arc::new(CoreSession::new(
        project,
        base.join("index-state"),
        inventory,
        CoreLimits::default(),
    ));

    let mut kernel = ServiceKernel::new(CapabilityProfile::Core);
    let registered = register_core_tools(&mut kernel, Arc::clone(&session))
        .expect("register every core outcome handler");

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
        session,
        access,
        registered,
    }
}

impl Harness {
    async fn call(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<Value, godot_mcp_domain::contracts::OutcomeError> {
        self.kernel
            .call_tool(
                name,
                object(arguments),
                self.access.clone(),
                CancellationToken::new(),
                ProgressReporter::noop(),
            )
            .await
    }

    async fn ok(&self, name: &str, arguments: Value) -> Value {
        self.call(name, arguments)
            .await
            .unwrap_or_else(|error| panic!("{name} must succeed: {error}"))
    }

    async fn err(&self, name: &str, arguments: Value) -> godot_mcp_domain::contracts::OutcomeError {
        match self.call(name, arguments).await {
            Ok(value) => panic!("{name} must fail, got: {value}"),
            Err(error) => error,
        }
    }

    async fn cancelled(
        &self,
        name: &str,
        arguments: Value,
    ) -> godot_mcp_domain::contracts::OutcomeError {
        let token = CancellationToken::new();
        token.cancel();
        match self
            .kernel
            .call_tool(
                name,
                object(arguments),
                self.access.clone(),
                token,
                ProgressReporter::noop(),
            )
            .await
        {
            Ok(value) => panic!("{name} must observe cancellation, got: {value}"),
            Err(error) => error,
        }
    }
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("tool arguments must be an object, got {other}"),
    }
}

fn data(answer: &Value) -> &Value {
    answer.get("data").expect("every answer carries data")
}

fn confidence(answer: &Value) -> &str {
    answer
        .get("provenance")
        .and_then(|provenance| provenance.get("confidence"))
        .and_then(Value::as_str)
        .expect("every answer carries provenance confidence")
}

fn source(answer: &Value) -> &str {
    answer
        .get("provenance")
        .and_then(|provenance| provenance.get("source"))
        .and_then(Value::as_str)
        .expect("every answer carries a provenance source")
}

fn page(value: &Value) -> &Value {
    value.get("page").expect("a paginated section carries page")
}

fn array<'a>(value: &'a Value, key: &str) -> &'a Vec<Value> {
    value
        .get(key)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("{key} must be an array in {value}"))
}

/// Weakest confidence appearing anywhere inside a value, by the honesty order
/// `exact` < `inferred` < `unresolved`.
fn weakest_confidence(value: &Value) -> Option<String> {
    fn rank(confidence: &str) -> u8 {
        match confidence {
            "exact" => 0,
            "inferred" => 1,
            "unresolved" => 2,
            other => panic!("unknown confidence {other}"),
        }
    }
    fn walk(value: &Value, out: &mut Option<String>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    if (key == "confidence" || key == "resolution")
                        && let Some(found) = child.as_str()
                        && out
                            .as_deref()
                            .is_none_or(|current| rank(found) > rank(current))
                    {
                        *out = Some(found.to_owned());
                    }
                    walk(child, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, out);
                }
            }
            _ => {}
        }
    }
    let mut out = None;
    walk(value, &mut out);
    out
}

#[tokio::test]
async fn core_outcomes_happy_path() {
    let before = snapshot_tree(&committed_fixture());
    let harness = harness();

    // ---------------------------------------------------------------------
    // Every canonical `core` tool has a real registered handler.
    // ---------------------------------------------------------------------
    let canonical = canonical_manifest(CapabilityProfile::Core).tools;
    assert_eq!(
        harness.registered, canonical,
        "every canonical core tool must have a registered handler"
    );
    assert_eq!(canonical.len(), 15, "the core profile has 15 tools");
    for profile in [
        CapabilityProfile::Core,
        CapabilityProfile::Editor,
        CapabilityProfile::Runtime,
        CapabilityProfile::Full,
    ] {
        let mut kernel = ServiceKernel::new(profile);
        let registered = register_core_tools(&mut kernel, Arc::clone(&harness.session))
            .expect("every profile advertising core tools accepts their handlers");
        assert_eq!(
            registered, canonical,
            "{profile:?} must register the canonical core tool set"
        );
    }
    for tool in &canonical {
        let answer = harness.call(tool, json!({})).await;
        assert!(
            !matches!(
                answer.as_ref().err().map(|error| error.kind()),
                Some(ErrorKind::Unavailable)
            ),
            "{tool} must not report an unimplemented handler"
        );
    }

    // ---------------------------------------------------------------------
    // Asking the impact of a script retrieves a bounded, provenance-rich graph.
    // ---------------------------------------------------------------------
    let impact = harness
        .ok(
            "gdscript.impact.get",
            json!({ "script": "scripts/player.gd" }),
        )
        .await;
    let payload = data(&impact);
    let impacted = array(payload, "impacted");
    assert!(
        !impacted.is_empty(),
        "the fixture script has referrers: {payload}"
    );
    let impacted_files: BTreeSet<&str> = impacted
        .iter()
        .filter_map(|node| node.get("file").and_then(Value::as_str))
        .collect();
    assert!(
        impacted_files.contains("scenes/main.tscn"),
        "the scene that attaches the script is impacted: {impacted_files:?}"
    );
    assert!(
        impacted_files.contains("resources/buff.tres"),
        "the resource that binds the script is impacted: {impacted_files:?}"
    );
    for node in impacted {
        assert!(
            node.get("kind").and_then(Value::as_str).is_some(),
            "every impacted node names its kind: {node}"
        );
        assert!(
            node.get("confidence").and_then(Value::as_str).is_some(),
            "every impacted node carries confidence: {node}"
        );
    }
    let edges = array(payload, "referencing_edges");
    assert!(
        !edges.is_empty(),
        "the impact answer explains itself with edges: {payload}"
    );
    for edge in edges {
        assert!(
            edge.get("resolution").and_then(Value::as_str).is_some(),
            "every edge states how certainly it resolved: {edge}"
        );
        assert!(
            edge.get("file").and_then(Value::as_str).is_some(),
            "every edge names the file it was written in: {edge}"
        );
    }
    // The answer is bounded: both sections page, and neither is unbounded.
    assert!(
        page(payload).get("total").is_some(),
        "impact pages: {payload}"
    );
    // Honesty: the answer is never stronger than its weakest included item.
    let weakest = weakest_confidence(payload).expect("the impact answer carries item confidences");
    assert_eq!(
        confidence(&impact),
        weakest,
        "the impact answer must not be stronger than its weakest item"
    );
    assert!(
        matches!(source(&impact), "static_index" | "heuristic"),
        "impact evidence is static: {}",
        source(&impact)
    );
    // Known limitations are represented rather than hidden.
    assert!(
        payload.get("limitations").is_some(),
        "the impact answer discloses analyzer limitations: {payload}"
    );

    // A dynamic GDScript call can never be exact, and the envelope must inherit
    // that weakness rather than presenting the graph's best guess as proven.
    let symbol_impact = harness
        .ok(
            "gdscript.impact.get",
            json!({ "script": "scripts/player.gd", "symbol": "take_damage" }),
        )
        .await;
    let symbol_payload = data(&symbol_impact);
    assert!(
        array(symbol_payload, "referencing_edges")
            .iter()
            .any(
                |edge| edge.get("relation").and_then(Value::as_str) == Some("calls")
                    && edge.get("resolution").and_then(Value::as_str) == Some("inferred")
            ),
        "a cross-script GDScript call stays inferred: {symbol_payload}"
    );
    assert_eq!(
        confidence(&symbol_impact),
        "inferred",
        "an answer containing an inferred call must not claim to be exact"
    );
    assert_eq!(
        source(&symbol_impact),
        "heuristic",
        "inferred evidence must be labeled heuristic, which Todo 3 enforces"
    );

    // No internal graph identity, no SQL and no local path may appear.
    let rendered = serde_json::to_string(&impact).expect("serialize the impact answer");
    for leak in [
        "script:",
        "scene:",
        "scene_node:",
        "resource:",
        "function:",
        "signal:",
        "autoload:",
        "SELECT",
        "sqlite",
        "index.sqlite3",
    ] {
        assert!(
            !rendered.contains(leak),
            "the public answer must not leak `{leak}`: {rendered}"
        );
    }
    assert!(
        !rendered.contains(&harness.root.to_string_lossy().into_owned()),
        "the public answer must not leak the local project path"
    );

    // ---------------------------------------------------------------------
    // Pagination is deterministic and truncation is declared.
    // ---------------------------------------------------------------------
    let all = harness
        .ok(
            "gdscript.symbols.get",
            json!({ "script": "scripts/player.gd" }),
        )
        .await;
    let all_symbols = array(data(&all), "symbols");
    assert!(
        all_symbols.len() >= 3,
        "the fixture script declares several symbols: {all_symbols:?}"
    );
    let first = harness
        .ok(
            "gdscript.symbols.get",
            json!({ "script": "scripts/player.gd", "limit": 1 }),
        )
        .await;
    let first_page = data(&first);
    assert_eq!(array(first_page, "symbols").len(), 1, "limit truncates");
    assert_eq!(
        page(first_page).get("truncated"),
        Some(&Value::Bool(true)),
        "a truncated page says so: {first_page}"
    );
    assert_eq!(
        page(first_page).get("next_offset").and_then(Value::as_u64),
        Some(1),
        "a truncated page names its continuation: {first_page}"
    );
    let second = harness
        .ok(
            "gdscript.symbols.get",
            json!({ "script": "scripts/player.gd", "offset": 1, "limit": 1 }),
        )
        .await;
    assert_eq!(
        array(data(&second), "symbols")[0],
        all_symbols[1],
        "paging is stable against the unpaged order"
    );
    assert_eq!(
        array(first_page, "symbols")[0],
        all_symbols[0],
        "the first page matches the unpaged order"
    );

    // ---------------------------------------------------------------------
    // Project, discovery, dependency, tree and diagnostics outcomes.
    // ---------------------------------------------------------------------
    let info = harness.ok("project.info.get", json!({})).await;
    assert_eq!(
        data(&info).get("name").and_then(Value::as_str),
        Some("Core Outcome Fixture"),
        "project info reports the manifest name: {info}"
    );
    assert_eq!(
        data(&info).get("main_scene").and_then(Value::as_str),
        Some("scenes/main.tscn"),
        "project info resolves the boot scene: {info}"
    );

    let discover = harness.ok("project.discover", json!({})).await;
    assert_eq!(
        page(data(&discover)).get("total").and_then(Value::as_u64),
        Some(INVENTORY.len() as u64),
        "discovery reports the whole indexed inventory: {discover}"
    );

    let dependencies = harness.ok("project.dependencies.get", json!({})).await;
    let autoloads = array(data(&dependencies), "autoloads");
    assert_eq!(autoloads.len(), 1, "the fixture registers one autoload");
    assert_eq!(
        autoloads[0].get("name").and_then(Value::as_str),
        Some("GameState")
    );

    let tree = harness
        .ok("scene.tree.get", json!({ "scene": "scenes/main.tscn" }))
        .await;
    let nodes = array(data(&tree), "nodes");
    assert!(
        nodes
            .iter()
            .any(|node| node.get("name").and_then(Value::as_str) == Some("Player")),
        "the scene tree contains the Player node: {tree}"
    );

    let resource = harness
        .ok(
            "resource.tree.get",
            json!({ "resource": "resources/buff.tres" }),
        )
        .await;
    assert!(
        !array(data(&resource), "properties").is_empty(),
        "the resource tree reports the resource body: {resource}"
    );

    let scene_dependencies = harness
        .ok(
            "scene.dependencies.get",
            json!({ "scene": "scenes/main.tscn" }),
        )
        .await;
    assert!(
        !array(data(&scene_dependencies), "dependencies").is_empty(),
        "the scene depends on its scripts and sub-scene: {scene_dependencies}"
    );

    let resource_dependencies = harness
        .ok(
            "resource.dependencies.get",
            json!({ "resource": "resources/buff.tres" }),
        )
        .await;
    assert!(
        !array(data(&resource_dependencies), "dependencies").is_empty(),
        "the resource depends on its script: {resource_dependencies}"
    );

    let references = harness
        .ok(
            "gdscript.references.get",
            json!({ "script": "scripts/player.gd", "symbol": "take_damage" }),
        )
        .await;
    assert!(
        !array(data(&references), "references").is_empty()
            || !array(data(&references), "callers").is_empty(),
        "a called function has referrers: {references}"
    );
    assert_eq!(
        confidence(&references),
        weakest_confidence(data(&references)).unwrap_or_else(|| "exact".to_owned()),
        "reference answers are never stronger than their weakest item"
    );

    let diagnostics = harness.ok("gdscript.diagnostics.get", json!({})).await;
    assert!(
        page(data(&diagnostics)).get("total").is_some(),
        "project-wide diagnostics are bounded: {diagnostics}"
    );

    let audit = harness.ok("project.validate", json!({})).await;
    let dangling = array(data(&audit), "dangling");
    assert!(
        dangling
            .iter()
            .any(|entry| entry.get("kind").and_then(Value::as_str) == Some("target_absent")),
        "the dangling audit reports the fixture's absent script: {audit}"
    );
    assert!(
        array(data(&audit), "limitations")
            .iter()
            .any(|limitation| limitation
                .get("code")
                .and_then(Value::as_str)
                .is_some_and(|code| code.contains("engine"))),
        "static validation discloses that engine validation was not performed: {audit}"
    );

    let script_validation = harness
        .ok("script.validate", json!({ "script": "scripts/player.gd" }))
        .await;
    assert_eq!(
        data(&script_validation)
            .get("verdict")
            .and_then(Value::as_str),
        Some("valid_static"),
        "the clean fixture script passes static validation: {script_validation}"
    );

    let health = harness.ok("health.get", json!({})).await;
    assert_eq!(
        health.get("status").and_then(Value::as_str),
        Some("ready"),
        "a coherent index reports ready: {health}"
    );

    let capabilities = harness.ok("capabilities.get", json!({})).await;
    assert!(
        !capabilities.is_null(),
        "capabilities are reported: {capabilities}"
    );

    assert_eq!(
        before,
        snapshot_tree(&committed_fixture()),
        "the committed fixture must not be mutated"
    );
}

#[tokio::test]
async fn core_outcomes_failure_paths() {
    let before = snapshot_tree(&committed_fixture());
    let harness = harness();

    // ---------------------------------------------------------------------
    // A binary Godot resource is `unsupported` with remediation — never an
    // empty success.
    // ---------------------------------------------------------------------
    for (tool, key, path) in [
        ("scene.tree.get", "scene", "scenes/legacy.scn"),
        ("resource.tree.get", "resource", "resources/legacy.res"),
        ("scene.dependencies.get", "scene", "scenes/legacy.scn"),
        (
            "resource.dependencies.get",
            "resource",
            "resources/legacy.res",
        ),
    ] {
        let error = harness.err(tool, json!({ key: path })).await;
        assert_eq!(
            error.kind(),
            ErrorKind::Unsupported,
            "{tool} must refuse a binary Godot document: {error}"
        );
        let remediation = error
            .details()
            .get("remediation")
            .unwrap_or_else(|| panic!("{tool} must carry remediation, got {:?}", error.details()));
        let remediation = match remediation {
            godot_mcp_domain::contracts::GodotValue::String(text) => text.clone(),
            other => panic!("remediation must be text, got {other:?}"),
        };
        assert!(
            remediation.contains(".tscn") || remediation.contains(".tres"),
            "remediation must name the supported text format: {remediation}"
        );
        assert!(
            !error.retryable(),
            "an unsupported format is not fixed by retrying: {error}"
        );
    }

    // ---------------------------------------------------------------------
    // Stable, classified errors for every other refusal.
    // ---------------------------------------------------------------------
    let cases: Vec<(&str, Value, ErrorKind)> = vec![
        // A missing required argument is invalid input, not an empty answer.
        ("scene.tree.get", json!({}), ErrorKind::InvalidInput),
        ("resource.tree.get", json!({}), ErrorKind::InvalidInput),
        ("gdscript.symbols.get", json!({}), ErrorKind::InvalidInput),
        ("gdscript.impact.get", json!({}), ErrorKind::InvalidInput),
        // An unknown argument is refused rather than silently ignored.
        (
            "project.info.get",
            json!({ "unexpected": true }),
            ErrorKind::InvalidInput,
        ),
        // A wrong-but-textual extension is a caller mistake.
        (
            "scene.tree.get",
            json!({ "scene": "resources/buff.tres" }),
            ErrorKind::InvalidInput,
        ),
        (
            "resource.tree.get",
            json!({ "resource": "scenes/main.tscn" }),
            ErrorKind::InvalidInput,
        ),
        (
            "gdscript.symbols.get",
            json!({ "script": "scenes/main.tscn" }),
            ErrorKind::InvalidInput,
        ),
        // An escaping path is refused by the lease, not by string matching here.
        (
            "gdscript.symbols.get",
            json!({ "script": "../outside.gd" }),
            ErrorKind::InvalidInput,
        ),
        // The engine cache is never project source.
        (
            "gdscript.symbols.get",
            json!({ "script": ".godot/imported/cached.gd" }),
            ErrorKind::PermissionDenied,
        ),
        // An absent file is not found rather than reported as empty.
        (
            "gdscript.symbols.get",
            json!({ "script": "scripts/absent.gd" }),
            ErrorKind::NotFound,
        ),
        (
            "gdscript.impact.get",
            json!({ "script": "scripts/absent.gd" }),
            ErrorKind::NotFound,
        ),
        // A symbol that does not exist is not found.
        (
            "gdscript.impact.get",
            json!({ "script": "scripts/player.gd", "symbol": "no_such_function" }),
            ErrorKind::NotFound,
        ),
        // A malformed page request is refused deterministically.
        (
            "gdscript.symbols.get",
            json!({ "script": "scripts/player.gd", "limit": 0 }),
            ErrorKind::InvalidInput,
        ),
        (
            "gdscript.symbols.get",
            json!({ "script": "scripts/player.gd", "limit": 100_000 }),
            ErrorKind::InvalidInput,
        ),
        (
            "gdscript.symbols.get",
            json!({ "script": "scripts/player.gd", "offset": -1 }),
            ErrorKind::InvalidInput,
        ),
    ];
    for (tool, arguments, expected) in cases {
        let error = harness.err(tool, arguments.clone()).await;
        assert_eq!(
            error.kind(),
            expected,
            "{tool}{arguments} must fail as {expected:?}, got {error}"
        );
        assert!(
            !error.message().contains("SELECT"),
            "an error must not leak SQL: {error}"
        );
    }

    for tool in ["gdscript.references.get", "gdscript.impact.get"] {
        let error = harness
            .err(
                tool,
                json!({ "script": "scripts/player.gd", "symbol": "BUFF" }),
            )
            .await;
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.message(),
            "scripts/player.gd declares `BUFF` as `const`, but reference and impact queries resolve only functions, signals, and script classes"
        );
    }

    // A tool outside the profile is not found, not unavailable.
    let outside = harness
        .call("editor.scene.open", json!({}))
        .await
        .expect_err("an editor tool is absent from the core profile");
    assert_eq!(outside.kind(), ErrorKind::NotFound);

    // ---------------------------------------------------------------------
    // Cancellation is observed on the long, project-wide paths.
    // ---------------------------------------------------------------------
    for (tool, arguments) in [
        ("gdscript.diagnostics.get", json!({})),
        ("project.validate", json!({})),
        (
            "gdscript.impact.get",
            json!({ "script": "scripts/player.gd" }),
        ),
    ] {
        let error = harness.cancelled(tool, arguments).await;
        assert_eq!(
            error.kind(),
            ErrorKind::Cancelled,
            "{tool} must observe cancellation: {error}"
        );
    }

    assert_eq!(
        before,
        snapshot_tree(&committed_fixture()),
        "the committed fixture must not be mutated"
    );
}
