//! The one-shot gate, the exec barrier and the bounded wait.
//!
//! Every supervised process is created **barrier-gated**: the process the
//! boundary creates is the bundled `godot-mcp-supervisor barrier` program, whose
//! argument vector carries [`BARRIER_ARGV_MARKER`] and which replaces its own
//! image with the requested program only after its gate opens. Two properties
//! follow, and both are real OS observations rather than promises:
//!
//! * while the marker is present in a live process's `argv`, that process has
//!   provably not started user code;
//! * because the barrier `exec`s in place, the PID and process start time bound
//!   into the durable receipt are the *same* PID and start time user code later
//!   runs under, so a receipt can never describe a different process than the one
//!   that ran.
//!
//! The gate itself is a durable three-state artifact, never a mere "file
//! exists?" test: it is created holding [`GATE_CLOSED`] before the child exists
//! and is atomically replaced with [`GATE_OPEN`] at release time. A missing or
//! unreadable gate artifact is [`GateState::Unknown`] — explicitly *not*
//! "closed" — because reconciliation must be able to tell "release provably never
//! happened" apart from "the release history cannot be determined".

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use super::boundary::{BARRIER_ARGV_MARKER, BoundaryError, SpawnPlan, WaitOutcome};

/// Token a freshly created, still-closed gate holds.
pub const GATE_CLOSED: &str = "closed";

/// Token an opened gate holds. The barrier execs user code only on this exact
/// token.
pub const GATE_OPEN: &str = "release";

/// How often the barrier re-reads its gate while blocked.
pub const GATE_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// What can be proven about a gate right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateState {
    /// The gate artifact exists and is still closed: release provably never
    /// happened.
    Closed,
    /// The gate artifact exists and is open: release provably happened.
    Open,
    /// The gate artifact is missing or unreadable. Neither outcome is provable.
    Unknown(String),
}

/// Create the gate in its closed state. Must happen before the child exists.
///
/// # Errors
/// Returns [`BoundaryError::Create`] when the gate artifact cannot be written.
pub fn create(path: &Path) -> Result<(), BoundaryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| BoundaryError::Create(error.to_string()))?;
    }
    write_durably(path, GATE_CLOSED).map_err(|error| BoundaryError::Create(error.to_string()))
}

/// Open the gate. The replacement is atomic, so a concurrent reader never sees a
/// partially written token.
///
/// # Errors
/// Returns [`BoundaryError::Release`] when the gate cannot be opened.
pub fn open(path: &Path) -> Result<(), BoundaryError> {
    let staging = path.with_extension("release");
    write_durably(&staging, GATE_OPEN)
        .map_err(|error| BoundaryError::Release(error.to_string()))?;
    std::fs::rename(&staging, path).map_err(|error| BoundaryError::Release(error.to_string()))?;
    if let Some(parent) = path.parent()
        && let Ok(dir) = File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Observe the gate's current state.
#[must_use]
pub fn state(path: &Path) -> GateState {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim() == GATE_OPEN => GateState::Open,
        Ok(text) if text.trim() == GATE_CLOSED => GateState::Closed,
        Ok(text) => {
            GateState::Unknown(format!("gate holds an unrecognised token: {}", text.trim()))
        }
        Err(error) => GateState::Unknown(format!(
            "gate artifact {} is unreadable: {error}",
            path.display()
        )),
    }
}

fn write_durably(path: &Path, token: &str) -> std::io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(token.as_bytes())?;
    file.sync_all()
}

/// Open (creating/truncating) one capture file for a child's stdout or stderr.
///
/// The protocol's own stdout is never inherited by a child: every supervised
/// process writes to one of these files instead.
///
/// # Errors
/// Returns [`BoundaryError::Spawn`] when the capture file cannot be created.
pub fn capture_file(path: &Path) -> Result<File, BoundaryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| BoundaryError::Spawn(error.to_string()))?;
    }
    File::create(path).map_err(|error| BoundaryError::Spawn(error.to_string()))
}

/// The barrier-gated argument array for one spawn plan.
///
/// This is an argument *array* all the way down — never a shell command string —
/// and the requested program appears only after the `--` terminator, so it can
/// never be re-parsed as an option of the barrier.
#[must_use]
pub fn barrier_argv(plan: &SpawnPlan) -> Vec<String> {
    let mut argv = vec![
        plan.barrier_binary.display().to_string(),
        "barrier".to_owned(),
        BARRIER_ARGV_MARKER.to_owned(),
        "--gate".to_owned(),
        plan.gate_path.display().to_string(),
    ];
    if let Some(join) = &plan.join_cgroup {
        argv.push("--join-cgroup".to_owned());
        argv.push(join.display().to_string());
    }
    argv.push("--".to_owned());
    argv.push(plan.program.display().to_string());
    argv.extend(plan.arguments.iter().cloned());
    argv
}

/// The complete argument array a launch actually issued, for diagnostics.
#[must_use]
pub fn requested_argv(plan: &SpawnPlan) -> Vec<String> {
    let mut argv = vec![plan.program.display().to_string()];
    argv.extend(plan.arguments.iter().cloned());
    argv
}

/// Block until the gate is provably open.
///
/// # Errors
/// Returns the reason the gate could not be proven open within `deadline`.
pub fn wait_for_open(path: &Path, deadline: Duration) -> Result<(), String> {
    let expires = Instant::now() + deadline;
    loop {
        match state(path) {
            GateState::Open => return Ok(()),
            GateState::Closed => {}
            GateState::Unknown(reason) => return Err(reason),
        }
        if Instant::now() >= expires {
            return Err(format!(
                "the gate {} was not open within its budget",
                path.display()
            ));
        }
        std::thread::sleep(GATE_POLL_INTERVAL);
    }
}

/// Block until `pid` is observably sitting at its barrier, and return that
/// observation.
///
/// This is what lets `staged_fsynced` mean "confirmed created but not yet running
/// user code" rather than merely "spawn returned": the caller only proceeds once
/// the OS itself shows the barrier marker in the live process's argument vector.
/// `Ok(None)` means the process disappeared before it ever reached the barrier.
///
/// # Errors
/// Returns an error when the platform cannot observe process identity at all, or
/// when the process never reaches its barrier within `deadline`.
pub fn wait_for_barrier(
    pid: u32,
    deadline: Duration,
) -> std::io::Result<Option<super::procfs::ProcessIdentity>> {
    let expires = Instant::now() + deadline;
    loop {
        match super::procfs::process_identity(pid)? {
            Some(identity) if identity.is_gated_barrier() => return Ok(Some(identity)),
            Some(_) => {}
            None => return Ok(None),
        }
        if Instant::now() >= expires {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("process {pid} never reached its exec barrier"),
            ));
        }
        std::thread::sleep(GATE_POLL_INTERVAL);
    }
}

/// Wait for one directly waitable child, bounded by `deadline`.
///
/// On timeout `on_timeout` runs first (so the whole boundary is torn down, not
/// just this one process) and the outcome is reported as timed out.
///
/// # Errors
/// Returns [`BoundaryError::Wait`] when waiting itself fails.
pub fn wait_for_child(
    child: &mut Child,
    deadline: Duration,
    on_timeout: impl FnOnce(),
) -> Result<WaitOutcome, BoundaryError> {
    let expires = Instant::now() + deadline;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(outcome_of(&status, false)),
            Ok(None) => {}
            Err(error) => return Err(BoundaryError::Wait(error.to_string())),
        }
        if Instant::now() >= expires {
            on_timeout();
            let status = child
                .wait()
                .map_err(|error| BoundaryError::Wait(error.to_string()))?;
            return Ok(outcome_of(&status, true));
        }
        std::thread::sleep(GATE_POLL_INTERVAL);
    }
}

/// Translate an exit status into the structured outcome.
#[must_use]
pub fn outcome_of(status: &std::process::ExitStatus, timed_out: bool) -> WaitOutcome {
    WaitOutcome {
        exit_code: status.code(),
        signal: signal_name(status),
        timed_out,
    }
}

#[cfg(unix)]
fn signal_name(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal().map(describe_signal)
}

#[cfg(not(unix))]
fn signal_name(_status: &std::process::ExitStatus) -> Option<String> {
    None
}

/// Stable signal spelling for the structured diagnostics.
#[must_use]
pub fn describe_signal(number: i32) -> String {
    match number {
        1 => "SIGHUP".to_owned(),
        2 => "SIGINT".to_owned(),
        3 => "SIGQUIT".to_owned(),
        6 => "SIGABRT".to_owned(),
        9 => "SIGKILL".to_owned(),
        11 => "SIGSEGV".to_owned(),
        13 => "SIGPIPE".to_owned(),
        15 => "SIGTERM".to_owned(),
        other => format!("SIG{other}"),
    }
}

/// One captured output stream, capped at the request's byte budget.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CapturedStream {
    /// The retained prefix, never longer than the cap.
    pub bytes: Vec<u8>,
    /// How many bytes the child actually produced.
    pub total_bytes: u64,
    /// Whether the retained prefix is shorter than what the child produced.
    pub truncated: bool,
}

/// Read one capture file and apply the byte cap.
#[must_use]
pub fn capture(path: &Path, cap: usize) -> CapturedStream {
    let bytes = std::fs::read(path).unwrap_or_default();
    let total_bytes = bytes.len() as u64;
    let truncated = bytes.len() > cap;
    let mut retained = bytes;
    retained.truncate(cap);
    CapturedStream {
        bytes: retained,
        total_bytes,
        truncated,
    }
}

/// Where one launch's transient artifacts live, all under the request's work
/// directory so nothing escapes the project's own scratch space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPaths {
    /// The one-shot gate.
    pub gate: PathBuf,
    /// The child's captured stdout.
    pub stdout: PathBuf,
    /// The child's captured stderr.
    pub stderr: PathBuf,
}

impl LaunchPaths {
    /// Derive the artifact paths for one transaction.
    #[must_use]
    pub fn for_transaction(work_dir: &Path, transaction_id: &str) -> Self {
        let token = transaction_token(transaction_id);
        let base = work_dir.join(".godot-mcp").join(format!("launch-{token}"));
        Self {
            gate: base.join("gate"),
            stdout: base.join("stdout"),
            stderr: base.join("stderr"),
        }
    }

    /// Remove this launch's complete scratch directory. A missing directory is
    /// already clean and therefore succeeds.
    ///
    /// # Errors
    /// Returns the filesystem error when an existing directory cannot be removed.
    pub fn remove_scratch(&self) -> std::io::Result<()> {
        let Some(directory) = self.gate.parent() else {
            return Ok(());
        };
        match std::fs::remove_dir_all(directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

/// Remove a recorded launch's scratch only when the gate path still has the
/// exact transaction-derived shape produced by [`LaunchPaths::for_transaction`].
///
/// # Errors
/// Returns invalid input for a path outside that shape, or the filesystem error
/// when an existing directory cannot be removed.
pub fn remove_recorded_scratch(gate_path: &Path, transaction_id: &str) -> std::io::Result<()> {
    if gate_path.file_name().and_then(|name| name.to_str()) != Some("gate") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the recorded gate path does not name the gate artifact",
        ));
    }
    let expected = format!("launch-{}", transaction_token(transaction_id));
    let Some(directory) = gate_path.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the recorded gate path has no launch directory",
        ));
    };
    if directory.file_name().and_then(|name| name.to_str()) != Some(expected.as_str()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the recorded gate path does not match its transaction",
        ));
    }
    match std::fs::remove_dir_all(directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Sweep terminal or untracked launch scratch while retaining the exact
/// transaction identifiers the caller still considers evidence-bearing.
/// Symlinks and non-launch entries are never followed or removed.
///
/// # Errors
/// Returns the filesystem error when the scratch root cannot be enumerated or an
/// eligible directory cannot be removed.
pub fn sweep_scratch<'a>(
    work_dir: &Path,
    retained_transactions: impl IntoIterator<Item = &'a str>,
) -> std::io::Result<usize> {
    let retained = retained_transactions
        .into_iter()
        .map(|transaction_id| format!("launch-{}", transaction_token(transaction_id)))
        .collect::<BTreeSet<_>>();
    let root = work_dir.join(".godot-mcp");
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(token) = name.strip_prefix("launch-") else {
            continue;
        };
        if token.is_empty()
            || !token
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
            || retained.contains(name)
            || !entry.file_type()?.is_dir()
        {
            continue;
        }
        std::fs::remove_dir_all(entry.path())?;
        removed += 1;
    }
    Ok(removed)
}

fn transaction_token(transaction_id: &str) -> String {
    transaction_id
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_gate_is_unknown_and_never_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gate = dir.path().join("gate");
        assert!(matches!(state(&gate), GateState::Unknown(_)));
        create(&gate).expect("create the gate");
        assert_eq!(state(&gate), GateState::Closed);
        open(&gate).expect("open the gate");
        assert_eq!(state(&gate), GateState::Open);
        std::fs::remove_file(&gate).expect("remove the gate");
        assert!(
            matches!(state(&gate), GateState::Unknown(_)),
            "a gate whose history was destroyed is never reported as provably closed"
        );
    }

    #[test]
    fn the_barrier_argv_is_an_array_with_the_program_after_a_terminator() {
        let plan = SpawnPlan {
            program: PathBuf::from("/bin/godot"),
            arguments: vec!["--headless".to_owned(), "--quit".to_owned()],
            environment: Vec::new(),
            work_dir: PathBuf::from("/tmp"),
            stdout_path: PathBuf::from("/tmp/out"),
            stderr_path: PathBuf::from("/tmp/err"),
            gate_path: PathBuf::from("/tmp/gate"),
            barrier_binary: PathBuf::from("/bin/godot-mcp-supervisor"),
            join_cgroup: None,
        };
        let argv = barrier_argv(&plan);
        assert_eq!(argv[0], "/bin/godot-mcp-supervisor");
        assert_eq!(argv[1], "barrier");
        assert_eq!(argv[2], BARRIER_ARGV_MARKER);
        let terminator = argv
            .iter()
            .position(|item| item == "--")
            .expect("terminator");
        assert_eq!(argv[terminator + 1], "/bin/godot");
        assert_eq!(&argv[terminator + 2..], &["--headless", "--quit"]);
    }

    #[test]
    fn capture_applies_the_byte_cap_and_reports_the_real_total() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("stdout");
        std::fs::write(&path, vec![b'x'; 300]).expect("write");
        let captured = capture(&path, 64);
        assert_eq!(captured.bytes.len(), 64);
        assert_eq!(captured.total_bytes, 300);
        assert!(captured.truncated);
    }

    #[test]
    fn startup_sweep_removes_stale_scratch_and_retains_open_transactions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let retained_id = "00000000-0000-4000-8000-000000000501";
        let stale_id = "00000000-0000-4000-8000-000000000502";
        let retained = LaunchPaths::for_transaction(dir.path(), retained_id);
        let stale = LaunchPaths::for_transaction(dir.path(), stale_id);
        create(&retained.gate).expect("retained gate");
        create(&stale.gate).expect("stale gate");
        let unrelated = dir.path().join(".godot-mcp").join("other");
        std::fs::create_dir_all(&unrelated).expect("unrelated directory");

        let removed = sweep_scratch(dir.path(), [retained_id]).expect("sweep scratch");

        assert_eq!(removed, 1);
        assert!(retained.gate.exists(), "retained transaction must survive");
        assert!(!stale.gate.exists(), "terminal scratch must be swept");
        assert!(unrelated.exists(), "non-launch entries must not be removed");
    }
}
