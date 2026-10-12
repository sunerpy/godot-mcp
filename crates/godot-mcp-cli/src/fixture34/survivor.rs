//! The live survivor processes each `process_launch` fixture requires.
//!
//! Todo 14's crash-recovery state machine distinguishes six release dispositions,
//! and Todo 34's fixture has to present six of them *as real OS state*, not as
//! JSON claims: a process blocked in a syscall, a resumed one still running, one
//! killed by a signal, one that exited nonzero, one that exited cleanly while
//! leaving a live descendant, and one that is simply still there.
//!
//! Two rules hold throughout:
//!
//! * every identity written down is **observed**, never assumed — the PID comes
//!   from the spawn and the start time from
//!   [`procfs::process_identity`](godot_mcp_engine::supervision::procfs::process_identity),
//!   so a recorded identity is one the OS confirmed;
//! * a process whose recorded start time no longer matches is a *different*
//!   process after PID reuse and is never signalled.
//!
//! The blocked survivor blocks on opening a FIFO for reading, which no writer
//! ever opens. That is a genuine indefinite block inside the kernel that outlives
//! the process which created it, unlike a pipe held open by the parent, which the
//! OS closes the moment setup exits.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use godot_mcp_engine::supervision::procfs;

use super::FixtureError;
use super::state::SurvivorRecord;

/// How long cleanup waits for one signalled process to actually disappear.
pub const TERMINATION_DEADLINE: Duration = Duration::from_secs(10);

/// How often cleanup re-observes a process it has signalled.
const TERMINATION_POLL: Duration = Duration::from_millis(20);

/// How long setup waits for a freshly spawned process to become observable.
const SPAWN_OBSERVE_DEADLINE: Duration = Duration::from_secs(10);

/// One observed process identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    /// The PID the OS assigned.
    pub pid: u32,
    /// The platform-native start time the OS reported.
    pub start_time: Option<String>,
}

impl Observed {
    /// Turn this observation into a manifest record.
    #[must_use]
    pub fn record(&self, label: &str, transaction_id: &str, expect_live: bool) -> SurvivorRecord {
        SurvivorRecord {
            label: label.to_owned(),
            transaction_id: transaction_id.to_owned(),
            pid: self.pid,
            start_time: self.start_time.clone(),
            expect_live,
        }
    }
}

/// How one process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    /// The identity observed while the process was still alive.
    pub observed: Observed,
    /// The exit code, when the process exited normally.
    pub exit_code: Option<i64>,
    /// The terminating signal's canonical name, when the process was signalled.
    pub signal: Option<String>,
}

/// Refuse to run at all on a platform that cannot make the live observations the
/// fixture must record.
///
/// # Errors
/// Returns [`FixtureError::Unsupported`] when process observation is unavailable.
pub fn require_observability() -> Result<(), FixtureError> {
    if procfs::observations_available() {
        return Ok(());
    }
    Err(FixtureError::Unsupported {
        detail: "this platform cannot observe live process identity, so the Task-34 fixture \
                 cannot record an observed PID/start-time pair"
            .to_owned(),
    })
}

/// Observe one PID, waiting briefly for it to become visible.
///
/// # Errors
/// Returns [`FixtureError::Process`] when the process never becomes observable or
/// the platform reports no start time, since an identity without a start time can
/// never be safely matched again.
pub fn observe(pid: u32) -> Result<Observed, FixtureError> {
    let deadline = Instant::now() + SPAWN_OBSERVE_DEADLINE;
    loop {
        match procfs::process_identity(pid) {
            Ok(Some(identity)) if identity.start_time.is_some() => {
                return Ok(Observed {
                    pid,
                    start_time: identity.start_time,
                });
            }
            Ok(_) if Instant::now() < deadline => std::thread::sleep(TERMINATION_POLL),
            Ok(_) => {
                return Err(FixtureError::Process {
                    detail: format!("process {pid} never became observable with a start time"),
                });
            }
            Err(error) => {
                return Err(FixtureError::Process {
                    detail: format!("process {pid} could not be observed: {error}"),
                });
            }
        }
    }
}

/// Start a survivor blocked indefinitely on opening `fifo` for reading.
///
/// # Errors
/// Returns [`FixtureError::Process`] when the FIFO or the process cannot be
/// created, and [`FixtureError::Unsupported`] off Unix.
pub fn spawn_blocked(fifo: &Path) -> Result<Observed, FixtureError> {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(fifo);
        let status = Command::new("mkfifo")
            .arg(fifo)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| FixtureError::Process {
                detail: format!("mkfifo could not be run: {error}"),
            })?;
        if !status.success() {
            return Err(FixtureError::Process {
                detail: format!("mkfifo {} failed with {status}", fifo.display()),
            });
        }
        // The redirection is performed by the shell *before* it execs, so the
        // recorded PID is a process blocked in `open(2)` on a FIFO with no writer.
        let child = shell(r#"exec cat < "$1""#, &[fifo.display().to_string()])?;
        let observed = observe(child.id())?;
        drop(child);
        Ok(observed)
    }
    #[cfg(not(unix))]
    {
        let _ = fifo;
        Err(FixtureError::Unsupported {
            detail: "a FIFO-blocked survivor is only available on Unix".to_owned(),
        })
    }
}

/// Start a survivor that stays alive until it is signalled.
///
/// # Errors
/// Returns [`FixtureError::Process`] when the process cannot be started or
/// observed.
pub fn spawn_live() -> Result<Observed, FixtureError> {
    let child = shell(r#"exec sleep 86400"#, &[])?;
    let observed = observe(child.id())?;
    drop(child);
    Ok(observed)
}

/// Start a process, observe it, then `SIGKILL` it and reap its signalled status.
///
/// # Errors
/// Returns [`FixtureError::Process`] when any step fails, or when the process did
/// not in fact end on a signal.
pub fn spawn_then_signal() -> Result<Ended, FixtureError> {
    let mut child = shell(r#"exec sleep 86400"#, &[])?;
    let observed = observe(child.id())?;
    child.kill().map_err(|error| FixtureError::Process {
        detail: format!("the signalled fixture process could not be killed: {error}"),
    })?;
    let ended = reap(&mut child, observed)?;
    if ended.signal.is_none() {
        return Err(FixtureError::Process {
            detail: "the signalled fixture process did not end on a signal".to_owned(),
        });
    }
    Ok(ended)
}

/// Start a process, observe it, then release it so it exits with `code`.
///
/// # Errors
/// Returns [`FixtureError::Process`] when any step fails, or when the observed
/// exit code is not `code`.
pub fn spawn_then_exit(code: i32) -> Result<Ended, FixtureError> {
    let mut child = shell(r#"read _ack; exit "$1""#, &[code.to_string()])?;
    let observed = observe(child.id())?;
    release(&mut child)?;
    let ended = reap(&mut child, observed)?;
    if ended.exit_code != Some(i64::from(code)) {
        return Err(FixtureError::Process {
            detail: format!(
                "the fixture process exited with {:?}, not {code}",
                ended.exit_code
            ),
        });
    }
    Ok(ended)
}

/// Start a process that leaves one live descendant behind, then release it so it
/// exits cleanly while that descendant is still running.
///
/// Returns the parent's terminal observation and the descendant's live identity,
/// which together are what "exited clean, boundary not empty" means as OS state.
///
/// # Errors
/// Returns [`FixtureError::Process`] when any step fails, or when the parent did
/// not exit cleanly.
pub fn spawn_leaving_descendant() -> Result<(Ended, Observed), FixtureError> {
    // The descendant's own stdout is detached, so reading the parent's stdout can
    // never block on a descendant that outlives it.
    let mut child = shell(
        r#"sh -c 'exec sleep 86400' >/dev/null 2>&1 & echo "$!"; read _ack; exit 0"#,
        &[],
    )?;
    let observed = observe(child.id())?;
    let descendant = read_pid_line(&mut child)?;
    let descendant = observe(descendant)?;
    release(&mut child)?;
    let ended = reap(&mut child, observed)?;
    if ended.exit_code != Some(0) || ended.signal.is_some() {
        return Err(FixtureError::Process {
            detail: format!(
                "the boundary-leak fixture parent did not exit cleanly: {:?}/{:?}",
                ended.exit_code, ended.signal
            ),
        });
    }
    Ok((ended, descendant))
}

/// Whether the recorded identity still designates a live process.
///
/// A missing recorded start time, an absent process, or a start-time mismatch all
/// answer `false`: after PID reuse the same number is a different process.
///
/// # Errors
/// Returns [`FixtureError::Process`] when the platform cannot answer at all.
pub fn is_live(record: &SurvivorRecord) -> Result<bool, FixtureError> {
    let Some(recorded) = record.start_time.as_deref() else {
        return Ok(false);
    };
    match procfs::process_identity(record.pid) {
        Ok(Some(identity)) => Ok(identity.matches_recorded(record.pid, Some(recorded), None)),
        Ok(None) => Ok(false),
        Err(error) => Err(FixtureError::Process {
            detail: format!("process {} could not be observed: {error}", record.pid),
        }),
    }
}

/// Terminate the exact process the record identifies, and prove it is gone.
///
/// Returns `true` when a live process was terminated and `false` when the record
/// already designated no live process. Nothing is signalled unless the recorded
/// PID **and** start time both match, so an unrelated process that inherited the
/// PID is never touched.
///
/// # Errors
/// Returns [`FixtureError::NotTerminated`] when the process is still there after
/// [`TERMINATION_DEADLINE`]; the wait is bounded, so this never hangs.
pub fn terminate(record: &SurvivorRecord) -> Result<bool, FixtureError> {
    if !is_live(record)? {
        return Ok(false);
    }
    if let Err(error) = procfs::kill_pid(record.pid) {
        return Err(FixtureError::NotTerminated {
            detail: format!(
                "{} (pid {}) could not be signalled: {error}",
                record.label, record.pid
            ),
        });
    }
    let deadline = Instant::now() + TERMINATION_DEADLINE;
    loop {
        if !is_live(record)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Err(FixtureError::NotTerminated {
                detail: format!(
                    "{} (pid {}) is still live {}s after being signalled",
                    record.label,
                    record.pid,
                    TERMINATION_DEADLINE.as_secs()
                ),
            });
        }
        std::thread::sleep(TERMINATION_POLL);
    }
}

fn shell(script: &str, arguments: &[String]) -> Result<Child, FixtureError> {
    Command::new("sh")
        .arg("-c")
        .arg(script)
        .arg("sh")
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| FixtureError::Process {
            detail: format!("a fixture process could not be started: {error}"),
        })
}

fn release(child: &mut Child) -> Result<(), FixtureError> {
    let Some(stdin) = child.stdin.as_mut() else {
        return Err(FixtureError::Process {
            detail: "a fixture process has no stdin to release it through".to_owned(),
        });
    };
    stdin
        .write_all(b"go\n")
        .and_then(|()| stdin.flush())
        .map_err(|error| FixtureError::Process {
            detail: format!("a fixture process could not be released: {error}"),
        })?;
    child.stdin.take();
    Ok(())
}

fn read_pid_line(child: &mut Child) -> Result<u32, FixtureError> {
    let Some(stdout) = child.stdout.take() else {
        return Err(FixtureError::Process {
            detail: "a fixture process has no stdout to report its descendant on".to_owned(),
        });
    };
    let mut line = String::new();
    let mut reader = BufReader::new(stdout);
    reader
        .read_line(&mut line)
        .map_err(|error| FixtureError::Process {
            detail: format!("the descendant PID could not be read: {error}"),
        })?;
    line.trim()
        .parse::<u32>()
        .map_err(|error| FixtureError::Process {
            detail: format!("the descendant PID {line:?} is not a PID: {error}"),
        })
}

fn reap(child: &mut Child, observed: Observed) -> Result<Ended, FixtureError> {
    let status = child.wait().map_err(|error| FixtureError::Process {
        detail: format!("a fixture process could not be reaped: {error}"),
    })?;
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt as _;
        status.signal().map(signal_name)
    };
    #[cfg(not(unix))]
    let signal: Option<String> = None;
    Ok(Ended {
        observed,
        exit_code: status.code().map(i64::from),
        signal,
    })
}

#[cfg(unix)]
fn signal_name(number: i32) -> String {
    match number {
        1 => "SIGHUP".to_owned(),
        2 => "SIGINT".to_owned(),
        6 => "SIGABRT".to_owned(),
        9 => "SIGKILL".to_owned(),
        11 => "SIGSEGV".to_owned(),
        15 => "SIGTERM".to_owned(),
        other => format!("SIG{other}"),
    }
}
