//! Callability and contract smoke tests for the eight non-shipping
//! `qualification-tools` binaries.
//!
//! Todo 1 required every binary to exist as a callable cargo target with a frozen
//! CLI surface, and asserted that six of them were scaffolded stubs. Todo 37
//! implements those six, so this test now asserts the *actual* contract of all
//! eight rather than keeping a stub expectation that has become false:
//!
//! * Todo 6's `batch-review-bundle` and `finalize-batch-review` fail with a usage
//!   error naming `--batch`.
//! * Todo 37's `produce-qualification`, `f1-plan-compliance`, `f2-code-quality`,
//!   `f3-real-qa`, `f4-scope-fidelity` and `finalize-approval` fail with a usage
//!   error naming their own first required flag.
//!
//! Every binary is still asserted to be callable and to fail closed with no
//! stdout, and — this is what replaces the old stub assertion without weakening
//! it — to **never** print the scaffold notice, because none of them is a stub any
//! more. Their real accept/reject behaviour is covered end to end by
//! `tests/batch_gates.rs` and `tests/qualification_gates.rs`.

use assert_cmd::Command;
use predicates::prelude::*;
use qualification_tools::gates::USAGE_EXIT;

/// Every binary, paired with a required flag its usage error must name.
const BINARIES: &[(&str, &str)] = &[
    ("batch-review-bundle", "--batch"),
    ("finalize-batch-review", "--batch"),
    ("produce-qualification", "--candidate-worktree"),
    ("f1-plan-compliance", "--identity"),
    ("f2-code-quality", "--identity"),
    ("f3-real-qa", "--identity"),
    ("f4-scope-fidelity", "--identity"),
    ("finalize-approval", "--input"),
];

#[test]
fn all_eight_binaries_are_callable() {
    let mut seen = 0_usize;
    for (bin, required_flag) in BINARIES {
        let mut cmd = Command::cargo_bin(bin)
            .unwrap_or_else(|e| panic!("binary {bin} must exist as a workspace target: {e}"));
        cmd.assert()
            .failure()
            .code(i32::from(USAGE_EXIT))
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("usage:"))
            .stderr(predicate::str::contains(*required_flag))
            .stderr(predicate::str::contains("scaffolded").not());
        seen += 1;
    }
    assert_eq!(
        seen, 8,
        "all eight qualification-tools binaries are covered"
    );
}
