//! `qa-fixture-34` — NON-SHIPPING, test-only fixture-control binary.
//!
//! Gated behind the `test-hooks` feature via `required-features` so it is never
//! built into a release archive; the release-graph audit excludes
//! `test-hooks`-only binaries by construction.
//!
//! This binary is the **sole** owner and only permitted writer/resetter of
//! `tests/fixtures/quarantined` and `tests/fixtures/repair/*.json`. Exactly two
//! subcommands exist, `setup --scenario task34` and `cleanup --scenario task34`,
//! and both are invoked only through the locked Cargo command forms
//! `qa/tasks/34.json` records — never a bare binary path assumed to be on `PATH`.
//! The lifecycle itself lives in [`godot_mcp_cli::fixture34`].
//!
//! # One re-execution, no third subcommand
//!
//! `setup` has to leave a fake editor bridge listening after it returns, so the
//! bridge cannot be a thread. It is this same binary re-executed with
//! [`BRIDGE_CONFIG_VARIABLE`] pointing at a configuration file inside the fixture
//! tree, which keeps the command-line surface at exactly the two subcommands the
//! contract allows.

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use godot_mcp_cli::fixture34::bridge::BRIDGE_CONFIG_VARIABLE;
use godot_mcp_cli::fixture34::{self, SCENARIO};

#[derive(Debug, Parser)]
#[command(
    name = "qa-fixture-34",
    about = "Test-only fixture control for QA task 34"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Deterministically (re)build the private fixture state for a scenario.
    Setup {
        /// Scenario name. Only `task34` is defined.
        #[arg(long)]
        scenario: String,
    },
    /// Unconditionally tear down and reset the scenario's fixture state.
    Cleanup {
        /// Scenario name. Only `task34` is defined.
        #[arg(long)]
        scenario: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let (scenario, is_setup) = match &cli.command {
        Command::Setup { scenario } => (scenario, true),
        Command::Cleanup { scenario } => (scenario, false),
    };
    if scenario != SCENARIO {
        eprintln!("qa-fixture-34: unknown scenario '{scenario}'");
        return ExitCode::FAILURE;
    }

    if let Some(config) = std::env::var_os(BRIDGE_CONFIG_VARIABLE) {
        // Re-executed as the fake bridge. `serve` only ever returns an error.
        let Err(error) = fixture34::bridge::serve(std::path::Path::new(&config));
        eprintln!("qa-fixture-34 bridge: {error}");
        return ExitCode::FAILURE;
    }

    let start = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("qa-fixture-34: the current directory is unreadable: {error}");
            return ExitCode::FAILURE;
        }
    };

    let outcome = if is_setup {
        fixture34::setup(&start).map(|paths| {
            eprintln!(
                "qa-fixture-34: setup complete at {}",
                paths.project_root.display()
            );
        })
    } else {
        fixture34::cleanup(&start).map(|()| {
            eprintln!("qa-fixture-34: cleanup complete");
        })
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("qa-fixture-34: {error}");
            ExitCode::FAILURE
        }
    }
}
