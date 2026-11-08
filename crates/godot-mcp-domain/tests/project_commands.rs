//! The project command declaration (`godot_mcp_commands.json`): what the
//! server accepts, every limit it holds a declaration to, how a command whose
//! input schema leaves the validator's subset is listed but not callable, and
//! how arguments are checked and spelled for the editor bridge.

use std::path::Path;

use godot_mcp_domain::contracts::GodotValue;
use godot_mcp_domain::jsonschema;
use godot_mcp_domain::project_commands::{
    self, DECLARATION_FILE, Declaration, MAX_ARGUMENTS_BYTES, MAX_COMMANDS, MAX_DECLARATION_BYTES,
    MAX_DESCRIPTION_CHARS, MAX_INPUT_SCHEMA_BYTES, MAX_NAME_CHARS, MAX_PROVIDERS,
    MAX_SCRIPT_PATH_CHARS, MAX_TITLE_CHARS,
};
use serde_json::{Map, Value, json};

fn command(name: &str) -> Value {
    json!({
        "name": name,
        "title": "Rebuild the level",
        "description": "Regenerates the level from its seed.",
        "input_schema": {
            "type": "object",
            "additionalProperties": false,
            "required": ["seed"],
            "properties": {
                "seed": { "type": "integer", "minimum": 0 },
                "name": { "type": "string", "maxLength": 32 }
            }
        },
        "mutates": true
    })
}

fn declaration(providers: Value) -> Value {
    json!({ "version": 1, "providers": providers })
}

fn one_command() -> Value {
    declaration(json!([{
        "id": "levels",
        "script": "res://tools/level_commands.gd",
        "commands": [command("rebuild")]
    }]))
}

fn parse(value: &Value) -> Declaration {
    Declaration::parse(&serde_json::to_vec(value).expect("the fixture serializes"))
}

/// The one problem `value` has, as `(pointer, message)`.
fn only_problem(value: &Value) -> (String, String) {
    let parsed = parse(value);
    assert!(!parsed.is_valid(), "{value} is refused");
    assert_eq!(
        parsed.problems.len(),
        1,
        "one problem: {:?}",
        parsed.problems
    );
    let problem = &parsed.problems[0];
    (problem.pointer.clone(), problem.message.clone())
}

fn arguments(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("arguments are an object, not {other}"),
    }
}

#[test]
fn the_declaration_lives_at_the_project_root() {
    assert_eq!(DECLARATION_FILE, "godot_mcp_commands.json");
}

#[test]
fn a_valid_declaration_lists_its_providers_and_commands() {
    let mut value = one_command();
    value["$schema"] = json!("./schemas/project-commands-v1.schema.json");
    value["providers"]
        .as_array_mut()
        .expect("providers")
        .push(json!({
            "id": "qa.tools",
            "script": "res://addons/qa/qa_commands.gd",
            "commands": [command("count-nodes"), command("snapshot_v2")]
        }));
    let parsed = parse(&value);
    assert!(parsed.is_valid(), "{:?}", parsed.problems);
    assert_eq!(parsed.command_count(), 3);
    assert_eq!(parsed.providers.len(), 2);
    let (provider, found) = parsed.find("qa.tools", "count-nodes").expect("declared");
    assert_eq!(provider.script, "res://addons/qa/qa_commands.gd");
    assert_eq!(found.title, "Rebuild the level");
    assert!(found.mutates);
    assert_eq!(found.unavailable, None);
    assert!(parsed.find("qa.tools", "rebuild").is_none());
    assert!(parsed.find("levels", "count-nodes").is_none());
}

#[test]
fn an_empty_provider_list_is_a_valid_declaration_of_nothing() {
    let parsed = parse(&declaration(json!([])));
    assert!(parsed.is_valid());
    assert_eq!(parsed.command_count(), 0);
}

#[test]
fn bytes_that_are_not_a_declaration_are_refused_at_the_root() {
    for bytes in [
        &b"not json"[..],
        b"[1, 2]",
        b"\xef\xbb\xbf{\"version\": 1, \"providers\": []}",
    ] {
        let parsed = Declaration::parse(bytes);
        assert!(!parsed.is_valid());
        assert_eq!(parsed.problems[0].pointer, "/");
        assert!(parsed.providers.is_empty());
    }
}

#[test]
fn a_file_over_the_size_limit_is_refused_before_it_is_parsed() {
    assert_eq!(MAX_DECLARATION_BYTES, 262_144);
    let mut bytes = serde_json::to_vec(&one_command()).expect("serializes");
    bytes.extend(std::iter::repeat_n(
        b' ',
        MAX_DECLARATION_BYTES + 1 - bytes.len(),
    ));
    let parsed = Declaration::parse(&bytes);
    assert!(!parsed.is_valid());
    assert_eq!(parsed.problems[0].pointer, "/");
    assert!(
        parsed.problems[0].message.contains("262144"),
        "{:?}",
        parsed.problems
    );
    bytes.pop();
    assert!(
        Declaration::parse(&bytes).is_valid(),
        "exactly the limit is accepted"
    );
}

#[test]
fn every_structural_rule_is_reported_where_it_is_broken() {
    let cases: Vec<(Value, &str)> = vec![
        (json!({ "version": 2, "providers": [] }), "/version"),
        (json!({ "providers": [] }), "/version"),
        (json!({ "version": 1 }), "/providers"),
        (
            json!({ "version": 1, "providers": [], "extra": true }),
            "/extra",
        ),
        (
            json!({ "version": 1, "providers": [], "$schema": 7 }),
            "/$schema",
        ),
        (declaration(json!({})), "/providers"),
        (declaration(json!([7])), "/providers/0"),
        (
            declaration(
                json!([{ "id": "a", "script": "res://a.gd", "commands": [command("x")], "more": 1 }]),
            ),
            "/providers/0/more",
        ),
        (
            declaration(json!([{ "id": "a", "script": "res://a.gd", "commands": [] }])),
            "/providers/0/commands",
        ),
        (
            declaration(json!([{ "id": "a", "script": "res://a.gd" }])),
            "/providers/0/commands",
        ),
        (
            declaration(json!([
                { "id": "a", "script": "res://a.gd", "commands": [command("x")] },
                { "id": "a", "script": "res://b.gd", "commands": [command("y")] }
            ])),
            "/providers/1/id",
        ),
        (
            declaration(
                json!([{ "id": "a", "script": "res://a.gd", "commands": [command("x"), command("x")] }]),
            ),
            "/providers/0/commands/1/name",
        ),
    ];
    for (value, pointer) in cases {
        let (found, message) = only_problem(&value);
        assert_eq!(found, pointer, "{value}: {message}");
    }
}

#[test]
fn every_command_field_is_required_and_typed() {
    for (field, replacement) in [
        ("name", Value::Null),
        ("title", json!(1)),
        ("description", json!(["x"])),
        ("input_schema", json!("object")),
        ("mutates", json!("yes")),
    ] {
        let mut broken = command("x");
        broken[field] = replacement;
        let value =
            declaration(json!([{ "id": "a", "script": "res://a.gd", "commands": [broken] }]));
        assert_eq!(
            only_problem(&value).0,
            format!("/providers/0/commands/0/{field}")
        );

        let mut missing = command("x");
        missing.as_object_mut().expect("object").remove(field);
        let value =
            declaration(json!([{ "id": "a", "script": "res://a.gd", "commands": [missing] }]));
        assert_eq!(
            only_problem(&value).0,
            format!("/providers/0/commands/0/{field}")
        );
    }
    let mut extra = command("x");
    extra["hidden"] = json!(true);
    let value = declaration(json!([{ "id": "a", "script": "res://a.gd", "commands": [extra] }]));
    assert_eq!(only_problem(&value).0, "/providers/0/commands/0/hidden");
}

#[test]
fn names_and_ids_are_lowercase_identifiers_of_at_most_64_characters() {
    assert_eq!(MAX_NAME_CHARS, 64);
    for name in [
        "a",
        "rebuild",
        "count-nodes",
        "snapshot_v2",
        "qa.tools",
        &"a".repeat(64),
    ] {
        assert!(project_commands::valid_name(name), "{name}");
    }
    for name in [
        "",
        "Rebuild",
        "1a",
        "-a",
        "a b",
        "a/b",
        "ü",
        &"a".repeat(65),
    ] {
        assert!(!project_commands::valid_name(name), "{name}");
        let value = declaration(
            json!([{ "id": "a", "script": "res://a.gd", "commands": [command(name)] }]),
        );
        assert_eq!(
            only_problem(&value).0,
            "/providers/0/commands/0/name",
            "{name}"
        );
        let value = declaration(
            json!([{ "id": name, "script": "res://a.gd", "commands": [command("x")] }]),
        );
        assert_eq!(only_problem(&value).0, "/providers/0/id", "{name}");
    }
}

#[test]
fn a_provider_script_is_a_gdscript_file_inside_the_project() {
    assert_eq!(MAX_SCRIPT_PATH_CHARS, 1_024);
    let long_ok = format!("res://{}.gd", "a".repeat(MAX_SCRIPT_PATH_CHARS - 9));
    for script in [
        "res://a.gd",
        "res://tools/level commands.gd",
        "res://addons/qa/ü.gd",
        &long_ok,
    ] {
        let value =
            declaration(json!([{ "id": "a", "script": script, "commands": [command("x")] }]));
        assert!(parse(&value).is_valid(), "{script}");
    }
    let long_bad = format!("res://{}.gd", "a".repeat(MAX_SCRIPT_PATH_CHARS - 8));
    for script in [
        "tools/a.gd",
        "/tmp/a.gd",
        "user://a.gd",
        "res://a.tscn",
        "res://a.gd.txt",
        "res://.gd",
        "res://../a.gd",
        "res://tools/../a.gd",
        "res://tools/./a.gd",
        "res://tools//a.gd",
        "res://.godot/a.gd",
        "res://tools\\a.gd",
        "res://a.gd/",
        &long_bad,
    ] {
        let value =
            declaration(json!([{ "id": "a", "script": script, "commands": [command("x")] }]));
        assert_eq!(only_problem(&value).0, "/providers/0/script", "{script}");
    }
}

#[test]
fn titles_and_descriptions_are_bounded_and_not_empty() {
    assert_eq!(MAX_TITLE_CHARS, 128);
    assert_eq!(MAX_DESCRIPTION_CHARS, 1_024);
    for (field, limit) in [
        ("title", MAX_TITLE_CHARS),
        ("description", MAX_DESCRIPTION_CHARS),
    ] {
        let mut at_limit = command("x");
        at_limit[field] = json!("é".repeat(limit));
        let value =
            declaration(json!([{ "id": "a", "script": "res://a.gd", "commands": [at_limit] }]));
        assert!(parse(&value).is_valid(), "{field} of {limit} characters");
        for text in [String::new(), "é".repeat(limit + 1)] {
            let mut broken = command("x");
            broken[field] = json!(text);
            let value =
                declaration(json!([{ "id": "a", "script": "res://a.gd", "commands": [broken] }]));
            assert_eq!(
                only_problem(&value).0,
                format!("/providers/0/commands/0/{field}")
            );
        }
    }
}

#[test]
fn an_input_schema_is_bounded_by_its_serialized_size() {
    assert_eq!(MAX_INPUT_SCHEMA_BYTES, 16_384);
    let schema_of = |description_chars: usize| json!({ "type": "object", "description": "a".repeat(description_chars) });
    let overhead = serde_json::to_vec(&schema_of(0)).expect("serializes").len();
    let mut at_limit = command("x");
    at_limit["input_schema"] = schema_of(MAX_INPUT_SCHEMA_BYTES - overhead);
    let value = declaration(json!([{ "id": "a", "script": "res://a.gd", "commands": [at_limit] }]));
    assert!(parse(&value).is_valid());
    let mut over = command("x");
    over["input_schema"] = schema_of(MAX_INPUT_SCHEMA_BYTES - overhead + 1);
    let value = declaration(json!([{ "id": "a", "script": "res://a.gd", "commands": [over] }]));
    assert_eq!(
        only_problem(&value).0,
        "/providers/0/commands/0/input_schema"
    );
}

#[test]
fn the_provider_and_command_counts_are_bounded() {
    assert_eq!(MAX_PROVIDERS, 16);
    assert_eq!(MAX_COMMANDS, 64);
    let providers = |count: usize, commands_each: usize| -> Value {
        Value::Array(
            (0..count)
                .map(|provider| {
                    json!({
                        "id": format!("p{provider}"),
                        "script": format!("res://p{provider}.gd"),
                        "commands": (0..commands_each).map(|index| command(&format!("c{index}"))).collect::<Vec<_>>()
                    })
                })
                .collect(),
        )
    };
    assert!(
        parse(&declaration(providers(16, 4))).is_valid(),
        "16 providers, 64 commands"
    );
    assert_eq!(only_problem(&declaration(providers(17, 1))).0, "/providers");
    assert_eq!(only_problem(&declaration(providers(2, 33))).0, "/providers");
    assert_eq!(
        only_problem(&declaration(providers(1, 65))).0,
        "/providers/0/commands"
    );
}

#[test]
fn a_command_whose_schema_leaves_the_subset_is_listed_but_unavailable() {
    for (input_schema, reason_part) in [
        (
            json!({ "type": "object", "properties": { "mail": { "type": "string", "format": "email" } } }),
            "unsupported schema keyword `format`",
        ),
        (
            json!({ "type": "object", "properties": { "id": { "type": "string", "pattern": "^a.b$" } } }),
            "unsupported wildcard",
        ),
        (
            json!({ "type": "object", "$ref": "#/$defs/args" }),
            "`$ref`",
        ),
        (json!({ "properties": {} }), "\"type\": \"object\""),
        (json!({ "type": "string" }), "\"type\": \"object\""),
    ] {
        let mut declared = command("x");
        declared["input_schema"] = input_schema.clone();
        let parsed = parse(&declaration(
            json!([{ "id": "a", "script": "res://a.gd", "commands": [declared] }]),
        ));
        assert!(parsed.is_valid(), "{input_schema}: {:?}", parsed.problems);
        let (_, found) = parsed.find("a", "x").expect("listed");
        let reason = found.unavailable.as_deref().expect("unavailable");
        assert!(reason.contains(reason_part), "{input_schema}: {reason}");
    }
}

#[test]
fn arguments_must_match_the_declared_schema() {
    let parsed = parse(&one_command());
    let (_, found) = parsed.find("levels", "rebuild").expect("declared");
    let tagged =
        project_commands::prepare_arguments(found, &arguments(json!({ "seed": 7, "name": "ice" })))
            .expect("valid arguments");
    assert_eq!(
        serde_json::to_value(&tagged).expect("serializes"),
        json!({ "type": "dictionary", "value": {
            "name": { "type": "string", "value": "ice" },
            "seed": { "type": "integer", "value": 7 }
        }})
    );
    for (refused, part) in [
        (json!({}), "missing required property `seed`"),
        (json!({ "seed": "7" }), "/seed"),
        (json!({ "seed": -1 }), "below the minimum"),
        (json!({ "seed": 1, "extra": true }), "`extra`"),
        (json!({ "seed": 1, "name": "x".repeat(33) }), "/name"),
    ] {
        let error = project_commands::prepare_arguments(found, &arguments(refused.clone()))
            .expect_err("refused");
        assert!(error.contains(part), "{refused}: {error}");
    }
}

#[test]
fn a_declared_maximum_bounds_a_numeric_argument() {
    let mut declared = command("rebuild");
    declared["input_schema"]["properties"]["seed"] =
        json!({ "type": "integer", "minimum": 0, "maximum": 99 });
    let parsed = parse(&declaration(json!([{
        "id": "levels",
        "script": "res://tools/level_commands.gd",
        "commands": [declared]
    }])));
    assert!(parsed.is_valid(), "{:?}", parsed.problems);
    let (_, found) = parsed.find("levels", "rebuild").expect("declared");
    assert_eq!(found.unavailable, None, "`maximum` is inside the subset");
    project_commands::prepare_arguments(found, &arguments(json!({ "seed": 99 })))
        .expect("the bound itself is accepted");
    let error = project_commands::prepare_arguments(found, &arguments(json!({ "seed": 100 })))
        .expect_err("a value above the bound is refused");
    assert!(error.contains("above the maximum"), "{error}");
}

#[test]
fn arguments_are_spelled_as_tagged_values_the_bridge_carries_exactly() {
    let tagged = project_commands::tagged_arguments(&arguments(json!({
        "nothing": null,
        "flag": true,
        "count": 3,
        "ratio": 0.5,
        "whole_float": 2.0,
        "label": "a",
        "list": [1, "b", [false]],
        "nested": { "k": { "deep": -4 } }
    })))
    .expect("carried");
    let GodotValue::Dictionary(entries) = tagged else {
        panic!("a dictionary");
    };
    assert_eq!(entries["nothing"], GodotValue::Null);
    assert_eq!(entries["flag"], GodotValue::Bool(true));
    assert_eq!(entries["count"], GodotValue::Integer(3));
    assert_eq!(entries["ratio"], GodotValue::Float(0.5));
    assert_eq!(entries["whole_float"], GodotValue::Float(2.0));
    assert_eq!(entries["label"], GodotValue::String("a".to_owned()));
    assert_eq!(
        entries["list"],
        GodotValue::Array(vec![
            GodotValue::Integer(1),
            GodotValue::String("b".to_owned()),
            GodotValue::Array(vec![GodotValue::Bool(false)]),
        ])
    );
    let edge = 9_007_199_254_740_992_i64;
    for accepted in [json!({ "n": edge }), json!({ "n": -edge })] {
        assert!(project_commands::tagged_arguments(&arguments(accepted)).is_ok());
    }
    let mut deep = json!(1);
    for _ in 0..16 {
        deep = json!([deep]);
    }
    assert!(project_commands::tagged_arguments(&arguments(json!({ "d": deep.clone() }))).is_err());
    assert!(
        project_commands::tagged_arguments(&arguments(json!({ "d": deep[0].clone() }))).is_ok()
    );
    for (refused, part) in [
        (json!({ "n": edge + 1 }), "2^53"),
        (json!({ "n": -edge - 1 }), "2^53"),
        (json!({ "n": u64::MAX }), "2^53"),
        (json!({ "s": "é".repeat(65_537) }), "65536"),
        (json!({ "a": vec![0; 4_097] }), "4096"),
        (
            json!({ "o": (0..4_097).map(|index| (format!("k{index}"), json!(0))).collect::<Map<_, _>>() }),
            "4096",
        ),
    ] {
        let error = project_commands::tagged_arguments(&arguments(refused)).expect_err("refused");
        assert!(error.contains(part), "{error}");
    }
    assert!(
        project_commands::tagged_arguments(&arguments(json!({ "s": "é".repeat(65_536) }))).is_ok()
    );
}

#[test]
fn arguments_over_the_size_limit_are_refused_before_validation() {
    assert_eq!(MAX_ARGUMENTS_BYTES, 262_144);
    let mut open = command("x");
    open["input_schema"] = json!({ "type": "object" });
    let parsed = parse(&declaration(
        json!([{ "id": "a", "script": "res://a.gd", "commands": [open] }]),
    ));
    let (_, found) = parsed.find("a", "x").expect("declared");
    let chunk = "a".repeat(60_000);
    let big = arguments(json!({ "a": chunk, "b": chunk, "c": chunk, "d": chunk, "e": chunk }));
    let error = project_commands::prepare_arguments(found, &big).expect_err("too large");
    assert!(error.contains("262144"), "{error}");
}

fn committed_schema() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../schemas/domain/project-commands-v1.schema.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&text).expect("the committed schema is JSON")
}

#[test]
fn the_committed_schema_is_the_declaration_schema() {
    assert_eq!(committed_schema(), project_commands::declaration_schema());
    assert_eq!(jsonschema::check_schema(&committed_schema()), Ok(()));
}

/// The committed schema is what an editor checks a declaration with; it
/// accepts every declaration the server accepts and refuses the structural
/// mistakes, leaving the cross-field rules (duplicates, the command total,
/// path segments) to the server.
#[test]
fn the_committed_schema_accepts_what_the_server_accepts() {
    let schema = committed_schema();
    let mut valid = one_command();
    valid["$schema"] = json!("./project-commands-v1.schema.json");
    assert!(parse(&valid).is_valid());
    assert_eq!(jsonschema::validate(&schema, &valid), Ok(()));
    for refused in [
        json!({ "version": 2, "providers": [] }),
        json!({ "version": 1, "providers": [], "extra": true }),
        declaration(json!([{ "id": "A", "script": "res://a.gd", "commands": [command("x")] }])),
        declaration(json!([{ "id": "a", "script": "a.gd", "commands": [command("x")] }])),
        declaration(json!([{ "id": "a", "script": "res://a.gd", "commands": [] }])),
    ] {
        assert!(!parse(&refused).is_valid());
        assert!(
            jsonschema::validate(&schema, &refused).is_err(),
            "{refused}"
        );
    }
}
