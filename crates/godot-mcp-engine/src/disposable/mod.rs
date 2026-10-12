//! Disposable-copy execution and the explicit untrusted-project posture.
//!
//! # What this module is
//!
//! Loading a Godot project executes the project's own code. The official
//! documentation states that `@tool` "makes it execute in the editor", warns that
//! "the editor does not include protections for potential misuse of `@tool`
//! scripts", and notes that "Modifications in the editor are permanent, with no
//! undo/redo possible"
//! (<https://docs.godotengine.org/en/stable/tutorials/plugins/running_code_in_the_editor.html>).
//! An autoload is instantiated as soon as the project loads, and `--import` sets
//! `editor = true`. So a plain import of an unfamiliar project is arbitrary code
//! execution against the operator's machine, with permanent effects on their
//! files.
//!
//! This module's answer is to run that code against a **deterministic filtered
//! copy** of the project and then throw the copy away:
//!
//! | module | responsibility |
//! | --- | --- |
//! | [`posture`] | what a copy does and does not contain, in shipped constants |
//! | [`exclusion`] | the explicit, enumerable policy for what never enters a copy |
//! | [`copy`] | making, hashing and comparing the copy; symlinks refused |
//! | [`error`] | the closed failure set, mapped to Todo 3's error kinds |
//! | [`session`] | the normative order: isolate, copy, run, discard |
//!
//! # What this module is **not**
//!
//! It is **not a code sandbox**. A copy redirects which project tree an
//! untrusted run may mutate; it restricts nothing about what that code may
//! execute, read or reach. An absolute-path write, a network connection and a
//! read of the original files are all still possible.
//! [`posture::UNCONTAINED_BY_A_COPY`] enumerates them, individually, so no
//! consumer of this API can fold them into a reassurance.
//!
//! Confining the *process* is Todo 14's job, and for untrusted code only a
//! **strong** boundary qualifies (a Windows Job Object, a Linux cgroup v2
//! sub-tree, or a configured container/VM adapter). Three rules follow, and all
//! three are enforced in [`session::DisposableSession::prepare`] rather than
//! merely documented:
//!
//! 1. the boundary is selected **before** anything is created, by Todo 14's own
//!    fail-closed [`crate::supervision::boundary::select_provider`]; when nothing
//!    strong is available the operation fails with `unsupported` and the
//!    filesystem is untouched;
//! 2. a weak process-group/locator boundary is **never** treated as sufficient
//!    for untrusted code — Todo 14 refuses it, and this module does not implement
//!    a second opinion;
//! 3. isolation is **never** silently downgraded: there is no fallback path from
//!    "no strong boundary" to "run it anyway".
//!
//! And one rule about the original tree: when disposable mode is requested, the
//! untrusted project is never loaded in place.
//! [`session::DisposableSession::import`] rebinds the caller's
//! [`crate::validation::runner::EngineBinding`] to the copy's root, so a binding
//! that still names the original cannot reach the engine.
//!
//! # What runs the engine
//!
//! Nothing here spawns a process. Execution goes through Todo 15's
//! [`crate::validation`] operations, which go through Todo 14's launch protocol:
//! pre-created boundary, durable identity receipt before any user code, byte-
//! capped output, supervisor-enforced deadline. No module in this plane
//! constructs an OS process itself, which the `no_module_in_this_plane_spawns_a_process_itself`
//! test asserts mechanically against the module sources.

pub mod copy;
pub mod error;
pub mod exclusion;
pub mod posture;
pub mod session;

pub use copy::{CopiedEntry, CopyManifest, ExcludedEntry, digest_source_tree};
pub use error::DisposableError;
pub use exclusion::{ExclusionPolicy, ExclusionReason};
pub use session::{DisposableRequest, DisposableRun, DisposableSession, SelectedBoundary};

#[cfg(test)]
mod tests {
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
        // Execution is Todo 15's validation path through Todo 14's launch
        // protocol. A process built here would exist outside the containment
        // boundary and outside the launch journal.
        for name in [
            "mod.rs",
            "posture.rs",
            "exclusion.rs",
            "copy.rs",
            "error.rs",
            "session.rs",
        ] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src/disposable")
                .join(name);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            for spelling in forbidden_spellings() {
                assert!(
                    !text.contains(&spelling),
                    "src/disposable/{name} must not build a process itself ({spelling})"
                );
            }
        }
    }
}
