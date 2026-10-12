//! The frozen Task-34 fixture identifiers.
//!
//! Every identifier here is normative: `qa/tasks/34.json` names these exact
//! transaction IDs and proof-file names, and the descriptor-inventory validator
//! asserts they appear verbatim. Nothing in this module may be renamed without
//! changing the descriptor, which is owned elsewhere.

/// The `file` transaction whose live bytes still equal its recorded pre-state.
pub const FILE_PRE: &str = "34000000-0000-4000-8000-000000000101";
/// The `file` transaction whose live bytes already equal its intended post-state.
pub const FILE_POST: &str = "34000000-0000-4000-8000-000000000102";
/// The `process_launch` transaction left with a still-blocked survivor.
pub const PROCESS_PRE: &str = "34000000-0000-4000-8000-000000000201";
/// The `process_launch` transaction left with a resumed, still-contained survivor.
pub const PROCESS_POST: &str = "34000000-0000-4000-8000-000000000202";
/// The `editor_runtime` transaction claimed in its pre-state.
pub const EDITOR_PRE: &str = "34000000-0000-4000-8000-000000000301";
/// The `editor_runtime` transaction claimed in its post-state.
pub const EDITOR_POST: &str = "34000000-0000-4000-8000-000000000302";

/// Composite: the linked `process_launch` proof is deliberately unrecorded.
pub const COMPOSITE_PROOF_MISSING: &str = "34000000-0000-4000-8000-000000001001";
/// Composite: the recorded linked transaction ID names an unrelated transaction.
pub const COMPOSITE_WRONG_TRANSACTION: &str = "34000000-0000-4000-8000-000000001002";
/// Composite: the linked process is deliberately left live and unterminated.
pub const COMPOSITE_PROCESS_LIVE: &str = "34000000-0000-4000-8000-000000001003";
/// Composite: the linked process is deliberately left in a signaled terminal state.
pub const COMPOSITE_PROCESS_SIGNALED: &str = "34000000-0000-4000-8000-000000001004";
/// Composite: the linked process is deliberately left with a nonzero exit code.
pub const COMPOSITE_PROCESS_NONZERO_EXIT: &str = "34000000-0000-4000-8000-000000001005";
/// Composite: the linked process leaves a non-empty post-exit descendant set.
pub const COMPOSITE_BOUNDARY_NONEMPTY: &str = "34000000-0000-4000-8000-000000001006";

/// The linked `process_launch` transaction setup creates for each composite, in
/// composite order. `LINKED[1]` belongs to [`COMPOSITE_WRONG_TRANSACTION`] and is
/// deliberately *not* the ID that composite's record names.
pub const LINKED: [&str; 6] = [
    "34000000-0000-4000-8000-000000002001",
    "34000000-0000-4000-8000-000000002002",
    "34000000-0000-4000-8000-000000002003",
    "34000000-0000-4000-8000-000000002004",
    "34000000-0000-4000-8000-000000002005",
    "34000000-0000-4000-8000-000000002006",
];

/// The unrelated transaction [`COMPOSITE_WRONG_TRANSACTION`] deliberately points
/// at. It is a `file` transaction, so it can never satisfy a linked
/// `process_launch` re-derivation, and no descriptor command ever repairs it —
/// the refusal is therefore stable regardless of command ordering.
pub const UNRELATED_DECOY: &str = "34000000-0000-4000-8000-000000002999";

/// Every base transaction, in descriptor order.
pub const BASE: [&str; 6] = [
    FILE_PRE,
    FILE_POST,
    PROCESS_PRE,
    PROCESS_POST,
    EDITOR_PRE,
    EDITOR_POST,
];

/// Every Todo 17 composite transaction, in descriptor order.
pub const COMPOSITE: [&str; 6] = [
    COMPOSITE_PROOF_MISSING,
    COMPOSITE_WRONG_TRANSACTION,
    COMPOSITE_PROCESS_LIVE,
    COMPOSITE_PROCESS_SIGNALED,
    COMPOSITE_PROCESS_NONZERO_EXIT,
    COMPOSITE_BOUNDARY_NONEMPTY,
];

/// The twelve proof-file names, paired with the transaction each one proves.
pub const PROOF_FILES: [(&str, &str); 12] = [
    ("file-pre-state-match.json", FILE_PRE),
    ("file-post-state-match.json", FILE_POST),
    ("process-pre-state-match.json", PROCESS_PRE),
    ("process-post-state-match.json", PROCESS_POST),
    ("editor-pre-state-match.json", EDITOR_PRE),
    ("editor-post-state-match.json", EDITOR_POST),
    (
        "composite-linked-proof-missing.json",
        COMPOSITE_PROOF_MISSING,
    ),
    (
        "composite-linked-proof-wrong-transaction.json",
        COMPOSITE_WRONG_TRANSACTION,
    ),
    ("composite-linked-process-live.json", COMPOSITE_PROCESS_LIVE),
    (
        "composite-linked-process-signaled.json",
        COMPOSITE_PROCESS_SIGNALED,
    ),
    (
        "composite-linked-process-nonzero-exit.json",
        COMPOSITE_PROCESS_NONZERO_EXIT,
    ),
    (
        "composite-linked-boundary-nonempty.json",
        COMPOSITE_BOUNDARY_NONEMPTY,
    ),
];

/// The project-relative path each `file`-kind transaction touches.
#[must_use]
pub fn touched_path(transaction_id: &str) -> Option<&'static str> {
    match transaction_id {
        FILE_PRE => Some("scenes/base_pre.tscn"),
        FILE_POST => Some("scenes/base_post.tscn"),
        COMPOSITE_PROOF_MISSING => Some("scenes/composite_01.tscn"),
        COMPOSITE_WRONG_TRANSACTION => Some("scenes/composite_02.tscn"),
        COMPOSITE_PROCESS_LIVE => Some("scenes/composite_03.tscn"),
        COMPOSITE_PROCESS_SIGNALED => Some("scenes/composite_04.tscn"),
        COMPOSITE_PROCESS_NONZERO_EXIT => Some("scenes/composite_05.tscn"),
        COMPOSITE_BOUNDARY_NONEMPTY => Some("scenes/composite_06.tscn"),
        UNRELATED_DECOY => Some("scenes/decoy.tscn"),
        _ => None,
    }
}
