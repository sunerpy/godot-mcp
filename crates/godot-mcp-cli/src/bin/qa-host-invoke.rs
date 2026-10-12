//! `qa-host-invoke` — NON-SHIPPING, test-only deterministic packaged-binary
//! resolver for Task 34.
//!
//! Gated behind `test-hooks` via `required-features` so it never enters a
//! release archive; the release-graph audit excludes `test-hooks`-only binaries
//! by construction.
//!
//! Task 34's repair commands must never invoke an ambient-PATH `godot-mcp`. This
//! resolver enforces the deterministic packaged-binary contract: it loads
//! `target/qa-task-34/host-package/manifest.json` (the `--manifest` argument),
//! verifies the manifest schema, that its `executable_path` is an absolute
//! regular file, and that the live executable's SHA-256 matches the recorded
//! value, and only then invokes that exact packaged binary with the arguments
//! after `--`.
//!
//! Todo 1 ships this as the resolver scaffold only: it validates the manifest
//! and spawns the exact packaged binary, but implements none of the Todo 34
//! repair behavior itself. If the manifest or packaged binary is absent (as it
//! is at Todo 1), the resolver fails closed with a nonzero exit before any
//! spawn.

use std::process::ExitCode;

use clap::Parser;

use godot_mcp_cli::host_package::HostPackageManifest;

#[derive(Debug, Parser)]
#[command(
    name = "qa-host-invoke",
    about = "Test-only deterministic packaged-binary resolver for QA task 34",
    // Everything after `--` is forwarded verbatim to the packaged binary.
    trailing_var_arg = true
)]
struct Cli {
    /// Path to the Task-34 host-package manifest that pins the exact packaged
    /// `godot-mcp` executable (its absolute path and SHA-256).
    #[arg(long)]
    manifest: String,

    /// Arguments passed to the resolved packaged binary (after `--`).
    args: Vec<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Resolve + verify the packaged binary before any spawn (fail closed).
    let exe = match HostPackageManifest::load_and_verify(std::path::Path::new(&cli.manifest)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("qa-host-invoke: {e}");
            // EX_UNAVAILABLE: manifest/binary not present or not verified.
            return ExitCode::from(69);
        }
    };

    // Invoke the exact packaged binary with the forwarded arguments.
    match exe.status(&cli.args) {
        Ok(status) => {
            let code = status.code().unwrap_or(70);
            ExitCode::from(u8::try_from(code).unwrap_or(70))
        }
        Err(e) => {
            eprintln!(
                "qa-host-invoke: spawn {}: {e}",
                exe.display_path().display()
            );
            ExitCode::from(70)
        }
    }
}
