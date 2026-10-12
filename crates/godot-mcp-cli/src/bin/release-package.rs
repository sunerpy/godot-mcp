//! `release-package` — the Todo 34 release packager and release-graph gate.
//!
//! It is gated behind `test-hooks` via `required-features` for exactly one
//! reason: a packaging tool must never be able to end up inside the artifact it
//! packages. The release-graph audit excludes `test-hooks`-only binaries by
//! construction, and `self_check` independently requires that an archive's `bin/`
//! hold nothing but the one declared executable.
//!
//! Seven subcommands, all offline and deterministic:
//!
//! * `host-package` builds the host platform's published archive, extracts it, and writes the
//!   Task-34 host-package manifest the QA lane resolves;
//! * `check-host` refuses before a release build when its SDK or compiler is unavailable;
//! * `dist` writes the publishable archive set, `SHA256SUMS.txt` and the
//!   provenance attestation;
//! * `reproducibility` compares two clean builds of the same target and records
//!   the declared result, failing the gate on any difference;
//! * `ci-matrix` prints the lock-derived CI matrices the workflows consume;
//! * `release-gate-specs` prints the required-release-gate catalogue;
//! * `release-gates` is the Todo-35 release-gates harness behind
//!   `make release-gates-harness`.
//!
//! The three packaging subcommands run the full first-release native-dependency
//! guard over the artifacts they produced and refuse to succeed if the guard
//! reports anything. The three release-gate subcommands touch no artifact: they
//! derive from the committed lock and from CI's deposited observations.
//!
//! ## Host honesty
//!
//! Only the published target for the host platform is built. A Linux host must
//! expose `x86_64-linux-musl-gcc` before it can claim the self-contained musl
//! target; macOS and Windows still require their native SDK/toolchain. Missing
//! capability is reported rather than fabricated into an archive or checksum.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use godot_mcp_cli::audit::{ArchiveUnderAudit, ReleaseAuditInputs, audit_full_release_graph};
use godot_mcp_cli::package::{
    self, PackageInputs, ProducedArchive, ReleaseTarget, sbom, write_host_package,
};
use godot_mcp_cli::release_gates::{harness, lock_matrix};

#[derive(Debug, Parser)]
#[command(
    name = "release-package",
    about = "Build reproducible godot-mcp release artifacts and gate the release graph"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Verify that this host can build its published release target.
    CheckHost,
    /// Build the host platform's archive and write the Task-34 host-package manifest.
    HostPackage {
        /// The already-built `godot-mcp` executable to package.
        #[arg(long)]
        executable: PathBuf,
        /// The already-built `godot-mcp-supervisor` executable to package.
        #[arg(long)]
        supervisor: PathBuf,
        /// Which cargo profile produced it, recorded in the payload manifest.
        #[arg(long, default_value = "debug")]
        profile: String,
        /// Directory to write the archive, extraction and manifest into.
        #[arg(long, default_value = "target/qa-task-34/host-package")]
        out: PathBuf,
    },
    /// Write the publishable archive set, checksums and provenance.
    Dist {
        /// The already-built `godot-mcp` executable to package.
        #[arg(long)]
        executable: PathBuf,
        /// The already-built `godot-mcp-supervisor` executable to package.
        #[arg(long)]
        supervisor: PathBuf,
        /// Which cargo profile produced it.
        #[arg(long, default_value = "release")]
        profile: String,
        /// Directory to publish into.
        #[arg(long, default_value = package::DIST_DIR)]
        out: PathBuf,
    },
    /// Print the lock-derived CI matrices as the JSON the workflows read through
    /// `fromJSON`. Every version label, URL and digest comes from
    /// `tests/fixtures/godot-releases.lock.json`.
    CiMatrix,
    /// Print the complete required-release-gate catalogue: every check name and
    /// the exact command its runner must execute.
    ReleaseGateSpecs,
    /// Join the required-check catalogue to a candidate's deposited CI
    /// observations and emit the release-gates document.
    ReleaseGates {
        /// Candidate worktree holding `ci/observations/*.json`.
        #[arg(long)]
        candidate_worktree: PathBuf,
        /// The candidate commit under test.
        #[arg(long)]
        source_commit: String,
        /// The candidate tree under test.
        #[arg(long)]
        source_tree: String,
        /// Directory holding the release archives and `SHA256SUMS.txt`.
        #[arg(long)]
        archives_dir: PathBuf,
        /// Where to write the gates document.
        #[arg(long)]
        output: PathBuf,
        /// Where to write its strict sidecar.
        #[arg(long)]
        output_sha: PathBuf,
        /// `0` for an in-progress observation, `1` for Todo 37's single final
        /// emission (rejected without its producer-owned orchestration context).
        #[arg(long)]
        qualifying: String,
    },
    /// Compare two clean builds of the same target and record the result.
    Reproducibility {
        /// The first clean build's executable.
        #[arg(long)]
        first: PathBuf,
        /// The second clean build's executable.
        #[arg(long)]
        second: PathBuf,
        /// Directory to write `reproducibility.json` into.
        #[arg(long, default_value = package::DIST_DIR)]
        out: PathBuf,
    },
}

fn host_target() -> Result<ReleaseTarget, String> {
    let target = ReleaseTarget::host().ok_or_else(|| {
        format!(
            "this host ({} {}) is not one of the published release targets",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    target.require_buildable_on_host()?;
    Ok(target)
}

fn build(
    workspace_root: &Path,
    executable: &Path,
    supervisor: &Path,
    profile: &str,
) -> Result<ProducedArchive, String> {
    let bytes = std::fs::read(executable)
        .map_err(|error| format!("read {}: {error}", executable.display()))?;
    let supervisor_bytes = std::fs::read(supervisor)
        .map_err(|error| format!("read {}: {error}", supervisor.display()))?;
    package::build_archive(&PackageInputs {
        workspace_root: workspace_root.to_path_buf(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        target: host_target()?,
        executable: bytes,
        supervisor: supervisor_bytes,
        profile: profile.to_string(),
    })
}

/// Run the full guard over the workspace, the SBOM that shipped and the produced
/// archives. Any finding is a hard failure: the artifacts stay on disk so they can
/// be inspected, but the gate does not pass.
fn gate(workspace_root: &Path, archives: &[ProducedArchive]) -> Result<(), String> {
    let locked = package::lock::CargoLock::load(&workspace_root.join("Cargo.lock"))?;
    let document = sbom::from_cargo_lock(
        &locked,
        package::RELEASE_ROOT_CRATE,
        package::RELEASE_BINARY,
        env!("CARGO_PKG_VERSION"),
    )?;
    let views: Vec<ArchiveUnderAudit<'_>> = archives
        .iter()
        .map(|produced| ArchiveUnderAudit {
            file_name: &produced.file_name,
            entries: &produced.entries,
        })
        .collect();
    let found = audit_full_release_graph(&ReleaseAuditInputs {
        workspace_root,
        sbom: Some(&document),
        archives: &views,
    })?;
    if found.is_empty() {
        return Ok(());
    }
    let detail = found
        .iter()
        .map(|artifact| format!("{} in {}", artifact.artifact, artifact.location))
        .collect::<Vec<_>>()
        .join("; ");
    Err(format!(
        "release-graph audit rejected {} artifact(s): {detail}",
        found.len()
    ))
}

fn run() -> Result<String, String> {
    let cli = Cli::parse();
    let workspace_root = godot_mcp_cli::audit::default_workspace_root();
    match cli.command {
        Command::CheckHost => {
            let target = host_target()?;
            Ok(format!("buildable target {}\n", target.triple()))
        }
        Command::HostPackage {
            executable,
            supervisor,
            profile,
            out,
        } => {
            let produced = build(&workspace_root, &executable, &supervisor, &profile)?;
            gate(&workspace_root, std::slice::from_ref(&produced))?;
            let host = write_host_package(&out, &produced, env!("CARGO_PKG_VERSION"))?;
            let report = package::self_check(&host.extracted_root.join(package::payload_root(
                env!("CARGO_PKG_VERSION"),
                produced.target,
            )))?;
            if !report.is_clean() {
                return Err(format!(
                    "the extracted package failed its own self-check: {}",
                    report.problems.join("; ")
                ));
            }
            Ok(format!(
                "host-package manifest {}\narchive {}\nexecutable {}\nsha256 {}\n",
                host.manifest_path.display(),
                host.archive_path.display(),
                host.executable_path.display(),
                host.executable_sha256
            ))
        }
        Command::Dist {
            executable,
            supervisor,
            profile,
            out,
        } => {
            let produced = build(&workspace_root, &executable, &supervisor, &profile)?;
            gate(&workspace_root, std::slice::from_ref(&produced))?;
            let written = package::write_dist(
                &workspace_root,
                &out,
                env!("CARGO_PKG_VERSION"),
                std::slice::from_ref(&produced),
            )?;
            let mut report = String::new();
            for path in written {
                report.push_str(&format!("wrote {}\n", path.display()));
            }
            for target in ReleaseTarget::ALL.iter().copied() {
                if target != produced.target {
                    report.push_str(&format!(
                        "not built here {} (no toolchain on this host)\n",
                        target.triple()
                    ));
                }
            }
            Ok(report)
        }
        Command::CiMatrix => {
            let lock = lock_matrix::GodotReleasesLock::load_from_workspace(&workspace_root)?;
            let document = lock.matrix_document()?;
            let mut text = serde_json::to_string_pretty(&document)
                .map_err(|error| format!("serialize CI matrix: {error}"))?;
            text.push('\n');
            Ok(text)
        }
        Command::ReleaseGateSpecs => {
            let lock = lock_matrix::GodotReleasesLock::load_from_workspace(&workspace_root)?;
            let specs = harness::required_checks(&lock, env!("CARGO_PKG_VERSION"))?;
            let rows: Vec<serde_json::Value> = specs
                .iter()
                .map(|spec| {
                    serde_json::json!({
                        "name": spec.name,
                        "required": spec.required,
                        "command": spec.command,
                        "kind": spec.kind.label(),
                        "archive_file_name": spec.archive_file_name,
                    })
                })
                .collect();
            let mut text = serde_json::to_string_pretty(&serde_json::json!({ "checks": rows }))
                .map_err(|error| format!("serialize release-gate specs: {error}"))?;
            text.push('\n');
            Ok(text)
        }
        Command::ReleaseGates {
            candidate_worktree,
            source_commit,
            source_tree,
            archives_dir,
            output,
            output_sha,
            qualifying,
        } => {
            let request = harness::HarnessRequest {
                candidate_worktree,
                source_commit,
                source_tree,
                archives_dir,
                output,
                output_sha,
                qualifying: harness::Qualifying::parse(&qualifying)?,
            };
            let outcome = harness::run(&request, &workspace_root, env!("CARGO_PKG_VERSION"))?;
            let label = if request.qualifying.is_qualifying() {
                "qualifying"
            } else {
                "NON-QUALIFYING observation"
            };
            Ok(format!(
                "release-gates {label}\nchecks {}\nwrote {}\nwrote {}\nsha256 {}\n",
                outcome.entries.len(),
                request.output.display(),
                request.output_sha.display(),
                outcome.digest
            ))
        }
        Command::Reproducibility { first, second, out } => {
            let report = reproducibility(&first, &second)?;
            std::fs::create_dir_all(&out)
                .map_err(|error| format!("create {}: {error}", out.display()))?;
            let path = out.join(package::REPRODUCIBILITY_FILE);
            let mut bytes = serde_json::to_vec_pretty(&report)
                .map_err(|error| format!("serialize reproducibility report: {error}"))?;
            bytes.push(b'\n');
            std::fs::write(&path, bytes)
                .map_err(|error| format!("write {}: {error}", path.display()))?;
            if report["byte_identical"] == serde_json::Value::Bool(true) {
                Ok(format!(
                    "reproducibility byte-identical\nwrote {}\n",
                    path.display()
                ))
            } else {
                Err(format!(
                    "two clean builds of {} are NOT byte-identical; the documented diff is in {}",
                    host_target()?.triple(),
                    path.display()
                ))
            }
        }
    }
}

/// Compare two clean builds byte for byte and describe the difference exactly.
///
/// A difference is never smoothed over: the report records both digests, both
/// sizes and the first differing offset, which is what makes the diff
/// deterministic enough to act on.
fn reproducibility(first: &Path, second: &Path) -> Result<serde_json::Value, String> {
    let left = std::fs::read(first).map_err(|e| format!("read {}: {e}", first.display()))?;
    let right = std::fs::read(second).map_err(|e| format!("read {}: {e}", second.display()))?;
    let left_digest = godot_mcp_cli::tooling_lock::sha256_hex_bytes(&left);
    let right_digest = godot_mcp_cli::tooling_lock::sha256_hex_bytes(&right);
    let identical = left == right;
    let first_difference = left
        .iter()
        .zip(right.iter())
        .position(|(a, b)| a != b)
        .or_else(|| (!identical).then(|| left.len().min(right.len())));
    Ok(serde_json::json!({
        "schema_version": package::PACKAGE_SCHEMA_VERSION,
        "target": host_target()?.triple(),
        "declared_result": if identical { "byte-identical" } else { "documented-deterministic-diff" },
        "byte_identical": identical,
        "first": {
            "path": first.to_string_lossy(),
            "sha256": left_digest,
            "size": left.len(),
        },
        "second": {
            "path": second.to_string_lossy(),
            "sha256": right_digest,
            "size": right.len(),
        },
        "first_differing_offset": first_difference,
    }))
}

fn main() -> ExitCode {
    match run() {
        Ok(report) => {
            print!("{report}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("release-package: {error}");
            ExitCode::from(1)
        }
    }
}
