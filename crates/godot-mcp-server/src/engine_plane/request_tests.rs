use std::collections::BTreeSet;
use std::io::Write;
use std::process::{Command, Stdio};

use godot_mcp_domain::{CapabilityProfile, ErrorKind};
use godot_mcp_engine::validation::{PROJECT_RELATIVE_PATH_PATTERN, is_valid_project_relative_path};
use serde_json::{Map, Value, json};

use super::{CanonicalOperation, EngineCall as _, EngineOperation, EngineRequest};
use crate::kernel::ServiceKernel;

/// The schema a client sees for `operation`.
///
/// Discovery is the tool catalog: an operation is advertised exactly when the
/// catalog lists it, and its schema is read from the same owner discovery
/// uses.
fn advertised_schema(operation: EngineOperation) -> Map<String, Value> {
    let name = operation.tool_name();
    let listed = ServiceKernel::new(CapabilityProfile::Full)
        .tools()
        .into_iter()
        .find(|tool| tool.name == name);
    assert_eq!(
        listed.is_some(),
        godot_mcp_domain::catalog::descriptor(name).is_some(),
        "{name} must be advertised exactly when the catalog lists it"
    );
    listed.map_or_else(
        || crate::kernel::tool_schema(name, crate::core::CoreLimits::default()),
        |tool| tool.input_schema,
    )
}

fn string_set(values: impl IntoIterator<Item = impl AsRef<str>>) -> BTreeSet<String> {
    values
        .into_iter()
        .map(|value| value.as_ref().to_owned())
        .collect()
}

fn published_pattern_matches(pattern: &str, paths: &[&str]) -> Vec<bool> {
    // UTF-8 mode, so the paths on stdin decode the same way on every host: Windows
    // would otherwise read them in its ANSI code page, which turns a C1 control
    // into printable text and makes the regex accept what the runtime refuses.
    let mut child = Command::new("python3")
        .args([
            "-X",
            "utf8",
            "-c",
            "import json, re, sys; pattern = re.compile(sys.argv[1]); values = json.load(sys.stdin); json.dump([pattern.fullmatch(value) is not None for value in values], sys.stdout)",
            pattern,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("python3 must execute the published JSON Schema regex");
    serde_json::to_writer(child.stdin.as_mut().expect("python stdin is piped"), paths)
        .expect("serialize regex cases");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for regex evaluator");
    assert!(
        output.status.success(),
        "published regex evaluator failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("regex evaluator returns boolean JSON")
}

fn full_scalar_pattern_divergences() -> Vec<(u32, bool, bool)> {
    // UTF-8 mode, so the paths on stdin decode the same way on every host: Windows
    // would otherwise read them in its ANSI code page, which turns a C1 control
    // into printable text and makes the regex accept what the runtime refuses.
    let mut child = Command::new("python3")
        .args([
            "-X",
            "utf8",
            "-c",
            "import json, re, sys\npattern = re.compile(sys.argv[1])\nfor line in sys.stdin:\n    codepoint, value, runtime = json.loads(line)\n    published = pattern.fullmatch(value) is not None\n    if published != runtime:\n        print(json.dumps([codepoint, published, runtime]))",
            PROJECT_RELATIVE_PATH_PATTERN,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("python3 must execute the published JSON Schema regex");
    {
        let stdin = child.stdin.as_mut().expect("python stdin is piped");
        for codepoint in 0..=0x10_FFFF {
            let Some(character) = char::from_u32(codepoint) else {
                continue;
            };
            let path = format!("a{character}b");
            serde_json::to_writer(
                &mut *stdin,
                &(codepoint, &path, is_valid_project_relative_path(&path)),
            )
            .expect("serialize scalar regex case");
            stdin.write_all(b"\n").expect("terminate scalar regex case");
        }
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for regex evaluator");
    assert!(
        output.status.success(),
        "full-scalar regex evaluator failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::Deserializer::from_slice(&output.stdout)
        .into_iter()
        .collect::<Result<_, _>>()
        .expect("regex evaluator returns one JSON tuple per divergence")
}

fn operation_contracts() -> [(
    EngineOperation,
    &'static [&'static str],
    &'static [&'static str],
); 17] {
    [
        (
            EngineOperation::ImportProject,
            &["expected_artifacts", "assets", "deadline_ms"],
            &[],
        ),
        (
            EngineOperation::SceneCreate,
            &[
                "path",
                "rootType",
                "rootName",
                "operations",
                "expectedRevision",
                "dryRun",
            ],
            &["path", "idempotencyId"],
        ),
        (
            EngineOperation::SceneBatch,
            &[
                "path",
                "expectedSha256",
                "operations",
                "expectedRevision",
                "dryRun",
            ],
            &["path", "idempotencyId", "expectedSha256", "operations"],
        ),
        (
            EngineOperation::ResourceCreate,
            &["path", "class", "operations", "expectedRevision", "dryRun"],
            &["path", "idempotencyId"],
        ),
        (
            EngineOperation::ResourceBatch,
            &[
                "path",
                "expectedSha256",
                "operations",
                "expectedRevision",
                "dryRun",
            ],
            &["path", "idempotencyId", "expectedSha256", "operations"],
        ),
        (
            EngineOperation::UpdateUid,
            &["target", "resource_name", "dry_run"],
            &["target"],
        ),
        (
            EngineOperation::UpdateResource,
            &["target", "resource_name", "dry_run"],
            &["target"],
        ),
        (
            EngineOperation::SaveScene,
            &["target", "resource_name", "dry_run"],
            &["target"],
        ),
        (
            EngineOperation::RunProject,
            &["main_scene", "headless", "deadline_ms"],
            &[],
        ),
        (
            EngineOperation::RunTests,
            &["selection", "deadline_ms"],
            &[],
        ),
        (
            EngineOperation::CheckScript,
            &["script", "deadline_ms"],
            &["script"],
        ),
        (
            EngineOperation::RunScript,
            &["source", "context", "deadline_ms"],
            &["source", "idempotencyId"],
        ),
        (
            EngineOperation::SceneRead,
            &["path", "calls"],
            &["path", "calls"],
        ),
        (
            EngineOperation::ResourceRead,
            &["path", "calls"],
            &["path", "calls"],
        ),
        (EngineOperation::ShaderCheck, &["path"], &["path"]),
        (
            EngineOperation::ProjectBatch,
            &["files", "expectedRevision", "dryRun"],
            &["files", "idempotencyId"],
        ),
        (
            EngineOperation::SceneRecipe,
            &[
                "path",
                "expectedSha256",
                "recipe",
                "parent",
                "expectedRevision",
                "dryRun",
            ],
            &["path", "idempotencyId", "expectedSha256", "recipe"],
        ),
    ]
}

const RUN_SOURCE: &str = "extends RefCounted\n\nfunc run():\n\treturn 1\n";

fn valid_arguments(operation: EngineOperation) -> Map<String, Value> {
    match operation {
        EngineOperation::UpdateUid | EngineOperation::UpdateResource => {
            Map::from_iter([("target".to_owned(), json!("data/value.tres"))])
        }
        EngineOperation::SaveScene => {
            Map::from_iter([("target".to_owned(), json!("scenes/main.tscn"))])
        }
        EngineOperation::CheckScript => {
            Map::from_iter([("script".to_owned(), json!("scripts/main.gd"))])
        }
        EngineOperation::RunScript => Map::from_iter([("source".to_owned(), json!(RUN_SOURCE))]),
        EngineOperation::SceneRead => Map::from_iter([
            ("path".to_owned(), json!("scenes/main.tscn")),
            ("calls".to_owned(), json!([{ "method": "get_used_cells" }])),
        ]),
        EngineOperation::ResourceRead => Map::from_iter([
            ("path".to_owned(), json!("data/value.tres")),
            ("calls".to_owned(), json!([{ "method": "get_length" }])),
        ]),
        EngineOperation::ShaderCheck => {
            Map::from_iter([("path".to_owned(), json!("fx/glow.gdshader"))])
        }
        EngineOperation::ProjectBatch => Map::from_iter([(
            "files".to_owned(),
            json!([{
                "kind": "scene",
                "path": "scenes/main.tscn",
                "expectedSha256": "0".repeat(64),
                "operations": [{ "op": "add_node", "name": "Child" }],
            }]),
        )]),
        EngineOperation::SceneCreate => {
            Map::from_iter([("path".to_owned(), json!("scenes/new.tscn"))])
        }
        EngineOperation::ResourceCreate => {
            Map::from_iter([("path".to_owned(), json!("data/new.tres"))])
        }
        EngineOperation::SceneBatch => Map::from_iter([
            ("path".to_owned(), json!("scenes/main.tscn")),
            ("expectedSha256".to_owned(), json!("0".repeat(64))),
            (
                "operations".to_owned(),
                json!([{ "op": "add_node", "name": "Child" }]),
            ),
        ]),
        EngineOperation::ResourceBatch => Map::from_iter([
            ("path".to_owned(), json!("data/value.tres")),
            ("expectedSha256".to_owned(), json!("0".repeat(64))),
            (
                "operations".to_owned(),
                json!([{ "op": "clear_property", "property": "resource_name" }]),
            ),
        ]),
        EngineOperation::SceneRecipe => Map::from_iter([
            ("path".to_owned(), json!("scenes/main.tscn")),
            ("expectedSha256".to_owned(), json!("0".repeat(64))),
            ("recipe".to_owned(), json!({ "body-2d": {} })),
        ]),
        EngineOperation::ImportProject
        | EngineOperation::RunProject
        | EngineOperation::RunTests => Map::new(),
    }
}

#[derive(Debug, Clone, Copy)]
enum EnginePathField {
    ExpectedArtifact,
    ImportedAsset,
    Target,
    MainScene,
    Selection,
    Script,
}

impl EnginePathField {
    const ALL: [Self; 6] = [
        Self::ExpectedArtifact,
        Self::ImportedAsset,
        Self::Target,
        Self::MainScene,
        Self::Selection,
        Self::Script,
    ];

    const fn operation(self) -> EngineOperation {
        match self {
            Self::ExpectedArtifact => EngineOperation::ImportProject,
            Self::ImportedAsset => EngineOperation::ImportProject,
            Self::Target => EngineOperation::UpdateResource,
            Self::MainScene => EngineOperation::RunProject,
            Self::Selection => EngineOperation::RunTests,
            Self::Script => EngineOperation::CheckScript,
        }
    }

    const fn key(self) -> &'static str {
        match self {
            Self::ExpectedArtifact => "expected_artifacts",
            Self::ImportedAsset => "assets",
            Self::Target => "target",
            Self::MainScene => "main_scene",
            Self::Selection => "selection",
            Self::Script => "script",
        }
    }

    fn arguments(self, path: &str) -> Map<String, Value> {
        let value = match self {
            Self::ExpectedArtifact | Self::ImportedAsset | Self::Selection => json!([path]),
            Self::Target | Self::MainScene | Self::Script => json!(path),
        };
        Map::from_iter([(self.key().to_owned(), value)])
    }

    fn advertised_path_schema(self) -> Value {
        let schema = advertised_schema(self.operation());
        let property = &schema["properties"][self.key()];
        match self {
            Self::ExpectedArtifact | Self::ImportedAsset | Self::Selection => {
                property["items"].clone()
            }
            Self::Target | Self::MainScene | Self::Script => property.clone(),
        }
    }

    fn expected_pattern(self) -> Value {
        match self {
            Self::Selection => json!(super::test_selection_path_pattern()),
            Self::ExpectedArtifact
            | Self::ImportedAsset
            | Self::Target
            | Self::MainScene
            | Self::Script => {
                json!(godot_mcp_engine::validation::PROJECT_RELATIVE_PATH_PATTERN)
            }
        }
    }
}

#[test]
fn import_request_parses_expected_artifacts_and_deadline() {
    let arguments = Map::from_iter([
        (
            "expected_artifacts".to_owned(),
            json!([".godot/uid_cache.bin"]),
        ),
        ("deadline_ms".to_owned(), json!(45_000)),
    ]);

    let request = EngineRequest::parse(EngineOperation::ImportProject, arguments)
        .expect("valid import request");

    assert_eq!(
        request,
        EngineRequest::ImportProject {
            expected_artifacts: vec![".godot/uid_cache.bin".to_owned()],
            assets: Vec::new(),
            deadline_ms: 45_000,
        }
    );
}

#[test]
fn canonical_request_requires_a_project_relative_target() {
    let arguments = Map::from_iter([("target".to_owned(), json!("../outside.tres"))]);

    let error = EngineRequest::parse(EngineOperation::UpdateResource, arguments)
        .expect_err("escaping target must be refused");

    assert_eq!(error.kind(), ErrorKind::InvalidInput);
}

#[test]
fn canonical_update_operations_reject_create_only_root_arguments() {
    for operation in [
        EngineOperation::UpdateUid,
        EngineOperation::UpdateResource,
        EngineOperation::SaveScene,
    ] {
        let schema = advertised_schema(operation);
        let properties = schema["properties"].as_object().expect("properties object");

        for key in ["root_type", "root_name"] {
            assert!(
                !properties.contains_key(key),
                "{}.{} must not be advertised because update helpers do not read it",
                operation.tool_name(),
                key
            );
            let mut arguments = valid_arguments(operation);
            arguments.insert(key.to_owned(), json!("IgnoredValue"));
            let error = EngineRequest::parse(operation, arguments)
                .expect_err("a create-only argument must be rejected by update tools");
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
            assert!(
                error.message().contains(&format!("unknown field `{key}`")),
                "{}.{} returned an unrelated rejection: {}",
                operation.tool_name(),
                key,
                error.message()
            );
        }
    }
}

#[test]
fn every_engine_path_argument_uses_the_same_project_relative_constraint() {
    let cases = [
        ("main.tscn", true),
        ("scenes/main.tscn", true),
        ("res://scenes/main.tscn", true),
        (".godot/uid_cache.bin", true),
        ("", false),
        ("res://", false),
        ("res://res://x.import", false),
        (".godot\\uid_cache.bin", false),
        ("res://.godot\\uid_cache.bin", false),
        ("http://x", false),
        ("https://x.import", false),
        ("res://https://x.import", false),
        ("../outside.tscn", false),
        ("scenes/../outside.tscn", false),
        ("/etc/passwd", false),
        ("res:///etc/passwd", false),
        ("--editor", false),
        ("res://--editor", false),
        ("C:/outside.tscn", false),
        ("res://C:/outside.tscn", false),
        ("scenes/\u{0000}main.tscn", false),
        ("scenes/a\u{0085}b.tscn", false),
    ];

    for field in EnginePathField::ALL {
        let path_schema = field.advertised_path_schema();
        assert_eq!(path_schema["type"], json!("string"));
        assert_eq!(path_schema["minLength"], json!(1));
        assert_eq!(
            path_schema["pattern"],
            field.expected_pattern(),
            "{}.{} must advertise the shared project-relative path pattern",
            field.operation().tool_name(),
            field.key()
        );
        let paths = cases.map(|(path, _)| path);
        let published = published_pattern_matches(
            path_schema["pattern"]
                .as_str()
                .expect("path schema pattern is a string"),
            &paths,
        );

        for ((path, expected_valid), published_valid) in cases.into_iter().zip(published) {
            let result = EngineRequest::parse(field.operation(), field.arguments(path));
            assert_eq!(
                published_valid,
                result.is_ok(),
                "{}.{} published regex and runtime predicate disagree for path {path:?}: regex={published_valid}, runtime={result:?}",
                field.operation().tool_name(),
                field.key()
            );
            assert_eq!(
                result.is_ok(),
                expected_valid,
                "{}.{} path {path:?} runtime decision drifted: {result:?}",
                field.operation().tool_name(),
                field.key()
            );
            if let Err(error) = result {
                assert_eq!(error.kind(), ErrorKind::InvalidInput);
            }
        }
    }

    let valid = Map::from_iter([("main_scene".to_owned(), json!("main.tscn"))]);
    assert_eq!(
        EngineRequest::parse(EngineOperation::RunProject, valid).expect("valid main scene"),
        EngineRequest::RunProject {
            main_scene: Some("main.tscn".to_owned()),
            headless: true,
            deadline_ms: 120_000,
        }
    );

    let selection = Map::from_iter([("selection".to_owned(), json!(["res://tests/unit"]))]);
    assert_eq!(
        EngineRequest::parse(EngineOperation::RunTests, selection)
            .expect("a valid res:// test path must continue to framework detection"),
        EngineRequest::RunTests {
            selection: vec!["res://tests/unit".to_owned()],
            deadline_ms: 120_000,
        },
        "framework-facing paths retain their native res:// spelling"
    );

    let artifact = Map::from_iter([(
        "expected_artifacts".to_owned(),
        json!(["res://.godot/uid_cache.bin"]),
    )]);
    assert_eq!(
        EngineRequest::parse(EngineOperation::ImportProject, artifact)
            .expect("a valid res:// artifact"),
        EngineRequest::ImportProject {
            expected_artifacts: vec!["res://.godot/uid_cache.bin".to_owned()],
            assets: Vec::new(),
            deadline_ms: 60_000,
        },
        "artifact reports retain the caller's spelling"
    );

    let canonical = Map::from_iter([("target".to_owned(), json!("res://data/value.tres"))]);
    let EngineRequest::UpdateResource(canonical) =
        EngineRequest::parse(EngineOperation::UpdateResource, canonical)
            .expect("a valid res:// canonical target")
    else {
        panic!("resource.update must remain a canonical operation");
    };
    let canonical = canonical.into_request(CanonicalOperation::UpdateResource);
    assert_eq!(canonical.target, "data/value.tres");
    assert!(
        canonical.validate().is_ok(),
        "filesystem-facing canonical targets use project-relative spelling"
    );
}

/// A client reads a `pattern` as ECMA-262, whose `.` refuses the line
/// terminators `\n`, `\r`, U+2028 and U+2029. Every engine path schema gives
/// the runtime check's verdict on each of them, wherever it falls beside the
/// rules the check applies: the check takes U+2028 and U+2029, which are not
/// control characters, and refuses `\n` and `\r`, which are.
#[test]
fn every_engine_path_schema_reads_line_terminators_as_the_runtime_does() {
    let mut paths = Vec::new();
    for terminator in ['\n', '\r', '\u{2028}', '\u{2029}'] {
        for rest in [
            "b.gd", "/b.gd", "/../b.gd", "/..", "\\b.gd", "://b.gd", ",b.gd",
        ] {
            paths.push(format!("a{terminator}{rest}"));
            paths.push(format!("res://{terminator}{rest}"));
        }
    }
    for field in EnginePathField::ALL {
        let schema = field.advertised_path_schema();
        for path in &paths {
            let result = EngineRequest::parse(field.operation(), field.arguments(path));
            assert_eq!(
                crate::schema::conformance::conforms(&schema, &json!(path)),
                result.is_ok(),
                "{}.{} schema and runtime disagree on {path:?}: runtime={result:?}",
                field.operation().tool_name(),
                field.key()
            );
        }
    }
}

#[test]
fn published_project_relative_pattern_matches_runtime_for_every_unicode_scalar() {
    let divergences = full_scalar_pattern_divergences();
    assert!(
        divergences.is_empty(),
        "published regex and runtime predicate diverged; first mismatch: {}",
        divergences.first().map_or_else(
            || "none".to_owned(),
            |(codepoint, published, runtime)| format!(
                "U+{codepoint:04X} in {path:?}: regex={published}, runtime={runtime}",
                path = format!("a{}b", char::from_u32(*codepoint).expect("Unicode scalar"))
            )
        )
    );
}

#[test]
fn test_selection_rejects_element_internal_framework_separator() {
    for escaped in ["res://tests,../esc", "res://a,b"] {
        let arguments = Map::from_iter([("selection".to_owned(), json!([escaped]))]);
        let error = match EngineRequest::parse(EngineOperation::RunTests, arguments) {
            Ok(request) => {
                panic!("{escaped} must be refused before framework detection: {request:?}")
            }
            Err(error) => error,
        };

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(
            error.message().contains(escaped),
            "the rejection must name the offending selection: {error}"
        );
    }

    let control = Map::from_iter([("selection".to_owned(), json!(["res://tests/unit"]))]);
    assert!(
        EngineRequest::parse(EngineOperation::RunTests, control).is_ok(),
        "res://tests/unit must remain a valid selection"
    );

    let schema = advertised_schema(EngineOperation::RunTests);
    assert_eq!(
        schema["properties"]["selection"]["items"]["pattern"],
        json!(super::test_selection_path_pattern()),
        "the advertised item pattern and runtime separator source must not drift"
    );
}

#[test]
fn test_request_parses_selection_without_shell_arguments() {
    let arguments = Map::from_iter([(
        "selection".to_owned(),
        json!(["res://tests/unit", "res://tests/integration"]),
    )]);

    let request =
        EngineRequest::parse(EngineOperation::RunTests, arguments).expect("valid test request");

    assert_eq!(
        request,
        EngineRequest::RunTests {
            selection: vec![
                "res://tests/unit".to_owned(),
                "res://tests/integration".to_owned(),
            ],
            deadline_ms: 120_000,
        }
    );
}

#[test]
fn script_check_request_requires_a_project_relative_script_and_uses_its_default_deadline() {
    let arguments = Map::from_iter([("script".to_owned(), json!("res://scripts/main.gd"))]);

    let request = EngineRequest::parse(EngineOperation::CheckScript, arguments)
        .expect("valid script check request");

    assert_eq!(
        request,
        EngineRequest::CheckScript {
            script: "res://scripts/main.gd".to_owned(),
            deadline_ms: super::default_script_deadline_ms(),
        }
    );
    let missing = EngineRequest::parse(EngineOperation::CheckScript, Map::new())
        .expect_err("script.check requires a script path");
    assert_eq!(missing.kind(), ErrorKind::InvalidInput);
}

#[test]
fn non_nullable_engine_arguments_reject_explicit_null() {
    let schema = EngineOperation::RunProject.input_schema();
    assert_eq!(schema["properties"]["main_scene"]["type"], json!("string"));

    let arguments = Map::from_iter([("main_scene".to_owned(), json!(null))]);
    let error = EngineRequest::parse(EngineOperation::RunProject, arguments)
        .expect_err("a non-nullable engine argument must reject explicit null");

    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert_eq!(
        error.message(),
        "invalid engine request: `main_scene` must not be null"
    );

    for &operation in EngineOperation::ALL {
        let schema = operation.input_schema();
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{} properties must be an object", operation.tool_name()));
        for key in properties.keys() {
            let mut arguments = valid_arguments(operation);
            arguments.insert(key.clone(), Value::Null);
            let error = EngineRequest::parse(operation, arguments).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
            assert_eq!(
                error.message(),
                format!("invalid engine request: `{key}` must not be null"),
                "runtime null handling for `{}.{key}` drifted from its schema",
                operation.tool_name()
            );
        }
    }
}

#[test]
fn advertised_properties_and_required_keys_cover_all_engine_operations() {
    assert_eq!(operation_contracts().len(), EngineOperation::ALL.len());
    for (operation, local_properties, required) in operation_contracts() {
        let schema = advertised_schema(operation);
        assert_eq!(schema["additionalProperties"], json!(false));

        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{} properties must be an object", operation.tool_name()));
        let mut expected_properties = string_set(local_properties);
        expected_properties.insert("idempotencyId".to_owned());
        assert_eq!(
            string_set(properties.keys()),
            expected_properties,
            "advertised properties drifted for `{}`",
            operation.tool_name()
        );

        let advertised_required = schema["required"]
            .as_array()
            .unwrap_or_else(|| panic!("{} required must be an array", operation.tool_name()));
        assert_eq!(
            string_set(advertised_required.iter().map(|value| {
                value.as_str().unwrap_or_else(|| {
                    panic!("{} required keys must be strings", operation.tool_name())
                })
            })),
            string_set(required),
            "advertised required keys drifted for `{}`",
            operation.tool_name()
        );
    }
}

#[test]
fn every_engine_operation_rejects_unknown_keys() {
    for &operation in EngineOperation::ALL {
        let mut arguments = valid_arguments(operation);
        arguments.insert("unexpected".to_owned(), json!(true));
        let error = EngineRequest::parse(operation, arguments)
            .expect_err("an unknown engine argument must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(
            error.message().contains("unknown field `unexpected`"),
            "{} returned an unrelated unknown-key error: {}",
            operation.tool_name(),
            error.message()
        );
    }
}

#[test]
fn engine_arguments_reject_wrong_declared_types_and_array_element_types() {
    for &operation in EngineOperation::ALL {
        let schema = operation.input_schema();
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{} properties must be an object", operation.tool_name()));
        for (key, property) in properties {
            let declared_type = property["type"]
                .as_str()
                .unwrap_or_else(|| panic!("{}.{key} must declare one type", operation.tool_name()));
            let wrong_value = match declared_type {
                "string" => json!(123),
                "integer" => json!("not an integer"),
                "boolean" => json!("not a boolean"),
                "array" => json!("not an array"),
                "object" => json!("not an object"),
                other => panic!(
                    "{}.{} uses unsupported type {other}",
                    operation.tool_name(),
                    key
                ),
            };
            let mut arguments = valid_arguments(operation);
            arguments.insert(key.clone(), wrong_value);
            let error = EngineRequest::parse(operation, arguments)
                .expect_err("a wrong declared type must be rejected");
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
        }
    }

    for (operation, key) in [
        (EngineOperation::ImportProject, "expected_artifacts"),
        (EngineOperation::RunTests, "selection"),
    ] {
        let arguments = Map::from_iter([(key.to_owned(), json!([123]))]);
        let error = EngineRequest::parse(operation, arguments)
            .expect_err("a non-string array element must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(
            error.message().contains("expected a string"),
            "{}.{key} returned an unrelated element error: {}",
            operation.tool_name(),
            error.message()
        );
    }
}

#[test]
fn engine_deadlines_follow_json_schema_integer_semantics_and_bounds() {
    for &operation in EngineOperation::ALL {
        match operation {
            EngineOperation::ImportProject
            | EngineOperation::RunProject
            | EngineOperation::RunTests
            | EngineOperation::CheckScript
            | EngineOperation::RunScript => {
                for literal in ["1.0", "600000"] {
                    let value: Value =
                        serde_json::from_str(&format!(r#"{{"deadline_ms":{literal}}}"#))
                            .expect("literal JSON number");
                    let mut arguments = valid_arguments(operation);
                    arguments.extend(value.as_object().expect("arguments object").clone());
                    EngineRequest::parse(operation, arguments).unwrap_or_else(|error| {
                        panic!(
                            "{} must accept JSON Schema integer deadline {literal}: {error}",
                            operation.tool_name()
                        )
                    });
                }

                for literal in ["1.5", "0", "600001"] {
                    let value: Value =
                        serde_json::from_str(&format!(r#"{{"deadline_ms":{literal}}}"#))
                            .expect("literal JSON number");
                    let mut arguments = valid_arguments(operation);
                    arguments.extend(value.as_object().expect("arguments object").clone());
                    let error = EngineRequest::parse(operation, arguments)
                        .expect_err("an invalid deadline must be rejected");
                    assert_eq!(error.kind(), ErrorKind::InvalidInput);
                }
            }
            EngineOperation::UpdateUid
            | EngineOperation::UpdateResource
            | EngineOperation::SaveScene
            | EngineOperation::SceneCreate
            | EngineOperation::SceneBatch
            | EngineOperation::ResourceCreate
            | EngineOperation::ResourceBatch
            | EngineOperation::SceneRead
            | EngineOperation::ResourceRead
            | EngineOperation::ShaderCheck
            | EngineOperation::ProjectBatch
            | EngineOperation::SceneRecipe => assert!(
                !operation.input_schema()["properties"]
                    .as_object()
                    .expect("properties object")
                    .contains_key("deadline_ms"),
                "{} must explicitly remain deadline-free",
                operation.tool_name()
            ),
        }
    }
}

#[test]
fn advertised_defaults_equal_runtime_defaults_for_all_engine_operations() {
    for &operation in EngineOperation::ALL {
        let schema = advertised_schema(operation);
        match operation {
            EngineOperation::ImportProject => {
                assert_eq!(
                    schema["properties"]["expected_artifacts"]["default"],
                    json!([])
                );
                assert_eq!(schema["properties"]["assets"]["default"], json!([]));
                assert_eq!(
                    schema["properties"]["deadline_ms"]["default"],
                    json!(super::default_import_deadline_ms())
                );
                assert_eq!(
                    EngineRequest::parse(operation, Map::new()).expect("runtime defaults"),
                    EngineRequest::ImportProject {
                        expected_artifacts: Vec::new(),
                        assets: Vec::new(),
                        deadline_ms: super::default_import_deadline_ms(),
                    }
                );
            }
            EngineOperation::SceneCreate
            | EngineOperation::SceneBatch
            | EngineOperation::ResourceCreate
            | EngineOperation::ResourceBatch
            | EngineOperation::SceneRecipe => {
                assert!(
                    schema["properties"]["dryRun"].get("default").is_none(),
                    "an absent dryRun means a real write"
                );
                let EngineRequest::Author(author) =
                    EngineRequest::parse(operation, valid_arguments(operation))
                        .expect("authored runtime defaults")
                else {
                    panic!(
                        "{} must parse into an authored request",
                        operation.tool_name()
                    );
                };
                assert!(!author.dry_run);
                assert_eq!(author.expected_revision, None);
            }
            EngineOperation::UpdateUid
            | EngineOperation::UpdateResource
            | EngineOperation::SaveScene => {
                assert_eq!(schema["properties"]["dry_run"]["default"], json!(false));
                assert!(
                    schema["properties"]["resource_name"]
                        .get("default")
                        .is_none()
                );
                let request = EngineRequest::parse(operation, valid_arguments(operation))
                    .expect("canonical runtime defaults");
                let arguments = match request {
                    EngineRequest::UpdateUid(arguments)
                    | EngineRequest::UpdateResource(arguments)
                    | EngineRequest::SaveScene(arguments) => arguments,
                    other => panic!("unexpected canonical request: {other:?}"),
                };
                assert!(!arguments.dry_run);
                assert!(arguments.resource_name.is_none());
            }
            EngineOperation::RunProject => {
                assert_eq!(schema["properties"]["headless"]["default"], json!(true));
                assert_eq!(
                    schema["properties"]["deadline_ms"]["default"],
                    json!(super::default_run_deadline_ms())
                );
                assert_eq!(
                    EngineRequest::parse(operation, Map::new()).expect("runtime defaults"),
                    EngineRequest::RunProject {
                        main_scene: None,
                        headless: super::default_true(),
                        deadline_ms: super::default_run_deadline_ms(),
                    }
                );
            }
            EngineOperation::RunTests => {
                assert_eq!(schema["properties"]["selection"]["default"], json!([]));
                assert_eq!(
                    schema["properties"]["deadline_ms"]["default"],
                    json!(super::default_test_deadline_ms())
                );
                assert_eq!(
                    EngineRequest::parse(operation, Map::new()).expect("runtime defaults"),
                    EngineRequest::RunTests {
                        selection: Vec::new(),
                        deadline_ms: super::default_test_deadline_ms(),
                    }
                );
            }
            EngineOperation::CheckScript => {
                assert_eq!(
                    schema["properties"]["deadline_ms"]["default"],
                    json!(super::default_script_deadline_ms())
                );
                assert_eq!(
                    EngineRequest::parse(operation, valid_arguments(operation))
                        .expect("runtime defaults"),
                    EngineRequest::CheckScript {
                        script: "scripts/main.gd".to_owned(),
                        deadline_ms: super::default_script_deadline_ms(),
                    }
                );
            }
            EngineOperation::RunScript => {
                assert_eq!(schema["properties"]["context"]["default"], json!("project"));
                assert_eq!(
                    schema["properties"]["deadline_ms"]["default"],
                    json!(super::default_script_run_deadline_ms())
                );
                assert_eq!(
                    EngineRequest::parse(operation, valid_arguments(operation))
                        .expect("runtime defaults"),
                    EngineRequest::RunScript {
                        source: RUN_SOURCE.to_owned(),
                        context: super::ScriptContext::Project,
                        deadline_ms: super::default_script_run_deadline_ms(),
                    }
                );
            }
            EngineOperation::ShaderCheck => {
                assert!(schema["properties"]["path"].get("default").is_none());
                assert!(matches!(
                    EngineRequest::parse(operation, valid_arguments(operation))
                        .expect("a shader check parses"),
                    EngineRequest::CheckShader(_)
                ));
            }
            EngineOperation::SceneRead | EngineOperation::ResourceRead => {
                for property in ["path", "calls"] {
                    assert!(
                        schema["properties"][property].get("default").is_none(),
                        "{}.{property} has no default",
                        operation.tool_name()
                    );
                }
                assert!(matches!(
                    EngineRequest::parse(operation, valid_arguments(operation))
                        .expect("a read parses"),
                    EngineRequest::Read(_)
                ));
            }
            EngineOperation::ProjectBatch => {
                assert!(
                    schema["properties"]["dryRun"].get("default").is_none(),
                    "an absent dryRun means a real write"
                );
                let EngineRequest::Batches(batches) =
                    EngineRequest::parse(operation, valid_arguments(operation))
                        .expect("a multi-file batch parses")
                else {
                    panic!("project.batch.apply must parse into a multi-file batch");
                };
                assert!(!batches.dry_run);
                assert_eq!(batches.expected_revision, None);
            }
        }
    }
}

/// The `script.run` schema and the parser give the same verdict on every
/// case, and it is the intended one: a source of 1 to the most characters,
/// one of the two contexts and a deadline in range. The bound counts
/// characters, as a schema does.
#[test]
fn the_script_run_schema_and_the_parser_agree() {
    let longest = "é".repeat(super::MAX_SCRIPT_SOURCE_CHARS);
    let too_long = format!("{longest}x");
    let cases = [
        (json!({ "source": RUN_SOURCE }), true),
        (json!({ "source": RUN_SOURCE, "context": "copy" }), true),
        (
            json!({ "source": RUN_SOURCE, "context": "project", "deadline_ms": 1 }),
            true,
        ),
        (json!({ "source": longest }), true),
        (json!({ "source": "" }), false),
        (json!({ "source": too_long }), false),
        (
            json!({ "source": RUN_SOURCE, "context": "elsewhere" }),
            false,
        ),
        (json!({ "source": RUN_SOURCE, "context": 1 }), false),
        (json!({ "source": RUN_SOURCE, "deadline_ms": 0 }), false),
        (json!({ "source": 5 }), false),
        (json!({ "context": "copy" }), false),
    ];
    let mut schema = advertised_schema(EngineOperation::RunScript);
    // The kernel takes the idempotency identifier off before the handler
    // parses; the schema is checked on what the handler sees.
    schema["required"] = json!(["source"]);
    let schema = Value::Object(schema);
    for (case, expected) in &cases {
        assert_eq!(
            crate::schema::conformance::conforms(&schema, case),
            *expected,
            "the schema's verdict on {}",
            case.to_string().chars().take(120).collect::<String>()
        );
        assert_eq!(
            EngineRequest::parse(
                EngineOperation::RunScript,
                case.as_object().expect("arguments").clone()
            )
            .is_ok(),
            *expected,
            "the parser's verdict on {}",
            case.to_string().chars().take(120).collect::<String>()
        );
    }
}

/// A request the recipe tool's schema and parser judge, beside the fields
/// every valid request carries.
fn recipe_request(extra: Value) -> Value {
    let mut request = json!({
        "path": "scenes/main.tscn",
        "expectedSha256": "0".repeat(64),
    });
    for (key, value) in extra.as_object().expect("extra fields") {
        request[key] = value.clone();
    }
    request
}

/// The `scene.recipe.apply` schema and the parser give the same verdict on
/// every case, and it is the intended one: one scene recipe keyed by its name,
/// parameters of the right shape, range and variant, and a node path parent.
#[test]
fn the_scene_recipe_schema_and_the_parser_agree() {
    let long_name = "名".repeat(godot_mcp_domain::recipes::MAX_NAME_CHARS);
    let longer_name = format!("{long_name}x");
    let long_parent = "a".repeat(godot_mcp_domain::recipes::MAX_PARENT_CHARS);
    let cells: Vec<Value> = (0..4096)
        .map(|index| json!({ "x": index % 100, "y": 0, "z": index / 100 }))
        .collect();
    let too_many_cells: Vec<Value> = (0..4097)
        .map(|index| json!({ "x": index % 100, "y": 0, "z": index / 100 }))
        .collect();
    let cases = [
        // The recipe object.
        (json!({ "recipe": { "body-2d": {} } }), true),
        (json!({ "recipe": {} }), false),
        (json!({ "recipe": { "body-2d": {}, "body-3d": {} } }), false),
        (json!({ "recipe": { "theme": {} } }), false),
        (json!({ "recipe": { "bodies": {} } }), false),
        (json!({ "recipe": "body-2d" }), false),
        (json!({ "recipe": { "body-2d": [] } }), false),
        (json!({}), false),
        (
            json!({ "recipe": { "body-2d": {} }, "operations": [] }),
            false,
        ),
        // Variants and their parameters.
        (json!({ "recipe": { "body-2d": { "height": 5 } } }), true),
        (
            json!({ "recipe": { "body-2d": { "shape": "capsule", "height": 5 } } }),
            true,
        ),
        (
            json!({ "recipe": { "body-2d": { "shape": "circle", "height": 5 } } }),
            false,
        ),
        (
            json!({ "recipe": { "body-2d": { "shape": "circle", "radius": 5 } } }),
            true,
        ),
        (
            json!({ "recipe": { "body-2d": { "size": { "x": 4, "y": 4 } } } }),
            false,
        ),
        (
            json!({ "recipe": { "body-2d": { "shape": "rectangle", "size": { "x": 4, "y": 4 } } } }),
            true,
        ),
        (
            json!({ "recipe": { "body-2d": { "shape": "rectangle", "size": { "x": 0, "y": 4 } } } }),
            false,
        ),
        (json!({ "recipe": { "body-2d": { "mass": 2 } } }), false),
        (
            json!({ "recipe": { "body-2d": { "body": "rigid", "mass": 2 } } }),
            true,
        ),
        (
            json!({ "recipe": { "body-2d": { "body": "rigid", "mass": 0 } } }),
            false,
        ),
        (
            json!({ "recipe": { "body-2d": { "ray_length": 8 } } }),
            false,
        ),
        (
            json!({ "recipe": { "body-2d": { "ground_ray": true, "ray_length": 8 } } }),
            true,
        ),
        (
            json!({ "recipe": { "body-2d": { "ground_ray": false, "ray_mask": 1 } } }),
            false,
        ),
        (
            json!({ "recipe": { "body-2d": { "collision_layer": 4_294_967_295_u64 } } }),
            true,
        ),
        (
            json!({ "recipe": { "body-2d": { "collision_layer": 4_294_967_296_u64 } } }),
            false,
        ),
        (
            json!({ "recipe": { "body-2d": { "collision_layer": 2.0 } } }),
            true,
        ),
        (
            json!({ "recipe": { "body-2d": { "collision_layer": 2.5 } } }),
            false,
        ),
        (
            json!({ "recipe": { "body-2d": { "collision_layer": -1 } } }),
            false,
        ),
        (
            json!({ "recipe": { "body-2d": { "body": "kinematic" } } }),
            false,
        ),
        (json!({ "recipe": { "body-2d": { "colour": 1 } } }), false),
        (
            json!({ "recipe": { "body-2d": { "radius": null } } }),
            false,
        ),
        (json!({ "recipe": { "body-3d": { "mesh": false } } }), true),
        (
            json!({ "recipe": { "body-3d": { "mesh": false, "roughness": 0.5 } } }),
            false,
        ),
        (
            json!({ "recipe": { "body-3d": { "roughness": 0.5 } } }),
            true,
        ),
        (
            json!({ "recipe": { "body-3d": { "roughness": 1.5 } } }),
            false,
        ),
        (
            json!({ "recipe": { "body-3d": { "body": "area", "friction": 0.5 } } }),
            false,
        ),
        (
            json!({ "recipe": { "body-3d": { "body": "static", "friction": 0.5 } } }),
            true,
        ),
        (
            json!({ "recipe": { "body-3d": { "color": { "red": 1, "green": 1, "blue": 1 } } } }),
            false,
        ),
        (
            json!({ "recipe": { "body-3d": { "color": { "red": 1, "green": 1, "blue": 1, "alpha": 17 } } } }),
            false,
        ),
        (
            json!({ "recipe": { "body-3d": { "shape": "sphere", "height": 2 } } }),
            false,
        ),
        (
            json!({ "recipe": { "body-3d": { "shape": "cylinder", "height": 2 } } }),
            true,
        ),
        (
            json!({ "recipe": { "light-3d": { "sun": false, "lamp": false, "sky": false } } }),
            true,
        ),
        (
            json!({ "recipe": { "light-3d": { "sun": false, "lamp": false, "sky": false, "camera": false } } }),
            false,
        ),
        (
            json!({ "recipe": { "light-3d": { "sun": false, "shadows": true } } }),
            false,
        ),
        (json!({ "recipe": { "light-3d": { "fov": 0.5 } } }), false),
        (
            json!({ "recipe": { "light-3d": { "name": "Stage" } } }),
            false,
        ),
        (
            json!({ "recipe": { "particles-2d": { "preset": "snow", "width": 300 } } }),
            true,
        ),
        (
            json!({ "recipe": { "particles-2d": { "width": 300 } } }),
            false,
        ),
        (
            json!({ "recipe": { "particles-2d": { "preset": "smoke", "width": 300 } } }),
            false,
        ),
        (
            json!({ "recipe": { "particles-2d": { "preset": "hail" } } }),
            false,
        ),
        (
            json!({ "recipe": { "particles-2d": { "amount": 0 } } }),
            false,
        ),
        (
            json!({ "recipe": { "particles-2d": { "texture": "res://fx/dot.png" } } }),
            true,
        ),
        (
            json!({ "recipe": { "particles-2d": { "texture": "fx/dot.png" } } }),
            false,
        ),
        (
            json!({ "recipe": { "particles-2d": { "texture": "res://fx//dot.png" } } }),
            false,
        ),
        (
            json!({ "recipe": { "particles-2d": { "texture": "res://fx/../dot.png" } } }),
            false,
        ),
        (
            json!({ "recipe": { "particles-2d": { "texture": "res://fx\\dot.png" } } }),
            false,
        ),
        (
            json!({ "recipe": { "navigation-2d": { "walker": false } } }),
            true,
        ),
        (
            json!({ "recipe": { "navigation-2d": { "walker": false, "avoidance": true } } }),
            false,
        ),
        (
            json!({ "recipe": { "navigation-2d": { "navigation_layers": 0 } } }),
            false,
        ),
        (json!({ "recipe": { "audio-2d": {} } }), false),
        (
            json!({ "recipe": { "audio-2d": { "stream": "res://audio/step.wav" } } }),
            true,
        ),
        (
            json!({ "recipe": { "audio-2d": { "stream": "res://audio/step.wav", "bus": "" } } }),
            false,
        ),
        (
            json!({ "recipe": { "audio-2d": { "stream": "res://audio/step.wav", "volume_db": 30 } } }),
            false,
        ),
        (
            json!({ "recipe": { "shader-2d": { "shader": "res://shaders/tint.gdshader" } } }),
            true,
        ),
        (
            json!({ "recipe": { "shader-2d": { "shader": "res://shaders/tint.GDSHADER" } } }),
            false,
        ),
        (
            json!({ "recipe": { "shader-2d": { "shader": "res://shaders/tint.txt" } } }),
            false,
        ),
        (
            json!({ "recipe": { "shader-2d": { "shader": "res://shaders/a\nb.gdshader" } } }),
            true,
        ),
        (
            json!({ "recipe": { "shader-2d": { "shader": "res://..gdshader" } } }),
            true,
        ),
        (
            json!({ "recipe": { "shader-2d": { "shader": "res://shaders/.gdshader" } } }),
            true,
        ),
        (
            json!({ "recipe": { "shader-2d": { "shader": "res://shaders//t.gdshader" } } }),
            false,
        ),
        (
            json!({ "recipe": { "shader-2d": { "shader": "res://../t.gdshader" } } }),
            false,
        ),
        (
            json!({ "recipe": { "shader-2d": { "shader": "res://t.gdshader/x" } } }),
            false,
        ),
        (
            json!({ "recipe": { "particles-2d": { "texture": "res://fx/a\nb.png" } } }),
            true,
        ),
        (
            json!({ "recipe": { "hud": { "score_text": "line one\nline two" } } }),
            true,
        ),
        (
            json!({ "recipe": { "shader-2d": {
                "shader": "res://s.gdshader",
                "uniforms": {
                    "strength": { "type": "float", "value": 0.5 },
                    "count": { "type": "integer", "value": 3 },
                    "on": { "type": "bool", "value": true },
                    "tint": { "type": "color", "value": { "red": 1, "green": 0.5, "blue": 0.2, "alpha": 1 } },
                    "offset": { "type": "vector2", "value": { "x": 1, "y": 2 } },
                    "noise": { "type": "resource", "value": { "resource": "res://n.png", "resource_type": "Texture2D" } }
                }
            } } }),
            true,
        ),
        (
            json!({ "recipe": { "shader-2d": {
                "shader": "res://s.gdshader",
                "uniforms": { "count": { "type": "integer", "value": 3.5 } }
            } } }),
            false,
        ),
        (
            json!({ "recipe": { "shader-2d": {
                "shader": "res://s.gdshader",
                "uniforms": { "1x": { "type": "float", "value": 1 } }
            } } }),
            false,
        ),
        (
            json!({ "recipe": { "shader-2d": {
                "shader": "res://s.gdshader",
                "uniforms": { "x": { "type": "string", "value": "a" } }
            } } }),
            false,
        ),
        (
            json!({ "recipe": { "shader-2d": { "shader": "res://s.gdshader", "uniforms": {} } } }),
            false,
        ),
        (
            json!({ "recipe": { "shader-2d": {
                "shader": "res://s.gdshader",
                "uniforms": { "noise": { "type": "resource", "value": { "resource": "res://n.png", "resource_type": "Texture 2D" } } }
            } } }),
            false,
        ),
        (json!({ "recipe": { "hud": { "score_text": "" } } }), true),
        (
            json!({ "recipe": { "hud": { "score_font_size": 0 } } }),
            false,
        ),
        (
            json!({ "recipe": { "gridmap-3d": { "cells": cells } } }),
            true,
        ),
        (
            json!({ "recipe": { "gridmap-3d": { "cells": too_many_cells } } }),
            false,
        ),
        (
            json!({ "recipe": { "gridmap-3d": { "cells": [] } } }),
            false,
        ),
        (
            json!({ "recipe": { "gridmap-3d": { "cells": [{ "x": 0, "y": 0, "z": 32768 }] } } }),
            false,
        ),
        (
            json!({ "recipe": { "gridmap-3d": { "cells": [{ "x": 0, "y": 0 }] } } }),
            false,
        ),
        (
            json!({ "recipe": { "joystick": { "actions": {} } } }),
            false,
        ),
        (
            json!({ "recipe": { "joystick": { "actions": { "left": "move_left" } } } }),
            true,
        ),
        (
            json!({ "recipe": { "joystick": { "actions": { "jump": "jump" } } } }),
            false,
        ),
        (
            json!({ "recipe": { "joystick": { "mode": "free" } } }),
            false,
        ),
        // Names and parents.
        (json!({ "recipe": { "body-2d": { "name": "Ball" } } }), true),
        (
            json!({ "recipe": { "body-2d": { "name": long_name } } }),
            true,
        ),
        (
            json!({ "recipe": { "body-2d": { "name": longer_name } } }),
            false,
        ),
        (json!({ "recipe": { "body-2d": { "name": "" } } }), false),
        (json!({ "recipe": { "body-2d": { "name": "a.b" } } }), false),
        (
            json!({ "recipe": { "body-2d": { "name": "a\u{1}b" } } }),
            false,
        ),
        (json!({ "recipe": { "body-2d": {} }, "parent": "." }), true),
        (
            json!({ "recipe": { "body-2d": {} }, "parent": "Level/Room" }),
            true,
        ),
        (
            json!({ "recipe": { "body-2d": {} }, "parent": long_parent }),
            true,
        ),
        (
            json!({ "recipe": { "body-2d": {} }, "parent": format!("{long_parent}a") }),
            false,
        ),
        (
            json!({ "recipe": { "body-2d": {} }, "parent": "Level/..x" }),
            true,
        ),
        (json!({ "recipe": { "body-2d": {} }, "parent": "" }), false),
        (
            json!({ "recipe": { "body-2d": {} }, "parent": "/Level" }),
            false,
        ),
        (
            json!({ "recipe": { "body-2d": {} }, "parent": "Level//Room" }),
            false,
        ),
        (
            json!({ "recipe": { "body-2d": {} }, "parent": "Level/.." }),
            false,
        ),
        (
            json!({ "recipe": { "body-2d": {} }, "parent": "Level/." }),
            false,
        ),
        (
            json!({ "recipe": { "body-2d": {} }, "parent": "nh1:0123" }),
            false,
        ),
        (json!({ "recipe": { "body-2d": {} }, "parent": 3 }), false),
        (json!({ "recipe": { "body-2d": {} }, "dryRun": true }), true),
        (
            json!({ "recipe": { "body-2d": {} }, "dryRun": "yes" }),
            false,
        ),
    ];
    let mut schema = advertised_schema(EngineOperation::SceneRecipe);
    // The kernel takes the idempotency identifier off before the handler
    // parses; the schema is checked on what the handler sees.
    schema["required"] = json!(["path", "expectedSha256", "recipe"]);
    let schema = Value::Object(schema);
    for (extra, expected) in &cases {
        let case =
            if extra.get("recipe").is_some() || extra.as_object().is_some_and(|o| o.is_empty()) {
                recipe_request(extra.clone())
            } else {
                extra.clone()
            };
        let shown = case.to_string().chars().take(160).collect::<String>();
        assert_eq!(
            crate::schema::conformance::conforms(&schema, &case),
            *expected,
            "the schema's verdict on {shown}"
        );
        assert_eq!(
            EngineRequest::parse(
                EngineOperation::SceneRecipe,
                case.as_object().expect("arguments").clone()
            )
            .is_ok(),
            *expected,
            "the parser's verdict on {shown}"
        );
    }
}

/// A recipe request becomes the scene batch its recipe expands to, under the
/// recipe tool's own name, and carries its recipe into the answer.
#[test]
fn a_recipe_request_is_the_scene_batch_its_recipe_expands_to() {
    let arguments = recipe_request(json!({
        "recipe": { "particles-2d": { "preset": "smoke", "amount": 12 } },
        "parent": "Level",
        "dryRun": true,
    }));
    let EngineRequest::Author(author) = EngineRequest::parse(
        EngineOperation::SceneRecipe,
        arguments.as_object().expect("arguments").clone(),
    )
    .expect("a recipe request parses") else {
        panic!("scene.recipe.apply must parse into an authored request");
    };
    assert_eq!(author.tool(), "scene.recipe.apply");
    assert_eq!(author.kind, super::authored::AuthorKind::SceneBatch);
    assert!(author.dry_run);
    let expansion = godot_mcp_domain::recipes::recipe("particles-2d")
        .expect("the particles recipe")
        .expand(
            json!({ "preset": "smoke", "amount": 12 })
                .as_object()
                .expect("parameters"),
            Some("Level"),
        )
        .expect("the parameters are accepted");
    assert_eq!(
        serde_json::from_str::<Value>(&author.batch).expect("the batch is JSON"),
        Value::Array(expansion.operations.clone())
    );
    let mut answer = Map::new();
    author.annotate(&mut answer);
    assert_eq!(answer["recipe"], expansion.describe());
    assert_eq!(answer["recipe"]["parameters"]["name"], json!("Smoke"));
    assert_eq!(answer["recipe"]["parameters"]["lifetime"], json!(2.5));
    assert_eq!(answer["operations"], Value::Array(expansion.operations));

    for (recipe, needle) in [
        (json!({ "theme": {} }), "resource.create"),
        (json!({ "fog": {} }), "there is no recipe \"fog\""),
        (
            json!({ "body-2d": { "shape": "circle", "height": 3 } }),
            "applies only when",
        ),
    ] {
        let error = EngineRequest::parse(
            EngineOperation::SceneRecipe,
            recipe_request(json!({ "recipe": recipe }))
                .as_object()
                .expect("arguments")
                .clone(),
        )
        .expect_err("the request is refused");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(error.to_string().contains(needle), "{error}");
    }
}
