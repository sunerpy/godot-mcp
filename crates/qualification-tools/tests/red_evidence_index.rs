//! Regression test for the committed Red evidence index.
//!
//! `.omo/evidence/godot-mcp-server-architecture/red-evidence-index.json` is the
//! machine-checkable record that every implemented todo was driven by a
//! genuinely failing test before it passed. `produce-qualification` binds it
//! into the frozen qualification input identity and `f1-plan-compliance`
//! re-verifies it, so a drifted, hand-edited or stale index would be discovered
//! only at release time.
//!
//! This test runs the real [`RedEvidenceIndex::verify`] against the committed
//! file at the real workspace root, so the index is held to exactly the
//! contract the release producer enforces: every named artifact exists, every
//! recorded digest matches the artifact's actual bytes, every Red exit is
//! nonzero, every Green exit is zero, and no cancelled todo claims a Red proof.
//! It additionally asserts the index is canonical (sorted todos, no duplicates)
//! so regenerating it stays byte-stable.

use std::fs;
use std::path::{Path, PathBuf};

use qualification_tools::qualification::{RedEvidenceIndex, is_cancelled};
use serde_json::Value;

/// The workspace root, two levels above this crate's manifest.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate manifest lives two levels below the workspace root")
        .to_path_buf()
}

fn committed_index(workspace: &Path) -> PathBuf {
    workspace
        .join(".omo")
        .join("evidence")
        .join("godot-mcp-server-architecture")
        .join("red-evidence-index.json")
}

/// Callers must still verify the copy against the real workspace root, not
/// `directory`: the copy keeps relative artifact paths, so only a real workspace
/// base resolves them to the frozen artifacts and leaves the mutated field as
/// the single difference from committed truth.
fn tampered_copy(workspace: &Path, directory: &Path, mutate: impl FnOnce(&mut Value)) -> PathBuf {
    let bytes = fs::read(committed_index(workspace)).expect("the committed index is readable");
    let mut document: Value =
        serde_json::from_slice(&bytes).expect("the committed index is valid JSON");
    mutate(&mut document);
    let path = directory.join("tampered-red-evidence-index.json");
    fs::write(
        &path,
        serde_json::to_vec(&document).expect("the tampered index re-serializes"),
    )
    .expect("the tampered index is writable");
    path
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn committed_red_evidence_index_verifies_against_its_artifacts() {
    let workspace = workspace_root();
    let index = workspace
        .join(".omo")
        .join("evidence")
        .join("godot-mcp-server-architecture")
        .join("red-evidence-index.json");
    assert!(
        index.is_file(),
        "the committed Red evidence index must exist at {}",
        index.display()
    );

    let parsed = RedEvidenceIndex::verify(&index, &workspace).unwrap_or_else(|error| {
        panic!("the committed Red evidence index must satisfy the real producer contract: {error}")
    });

    assert_eq!(
        parsed.schema_version, "1",
        "the index declares the gate schema version the producer expects"
    );
    assert!(
        !parsed.entries.is_empty(),
        "an index with no entries proves nothing"
    );

    let todos: Vec<u32> = parsed.entries.iter().map(|entry| entry.todo).collect();
    let mut sorted = todos.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        todos, sorted,
        "entries are sorted by ascending todo and never repeat a todo, so regenerating the index \
         is byte-stable"
    );

    for entry in &parsed.entries {
        assert!(
            !is_cancelled(entry.todo),
            "todo {} is cancelled and cannot carry a Red proof",
            entry.todo
        );
        assert!(
            entry.red_output != entry.green_output,
            "todo {}'s Red and Green artifacts must be distinct files",
            entry.todo
        );
        assert!(
            entry.red_output_sha256 != entry.green_output_sha256,
            "todo {}'s Red and Green artifacts must differ in content, or the failure and the \
             pass were the same run",
            entry.todo
        );
    }
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn a_fabricated_entry_is_rejected_by_the_real_validator() {
    let workspace = workspace_root();
    let directory = tempfile::tempdir().expect("a temp directory is available");

    let forged_digest = tampered_copy(&workspace, directory.path(), |document| {
        document["entries"][0]["red_output_sha256"] = Value::String("0".repeat(64));
    });
    let error = RedEvidenceIndex::verify(&forged_digest, &workspace)
        .expect_err("a digest the artifact bytes do not produce must be rejected");
    let message = error.to_string();
    assert!(
        message.contains("hashes to"),
        "the rejection must name the recomputed digest, got: {message}"
    );

    let zero_red_exit = tampered_copy(&workspace, directory.path(), |document| {
        document["entries"][0]["red_exit_code"] = Value::from(0);
    });
    let error = RedEvidenceIndex::verify(&zero_red_exit, &workspace)
        .expect_err("a Red proof that exited 0 never reproduced a failure");
    assert!(
        error.to_string().contains("exited 0"),
        "the rejection must say the Red run exited 0, got: {error}"
    );

    let cancelled_claim = tampered_copy(&workspace, directory.path(), |document| {
        document["entries"][0]["todo"] = Value::from(20);
    });
    let error = RedEvidenceIndex::verify(&cancelled_claim, &workspace)
        .expect_err("a cancelled todo cannot claim a Red proof");
    assert!(
        error.to_string().contains("cancelled"),
        "the rejection must name the cancelled todo, got: {error}"
    );
}
