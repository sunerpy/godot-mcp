//! `qa-audit` — NON-SHIPPING, test-only Red-fixture driver for Todo 1.
//!
//! Gated behind `test-hooks` via `required-features`, so it never enters a
//! release build and the release-graph audit excludes it by construction.
//!
//! It materializes one deliberately-corrupted workspace fixture in a temp dir
//! and runs the matching audit. When the audit correctly *rejects* the corrupt
//! fixture, `qa-audit` prints the failing check name and exits **nonzero** —
//! which is exactly the "failure command exits nonzero" contract that
//! `qa/tasks/1.json`'s failure array records. If the audit fails to detect the
//! planted violation, `qa-audit` exits 0 (the Red proof did not reproduce), so
//! a broken audit is caught too.

use std::process::ExitCode;

use clap::Parser;

use godot_mcp_cli::audit::{
    ReleaseAuditInputs, audit_architecture_boundaries, audit_full_release_graph,
    audit_release_graph,
};
use godot_mcp_cli::corrupt::{self, CorruptFixture};
use godot_mcp_cli::descriptors;
use godot_mcp_cli::tooling_lock::ToolingLock;

#[derive(Debug, Parser)]
#[command(
    name = "qa-audit",
    about = "Test-only Red-fixture audit driver for Todo 1"
)]
struct Cli {
    /// The corrupt fixture to build and audit. One of:
    /// domain-imports-server, shipping-godot, shipping-gdext-bridge,
    /// shipping-gdextension, shipping-native-bridge-lib,
    /// ordinary-dependency-library, unpopulated-tooling-lock,
    /// missing-descriptor, task34-angle-bracket, task34-ambient-invocation.
    #[arg(long)]
    fixture: String,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let Some(fixture) = CorruptFixture::from_name(&cli.fixture) else {
        eprintln!("qa-audit: unknown fixture '{}'", cli.fixture);
        return ExitCode::from(2);
    };

    let tmp = match tempdir() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("qa-audit: cannot create temp dir: {e}");
            return ExitCode::from(2);
        }
    };
    let root = match corrupt::build(fixture, &tmp) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("qa-audit: cannot build fixture: {e}");
            return ExitCode::from(2);
        }
    };

    let rejected = detect_violation(fixture, &root);
    let check = fixture.expected_failing_check();

    // Best-effort cleanup.
    let _ = std::fs::remove_dir_all(&tmp);

    // Exit code semantics are uniform: the audit rejecting a fixture is exit 1,
    // accepting it is exit 0. The descriptor decides which is "pass":
    // rejection-expected fixtures are recorded as failure commands with
    // expected_exit 1; the ordinary-dependency-library false-positive guard is
    // recorded as a happy command with expected_exit 0 (it must be accepted).
    if fixture.expects_rejection() {
        if rejected {
            eprintln!(
                "qa-audit: fixture '{}' correctly rejected by check `{check}`",
                cli.fixture
            );
            ExitCode::from(1)
        } else {
            eprintln!(
                "qa-audit: BROKEN AUDIT — fixture '{}' was NOT rejected by check `{check}`",
                cli.fixture
            );
            ExitCode::SUCCESS
        }
    } else if rejected {
        eprintln!(
            "qa-audit: FALSE POSITIVE — fixture '{}' was wrongly rejected by check `{check}`",
            cli.fixture
        );
        ExitCode::from(1)
    } else {
        eprintln!(
            "qa-audit: fixture '{}' correctly accepted by check `{check}` (no false positive)",
            cli.fixture
        );
        ExitCode::SUCCESS
    }
}

/// Returns true when the audit correctly rejects the planted violation.
fn detect_violation(fixture: CorruptFixture, root: &std::path::Path) -> bool {
    match fixture {
        CorruptFixture::DomainImportsServer => audit_architecture_boundaries(root)
            .map(|e| !e.is_empty())
            .unwrap_or(false),
        CorruptFixture::ShippingGodot
        | CorruptFixture::ShippingGdextBridge
        | CorruptFixture::ShippingGdextension
        | CorruptFixture::ShippingNativeBridgeLib
        | CorruptFixture::OrdinaryDependencyLibrary => audit_release_graph(root)
            .map(|f| !f.is_empty())
            .unwrap_or(false),
        CorruptFixture::UnpopulatedToolingLock => {
            match ToolingLock::load(&root.join("tooling.lock.json")) {
                Ok(lock) => lock.validate_populated().is_err(),
                Err(_) => true,
            }
        }
        CorruptFixture::MissingDescriptor
        | CorruptFixture::Task34AngleBracket
        | CorruptFixture::Task34AmbientInvocation => descriptors::validate_all(root).is_err(),
        CorruptFixture::SuppliedGodotDependency => audit_full_release_graph(&ReleaseAuditInputs {
            workspace_root: root,
            sbom: None,
            archives: &[],
        })
        .map_or(true, |found| !found.is_empty()),
    }
}

fn tempdir() -> std::io::Result<std::path::PathBuf> {
    let base = std::env::temp_dir();
    let unique = format!(
        "qa-audit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let dir = base.join(unique);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
