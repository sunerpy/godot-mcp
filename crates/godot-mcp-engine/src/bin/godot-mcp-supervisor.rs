//! The bundled minimal supervisor / exec barrier.
//!
//! This binary is deliberately tiny: it contains no Godot knowledge, no journal
//! access and no network code. All of its logic lives in
//! [`godot_mcp_engine::supervision::supervisor`], so the same argument parsing and
//! gate semantics the launch protocol relies on are unit-tested in the library
//! rather than only exercised through a spawned process.
//!
//! Roles:
//!
//! * `barrier` — join the containment boundary when required, block until the gate
//!   opens, then replace this process's image with the requested program.
//! * `supervise` — the trusted-Unix weak-path control protocol: announce, block,
//!   create the gated child on `create`, block again, open the gate only on an
//!   authenticated `release <nonce>`, then report the child's outcome.

use std::process::ExitCode;

use godot_mcp_engine::supervision::supervisor::{self, USAGE_EXIT};

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match supervisor::parse_role(&arguments) {
        Ok(role) => match supervisor::run(role) {
            Ok(status) => ExitCode::from(status),
            Err(error) => {
                eprintln!("{error}");
                ExitCode::from(USAGE_EXIT)
            }
        },
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(USAGE_EXIT)
        }
    }
}
