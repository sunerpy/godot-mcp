//! `qa-journal-foundation` — NON-SHIPPING, `test-hooks`-only acceptance driver.
//!
//! It exists so Todo 6's acceptance suite can drive the **real** stdio serve path
//! out of process (`assert_cmd`), proving that a core MCP request completes and
//! that stdout carries protocol frames only while diagnostics go to stderr.
//!
//! It contains no product logic of its own: it calls exactly the same
//! `ProjectRuntime::open` + `serve_stdio` entry points the shipping `godot-mcp`
//! CLI calls, on the same multi-threaded runtime, so a long effect leaves the
//! transport serving — a cancellation arriving meanwhile is read and acted on. Because it is gated behind `required-features = ["test-hooks"]`, it
//! never enters a release archive.
//!
//! `--config <toml>` loads a server configuration the way `godot-mcp serve
//! --config` does; one that selects HTTP is bound through the same
//! [`bind_configured_transport`] the CLI calls, and the server then serves
//! until its stdin closes. `--observed-at-unix-ms` sets the startup clock a
//! test needs to move past an artifact's retention without waiting for it.

use std::path::PathBuf;
use std::process::ExitCode;

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::config::{CliOverrides, ServerConfig, Transport, load_config};
use godot_mcp_domain::contracts::PrincipalId;
use godot_mcp_server::bootstrap::{BootstrapOptions, ServerSlot, serve_bootstrap_stdio};
use godot_mcp_server::stdio::{
    ProjectRuntime, StdioOptions, install_stdio_diagnostics, serve_stdio,
};

fn usage() -> ExitCode {
    eprintln!(
        "usage: qa-journal-foundation (--project <dir> | --workspace <dir>) \
         [--godot-binary <path>] [--profile core|editor|runtime|full] [--config <toml>] \
         [--observed-at-unix-ms <ms>]"
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let raw_arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if raw_arguments
        .first()
        .is_some_and(|argument| matches!(argument.as_str(), "barrier" | "supervise"))
    {
        return match godot_mcp_engine::supervision::supervisor::parse_role(&raw_arguments) {
            Ok(role) => match godot_mcp_engine::supervision::supervisor::run(role) {
                Ok(status) => ExitCode::from(status),
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::from(godot_mcp_engine::supervision::supervisor::USAGE_EXIT)
                }
            },
            Err(error) => {
                eprintln!("{error}");
                ExitCode::from(godot_mcp_engine::supervision::supervisor::USAGE_EXIT)
            }
        };
    }
    if let Err(error) = install_stdio_diagnostics(&ServerConfig::default()) {
        eprintln!("qa-journal-foundation: {error}");
        return ExitCode::from(70);
    }

    let mut project: Option<PathBuf> = None;
    let mut workspace: Option<PathBuf> = None;
    let mut godot_binary: Option<PathBuf> = None;
    let mut profile = CapabilityProfile::Core;
    let mut config = ServerConfig::default();
    let mut observed_at_unix_ms = 1_700_000_000_000_u64;
    let mut arguments = raw_arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--project" => match arguments.next() {
                Some(value) => project = Some(PathBuf::from(value)),
                None => return usage(),
            },
            "--workspace" => match arguments.next() {
                Some(value) => workspace = Some(PathBuf::from(value)),
                None => return usage(),
            },
            "--godot-binary" => match arguments.next() {
                Some(value) => godot_binary = Some(PathBuf::from(value)),
                None => return usage(),
            },
            "--profile" => match arguments.next().as_deref() {
                Some("core") => profile = CapabilityProfile::Core,
                Some("editor") => profile = CapabilityProfile::Editor,
                Some("runtime") => profile = CapabilityProfile::Runtime,
                Some("full") => profile = CapabilityProfile::Full,
                _ => return usage(),
            },
            "--config" => {
                let Some(path) = arguments.next() else {
                    return usage();
                };
                let environment = std::env::vars().collect();
                let loaded = std::fs::read_to_string(&path)
                    .map_err(|error| error.to_string())
                    .and_then(|text| {
                        load_config(Some(&text), &environment, &CliOverrides::default())
                            .map_err(|error| error.to_string())
                    });
                match loaded {
                    Ok(loaded) => config = loaded,
                    Err(error) => {
                        eprintln!("qa-journal-foundation: configuration rejected: {error}");
                        return ExitCode::from(2);
                    }
                }
            }
            "--observed-at-unix-ms" => match arguments.next().map(|value| value.parse()) {
                Some(Ok(value)) => observed_at_unix_ms = value,
                _ => return usage(),
            },
            _ => return usage(),
        }
    }
    let principal = match PrincipalId::parse("principal_0123456789abcdef") {
        Ok(principal) => principal,
        Err(error) => {
            eprintln!("qa-journal-foundation: invalid fixed principal: {error}");
            return ExitCode::from(2);
        }
    };
    let project = match (project, workspace) {
        (Some(project), None) => project,
        (None, Some(workspace)) => {
            return serve_workspace(BootstrapOptions {
                workspace,
                godot_binary,
                profile,
                principal,
                observed_at_unix_ms,
                limits: config.limits.clone(),
                process: config.process.clone(),
            });
        }
        _ => return usage(),
    };
    let options = StdioOptions {
        project,
        godot_binary,
        profile,
        principal,
        observed_at_unix_ms,
        limits: config.limits.clone(),
        process: config.process.clone(),
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("qa-journal-foundation: runtime unavailable: {error}");
            return ExitCode::from(70);
        }
    };
    runtime.block_on(async move {
        let project_runtime = match ProjectRuntime::open(&options) {
            Ok(project_runtime) => project_runtime,
            Err(error) => {
                eprintln!("qa-journal-foundation: startup failed: {error}");
                return ExitCode::from(69);
            }
        };
        for (conclusion, count) in project_runtime.reconciliation_counts() {
            eprintln!("qa-journal-foundation: reconciled {conclusion}={count}");
        }
        match project_runtime.write_gate_status() {
            Ok(()) => eprintln!("qa-journal-foundation: write gate open"),
            Err(reason) => eprintln!("qa-journal-foundation: write gate closed: {reason}"),
        }
        let served = match config.transport {
            Transport::Stdio => serve_stdio(&project_runtime)
                .await
                .map_err(|error| error.to_string()),
            Transport::Http => serve_http(&project_runtime, &config, profile).await,
        };
        match served {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("qa-journal-foundation: transport failed: {error}");
                ExitCode::from(70)
            }
        }
    })
}

/// Bind HTTP the way the shipping CLI does, announce where, and serve until
/// stdin closes.
#[cfg(feature = "http")]
async fn serve_http(
    project_runtime: &ProjectRuntime,
    config: &ServerConfig,
    profile: CapabilityProfile,
) -> Result<(), String> {
    use tokio::io::AsyncReadExt as _;

    let transport =
        godot_mcp_server::http::bind_configured_transport(project_runtime, config, profile).await?;
    eprintln!(
        "qa-journal-foundation: HTTP listening at {}",
        transport.endpoint()
    );
    if let Some(path) = transport.secret_path() {
        eprintln!(
            "qa-journal-foundation: launch secret published at {}",
            path.display()
        );
    }
    let mut stdin = tokio::io::stdin();
    let mut buffer = [0_u8; 256];
    while stdin.read(&mut buffer).await.is_ok_and(|read| read > 0) {}
    transport
        .shutdown()
        .await
        .map_err(|error| error.to_string())
}

#[cfg(not(feature = "http"))]
async fn serve_http(
    _project_runtime: &ProjectRuntime,
    _config: &ServerConfig,
    _profile: CapabilityProfile,
) -> Result<(), String> {
    Err("HTTP transport requires the http build feature".to_owned())
}

/// The same bootstrap entry point the shipping CLI calls.
fn serve_workspace(options: BootstrapOptions) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("qa-journal-foundation: runtime unavailable: {error}");
            return ExitCode::from(70);
        }
    };
    runtime.block_on(async move {
        let slot = match ServerSlot::open_bootstrap(options) {
            Ok(slot) => slot,
            Err(error) => {
                eprintln!("qa-journal-foundation: startup failed: {error}");
                return ExitCode::from(69);
            }
        };
        match serve_bootstrap_stdio(slot).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("qa-journal-foundation: transport failed: {error}");
                ExitCode::from(70)
            }
        }
    })
}
