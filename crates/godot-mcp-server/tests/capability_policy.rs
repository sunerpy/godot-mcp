//! Todo 26 acceptance suite: the canonical capability matrix and the single
//! policy authority that mints unforgeable dispatch permits.
//!
//! Both descriptor-owned tests are exhaustive by design: `capability_policy.rs`
//! is the mechanical entry point behind `make qa-task TASK=26`, so every
//! acceptance criterion is asserted here rather than spread across ad-hoc tests.
//!
//! The suite reaches the crate-private policy layer only through the
//! `test-hooks` harness, which deliberately never hands a test a permit value.
//! That is itself one of the asserted properties: a permit cannot be forged,
//! borrowed or smuggled, because no reachable API produces one.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::manifests::canonical_manifest;
use godot_mcp_server::test_support::{PolicyHarness, PolicyMatrixRow, policy_matrix};

const FIXTURE: &str = "tests/fixtures/minimal";

/// Fixed transaction IDs, so every assertion names an exact durable record.
const TX_COMMIT: &str = "00000000-0000-4000-8000-000000002601";
const TX_STALE: &str = "00000000-0000-4000-8000-000000002602";
const TX_FOREIGN: &str = "00000000-0000-4000-8000-000000002603";
const TX_QUARANTINE: &str = "00000000-0000-4000-8000-000000002604";
const TX_SURVIVOR: &str = "00000000-0000-4000-8000-000000002605";
const TX_GENERATION: &str = "00000000-0000-4000-8000-000000002606";

/// The four canonical profiles, in manifest order.
const PROFILES: [CapabilityProfile; 4] = [
    CapabilityProfile::Core,
    CapabilityProfile::Editor,
    CapabilityProfile::Runtime,
    CapabilityProfile::Full,
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root is two levels above the crate manifest")
        .to_path_buf()
}

fn fixture_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE)
}

/// Copy the committed fixture into a writable temp root so a test can mutate it
/// without touching the repository copy.
fn writable_project(label: &str) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::Builder::new()
        .prefix(&format!("godot-mcp-task26-{label}-"))
        .tempdir()
        .expect("temp project root");
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).expect("project root");
    std::fs::copy(
        fixture_source().join("project.godot"),
        root.join("project.godot"),
    )
    .expect("copy project marker");
    // Todo 2's `SafeRoot` publishes files relative to an already-held parent
    // directory handle and owns no directory-creation primitive, so the fixture
    // provides the destination directory the permitted write lands in.
    std::fs::create_dir_all(root.join("scenes")).expect("scenes directory");
    (temp, root)
}

fn open_harness(
    label: &str,
    profile: CapabilityProfile,
) -> (tempfile::TempDir, PathBuf, PolicyHarness) {
    let (temp, root) = writable_project(label);
    let harness = PolicyHarness::open(&root, profile).expect("policy harness opens");
    (temp, root, harness)
}

/// Index the canonical matrix by tool name, asserting each tool appears once.
fn matrix_by_tool() -> BTreeMap<String, PolicyMatrixRow> {
    let mut index = BTreeMap::new();
    for row in policy_matrix() {
        let tool = row.tool.clone();
        assert!(
            index.insert(tool.clone(), row).is_none(),
            "the canonical matrix must contain {tool} exactly once"
        );
    }
    index
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[test]
fn capability_policy_happy_path() {
    // 1. The canonical matrix covers every manifest tool of every profile
    //    exactly once, and names no tool the manifests do not advertise.
    let matrix = matrix_by_tool();
    let full_tools = canonical_manifest(CapabilityProfile::Full).tools;
    for profile in PROFILES {
        let manifest = canonical_manifest(profile);
        for tool in &manifest.tools {
            let row = matrix.get(tool).unwrap_or_else(|| {
                panic!(
                    "the canonical matrix has no row for {tool}, advertised by the {} profile",
                    profile.manifest_name()
                )
            });
            // Every row's required capability must be advertised by every
            // profile that advertises the tool, or the profile would advertise
            // an operation it can never authorize.
            let capabilities: BTreeSet<String> = manifest
                .capabilities
                .iter()
                .map(|capability| {
                    serde_json::to_value(capability)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_owned))
                        .expect("a capability serializes to its stable snake_case name")
                })
                .collect();
            assert!(
                capabilities.contains(&row.capability),
                "profile {} advertises {tool} but not its required capability {}",
                profile.manifest_name(),
                row.capability
            );
        }
    }
    for tool in matrix.keys() {
        assert!(
            full_tools.contains(tool),
            "the canonical matrix names {tool}, which no canonical profile advertises"
        );
    }
    assert_eq!(
        matrix.len(),
        full_tools.len(),
        "the matrix and the `full` manifest must have the same tool set"
    );

    // 2. An expected `editor`-profile project write is authorized, receives a
    //    private production permit, and executes against the existing SafeRoot
    //    handle: the bytes really land on disk.
    let (_temp, root, harness) = open_harness("happy", CapabilityProfile::Editor);
    assert_eq!(
        harness.touches().total(),
        0,
        "no boundary may be touched before any dispatch is authorized"
    );

    let read_only = harness.evaluate("scene.tree.get");
    assert!(
        read_only.allowed,
        "a `read` row inside the profile must be allowed: {}",
        read_only.message
    );
    assert!(
        !read_only.permit_issued,
        "a read-only row must mint no permit; there is nothing to authorize at a boundary"
    );
    assert_eq!(
        read_only.decision["verdict"], "allow",
        "an allowed decision must publish verdict=allow, got {}",
        read_only.decision
    );
    assert_eq!(
        read_only.decision["reason"], "granted",
        "an allowed decision must publish reason=granted, got {}",
        read_only.decision
    );

    let report = harness
        .commit_file_write(
            "resource.update",
            TX_COMMIT,
            "scenes/policy.tres",
            b"[gd_resource type=\"Resource\"]\n",
        )
        .unwrap_or_else(|outcome| {
            panic!(
                "an expected profile operation must be authorized and execute: [{:?}] {}",
                outcome.code, outcome.message
            )
        });
    assert_eq!(
        report.transaction_id, TX_COMMIT,
        "the dispatch must settle the exact journaled transaction"
    );
    assert_eq!(
        report.phase, "committed_fsynced",
        "a proven post-state must journal exactly `committed_fsynced`"
    );
    assert!(
        report.decision_id.starts_with("decision_"),
        "the audit decision ID must be a recognizable non-sensitive identifier, got {}",
        report.decision_id
    );
    assert_eq!(
        report.decision_id.len(),
        "decision_".len() + 64,
        "the decision ID must be a full 32-byte digest, got {}",
        report.decision_id
    );

    // The decision ID is non-sensitive: it carries no path spelling, no
    // principal plaintext and no project spelling.
    for leak in [
        "scenes/policy.tres",
        "policy.tres",
        "principal_",
        root.to_string_lossy().as_ref(),
    ] {
        assert!(
            !report.decision_id.contains(leak),
            "the decision ID must not carry {leak}"
        );
    }

    // 3. The write really happened through the held SafeRoot handle.
    let published = std::fs::read(root.join("scenes/policy.tres")).expect("permitted write lands");
    assert_eq!(
        published, b"[gd_resource type=\"Resource\"]\n",
        "the permitted write must publish exactly the staged bytes"
    );
    let touches = harness.touches();
    assert_eq!(
        touches.file, 1,
        "exactly one file-boundary publication may happen, got {touches:?}"
    );
    assert_eq!(
        touches.total(),
        1,
        "no other boundary may be touched, got {touches:?}"
    );

    // 4. The non-sensitive decision ID is in the durable audit log, recorded
    //    before the boundary was reached.
    let audit = harness
        .audit_entries(TX_COMMIT)
        .expect("audit entries load");
    let enforced = audit
        .iter()
        .find(|(event, _detail)| event == "policy_decision_enforced")
        .unwrap_or_else(|| panic!("the boundary re-check must be audited, got {audit:?}"));
    assert!(
        enforced.1.contains(&report.decision_id),
        "the audit record must name the decision ID, got {}",
        enforced.1
    );
    let enforced_index = audit
        .iter()
        .position(|(event, _)| event == "policy_decision_enforced")
        .expect("the enforcement record exists");
    let terminal_index = audit
        .iter()
        .position(|(event, _)| event == "committed_fsynced" || event.contains("committed"))
        .unwrap_or(usize::MAX);
    assert!(
        enforced_index < terminal_index,
        "the decision must be audited before the terminal record, got {audit:?}"
    );
}

// ---------------------------------------------------------------------------
// Failure paths
// ---------------------------------------------------------------------------

#[test]
fn capability_policy_failure_paths() {
    // 1. A permit cannot be forged from outside the crate, and cannot even be
    //    obtained by a test: no reachable API returns one. This is a structural
    //    property, so it is asserted against the sources that establish it.
    let lib = std::fs::read_to_string(repo_root().join("crates/godot-mcp-server/src/lib.rs"))
        .expect("lib source");
    assert!(
        lib.contains("pub(crate) mod policy;"),
        "the policy layer must be crate-private so no external crate can name a permit"
    );
    let permit_source =
        std::fs::read_to_string(repo_root().join("crates/godot-mcp-server/src/effect/permit.rs"))
            .expect("permit source");
    assert!(
        permit_source.contains("pub(crate) struct AuthoritySeal(());"),
        "the authority seal must keep a private unit field so a permit cannot be assembled"
    );
    let authority_source = std::fs::read_to_string(
        repo_root().join("crates/godot-mcp-server/src/policy/authority.rs"),
    )
    .expect("policy authority source");
    assert!(
        authority_source.contains("pub(crate) struct PolicyPermit"),
        "the policy permit wrapper must be crate-private"
    );
    let harness_source = std::fs::read_to_string(
        repo_root().join("crates/godot-mcp-server/src/test_support/policy_harness.rs"),
    )
    .expect("policy harness source");
    assert!(
        !harness_source.contains("-> PolicyPermit")
            && !harness_source.contains("Option<PolicyPermit>")
            && !harness_source.contains("Result<PolicyPermit"),
        "the test harness must never hand a test a permit value"
    );

    // 2. A `core` profile cannot mutate: every mutation row fails the capability
    //    gate with `permission_denied`, and no boundary is touched.
    let (_core_temp, _core_root, core_harness) = open_harness("core", CapabilityProfile::Core);
    let matrix = matrix_by_tool();
    let mutating: Vec<&PolicyMatrixRow> =
        matrix.values().filter(|row| row.requires_permit).collect();
    assert!(
        !mutating.is_empty(),
        "the matrix must contain mutating rows or there is nothing to deny"
    );
    for row in &mutating {
        let outcome = core_harness.evaluate(&row.tool);
        assert!(
            !outcome.allowed,
            "the core profile must not authorize the mutation {}",
            row.tool
        );
        assert_eq!(
            outcome.code,
            Some("permission_denied"),
            "a core-profile mutation must be permission_denied, got {:?} for {}",
            outcome.code,
            row.tool
        );
        assert!(
            !outcome.permit_issued,
            "a refused dispatch must mint no permit for {}",
            row.tool
        );
        assert_eq!(
            outcome.decision["verdict"], "deny",
            "a refusal must publish verdict=deny for {}, got {}",
            row.tool, outcome.decision
        );
    }
    assert_eq!(
        core_harness.touches().total(),
        0,
        "a core-profile mutation must never reach any boundary, got {:?}",
        core_harness.touches()
    );

    // 3. An operation with no matrix row is denied by default, not defaulted open.
    let unknown = core_harness.evaluate("unsafe_script");
    assert!(
        !unknown.allowed,
        "a tool absent from the canonical matrix must be denied"
    );
    assert_eq!(
        unknown.code,
        Some("not_found"),
        "an unknown operation must be not_found, got {:?}",
        unknown.code
    );
    assert_eq!(
        core_harness.touches().total(),
        0,
        "an unknown operation must never reach any boundary"
    );

    // 4. A missing permit: a mutating dispatch that names no already-journaled
    //    transaction is refused before anything is queued.
    let (_missing_temp, _missing_root, missing_harness) =
        open_harness("missing", CapabilityProfile::Full);
    let missing = missing_harness.evaluate("resource.update");
    assert!(
        !missing.allowed,
        "a mutating dispatch with no journaled transaction must be refused"
    );
    assert_eq!(
        missing.code,
        Some("permission_denied"),
        "a missing permit must be permission_denied, got {:?}",
        missing.code
    );
    assert!(
        !missing.permit_issued,
        "no permit may exist for a refused mutating dispatch"
    );
    assert_eq!(
        missing_harness.touches().total(),
        0,
        "a missing permit must never reach any boundary"
    );

    // 5. A stale permit: one minted by a superseded authority epoch is refused at
    //    the final boundary re-check, before the gateway is entered.
    let (_stale_temp, stale_root, stale_harness) = open_harness("stale", CapabilityProfile::Full);
    let stale = stale_harness.dispatch_with_stale_permit(
        "resource.update",
        TX_STALE,
        "scenes/stale.tres",
        b"stale\n",
    );
    assert!(
        !stale.allowed,
        "a permit from a superseded epoch must be refused: {}",
        stale.message
    );
    assert_eq!(
        stale.code,
        Some("permission_denied"),
        "a stale permit must be permission_denied, got {:?}: {}",
        stale.code,
        stale.message
    );
    assert_eq!(
        stale_harness.touches().total(),
        0,
        "a stale permit must never reach any boundary, got {:?}",
        stale_harness.touches()
    );
    assert!(
        !stale_root.join("scenes/stale.tres").exists(),
        "a stale permit must produce no file"
    );

    // 6. A permit whose decision was made for a different identity is refused:
    //    the decision ID re-derived at the boundary does not match.
    let (_foreign_temp, foreign_root, foreign_harness) =
        open_harness("foreign", CapabilityProfile::Full);
    let foreign = foreign_harness.dispatch_with_foreign_decision(
        "resource.update",
        TX_FOREIGN,
        "scenes/foreign.tres",
        b"foreign\n",
    );
    assert!(
        !foreign.allowed,
        "a permit authorized for another identity must be refused: {}",
        foreign.message
    );
    assert_eq!(
        foreign.code,
        Some("permission_denied"),
        "a foreign decision must be permission_denied, got {:?}: {}",
        foreign.code,
        foreign.message
    );
    assert_eq!(
        foreign_harness.touches().total(),
        0,
        "a foreign decision must never reach any boundary, got {:?}",
        foreign_harness.touches()
    );
    assert!(
        !foreign_root.join("scenes/foreign.tres").exists(),
        "a foreign decision must produce no file"
    );

    // 7. A changed project generation is refused with `conflict`.
    let (_gen_temp, _gen_root, gen_harness) = open_harness("generation", CapabilityProfile::Full);
    for tool in ["scene.tree.get", "resource.update"] {
        let outcome = gen_harness.evaluate_with_next_generation(tool);
        assert!(
            !outcome.allowed,
            "a changed project generation must refuse {tool}"
        );
        assert_eq!(
            outcome.code,
            Some("conflict"),
            "a changed generation must be conflict for {tool}, got {:?}: {}",
            outcome.code,
            outcome.message
        );
    }
    // A generation change must be refused even when a transaction was journaled.
    let generation_dispatch = gen_harness.dispatch_with_stale_permit(
        "resource.update",
        TX_GENERATION,
        "scenes/generation.tres",
        b"generation\n",
    );
    assert!(
        !generation_dispatch.allowed,
        "a superseded epoch must be refused for the generation probe too"
    );
    assert_eq!(
        gen_harness.touches().total(),
        0,
        "a generation mismatch must never reach any boundary, got {:?}",
        gen_harness.touches()
    );

    // 8. A presented identity that is not the bound identity is refused.
    let (_id_temp, _id_root, id_harness) = open_harness("identity", CapabilityProfile::Full);
    let other = id_harness.evaluate_as_other_principal("resource.update");
    assert!(
        !other.allowed,
        "a different principal must not be authorized"
    );
    assert_eq!(
        other.code,
        Some("permission_denied"),
        "a principal mismatch must be permission_denied, got {:?}: {}",
        other.code,
        other.message
    );
    assert_eq!(
        other.decision["reason"], "principal_mismatch",
        "a principal mismatch must publish its own frozen reason, got {}",
        other.decision
    );
    assert_eq!(
        id_harness.touches().total(),
        0,
        "an identity mismatch must never reach any boundary"
    );

    // 9. Quarantine blocks every dispatch, read and mutation alike, with
    //    `conflict`, before any boundary is touched.
    let (_q_temp, _q_root, q_harness) = open_harness("quarantine", CapabilityProfile::Full);
    q_harness
        .quarantine(TX_QUARANTINE, "scenes/quarantine.tres")
        .expect("harness can force a quarantined transaction");
    for tool in ["scene.tree.get", "resource.update", "project.run"] {
        let outcome = q_harness.evaluate(tool);
        assert!(!outcome.allowed, "quarantine must refuse {tool}");
        assert_eq!(
            outcome.code,
            Some("conflict"),
            "quarantine must be conflict for {tool}, got {:?}: {}",
            outcome.code,
            outcome.message
        );
        assert_eq!(
            outcome.decision["reason"], "project_quarantined",
            "quarantine must publish its own frozen reason for {tool}, got {}",
            outcome.decision
        );
    }
    assert_eq!(
        q_harness.touches().total(),
        0,
        "quarantine must never reach any boundary, got {:?}",
        q_harness.touches()
    );
    assert_eq!(
        q_harness
            .phase_of(TX_QUARANTINE)
            .expect("the quarantined record loads"),
        "quarantined",
        "the quarantined record must stay quarantined"
    );

    // 10. An unresolved launch survivor blocks every dispatch with `conflict`,
    //     and an *unreadable* survivor state is a refusal, never an assumed clear.
    let (_s_temp, s_root, s_harness) = open_harness("survivor", CapabilityProfile::Full);
    s_harness.set_unresolved_survivor(Some(
        "1 unresolved launch survivor(s) block every mutation pending operator repair",
    ));
    for tool in ["scene.tree.get", "resource.update"] {
        let outcome = s_harness.evaluate(tool);
        assert!(
            !outcome.allowed,
            "an unresolved survivor must refuse {tool}"
        );
        assert_eq!(
            outcome.code,
            Some("conflict"),
            "an unresolved survivor must be conflict for {tool}, got {:?}: {}",
            outcome.code,
            outcome.message
        );
    }
    let blocked = s_harness.commit_file_write(
        "resource.update",
        TX_SURVIVOR,
        "scenes/survivor.tres",
        b"survivor\n",
    );
    let blocked = blocked.expect_err("an unresolved survivor must block the write");
    assert_eq!(
        blocked.code,
        Some("conflict"),
        "a survivor-blocked write must be conflict, got {:?}: {}",
        blocked.code,
        blocked.message
    );
    assert!(
        !s_root.join("scenes/survivor.tres").exists(),
        "a survivor-blocked write must produce no file"
    );
    assert_eq!(
        s_harness.touches().total(),
        0,
        "a survivor-blocked dispatch must never reach any boundary, got {:?}",
        s_harness.touches()
    );

    s_harness.set_unresolved_survivor(None);
    s_harness.set_survivor_state_unreadable(true);
    let unreadable = s_harness.evaluate("resource.update");
    assert!(
        !unreadable.allowed,
        "an unreadable survivor state must refuse rather than assume clear"
    );
    assert_eq!(
        unreadable.code,
        Some("conflict"),
        "an unreadable survivor state must be conflict, got {:?}: {}",
        unreadable.code,
        unreadable.message
    );
    assert_eq!(
        s_harness.touches().total(),
        0,
        "an unreadable survivor state must never reach any boundary"
    );

    // 11. The editor plane is handled honestly. Under contract 0.2 the matrix is
    //     the catalog: the live-editor methods sit on the editor boundary, which
    //     the shipped `serve` assembly backs with the editor bridge. This
    //     harness compiles no editor boundary in, so every editor-boundary row —
    //     and every frozen version 0.1 editor shape — resolves to `unavailable`,
    //     not `permission_denied`, and never reaches a boundary.
    let (_e_temp, _e_root, e_harness) = open_harness("editor", CapabilityProfile::Full);
    let availability: BTreeMap<String, bool> =
        e_harness.boundary_availability().into_iter().collect();
    assert_eq!(
        availability.get("file"),
        Some(&true),
        "the file boundary ships in this release, got {availability:?}"
    );
    assert_eq!(
        availability.get("editor"),
        Some(&false),
        "this harness compiles no editor boundary in, got {availability:?}"
    );
    let editor_rows: BTreeSet<&str> = matrix
        .values()
        .filter(|row| row.boundary.as_deref() == Some("editor"))
        .map(|row| row.tool.as_str())
        .collect();
    assert_eq!(
        editor_rows,
        BTreeSet::from([
            "editor.addon.reload",
            "editor.redo",
            "editor.scene.open",
            "editor.scene.tree.get",
            "editor.selection.set",
            "editor.types.get",
            "editor.undo",
        ]),
        "exactly the live-editor methods sit on the editor boundary"
    );
    for tool in &editor_rows {
        let outcome = e_harness.evaluate(tool);
        let mutation = matrix.get(*tool).is_some_and(|row| row.requires_permit);
        if mutation {
            assert!(
                !outcome.allowed,
                "an absent editor boundary must refuse {tool}"
            );
            assert_eq!(
                outcome.code,
                Some("unavailable"),
                "an absent editor boundary is unavailable for {tool}, got {:?}: {}",
                outcome.code,
                outcome.message
            );
        }
    }
    let historical_editor = [
        "editor.selection.get",
        "editor.state.get",
        "node.create",
        "node.delete",
        "node.property.set",
        "node.reparent",
    ];
    for tool in historical_editor {
        let outcome = e_harness.evaluate_historical(tool);
        assert!(
            !outcome.allowed,
            "an unavailable boundary must refuse {tool}"
        );
        assert_eq!(
            outcome.code,
            Some("unavailable"),
            "an unshipped boundary must be unavailable for {tool}, got {:?}: {}",
            outcome.code,
            outcome.message
        );
        let listed = e_harness.evaluate(tool);
        assert_eq!(
            listed.code,
            Some("not_found"),
            "{tool} has no production handler, so it is not a catalog operation"
        );
    }
    assert_eq!(
        e_harness.touches().total(),
        0,
        "an unavailable boundary must never be invoked, got {:?}",
        e_harness.touches()
    );

    // 12. Exhaustiveness: every tool of every canonical profile has an explicit
    //     policy verdict. A tool the matrix forgot is a failure, not a default.
    for profile in PROFILES {
        let (_temp, _root, harness) =
            open_harness(&format!("exhaustive-{}", profile.manifest_name()), profile);
        for tool in &canonical_manifest(profile).tools {
            let outcome = harness.evaluate(tool);
            assert_ne!(
                outcome.code,
                Some("not_found"),
                "profile {} advertises {tool} but the policy matrix has no verdict for it",
                profile.manifest_name()
            );
            if !outcome.allowed {
                let code = outcome.code.expect("a refusal always carries a code");
                assert!(
                    matches!(code, "permission_denied" | "conflict" | "unavailable"),
                    "a refusal for {tool} in profile {} must be one of \
                     permission_denied/conflict/unavailable, got {code}: {}",
                    profile.manifest_name(),
                    outcome.message
                );
            }
        }
        assert_eq!(
            harness.touches().total(),
            0,
            "evaluation alone must never touch a boundary in profile {}",
            profile.manifest_name()
        );
    }
}
