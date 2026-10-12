//! Todo 7 acceptance tests for the self-contained GDScript syntax layer.
//!
//! Exactly two test cases, matching the `qa/tasks/7.json` descriptor:
//! `gdscript_parser_happy_path` and `gdscript_parser_failure_paths`.

use std::path::{Path, PathBuf};

use godot_mcp_analysis::gdscript::{
    GdCallKind, GdDiagnosticKind, GdScriptFile, GdScriptParser, GdVariableKind,
};
use godot_mcp_domain::contracts::ProvenanceSource;
use godot_mcp_domain::{Confidence, ErrorKind};

/// Absolute path of a committed in-repo fixture. The analysis layer must never
/// need a sibling `codegraph-rust` checkout at runtime.
fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("gdscript")
        .join(name)
}

fn parse_fixture(parser: &mut GdScriptParser, name: &str) -> GdScriptFile {
    let path = fixture_path(name);
    let source = std::fs::read_to_string(&path).expect("fixture is readable");
    parser
        .parse(&path.to_string_lossy(), &source, 7)
        .expect("committed fixture parses without a hard parser error")
}

#[test]
fn gdscript_parser_happy_path() {
    let mut parser = GdScriptParser::new().expect("GDScript grammar loads");
    let file = parse_fixture(&mut parser, "healthy_player.gd");

    // A clean file yields zero diagnostics and may therefore claim exactness.
    assert!(
        file.diagnostics().is_empty(),
        "clean fixture must not emit diagnostics: {:?}",
        file.diagnostics()
    );
    assert!(!file.is_partial());
    assert_eq!(file.provenance().source(), ProvenanceSource::StaticSyntax);
    assert_eq!(file.provenance().confidence(), Confidence::Exact);
    assert_eq!(file.provenance().project_generation(), 7);

    // Script header.
    assert_eq!(file.class_name_declaration(), Some("HealthyPlayer"));
    assert_eq!(file.extends(), Some("CharacterBody2D"));
    assert_eq!(file.icon_path(), None);

    // Functions, constructors and static flags, in source order.
    let function_names: Vec<&str> = file.functions().iter().map(|f| f.name()).collect();
    assert_eq!(
        function_names,
        vec![
            "capacity",
            "default_capacity",
            "_init",
            "_ready",
            "take_damage",
            "die",
            "clamp_health",
        ]
    );

    let capacity = &file.functions()[0];
    assert_eq!(capacity.owner(), Some("Inventory"));
    assert!(!capacity.is_static());
    assert!(!capacity.is_constructor());
    assert_eq!(capacity.return_type(), Some("int"));
    assert_eq!(capacity.signature(), Some("()"));
    assert_eq!(capacity.span().start_line(), 24);

    let default_capacity = &file.functions()[1];
    assert!(default_capacity.is_static());
    assert_eq!(default_capacity.owner(), Some("Inventory"));

    let init = &file.functions()[2];
    assert!(init.is_constructor());
    assert_eq!(init.name(), "_init");
    assert_eq!(init.owner(), None);
    assert_eq!(init.return_type(), Some("void"));

    let take_damage = &file.functions()[4];
    assert_eq!(
        take_damage.signature(),
        Some("(amount: int, source: Node = null)")
    );
    assert_eq!(take_damage.return_type(), Some("int"));
    assert_eq!(take_damage.parameters().len(), 2);
    assert_eq!(take_damage.parameters()[0].name(), "amount");
    assert_eq!(take_damage.parameters()[0].type_hint(), Some("int"));
    assert!(!take_damage.parameters()[0].has_default());
    assert_eq!(take_damage.parameters()[1].name(), "source");
    assert_eq!(take_damage.parameters()[1].type_hint(), Some("Node"));
    assert!(take_damage.parameters()[1].has_default());
    assert_eq!(
        take_damage.provenance().source(),
        ProvenanceSource::StaticSyntax
    );
    assert_eq!(take_damage.provenance().confidence(), Confidence::Exact);

    let clamp_health = file.functions().last().expect("trailing function");
    assert!(clamp_health.is_static());
    assert_eq!(clamp_health.name(), "clamp_health");

    // Inner classes.
    assert_eq!(file.classes().len(), 1);
    let inventory = &file.classes()[0];
    assert_eq!(inventory.name(), "Inventory");
    assert_eq!(inventory.owner(), None);
    assert_eq!(inventory.extends(), None);
    assert_eq!(inventory.provenance().confidence(), Confidence::Exact);

    // Enums, including inline and multi-line enumerator lists with values.
    let enum_names: Vec<&str> = file.enums().iter().map(|e| e.name()).collect();
    assert_eq!(enum_names, vec!["State", "Team"]);
    let state = &file.enums()[0];
    let state_members: Vec<&str> = state.members().iter().map(|m| m.name()).collect();
    assert_eq!(state_members, vec!["IDLE", "RUNNING", "DEAD"]);
    assert!(state.members().iter().all(|m| m.value().is_none()));
    let team = &file.enums()[1];
    assert_eq!(team.members().len(), 2);
    assert_eq!(team.members()[0].name(), "RED");
    assert_eq!(team.members()[0].value(), Some("0"));
    assert_eq!(team.members()[1].name(), "BLUE");
    assert_eq!(team.members()[1].value(), Some("1"));

    // Signals, with typed parameters.
    let signal_names: Vec<&str> = file.signals().iter().map(|s| s.name()).collect();
    assert_eq!(signal_names, vec!["health_changed", "died"]);
    assert_eq!(file.signals()[0].parameters().len(), 1);
    assert_eq!(file.signals()[0].parameters()[0].name(), "new_health");
    assert_eq!(file.signals()[0].parameters()[0].type_hint(), Some("int"));
    assert!(file.signals()[1].parameters().is_empty());

    // Constants and variables, with export/static classification.
    let variable_names: Vec<&str> = file.variables().iter().map(|v| v.name()).collect();
    assert_eq!(
        variable_names,
        vec!["MAX_HEALTH", "BULLET", "speed", "health", "_state", "slots"]
    );
    assert_eq!(file.variables()[0].kind(), GdVariableKind::Const);
    assert_eq!(file.variables()[0].type_hint(), Some("int"));
    assert_eq!(file.variables()[1].kind(), GdVariableKind::Const);
    assert_eq!(file.variables()[2].kind(), GdVariableKind::Export);
    assert_eq!(file.variables()[2].type_hint(), Some("float"));
    assert_eq!(file.variables()[3].kind(), GdVariableKind::Var);
    assert_eq!(file.variables()[5].owner(), Some("Inventory"));

    // `preload` resource references.
    assert_eq!(file.preloads().len(), 1);
    assert_eq!(file.preloads()[0].path(), "res://scenes/bullet.tscn");
    assert!(file.preloads()[0].is_preload());

    // Calls, in source order, with receiver and dispatch classification.
    let callees: Vec<&str> = file.calls().iter().map(|c| c.callee()).collect();
    assert_eq!(
        callees,
        vec![
            "preload", "get_node", "register", "emit", "max", "emit", "die", "emit", "clampi",
        ]
    );
    let register = &file.calls()[2];
    assert_eq!(register.receiver(), Some("manager"));
    assert_eq!(register.kind(), GdCallKind::Qualified);
    assert_eq!(register.enclosing_function(), Some("_ready"));
    assert_eq!(register.argument_count(), 1);
    assert_eq!(register.provenance().source(), ProvenanceSource::Heuristic);

    let die_call = &file.calls()[6];
    assert_eq!(die_call.callee(), "die");
    assert_eq!(die_call.receiver(), None);
    assert_eq!(die_call.kind(), GdCallKind::Local);
    assert_eq!(die_call.enclosing_function(), Some("take_damage"));
    assert_eq!(
        die_call.provenance().source(),
        ProvenanceSource::StaticSyntax
    );

    // Call targets are never claimed as statically proven by the syntax layer:
    // resolution needs the project index (Todo 10/11), not the parse tree.
    assert!(
        file.calls()
            .iter()
            .all(|c| c.provenance().confidence() == Confidence::Inferred),
        "an unresolved call target must never be labeled exact"
    );
}

#[test]
fn gdscript_parser_failure_paths() {
    let mut parser = GdScriptParser::new().expect("GDScript grammar loads");

    // --- Case 1: a syntactically damaged file keeps its recoverable symbols. --
    let damaged = parse_fixture(&mut parser, "damaged_enemy.gd");
    assert!(damaged.is_partial());
    assert!(
        !damaged.diagnostics().is_empty(),
        "a damaged fixture must emit diagnostics instead of silently dropping the error"
    );
    assert!(
        damaged
            .diagnostics()
            .iter()
            .any(|d| d.kind() == GdDiagnosticKind::SyntaxError && d.span().start_line() == 13),
        "the unterminated call on line 13 must surface as a syntax diagnostic: {:?}",
        damaged.diagnostics()
    );
    // Partial output is never labeled exact at file level.
    assert_eq!(damaged.provenance().source(), ProvenanceSource::Heuristic);
    assert_eq!(damaged.provenance().confidence(), Confidence::Inferred);

    // Symbols before and after the damaged region survive.
    assert_eq!(damaged.class_name_declaration(), Some("DamagedEnemy"));
    assert_eq!(damaged.extends(), Some("Node2D"));
    let damaged_functions: Vec<&str> = damaged.functions().iter().map(|f| f.name()).collect();
    assert_eq!(damaged_functions, vec!["attack", "spawn_cost", "patrol"]);
    let attack = &damaged.functions()[0];
    assert_eq!(attack.return_type(), Some("int"));
    assert_eq!(attack.provenance().confidence(), Confidence::Exact);
    let spawn_cost = &damaged.functions()[1];
    assert!(spawn_cost.is_static());
    assert_eq!(spawn_cost.provenance().confidence(), Confidence::Exact);
    assert_eq!(damaged.signals().len(), 1);
    assert_eq!(damaged.signals()[0].name(), "alerted");

    // The function containing the damage is downgraded, never exact.
    let patrol = &damaged.functions()[2];
    assert_eq!(patrol.provenance().source(), ProvenanceSource::Heuristic);
    assert_eq!(patrol.provenance().confidence(), Confidence::Inferred);

    // Nothing is invented out of the error region: only the intact call remains.
    let damaged_callees: Vec<&str> = damaged.calls().iter().map(|c| c.callee()).collect();
    assert_eq!(damaged_callees, vec!["deal_damage"]);

    // --- Case 2: a truncated construct reports the MISSING token. ------------
    let truncated = parse_fixture(&mut parser, "truncated_enum.gd");
    assert!(truncated.is_partial());
    assert!(
        truncated
            .diagnostics()
            .iter()
            .any(|d| d.kind() == GdDiagnosticKind::MissingToken),
        "an unterminated enumerator list must surface as a missing-token diagnostic: {:?}",
        truncated.diagnostics()
    );
    assert_eq!(truncated.enums().len(), 1);
    let flags = &truncated.enums()[0];
    assert_eq!(flags.name(), "Flags");
    let flag_members: Vec<&str> = flags.members().iter().map(|m| m.name()).collect();
    assert_eq!(flag_members, vec!["NONE", "FAST"]);
    assert_eq!(flags.provenance().confidence(), Confidence::Inferred);
    // The intact constant before the damage is still recovered exactly.
    assert_eq!(truncated.variables().len(), 1);
    assert_eq!(truncated.variables()[0].name(), "VERSION");
    assert_eq!(
        truncated.variables()[0].provenance().confidence(),
        Confidence::Exact
    );

    // --- Case 3: unrecoverable input is honestly reported as unresolved. -----
    let garbage = parser
        .parse(
            "res://garbage.gd",
            "%%% definitely not GDScript %%%\nfunc 123(:\n\t???\n",
            7,
        )
        .expect("unrecoverable input still returns a report, not a hard error");
    assert!(garbage.is_partial());
    assert!(!garbage.diagnostics().is_empty());
    assert!(garbage.functions().is_empty());
    assert!(garbage.classes().is_empty());
    assert_eq!(garbage.provenance().source(), ProvenanceSource::Unresolved);
    assert_eq!(garbage.provenance().confidence(), Confidence::Unresolved);

    // --- Case 3b: a source whose whole tree failed to parse is damage. -------
    // This file cut off mid-edit parses to a root `ERROR` node, with no
    // `source` node above it. Nothing inside an unparsed region may be
    // extracted, the root included.
    const CUT_OFF: &str = "\tmatch typeof(value):\n\t\tTYPE_NIL:\n\t\t\treturn \
                           _good({\"type\": \"transform3_d\", \"value\": {\n\t\t\tvar";
    let mut grammar = tree_sitter::Parser::new();
    grammar
        .set_language(&tree_sitter_gdscript::LANGUAGE.into())
        .expect("GDScript grammar loads");
    assert!(
        grammar
            .parse(CUT_OFF, None)
            .expect("a syntax tree")
            .root_node()
            .is_error(),
        "the case needs an input whose root is an ERROR node"
    );
    let cut_off = parser
        .parse("res://cut_off.gd", CUT_OFF, 7)
        .expect("a root ERROR still returns a report, not a hard error");
    assert!(cut_off.is_partial());
    assert!(
        cut_off
            .diagnostics()
            .iter()
            .any(|d| d.kind() == GdDiagnosticKind::SyntaxError && d.span().start_line() == 1),
        "the unparsed source is reported: {:?}",
        cut_off.diagnostics()
    );
    assert!(
        cut_off.calls().is_empty(),
        "no call is extracted from an unparsed region: {:?}",
        cut_off.calls()
    );
    assert!(cut_off.functions().is_empty());
    assert!(cut_off.variables().is_empty());
    assert_eq!(cut_off.provenance().source(), ProvenanceSource::Unresolved);
    assert_eq!(cut_off.provenance().confidence(), Confidence::Unresolved);

    // --- Case 4: an empty source path is rejected as invalid input. ----------
    let rejected = parser
        .parse("", "extends Node\n", 7)
        .expect_err("an empty source path must be rejected");
    assert_eq!(rejected.kind(), ErrorKind::InvalidInput);

    // --- Case 5: an empty file is clean-but-empty, not damaged. --------------
    let empty = parser
        .parse("res://empty.gd", "", 7)
        .expect("an empty file parses");
    assert!(!empty.is_partial());
    assert!(empty.diagnostics().is_empty());
    assert!(empty.functions().is_empty());
    assert_eq!(empty.provenance().source(), ProvenanceSource::StaticSyntax);
    assert_eq!(empty.provenance().confidence(), Confidence::Exact);
}
