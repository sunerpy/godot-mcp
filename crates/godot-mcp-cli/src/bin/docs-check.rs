//! `docs-check` — the non-shipping documentation checker entry point.
//!
//! Two modes:
//!
//! * default: run every documentation rule and exit 0 only on a clean tree;
//! * `--write`: regenerate the generated pages from their canonical sources.
//!
//! It is gated behind `test-hooks` for the same reason the release packager is: a
//! documentation tool has no place inside the artifact it describes, so the
//! release-graph audit excludes it by construction.

use std::path::PathBuf;
use std::process::ExitCode;

use godot_mcp_cli::audit::{default_workspace_root, find_workspace_root};
use godot_mcp_cli::docs;

fn workspace_root(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    match explicit {
        Some(root) => Ok(root),
        None => match std::env::current_dir() {
            Ok(cwd) => find_workspace_root(&cwd).or_else(|_| Ok(default_workspace_root())),
            Err(_) => Ok(default_workspace_root()),
        },
    }
}

fn main() -> ExitCode {
    let mut write = false;
    let mut root = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--write" => write = true,
            "--workspace-root" => match arguments.next() {
                Some(value) => root = Some(PathBuf::from(value)),
                None => {
                    eprintln!("docs-check: --workspace-root needs a path");
                    return ExitCode::from(2);
                }
            },
            other => {
                eprintln!("docs-check: unknown argument {other:?}");
                return ExitCode::from(2);
            }
        }
    }
    let root = match workspace_root(root) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("docs-check: {error}");
            return ExitCode::from(2);
        }
    };

    if write {
        return match docs::checker::write_generated(&root) {
            Ok(written) => {
                for page in written {
                    println!("regenerated {page}");
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("docs-check: {error}");
                ExitCode::from(1)
            }
        };
    }

    match docs::checker::check(&root) {
        Ok(findings) if findings.is_empty() => {
            println!("docs-check: clean");
            ExitCode::SUCCESS
        }
        Ok(findings) => {
            print!("{}", docs::render(&findings));
            eprintln!("docs-check: {} finding(s)", findings.len());
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("docs-check: {error}");
            ExitCode::from(2)
        }
    }
}
