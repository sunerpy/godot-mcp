//! Integration-test-only execution support for the committed GDScript codec.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicU32, Ordering};

use godot_mcp_bridge::protocol::harness::{GDSCRIPT_HARNESS_RES_PATH, GDSCRIPT_HARNESS_SOURCE};
use godot_mcp_engine::discovery::{DiscoveryError, EnvironmentSnapshot, HostPlatform, discover};
use thiserror::Error;

pub(super) const GODOT_BINARY_ENV: &str = "GODOT_MCP_GODOT_BINARY";
const CANONICAL_VECTOR_OUTPUT_PREFIX: &str = "bridge_codec_vector:";

static TEMP_PROJECT_COUNTER: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(super) struct GdscriptCanonicalVector {
    pub(super) name: String,
    pub(super) canonical_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GdscriptHarnessOutcome {
    Executed {
        canonical_vectors: Vec<GdscriptCanonicalVector>,
    },
    SkippedNoEngine,
}

#[derive(Debug, Error)]
pub(super) enum GdscriptHarnessError {
    #[error(transparent)]
    Discovery(#[from] DiscoveryError),
    #[error("{action} `{path}`: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "real GDScript codec harness failed via `{godot}` ({reason}, status {status})\nstdout:\n{stdout}\nstderr:\n{stderr}"
    )]
    UnexpectedOutput {
        godot: PathBuf,
        reason: &'static str,
        status: ExitStatus,
        stdout: String,
        stderr: String,
    },
}

struct TempProject {
    root: PathBuf,
}

impl TempProject {
    fn stage() -> Result<Self, GdscriptHarnessError> {
        let unique = TEMP_PROJECT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "godot-mcp-bridge-codec-{}-{unique}",
            std::process::id()
        ));
        if let Err(source) = std::fs::remove_dir_all(&root)
            && source.kind() != io::ErrorKind::NotFound
        {
            return Err(io_error("remove stale harness project", &root, source));
        }

        let bridge_dir = root.join("godot_mcp/bridge");
        std::fs::create_dir_all(&bridge_dir)
            .map_err(|source| io_error("create harness bridge directory", &bridge_dir, source))?;
        let project_marker = root.join("project.godot");
        std::fs::write(&project_marker, b"[application]\nconfig/name=\"harness\"\n")
            .map_err(|source| io_error("write harness project marker", &project_marker, source))?;
        let harness_path = bridge_dir.join("bridge_codec.gd");
        std::fs::write(&harness_path, GDSCRIPT_HARNESS_SOURCE)
            .map_err(|source| io_error("stage embedded GDScript harness", &harness_path, source))?;

        Ok(Self { root })
    }

    fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub(super) fn execute_gdscript_harness_if_available()
-> Result<GdscriptHarnessOutcome, GdscriptHarnessError> {
    let Some(godot) = locate_godot()? else {
        return Ok(GdscriptHarnessOutcome::SkippedNoEngine);
    };
    let project = TempProject::stage()?;
    let output = Command::new(&godot)
        .current_dir(project.root())
        .args(["--headless", "--script", GDSCRIPT_HARNESS_RES_PATH])
        .output()
        .map_err(|source| io_error("execute Godot binary", &godot, source))?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let reason = if !output.status.success() {
        Some("nonzero exit status")
    } else if stdout.contains("failure") || stderr.contains("failure") {
        Some("failure line emitted")
    } else if !stdout.contains("10/10 vectors byte-identical") {
        Some("missing canonical-vector proof")
    } else if !stdout.contains("5/5 fractional integer cases refused") {
        Some("missing fractional-refusal proof")
    } else if !stdout.contains("15/15 u64 path/boundary cases byte-identical") {
        Some("missing full-width-u64 proof")
    } else {
        None
    };
    if let Some(reason) = reason {
        return Err(GdscriptHarnessError::UnexpectedOutput {
            godot,
            reason,
            status: output.status,
            stdout,
            stderr,
        });
    }

    let canonical_vectors = parse_canonical_vectors(&stdout).map_err(|reason| {
        GdscriptHarnessError::UnexpectedOutput {
            godot,
            reason,
            status: output.status,
            stdout,
            stderr,
        }
    })?;

    Ok(GdscriptHarnessOutcome::Executed { canonical_vectors })
}

fn parse_canonical_vectors(stdout: &str) -> Result<Vec<GdscriptCanonicalVector>, &'static str> {
    let mut canonical_vectors = Vec::new();
    for line in stdout.lines() {
        let Some(payload) = line.strip_prefix(CANONICAL_VECTOR_OUTPUT_PREFIX) else {
            continue;
        };
        let vector = serde_json::from_str::<GdscriptCanonicalVector>(payload)
            .map_err(|_| "malformed canonical-vector payload")?;
        if vector.name.is_empty() || vector.canonical_json.is_empty() {
            return Err("empty canonical-vector field");
        }
        canonical_vectors.push(vector);
    }
    if canonical_vectors.is_empty() {
        return Err("missing canonical-vector payloads");
    }
    Ok(canonical_vectors)
}

fn locate_godot() -> Result<Option<PathBuf>, DiscoveryError> {
    let explicit = std::env::var_os(GODOT_BINARY_ENV).map(PathBuf::from);
    let environment = EnvironmentSnapshot::from_process_env();
    match discover(explicit.as_deref(), HostPlatform::current(), &environment) {
        Ok(resolved) => Ok(Some(resolved.path)),
        Err(DiscoveryError::NotFound { .. }) if explicit.is_none() => Ok(None),
        Err(error) => Err(error),
    }
}

fn io_error(action: &'static str, path: &Path, source: io::Error) -> GdscriptHarnessError {
    GdscriptHarnessError::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}
