//! What a disposable copy does and does not contain, stated once, in shipped
//! constants rather than only in prose.
//!
//! # This is mutation containment, **not a code sandbox**
//!
//! A disposable copy changes *which project tree* an untrusted Godot run is able
//! to mutate. That is all it does. It does not restrict what the process can
//! execute, which files it can read, or where else it can write. Anything that
//! claims otherwise is claiming a sandbox this project does not provide and does
//! not intend to provide.
//!
//! The reason the posture exists at all is that loading a Godot project is
//! already arbitrary code execution. The official documentation says a `@tool`
//! annotation "makes it execute in the editor", warns that "the editor does not
//! include protections for potential misuse of `@tool` scripts", and states that
//! "Modifications in the editor are permanent, with no undo/redo possible"
//! (<https://docs.godotengine.org/en/stable/tutorials/plugins/running_code_in_the_editor.html>).
//! `--import` sets `editor = true`, and an autoload is instantiated as soon as
//! the project loads — so a `@tool` autoload runs the project's own code during
//! an ordinary import, before anyone asked for a game to start.
//!
//! Because the code runs, the only honest questions are *what may it change* and
//! *what is still enforcing a limit on it*:
//!
//! * **What may it change** — a filtered copy, discarded afterwards. That is
//!   this module's subject.
//! * **What is still enforcing a limit** — Todo 14's containment boundary, and
//!   only a **strong** one for untrusted code (a Windows Job Object, a Linux
//!   cgroup v2 sub-tree, or a configured container/VM adapter). Even that is a
//!   membership and resource boundary, not a code sandbox.

/// The disclaimer every disposable operation carries. Nothing in this crate may
/// describe the disposable-copy mechanism in stronger terms than this string.
pub const NOT_A_SANDBOX: &str = "disposable execution is mutation containment, not a code sandbox: \
                                 it redirects which project tree an untrusted run may change, and \
                                 restricts nothing about what that code may execute, read or reach";

/// Risks a disposable copy explicitly does **not** contain. Named individually
/// so no caller can fold them into a vague reassurance.
pub const UNCONTAINED_BY_A_COPY: [&str; 5] = [
    "an absolute-path write anywhere the invoking user account can write",
    "a network connection made by project code",
    "a read of any file the invoking user account can read, including material excluded from the copy",
    "an exec of any program the invoking user account can run",
    "resource exhaustion, which only the containment boundary bounds",
];

/// Why an untrusted project is never loaded in its original tree.
pub const WHY_UNTRUSTED_BY_DEFAULT: &str = "a `@tool` script executes inside the editor and a `@tool` autoload runs as soon as the \
     project loads, while `--import` implies `--editor`; loading a project is therefore \
     arbitrary code execution, and the editor applies no protection against a hostile one";

/// One line naming both halves of the posture: the copy, and the boundary that
/// is doing the actual confinement.
#[must_use]
pub fn statement(boundary: &str) -> String {
    format!(
        "{NOT_A_SANDBOX}; confinement of the process itself is the containment boundary's job ({boundary})"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_disclaimer_refuses_the_sandbox_claim_in_words() {
        assert!(NOT_A_SANDBOX.contains("not a code sandbox"));
        assert!(NOT_A_SANDBOX.contains("mutation containment"));
    }

    #[test]
    fn the_uncontained_risks_are_enumerated_individually() {
        assert_eq!(UNCONTAINED_BY_A_COPY.len(), 5);
        assert!(
            UNCONTAINED_BY_A_COPY
                .iter()
                .all(|risk| !risk.trim().is_empty())
        );
        assert!(
            UNCONTAINED_BY_A_COPY
                .iter()
                .any(|risk| risk.contains("absolute"))
        );
        assert!(
            UNCONTAINED_BY_A_COPY
                .iter()
                .any(|risk| risk.contains("network"))
        );
    }

    #[test]
    fn the_statement_names_the_boundary_doing_the_real_work() {
        let statement = statement("windows_job_object/strong");
        assert!(statement.contains("not a code sandbox"));
        assert!(statement.contains("windows_job_object/strong"));
    }
}
