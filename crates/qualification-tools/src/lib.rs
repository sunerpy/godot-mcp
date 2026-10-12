//! NON-SHIPPING build/test-only support library for the `qualification-tools`
//! binaries.
//!
//! This crate is review/qualification tooling only. It is never linked into any
//! shipping crate, binary, archive, addon, or SBOM runtime component.
//!
//! Todo 1 scaffolded all eight binaries in `src/bin/` as compiling stubs. Each
//! has exactly one implementation owner:
//!
//! - **Todo 6** owns `batch-review-bundle` and `finalize-batch-review`. Both are
//!   now **implemented** (see [`gates`]), because the B3/B4 dual-review gates must
//!   be fully executable immediately after Todo 6 and before Todos 19/26 need
//!   them.
//! - **Todo 37** owns `produce-qualification`, `f1-plan-compliance`,
//!   `f2-code-quality`, `f3-real-qa`, `f4-scope-fidelity` and
//!   `finalize-approval`. All six are now **implemented** (see [`qualification`]),
//!   so no `qualification-tools` binary is a stub any more.
//!
//! This library therefore exposes the shared B3/B4 gate machinery ([`gates`]),
//! the shared Todo-37 qualification machinery ([`qualification`]), and the
//! historical scaffold diagnostic ([`stub_notice`]) with its nonzero exit code
//! ([`STUB_UNIMPLEMENTED_EXIT`]), retained only so a future scaffolded target
//! cannot be mistaken for a working implementation.

#![forbid(unsafe_code)]

pub mod gates;
pub mod qualification;

/// Exit code reported by every scaffolded `qualification-tools` binary.
///
/// `69` is `EX_UNAVAILABLE` from `sysexits.h`: the requested service exists as
/// a CLI target but is not available yet. It is deliberately nonzero so CI
/// gates and human callers fail loudly instead of treating a stub as a pass.
pub const STUB_UNIMPLEMENTED_EXIT: u8 = 69;

/// Write the standard "not implemented yet" diagnostic for a scaffolded binary.
///
/// The message goes to **stderr only** (never stdout), so a stub can never
/// pollute a machine-readable stdout contract that its owner Todo will later
/// define. The text always contains the substring `scaffolded`, which the
/// `binaries_smoke` integration test asserts on.
///
/// # Arguments
///
/// * `binary_name` — the binary's cargo target name, e.g. `"f2-code-quality"`.
/// * `owner_todo` — the single Todo that owns the real implementation, e.g.
///   `"Todo 37"`.
pub fn stub_notice(binary_name: &str, owner_todo: &str) {
    eprintln!(
        "{binary_name}: scaffolded but not implemented yet; \
         the real implementation is owned by {owner_todo}. \
         Exiting {STUB_UNIMPLEMENTED_EXIT} (EX_UNAVAILABLE)."
    );
}
