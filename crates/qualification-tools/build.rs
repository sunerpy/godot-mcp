//! Marks a maintainer workspace for the qualification suites.
//!
//! Those suites read the orchestration plan and evidence under `.omo/`, which
//! is gitignored and so absent from every clean checkout. Where it is absent
//! they are ignored (`maintainer_workspace` is not set), so `cargo test`
//! reports them as ignored instead of passing without running them; where it
//! is present they run with the rest of the workspace.
//!
//! No `rerun-if-changed` is printed: Cargo then reruns this script whenever a
//! file of this package changes, and a workspace gains or loses `.omo` only
//! when it is set up, after which `cargo clean -p qualification-tools` makes
//! the mark follow.

use std::path::Path;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(maintainer_workspace)");
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR");
    if Path::new(&manifest).join("../../.omo").is_dir() {
        println!("cargo::rustc-cfg=maintainer_workspace");
    }
}
