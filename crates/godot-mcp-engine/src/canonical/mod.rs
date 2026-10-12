//! Engine-owned canonical `.tscn` / `.tres` writes and `ResourceUID` operations.
//!
//! # Why the engine, and not this crate, writes the resource
//!
//! Two things here belong to Godot and are not reproducible in Rust without
//! reimplementing them:
//!
//! * **Canonical resource text.** `ResourceSaver.save()` owns the `.tscn` /
//!   `.tres` serialisation. Hand-written resource text is not canonical bytes: a
//!   plausible-looking file can differ in property ordering, sub-resource
//!   numbering, or an `ext_resource` spelling, and then load differently or not at
//!   all.
//! * **`ResourceUID` values.** `ResourceUID.create_id_for_path()` is seeded by the
//!   engine with the path *and* the project name, and `id_to_text()` owns the
//!   `uid://` spelling. Deriving either in Rust would be implementing UID hashing,
//!   which this project forbids outright.
//!
//! So a bundled, versioned, SHA-256-pinned **pure GDScript** helper does the
//! writing, inside a supervised, disposable, exiting Godot subprocess launched
//! through Todo 14's launch protocol. No gdext, no GDExtension, no compiled
//! artifact. The Rust side owns only where the helper is installed, what it is
//! asked for, and whether the result is provable.
//!
//! | module | responsibility |
//! | --- | --- |
//! | [`helper`] | the pinned helper bytes, its versioned install layout, its four exit codes |
//! | [`request`] | what a write asks for, and which targets are writable at all |
//! | [`transaction`] | the `file`-effect record, the composed proof rule, the three terminal classifications |
//! | [`stage`] | running the helper through the launch protocol and reading the linked proof back |
//! | [`publish`] | the normative order, the CAS publish, the composite commit gate |
//! | [`error`] | the closed failure set, mapped to Todo 3's error kinds |
//!
//! # This transaction's effect kind is `file`
//!
//! A canonical write is **never** `editor_runtime`. There is no live editor
//! session here and no `EditorUndoRedoManager` action — only a disposable
//! subprocess that exits. [`transaction::FileRecord`] hard-codes `effect_kind:
//! "file"`, so the misclassification is not representable.
//!
//! # A commit requires both halves, always
//!
//! Every canonical write carries
//! `required_subproofs.process_launch_transaction_id`, naming the helper-launch
//! transaction, and reaches `committed_fsynced` only when **both** hold:
//!
//! 1. the observed bytes equal the intended post-state recorded before the
//!    publish; **and**
//! 2. that linked transaction's `process_launch` proof independently shows a clean
//!    terminal process — confirmed containment, `exited-clean`, exit code 0, no
//!    signal, proven-empty boundary afterwards.
//!
//! Neither half alone is ever sufficient, and this is not a stylistic preference.
//! Todo 13 observed directly that a real Godot build **exits 0 with empty output
//! when handed a flag it does not support**, and the official command-line
//! reference states that unknown arguments "have no effect whatsoever. The engine
//! will **not** warn you". So the helper's exit status cannot establish that the
//! write happened. And matching bytes cannot either: they may match because a
//! previous attempt wrote them, because an external actor wrote them, or because
//! the helper is *still running* and may yet write more. A live survivor, a
//! signalled helper and a nonzero exit are all refused as post-state evidence even
//! when the bytes match exactly.
//!
//! # What this module never does
//!
//! * never creates or retrofits a second persistence layer —
//!   [`transaction::FileJournal`] is a port over the same Todo 6 store, with Todo
//!   6's canonical schema, Todo 6's four fsynced phases and Todo 6's three
//!   dispositions;
//! * never implements UID hashing or synthesises resource text;
//! * never mutates a binary `.scn` / `.res`;
//! * never overwrites an externally changed or editor-open resource, and offers no
//!   force-overwrite path;
//! * never bypasses quarantine, and never introduces a fourth proof kind;
//! * never builds an OS process itself — every launch goes through Todo 14, which
//!   the `no_module_in_this_plane_spawns_a_process_itself` test asserts against
//!   this module's own sources.

pub mod error;
pub mod helper;
pub mod publish;
pub mod request;
pub mod stage;
pub mod transaction;

pub use error::CanonicalError;
pub use helper::{HelperExit, HelperResponse, HelperScript};
pub use publish::{CanonicalDiff, CanonicalWrite, CanonicalWriter};
pub use request::{CanonicalOperation, CanonicalRequest, TextResourceKind};
pub use stage::{EngineContext, StagedArtifact};
pub use transaction::{
    FileEntry, FileJournal, FileOperation, FileProof, FileRecord, LinkedProof,
    StartupClassification,
};

#[cfg(test)]
mod tests {
    const MODULES: [&str; 7] = [
        "mod.rs",
        "error.rs",
        "helper.rs",
        "publish.rs",
        "request.rs",
        "stage.rs",
        "transaction.rs",
    ];

    fn source(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/canonical")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
    }

    /// The forbidden spellings, assembled at runtime so this test's own source
    /// text cannot satisfy the search it performs.
    fn forbidden_spellings() -> [String; 2] {
        let command = format!("Comm{}", "and");
        [
            format!("{command}::new"),
            format!("std::process::{command}"),
        ]
    }

    #[test]
    fn no_module_in_this_plane_spawns_a_process_itself() {
        // Every Godot invocation goes through Todo 15's runner and Todo 14's launch
        // protocol. A process built here would exist outside the containment
        // boundary and outside the launch journal.
        for name in MODULES {
            let text = source(name);
            for spelling in forbidden_spellings() {
                assert!(
                    !text.contains(&spelling),
                    "src/canonical/{name} must not build a process itself ({spelling})"
                );
            }
        }
    }

    #[test]
    fn a_dry_run_is_structurally_incapable_of_publishing() {
        // `preview` is the dry-run entry point. The property asserted here is
        // structural: the function body contains no publish call at all, so a dry
        // run cannot write bytes to a caller target even by mistake.
        let publish_source = source("publish.rs");
        let preview_start = publish_source
            .find("    pub fn preview(")
            .expect("the dry-run entry point is named preview");
        let preview_end = publish_source[preview_start..]
            .find("    /// Run one canonical write end to end.")
            .map(|offset| preview_start + offset)
            .expect("preview is followed by execute");
        let preview_body = &publish_source[preview_start..preview_end];
        assert!(
            !preview_body.contains("cas_write"),
            "preview() must contain no compare-and-swap publish call"
        );
        assert!(
            !preview_body.contains("self.publish("),
            "preview() must not call the publisher"
        );
        assert!(
            preview_body.contains("CanonicalDiff"),
            "preview() returns a diff instead of writing"
        );
    }

    #[test]
    fn the_only_publish_call_targets_the_recorded_preimage() {
        let publish_source = source("publish.rs");
        let calls = publish_source.matches(".cas_write(").count();
        assert_eq!(
            calls, 1,
            "there is exactly one compare-and-swap publish call on a caller target in this module"
        );
        assert!(
            publish_source.contains(".cas_write(Path::new(target), expected, &staged.bytes)"),
            "the publish is a compare-and-swap against a preimage the caller supplies, never an \
             unconditional overwrite"
        );
        assert!(
            publish_source
                .contains("self.publish(&request.target, before_bytes.as_deref(), &staged)"),
            "the preimage handed to the publish is the one captured *before* the helper ran; \
             re-reading the target at publish time would compare it against itself and silently \
             overwrite an external change"
        );
    }

    /// The forbidden override spellings, assembled at runtime for the same reason.
    fn forbidden_override_spellings() -> [String; 3] {
        let force = format!("for{}", "ce");
        [
            format!("{force}_overwrite"),
            format!("{force}_write"),
            format!("ig{}_conflict", "nore"),
        ]
    }

    #[test]
    fn no_module_offers_an_override_path() {
        for name in MODULES {
            let text = source(name).to_ascii_lowercase();
            for spelling in forbidden_override_spellings() {
                assert!(
                    !text.contains(&spelling),
                    "src/canonical/{name} must not offer a {spelling} path"
                );
            }
        }
    }

    #[test]
    fn this_transaction_is_never_classified_as_editor_runtime() {
        let transaction_source = source("transaction.rs");
        assert!(
            transaction_source.contains("pub const EFFECT_KIND: &str = \"file\";"),
            "the effect kind is the frozen constant `file`"
        );
        // The only occurrence of the quoted editor_runtime spelling is the
        // assert_ne that rejects it; it is never a value this module writes.
        let quoted = transaction_source
            .matches("json!(\"editor_runtime\")")
            .count();
        assert_eq!(
            quoted, 1,
            "editor_runtime appears only in the assertion that rejects it"
        );
    }
}
