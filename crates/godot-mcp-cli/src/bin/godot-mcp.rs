//! The `godot-mcp` command-line entry point.
//!
//! The surface below is exactly six top-level subcommands: `version`,
//! `check-config`, `serve`, `transaction`, `self-check` and `assets`. Todo 1
//! scaffolded the surface and transport-clean startup. Todo 2 added
//! `check-config`, which loads a typed configuration file, enforces every
//! security invariant, and maps the result to a process exit code (0 valid, 2
//! invalid). Todo 6 wires `serve --stdio --project <dir>` to the shared service
//! kernel: it acquires the project lease, opens the durable transaction journal,
//! reconciles every transaction left open by a previous process, then serves MCP
//! over stdin/stdout. Todo 34 adds `transaction repair`, the local-owner-only
//! operator-recovery command, plus `self-check` and `assets` over the release
//! package it produces.
//!
//! `serve` defaults to stdio. Supplying a validated `--config` may opt into the
//! authenticated loopback Streamable HTTP transport compiled into release
//! archives. Without that file no listener or credential is created. `--stdio`
//! takes an optional value: omitted or bare it affirms startup, and an explicit
//! `--stdio=false` parses and then reaches [`serve`], whose own refusal exits 2.
//! No `init`, `locate` or `configure` subcommand exists.
//!
//! Protocol stdout stays clean: any diagnostics go to stderr.
//!
//! ## `transaction repair` is local-only, by construction
//!
//! The match arm below is the **only** call site of
//! [`godot_mcp_cli::repair::run`] in the whole workspace, and therefore the only
//! way the Todo 6 operator-recovery authority's `RepairPermit` can ever be
//! requested. It is a `clap` subcommand reached from an operator's shell; it is
//! registered nowhere in the MCP tool/resource registry and has no Todo 26
//! capability-profile mapping, so no MCP/HTTP/network transport can reach it.
//! There is no `--force` flag and no other bypass: `--force` is not a recognized
//! argument and produces a `clap` parse error.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use godot_mcp_cli::package;
use godot_mcp_cli::repair::{RepairRequest, run as run_repair};
use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::config::{CliOverrides, ServerConfig, Transport, load_config};
use godot_mcp_domain::contracts::PrincipalId;
use godot_mcp_server::bootstrap::{BootstrapOptions, ServerSlot, serve_bootstrap_stdio};
#[cfg(feature = "http")]
use godot_mcp_server::http::bind_configured_transport;
use godot_mcp_server::observability::{TracingConfig, init_tracing};
use godot_mcp_server::router::RepairIntent;
use godot_mcp_server::stdio::{
    ProjectRuntime, StdioError, StdioOptions, install_stdio_diagnostics, serve_stdio,
};

#[derive(Debug, Parser)]
#[command(
    name = "godot-mcp",
    version,
    about = "Local-first, single-user Godot MCP server",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the server/contract version identifiers.
    Version,
    /// Validate a typed configuration file, enforcing every security invariant.
    /// Exits 0 when valid, 2 when the configuration is rejected.
    CheckConfig {
        /// Path to the TOML configuration file to validate.
        #[arg(long)]
        config: PathBuf,
    },
    /// Serve MCP over stdio (default) or configured authenticated loopback HTTP.
    ///
    /// Startup acquires the project lease, opens the durable transaction journal
    /// and reconciles every transaction a previous process left open before the
    /// first request is served. stdout carries protocol frames only.
    Serve {
        /// Affirm startup with the default stdio transport or the transport in
        /// `--config`. An explicit `--stdio=false` is refused with exit code 2.
        #[arg(
            long,
            num_args = 0..=1,
            default_value_t = true,
            default_missing_value = "true"
        )]
        stdio: bool,
        /// Optional typed configuration. When omitted, serve remains stdio-only
        /// and does not read a configuration file.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Candidate project directory. The real root is discovered upward from
        /// it by locating `project.godot`.
        #[arg(
            long,
            conflicts_with = "workspace",
            required_unless_present = "workspace"
        )]
        project: Option<PathBuf>,
        /// Workspace directory to start in bootstrap state instead of a project:
        /// only health, capabilities, workspace.projects.list, project.create and
        /// project.bind are listed until one project is bound for the process
        /// lifetime. stdio only.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Capability profile to expose: core (read-only analysis), editor
        /// (project authoring), runtime (running, testing and observing the
        /// project) or full (all three). A listed tool whose plane cannot be
        /// reached right now, such as a live editor or a running game, answers
        /// `unavailable` and names the plane.
        #[arg(long, default_value = "core")]
        profile: String,
    },
    /// Durable-transaction operator commands. Local owner only.
    Transaction {
        #[command(subcommand)]
        command: TransactionCommand,
    },
    /// Verify this extracted package against its own `MANIFEST.json`.
    ///
    /// Exits 0 when every payload file matches its recorded digest and size, the
    /// required notices/SBOM/example config are present, no payload file carries a
    /// credential marker and `bin/` holds exactly the declared executable. Exits 1
    /// when any of those fails, and 2 when this binary is not running out of an
    /// extracted package.
    SelfCheck {
        /// Package root to verify. Defaults to the package this binary was
        /// launched from.
        #[arg(long)]
        package: Option<PathBuf>,
    },
    /// Install, update, remove or report on the project assets this package
    /// carries. They install under `godot_mcp/`, never an `addons/` tree, because
    /// no editor plug-in ships.
    Assets {
        #[command(subcommand)]
        command: AssetsCommand,
    },
}

#[derive(Debug, Subcommand)]
enum AssetsCommand {
    /// First install into a project. It refuses, before writing anything, a
    /// payload path held by a file this installer does not own or by an owned file
    /// the user has since edited. A path already holding this installer's own
    /// unmodified files is not a conflict: those are overwritten in place, so a
    /// repeated install of the same payload succeeds.
    Install(AssetsArgs),
    /// Idempotent update. Re-running with no drift changes nothing; a
    /// user-modified owned file is backed up before being overwritten.
    Update(AssetsArgs),
    /// Remove every owned file whose digest still matches the install manifest.
    /// A user-modified owned file is deliberately retained.
    Remove {
        /// Project root to remove the assets from.
        #[arg(long)]
        project: PathBuf,
    },
    /// Report what an install or update would do, changing nothing.
    Status(AssetsArgs),
}

#[derive(Debug, Args)]
struct AssetsArgs {
    /// Project root to write into.
    #[arg(long)]
    project: PathBuf,
    /// Package root to take the payload from. Defaults to the package this
    /// binary was launched from.
    #[arg(long)]
    package: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum TransactionCommand {
    /// Repair one quarantined transaction from an independently re-derived proof.
    ///
    /// The command acquires the project lease, loads the journaled transaction,
    /// requires it to be quarantined, re-derives every proof field for its own
    /// recorded effect kind, and only then journals exactly one proof-backed
    /// transition. There is no force or bypass flag.
    Repair(RepairArgs),
}

#[derive(Debug, Args)]
struct RepairArgs {
    /// Project root whose lease is taken and whose journal is read.
    #[arg(long)]
    project: PathBuf,
    /// The journaled transaction to repair.
    #[arg(long)]
    transaction_id: String,
    /// Which proof-driven operation to apply.
    #[arg(long, value_parser = parse_operation)]
    operation: RepairIntent,
    /// The operator-supplied proof file, in the canonical repair-proof schema.
    #[arg(long)]
    proof_file: PathBuf,
    /// Re-derive and report the would-be transition without requesting a permit
    /// or writing anything.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

/// The four proof-driven operations, and nothing else. `--force` is deliberately
/// absent from this parser and from the whole argument surface.
fn parse_operation(value: &str) -> Result<RepairIntent, String> {
    RepairIntent::ALL
        .into_iter()
        .find(|intent| intent.as_str() == value)
        .ok_or_else(|| {
            format!(
                "unknown operation {value:?}; expected one of {}",
                RepairIntent::ALL
                    .iter()
                    .map(|intent| intent.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// Run one operator repair and map its outcome to a process exit code.
///
/// A proven repair prints its report to stdout and exits 0. Every refusal —
/// including an unproven state, a mismatched proof file, a wrong project object or
/// generation, a non-quarantined transaction and any I/O error while re-deriving —
/// prints to stderr and exits 1, leaving the transaction unchanged.
fn transaction_repair(arguments: &RepairArgs) -> ExitCode {
    let request = RepairRequest {
        project: arguments.project.clone(),
        transaction_id: arguments.transaction_id.clone(),
        operation: arguments.operation,
        proof_file: arguments.proof_file.clone(),
        dry_run: arguments.dry_run,
    };
    match run_repair(&request) {
        Ok(report) => {
            println!("{}", report.to_text());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("godot-mcp transaction repair: {error}");
            ExitCode::from(1)
        }
    }
}

fn resolve_package(explicit: Option<&PathBuf>) -> Result<PathBuf, String> {
    if let Some(package) = explicit {
        return if package.join("MANIFEST.json").is_file() {
            Ok(package.clone())
        } else {
            Err(format!(
                "{} is not an extracted godot-mcp package (no MANIFEST.json)",
                package.display()
            ))
        };
    }
    let executable =
        std::env::current_exe().map_err(|error| format!("locate this executable: {error}"))?;
    package::locate_package_root(&executable).ok_or_else(|| {
        format!(
            "{} is not running out of an extracted godot-mcp package; pass --package",
            executable.display()
        )
    })
}

fn self_check(explicit: Option<&PathBuf>) -> ExitCode {
    let root = match resolve_package(explicit) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("godot-mcp self-check: {error}");
            return ExitCode::from(2);
        }
    };
    match package::self_check(&root) {
        Ok(report) => {
            print!("{}", report.to_text());
            if report.is_clean() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(error) => {
            eprintln!("godot-mcp self-check: {error}");
            ExitCode::from(1)
        }
    }
}

/// Read the package version and target out of its own manifest, so the installer
/// payload is located by what the package says it is rather than by what this
/// binary was compiled as.
fn package_identity(root: &std::path::Path) -> Result<(String, package::ReleaseTarget), String> {
    let bytes = std::fs::read(root.join("MANIFEST.json"))
        .map_err(|error| format!("read package manifest: {error}"))?;
    let document: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse package manifest: {error}"))?;
    let version = document
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "package manifest has no version".to_string())?
        .to_string();
    let triple = document
        .get("target")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "package manifest has no target".to_string())?;
    let target = package::ReleaseTarget::from_triple(triple)
        .ok_or_else(|| format!("package manifest names an unknown target {triple}"))?;
    Ok((version, target))
}

fn assets_payload(
    explicit: Option<&PathBuf>,
) -> Result<package::installer::InstallPayload, String> {
    let root = resolve_package(explicit)?;
    let (version, target) = package_identity(&root)?;
    let extracted = root
        .parent()
        .ok_or_else(|| "package root has no parent".to_string())?;
    package::install_payload_from_package(extracted, &version, target)
}

fn assets(command: &AssetsCommand) -> ExitCode {
    let outcome = match command {
        AssetsCommand::Remove { project } => package::installer::remove(project),
        AssetsCommand::Status(arguments) => {
            return match assets_payload(arguments.package.as_ref()).and_then(|payload| {
                package::installer::plan(
                    &arguments.project,
                    &payload,
                    package::installer::WriteMode::Update,
                )
            }) {
                Ok(plan) => {
                    for file in &plan.files {
                        println!("{} {:?}", file.path, file.action);
                    }
                    println!("changes_anything {}", plan.changes_anything());
                    println!("conflicts {}", plan.conflicts().len());
                    if plan.conflicts().is_empty() {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::from(1)
                    }
                }
                Err(error) => {
                    eprintln!("godot-mcp assets status: {error}");
                    ExitCode::from(1)
                }
            };
        }
        AssetsCommand::Install(arguments) | AssetsCommand::Update(arguments) => {
            let mode = if matches!(command, AssetsCommand::Install(_)) {
                package::installer::WriteMode::Install
            } else {
                package::installer::WriteMode::Update
            };
            let root = match resolve_package(arguments.package.as_ref()) {
                Ok(root) => root,
                Err(error) => {
                    eprintln!("godot-mcp assets: {error}");
                    return ExitCode::from(2);
                }
            };
            let version = match package_identity(&root) {
                Ok((version, _)) => version,
                Err(error) => {
                    eprintln!("godot-mcp assets: {error}");
                    return ExitCode::from(1);
                }
            };
            match assets_payload(arguments.package.as_ref()) {
                Ok(payload) => {
                    package::installer::apply(&arguments.project, &payload, &version, mode)
                }
                Err(error) => Err(error),
            }
        }
    };
    match outcome {
        Ok(outcome) => {
            println!("created {}", outcome.created.len());
            println!("overwritten {}", outcome.overwritten.len());
            println!("backed_up {}", outcome.backed_up.len());
            println!("unchanged {}", outcome.unchanged.len());
            println!("removed {}", outcome.removed.len());
            println!("retained {}", outcome.retained.len());
            println!("changed {}", outcome.changed());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("godot-mcp assets: {error}");
            ExitCode::from(1)
        }
    }
}

fn load_serve_config(path: Option<&std::path::Path>) -> Result<ServerConfig, String> {
    let Some(path) = path else {
        return Ok(ServerConfig::default());
    };
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let environment = std::env::vars().collect::<BTreeMap<_, _>>();
    load_config(Some(&text), &environment, &CliOverrides::default()).map_err(|error| {
        format!(
            "configuration rejected [{}]: {error}",
            error.error_kind().code()
        )
    })
}

#[cfg(feature = "http")]
async fn serve_http(
    project_runtime: &ProjectRuntime,
    config: &ServerConfig,
    profile: CapabilityProfile,
) -> Result<(), String> {
    let transport = bind_configured_transport(project_runtime, config, profile).await?;
    eprintln!(
        "godot-mcp serve: HTTP listening at {}",
        transport.endpoint()
    );
    if let Some(path) = transport.secret_path() {
        eprintln!(
            "godot-mcp serve: launch secret published at {}",
            path.display()
        );
    }
    tokio::signal::ctrl_c()
        .await
        .map_err(|error| format!("shutdown signal unavailable: {error}"))?;
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
    Err("HTTP transport requires the http build feature".to_string())
}

/// What a serve request starts from.
enum ServeTarget {
    /// A project candidate: the server starts bound to it.
    Project(PathBuf),
    /// A workspace authority: the server starts in bootstrap state.
    Workspace(PathBuf),
}

fn serve(
    stdio: bool,
    config_path: Option<&std::path::Path>,
    target: &ServeTarget,
    profile: &str,
) -> ExitCode {
    if !stdio {
        eprintln!("godot-mcp serve: only --stdio is available in this build");
        return ExitCode::from(2);
    }
    let profile = match profile {
        "core" => CapabilityProfile::Core,
        "editor" => CapabilityProfile::Editor,
        "runtime" => CapabilityProfile::Runtime,
        "full" => CapabilityProfile::Full,
        other => {
            eprintln!(
                "godot-mcp serve: unknown profile {other:?}; expected core, editor, runtime or full"
            );
            return ExitCode::from(2);
        }
    };
    let mut config = match load_serve_config(config_path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("godot-mcp serve: {error}");
            return ExitCode::from(2);
        }
    };
    if let ServeTarget::Project(project) = target {
        config.project_root = Some(project.display().to_string());
    }
    config.profile = Some(profile);
    if matches!(target, ServeTarget::Workspace(_)) && config.transport != Transport::Stdio {
        eprintln!("godot-mcp serve: --workspace bootstrap is available over stdio only");
        return ExitCode::from(2);
    }
    if config.disposable {
        eprintln!(
            "godot-mcp serve: disposable=true is not a server-wide sandbox; this release only supports operation-scoped disposable execution and refuses to ignore the setting"
        );
        return ExitCode::from(2);
    }
    let diagnostics = match config.transport {
        Transport::Stdio => install_stdio_diagnostics(&config).map_err(|error| error.to_string()),
        Transport::Http => init_tracing(&TracingConfig::from_log_sink(&config.log_sink))
            .map_err(|error| error.to_string()),
    };
    if let Err(error) = diagnostics {
        eprintln!("godot-mcp serve: diagnostics failed: {error}");
        return ExitCode::from(70);
    }
    // A fixed local principal: stdio is a single-user local transport, so identity
    // comes from the OS session rather than from a credential on the wire.
    let principal = match PrincipalId::parse("principal_0123456789abcdef") {
        Ok(principal) => principal,
        Err(error) => {
            eprintln!("godot-mcp serve: invalid local principal: {error}");
            return ExitCode::from(70);
        }
    };
    let observed_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default();
    let godot_binary = config.godot_binary.as_deref().map(std::path::PathBuf::from);
    let project = match target {
        ServeTarget::Project(project) => project.clone(),
        ServeTarget::Workspace(workspace) => {
            return serve_bootstrap(BootstrapOptions {
                workspace: workspace.clone(),
                godot_binary,
                profile,
                principal,
                observed_at_unix_ms,
                limits: config.limits.clone(),
                process: config.process.clone(),
            });
        }
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
            eprintln!("godot-mcp serve: async runtime unavailable: {error}");
            return ExitCode::from(70);
        }
    };
    runtime.block_on(async move {
        let project_runtime = match ProjectRuntime::open(&options) {
            Ok(project_runtime) => project_runtime,
            Err(error) => {
                eprintln!("godot-mcp serve: startup failed: {error}");
                return match error {
                    StdioError::Audit(_) => ExitCode::from(70),
                    _ => ExitCode::from(69),
                };
            }
        };
        for (conclusion, count) in project_runtime.reconciliation_counts() {
            tracing::info!(
                event = "startup_reconciliation",
                conclusion = %conclusion,
                count,
                "godot-mcp serve: startup reconciliation {conclusion}={count}"
            );
        }
        if let Err(reason) = project_runtime.write_gate_status() {
            tracing::warn!(
                event = "write_gate_closed",
                reason = %reason,
                "godot-mcp serve: write gate closed: {reason}"
            );
        }
        let result = match config.transport {
            Transport::Stdio => serve_stdio(&project_runtime)
                .await
                .map_err(|error| error.to_string()),
            Transport::Http => serve_http(&project_runtime, &config, profile).await,
        };
        match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("godot-mcp serve: transport failed: {error}");
                ExitCode::from(70)
            }
        }
    })
}

/// Serve a workspace in bootstrap state over stdio.
fn serve_bootstrap(options: BootstrapOptions) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("godot-mcp serve: async runtime unavailable: {error}");
            return ExitCode::from(70);
        }
    };
    runtime.block_on(async move {
        let slot = match ServerSlot::open_bootstrap(options) {
            Ok(slot) => slot,
            Err(error) => {
                eprintln!("godot-mcp serve: startup failed: {error}");
                return ExitCode::from(69);
            }
        };
        match serve_bootstrap_stdio(slot).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("godot-mcp serve: transport failed: {error}");
                ExitCode::from(70)
            }
        }
    })
}

fn check_config(path: &std::path::Path) -> ExitCode {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("check-config: cannot read {}: {e}", path.display());
            return ExitCode::from(2);
        }
    };
    let environment = std::env::vars().collect::<BTreeMap<_, _>>();
    match load_config(Some(&text), &environment, &CliOverrides::default()) {
        Ok(_) => {}
        Err(e) => {
            eprintln!("check-config: rejected [{}]: {e}", e.error_kind().code());
            return ExitCode::from(2);
        }
    }
    eprintln!("check-config: OK");
    ExitCode::SUCCESS
}

fn run_with_default_diagnostics(command: &str, operation: impl FnOnce() -> ExitCode) -> ExitCode {
    match init_tracing(&TracingConfig::default()) {
        Ok(()) => operation(),
        Err(error) => {
            eprintln!("godot-mcp {command}: diagnostics failed: {error}");
            ExitCode::from(70)
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Version) | None => {
            println!("godot-mcp {}", env!("CARGO_PKG_VERSION"));
            println!("server-contract {}", godot_mcp_domain::CONTRACT_VERSION);
            ExitCode::SUCCESS
        }
        Some(Command::CheckConfig { config }) => {
            run_with_default_diagnostics("check-config", || check_config(&config))
        }
        Some(Command::Serve {
            stdio,
            config,
            project,
            workspace,
            profile,
        }) => {
            let target = match (project, workspace) {
                (Some(project), None) => ServeTarget::Project(project),
                (None, Some(workspace)) => ServeTarget::Workspace(workspace),
                _ => {
                    eprintln!("godot-mcp serve: pass exactly one of --project and --workspace");
                    return ExitCode::from(2);
                }
            };
            serve(stdio, config.as_deref(), &target, &profile)
        }
        Some(Command::Transaction {
            command: TransactionCommand::Repair(arguments),
        }) => run_with_default_diagnostics("transaction repair", || transaction_repair(&arguments)),
        Some(Command::SelfCheck { package }) => {
            run_with_default_diagnostics("self-check", || self_check(package.as_ref()))
        }
        Some(Command::Assets { command }) => {
            run_with_default_diagnostics("assets", || assets(&command))
        }
    }
}
