//! M0 acceptance: one canonical project context and one writer.
//!
//! Every spelling of one project — the root, a subdirectory, a relative path and
//! a lexical alias — must converge on the *same* lease, state root, journal and
//! Godot `--path`. A second owner through any spelling is refused before it can
//! open a journal or create a state directory, and a run's public result carries
//! its classification, parsed diagnostics and bounded output without exposing a
//! private locator or an absolute path.
//!
//! Findings F-01, F-02 and F-07 in `docs/contributor/complete-game-development.md`
//! are reproduced here first; each test names the finding it guards.

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::config::Limits;
use godot_mcp_domain::contracts::PrincipalId;
use godot_mcp_domain::lease::LeaseError;
use godot_mcp_server::stdio::{ProjectRuntime, StdioError, StdioOptions};
use godot_mcp_server::test_support::{
    JournalFoundationHarness, JournalPhase, MockFileEffect, MockFileOperation,
};
use serde_json::json;
use support::{ProjectTree, Spawn, StdioSession, structured};

fn options(project: &Path) -> StdioOptions {
    StdioOptions {
        project: project.to_path_buf(),
        godot_binary: None,
        profile: CapabilityProfile::Core,
        principal: PrincipalId::parse("principal_0123456789abcdef").expect("fixed principal"),
        observed_at_unix_ms: 1_700_000_000_000,
        limits: Limits::default(),
        process: Default::default(),
    }
}

fn assert_conflict(result: Result<ProjectRuntime, StdioError>, spelling: &str) {
    match result {
        Err(StdioError::Lease(LeaseError::Conflict { .. })) => {}
        Err(other) => panic!("{spelling}: expected a lease conflict, got {other}"),
        Ok(_) => {
            panic!("{spelling}: a second owner was admitted for an already-held project (F-01)")
        }
    }
}

/// F-01: root, subdirectory, deeper subdirectory and lexical alias all name one
/// project object, so exactly one of them may own it, and a refused spelling must
/// leave no state directory behind.
#[test]
fn every_spelling_of_one_project_converges_on_one_owner() {
    let tree = ProjectTree::new("m0-converge");
    let first = ProjectRuntime::open(&options(&tree.root)).expect("the first owner is admitted");
    let canonical_state = tree.root.join(".godot-mcp");
    assert_eq!(tree.state_dirs(), vec![canonical_state.clone()]);

    for spelling in [
        tree.root.join("sub"),
        tree.root.join("sub").join("deeper"),
        tree.root.join("sub").join(".."),
        tree.root
            .join(".")
            .join("sub")
            .join("deeper")
            .join("..")
            .join(".."),
    ] {
        assert_conflict(
            ProjectRuntime::open(&options(&spelling)),
            &spelling.display().to_string(),
        );
    }
    assert_eq!(
        tree.state_dirs(),
        vec![canonical_state.clone()],
        "a refused spelling must not create its own state directory"
    );

    drop(first);
    // Once the first owner is gone, a subdirectory spelling owns the *same*
    // canonical state root rather than a candidate-derived one.
    let second =
        ProjectRuntime::open(&options(&tree.root.join("sub"))).expect("subdirectory owner");
    assert_eq!(tree.state_dirs(), vec![canonical_state]);
    drop(second);
}

/// F-01 across processes: a second server launched through a different spelling
/// must be refused with a conflict before it opens a journal or creates a state
/// directory.
#[test]
fn a_second_server_through_any_spelling_is_refused_before_state_effects() {
    let tree = ProjectTree::new("m0-second-process");
    let mut owner = StdioSession::initialized(&Spawn::core(&tree.root));
    let before = tree.state_dirs();

    let subdirectory = tree.root.join("sub");
    let relative = PathBuf::from("game").join("sub").join("deeper");
    for spawn in [
        Spawn::core(&subdirectory),
        Spawn {
            project: &relative,
            current_dir: Some(&tree.base),
            profile: "core",
            godot: None,
        },
    ] {
        let exited = StdioSession::spawn(&spawn).wait_exit(Duration::from_secs(30));
        assert_eq!(
            exited.code,
            Some(69),
            "a second owner must fail startup with the ownership exit code: {}",
            exited.stderr
        );
        assert!(
            exited.stderr.contains("already owned"),
            "the refusal must name the ownership conflict: {}",
            exited.stderr
        );
    }
    assert_eq!(
        tree.state_dirs(),
        before,
        "a refused second server must not create a state directory"
    );

    let health = owner.call_tool(2, "health.get", json!({}));
    assert!(
        health.get("error").is_none(),
        "the first owner keeps serving: {health}"
    );
    let _ = owner.finish();
}

/// Replace `project.godot` the way the Godot editor saves it: write a new file
/// and rename it over the old one, so the marker becomes a different object.
fn save_marker_like_the_editor(root: &Path, contents: &str) {
    let staged = root.join("project.godot.tmp");
    std::fs::write(&staged, contents).expect("stage the new project.godot");
    std::fs::rename(&staged, root.join("project.godot")).expect("rename it over the marker");
}

fn project_object_id(response: &serde_json::Value) -> String {
    structured(response)["project"]["object_id"]
        .as_str()
        .unwrap_or_else(|| panic!("an answer names its project: {response}"))
        .to_owned()
}

/// A live editor rewrites `project.godot` by rename on startup and on every
/// settings save. The project is still the same object: the running owner keeps
/// writing, a second server is still refused, and a restart reports the same
/// project identity.
#[test]
fn an_editor_save_of_project_godot_keeps_one_owner_and_one_identity() {
    let tree = ProjectTree::new("m4-marker-save");
    let spawn = Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
        godot: None,
    };
    let mut owner = StdioSession::initialized(&spawn);
    let before = project_object_id(&owner.call_tool(2, "project.info.get", json!({})));

    let marker = std::fs::read_to_string(tree.root.join("project.godot")).expect("marker");
    save_marker_like_the_editor(&tree.root, &format!("{marker}\n; saved by the editor\n"));

    let created = owner.call_tool(
        3,
        "file.create",
        json!({ "path": "notes/after-save.txt", "content": "still writable\n", "idempotencyId": "after-save" }),
    );
    assert!(
        created.get("error").is_none() && created["result"]["isError"] != json!(true),
        "the owner keeps writing after an editor save of project.godot: {created}"
    );
    assert!(tree.root.join("notes/after-save.txt").is_file());

    let second = StdioSession::spawn(&Spawn::core(&tree.root)).wait_exit(Duration::from_secs(30));
    assert_eq!(
        second.code,
        Some(69),
        "a replaced project.godot must not admit a second owner: {}",
        second.stderr
    );

    let _ = owner.finish();
    let mut restarted = StdioSession::initialized(&spawn);
    let after = project_object_id(&restarted.call_tool(4, "project.info.get", json!({})));
    assert_eq!(after, before, "a restart reports the same project identity");
    let _ = restarted.finish();
}

/// F-02: a relative project spelling must be normalized once, before any state
/// directory or index is opened, so an indexed core call succeeds.
#[test]
fn a_relative_project_path_serves_an_indexed_core_call() {
    let tree = ProjectTree::new("m0-relative");
    tree.write(
        "scripts/player.gd",
        "extends Node\n\nfunc jump() -> void:\n\tpass\n",
    );
    let relative = PathBuf::from("game").join("sub");
    let mut session = StdioSession::initialized(&Spawn {
        project: &relative,
        current_dir: Some(&tree.base),
        profile: "core",
        godot: None,
    });

    // `gdscript.references.get` answers from the persisted semantic index, which is
    // exactly the component a relative state spelling used to break.
    let response = session.call_tool(
        2,
        "gdscript.references.get",
        json!({ "script": "scripts/player.gd", "symbol": "jump" }),
    );
    let payload = structured(&response);
    assert_ne!(
        response["result"]["isError"],
        json!(true),
        "an indexed core call through a relative spelling must succeed: {response}"
    );
    assert!(
        payload.to_string().contains("jump"),
        "the indexed answer must describe the script: {payload}"
    );
    let _ = session.finish();
    assert_eq!(tree.state_dirs(), vec![tree.root.join(".godot-mcp")]);
}

/// Section 16: no public response carries a project or state path. Core reads
/// and health are checked over the wire, through a subdirectory spelling.
#[test]
fn core_responses_carry_no_local_project_or_state_path() {
    let tree = ProjectTree::new("m0-no-paths");
    tree.write("scripts/player.gd", "extends Node\n");
    tree.write(
        "main.tscn",
        "[gd_scene format=3]\n\n[node name=\"Main\" type=\"Node2D\"]\n",
    );
    let subdirectory = tree.root.join("sub");
    let mut session = StdioSession::initialized(&Spawn::core(&subdirectory));
    let mut transcript = String::new();
    for (id, tool, arguments) in [
        (2, "health.get", json!({})),
        (3, "project.info.get", json!({})),
        (4, "project.discover", json!({})),
        (5, "scene.tree.get", json!({ "scene": "main.tscn" })),
        (
            6,
            "gdscript.diagnostics.get",
            json!({ "script": "scripts/missing.gd" }),
        ),
    ] {
        transcript.push_str(&session.call_tool(id, tool, arguments).to_string());
    }
    transcript.push_str(
        &session
            .request(
                7,
                "resources/read",
                json!({ "uri": "godot-mcp://resources/health" }),
            )
            .to_string(),
    );
    let _ = session.finish();
    for private in [
        tree.base.to_string_lossy().into_owned(),
        tree.root.to_string_lossy().into_owned(),
    ] {
        assert!(
            !transcript.contains(&private),
            "a public response contained {private:?}: {transcript}"
        );
    }
}

fn legacy_effect() -> MockFileEffect {
    MockFileEffect {
        transaction_id: "00000000-0000-4000-8000-00000000d001".to_owned(),
        path: "legacy.txt".to_owned(),
        operation: MockFileOperation::Create,
        bytes: b"legacy".to_vec(),
        required_process_transaction: None,
    }
}

/// Migration (plan section 14.3): when more than one state root of one project
/// object holds open records, no root can be chosen without possibly
/// discarding the only record of an unresolved effect, so writes stay blocked
/// for operator resolution and nothing is reconciled.
#[test]
fn open_records_in_more_than_one_state_root_block_writes() {
    let tree = ProjectTree::new("m0-legacy-split");
    {
        let canonical = JournalFoundationHarness::open(&tree.root).expect("canonical 0.1 state");
        canonical
            .open_transaction(&MockFileEffect {
                transaction_id: "00000000-0000-4000-8000-00000000d002".to_owned(),
                path: "canonical.txt".to_owned(),
                operation: MockFileOperation::Create,
                bytes: b"canonical".to_vec(),
                required_process_transaction: None,
            })
            .expect("leave an open canonical record");
    }
    {
        let legacy = JournalFoundationHarness::open(&tree.root.join("sub"))
            .expect("simulate a 0.1 server launched from a subdirectory");
        legacy
            .open_transaction(&legacy_effect())
            .expect("leave an open legacy record");
    }

    let runtime =
        ProjectRuntime::open(&options(&tree.root)).expect("startup inventories legacy state");
    let blocked = runtime
        .write_gate_status()
        .expect_err("open records in two state roots must block mutation admission");
    assert!(
        blocked.contains("sub/.godot-mcp") && blocked.contains("operator"),
        "the write block must name the split state roots and require an operator: {blocked}"
    );
    drop(runtime);

    let legacy = JournalFoundationHarness::open(&tree.root.join("sub")).expect("reopen legacy");
    let record = legacy
        .observe("00000000-0000-4000-8000-00000000d001")
        .expect("the legacy record is untouched");
    assert_eq!(
        record.phase,
        JournalPhase::Intent,
        "a split-state startup must not settle a legacy record"
    );
}

/// Migration (plan section 14.3, step 4): when exactly one state root holds open
/// records and it is a legacy root, those records are reconciled in place with
/// their original schema, and the selection is recorded durably.
#[test]
fn a_single_legacy_root_with_an_open_record_is_reconciled_in_place() {
    let tree = ProjectTree::new("m0-legacy-reconcile");
    {
        let legacy = JournalFoundationHarness::open(&tree.root.join("sub"))
            .expect("simulate a 0.1 server launched from a subdirectory");
        legacy
            .open_transaction(&legacy_effect())
            .expect("leave an open legacy record whose pre-state still holds");
    }

    let runtime = ProjectRuntime::open(&options(&tree.root)).expect("startup");
    runtime
        .write_gate_status()
        .expect("a legacy record proven at its pre-state no longer blocks writes");
    drop(runtime);

    let migration: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            tree.root
                .join(".godot-mcp")
                .join("state-root-migration.json"),
        )
        .expect("the state-root selection is recorded durably"),
    )
    .expect("the migration record is JSON");
    assert_eq!(migration["legacy"][0]["path"], json!("sub/.godot-mcp"));
    assert_eq!(migration["legacy"][0]["open_records"], json!(0));
    assert_eq!(migration["legacy"][0]["reconciled"], json!(true));
    assert_eq!(migration["write_blocked"], json!(false));

    let legacy = JournalFoundationHarness::open(&tree.root.join("sub")).expect("reopen legacy");
    let record = legacy
        .observe("00000000-0000-4000-8000-00000000d001")
        .expect("the legacy record is durable");
    assert_eq!(record.phase, JournalPhase::Compensated, "{record:?}");
}

/// A legacy record whose pre- and post-state are both refuted reconciles to a
/// write-blocking quarantine, exactly as it would in its own state root.
#[test]
fn a_legacy_record_that_reconciles_to_quarantine_keeps_writes_blocked() {
    let tree = ProjectTree::new("m0-legacy-quarantine");
    tree.write("legacy.txt", "before\n");
    {
        let legacy = JournalFoundationHarness::open(&tree.root.join("sub"))
            .expect("simulate a 0.1 server launched from a subdirectory");
        legacy
            .open_transaction(&MockFileEffect {
                operation: MockFileOperation::Update,
                bytes: b"after\n".to_vec(),
                ..legacy_effect()
            })
            .expect("leave an open legacy update");
    }
    tree.write("legacy.txt", "neither\n");

    let runtime = ProjectRuntime::open(&options(&tree.root)).expect("startup");
    let blocked = runtime
        .write_gate_status()
        .expect_err("an unprovable legacy record must keep writes blocked");
    assert!(blocked.contains("sub/.godot-mcp"), "{blocked}");
}

/// A legacy state root still locked by a live 0.1 owner is a second writer for
/// the same project object, so the canonical owner is refused.
#[test]
fn a_live_legacy_owner_is_a_conflict() {
    let tree = ProjectTree::new("m0-legacy-live");
    let _legacy = JournalFoundationHarness::open(&tree.root.join("sub"))
        .expect("simulate a live 0.1 server launched from a subdirectory");

    assert_conflict(ProjectRuntime::open(&options(&tree.root)), "root");
}

/// A legacy state root that holds only terminal records is inert: startup
/// records the migration selection and the write gate stays open.
#[test]
fn an_inert_legacy_state_root_is_recorded_and_does_not_block() {
    let tree = ProjectTree::new("m0-legacy-inert");
    {
        let _legacy = JournalFoundationHarness::open(&tree.root.join("sub"))
            .expect("simulate a 0.1 server that recorded nothing");
    }
    let runtime = ProjectRuntime::open(&options(&tree.root)).expect("startup");
    runtime
        .write_gate_status()
        .expect("an inert legacy state root does not block writes");
    let migration = std::fs::read_to_string(
        tree.root
            .join(".godot-mcp")
            .join("state-root-migration.json"),
    )
    .expect("the state-root selection is recorded durably");
    let migration: serde_json::Value =
        serde_json::from_str(&migration).expect("the migration record is JSON");
    assert_eq!(migration["selected"], json!(".godot-mcp"));
    assert_eq!(migration["legacy"][0]["path"], json!("sub/.godot-mcp"));
    assert_eq!(migration["legacy"][0]["open_records"], json!(0));
}

#[cfg(target_os = "linux")]
mod process_runs {
    use super::*;

    const SCRIPT_ERROR: &str =
        "SCRIPT ERROR: Parse Error: Expected statement, found \"Identifier\" instead.";

    /// F-01/F-07: Godot receives the discovered root even through a subdirectory
    /// spelling; a nonzero run is a completed call with a failed classification,
    /// its parsed diagnostic and bounded output; and the serialized response
    /// carries neither a locator nor any absolute project or state path.
    #[test]
    fn a_nonzero_run_is_classified_with_diagnostics_and_no_private_paths() {
        let tree = ProjectTree::new("m0-run");
        let tools = tempfile::tempdir().expect("fake engine directory");
        let (godot, argv_log) = support::fake_godot(
            tools.path(),
            &format!(
                "printf '%s\\n' 'Godot Engine v4.7.1.stable.official.b09f793f5 - https://godotengine.org'\n\
                 printf '%s\\n' '{SCRIPT_ERROR}' >&2\n\
                 printf '%s\\n' '   at: GDScript::reload (res://main.gd:3)' >&2\n\
                 exit 1"
            ),
        );
        let subdirectory = tree.root.join("sub");
        let mut session = StdioSession::initialized(&Spawn {
            project: &subdirectory,
            current_dir: None,
            profile: "runtime",
            godot: Some(&godot),
        });

        let response = session.call_tool(
            2,
            "project.run",
            json!({ "headless": true, "deadline_ms": 30_000 }),
        );
        let text = response.to_string();
        let payload = structured(&response);

        let argv = std::fs::read_to_string(&argv_log).expect("the engine recorded its argv");
        let launch: Vec<&str> = argv
            .split("--end--\n")
            .find(|launch| launch.lines().any(|line| line == "--path"))
            .expect("a run launch with --path")
            .lines()
            .collect();
        let path_index = launch
            .iter()
            .position(|argument| *argument == "--path")
            .expect("--path argument");
        assert_eq!(
            launch.get(path_index + 1).copied(),
            Some(tree.root.to_str().expect("UTF-8 root")),
            "Godot must receive the discovered project root, not the candidate spelling"
        );

        assert_eq!(payload["classification"], json!("failed"), "{payload}");
        assert_eq!(payload["exit_code"], json!(1), "{payload}");
        let diagnostics = payload["diagnostics"]
            .as_array()
            .expect("diagnostics array");
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic["category"] == json!("script_parse")
                    && diagnostic["location"]["file"] == json!("res://main.gd")
                    && diagnostic["location"]["line"] == json!(3)
            }),
            "the parsed engine error must be returned: {payload}"
        );
        assert!(
            payload["stderr"]["text"]
                .as_str()
                .is_some_and(|stderr| stderr.contains("Expected statement")),
            "bounded stderr text must be returned: {payload}"
        );
        assert_eq!(payload["stderr"]["truncated"], json!(false), "{payload}");

        for private in [
            tree.base.to_string_lossy().into_owned(),
            tree.root.join(".godot-mcp").to_string_lossy().into_owned(),
            "launches/".to_owned(),
            "boundary_identity".to_owned(),
        ] {
            assert!(
                !text.contains(&private),
                "the public response must not contain {private:?}: {text}"
            );
        }
        let _ = session.finish();
    }
}
