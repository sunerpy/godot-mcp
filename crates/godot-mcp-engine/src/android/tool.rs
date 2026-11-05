//! Running one SDK or JDK tool: an exact environment, a deadline, and output
//! kept to a bound.
//!
//! These are the operator's own tools, not project code, and `adb` starts a
//! server that outlives the client by design, so they run as plain child
//! processes rather than inside a launch boundary. Nothing of the server's
//! environment reaches them: they see exactly the variables the caller
//! names. Their output goes to private capture files, read back to the bound
//! once the tool has ended, so a server it leaves running can never hold the
//! read open; a tool still running at its deadline, or when the caller
//! cancels, is killed.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// One tool to run.
#[derive(Debug, Clone)]
pub struct ToolRun<'a> {
    /// The executable.
    pub program: &'a Path,
    /// Its arguments, never a shell command line.
    pub arguments: Vec<String>,
    /// Exactly the environment it sees.
    pub environment: Vec<(String, String)>,
    /// Its working directory.
    pub directory: &'a Path,
    /// How long it may run.
    pub deadline: Duration,
    /// The most bytes of each stream kept.
    pub output_cap: usize,
    /// A private directory for its capture files.
    pub scratch: &'a Path,
}

/// How a tool ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolExit {
    /// It exited with this status.
    Exited(i32),
    /// A signal ended it.
    Signaled(String),
    /// It was still running at its deadline and was killed.
    TimedOut,
    /// The caller cancelled it, and it was killed.
    Cancelled,
}

impl ToolExit {
    /// Whether it exited with status 0.
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        matches!(self, Self::Exited(0))
    }

    /// A short spelling: `exit 0`, `signal 9`, `timed out`, `cancelled`.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Exited(code) => format!("exit {code}"),
            Self::Signaled(signal) => format!("signal {signal}"),
            Self::TimedOut => "timed out".to_owned(),
            Self::Cancelled => "cancelled".to_owned(),
        }
    }
}

/// One captured stream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Captured {
    /// The kept prefix, decoded lossily.
    pub text: String,
    /// How many bytes the tool wrote.
    pub bytes: u64,
    /// Whether the kept prefix is shorter than what it wrote.
    pub truncated: bool,
}

/// What one run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    /// How it ended.
    pub exit: ToolExit,
    /// Its standard output.
    pub stdout: Captured,
    /// Its standard error.
    pub stderr: Captured,
    /// How long it ran.
    pub elapsed: Duration,
}

impl ToolOutput {
    /// Both streams, standard output first.
    #[must_use]
    pub fn combined(&self) -> String {
        match (self.stdout.text.is_empty(), self.stderr.text.is_empty()) {
            (_, true) => self.stdout.text.clone(),
            (true, false) => self.stderr.text.clone(),
            (false, false) => format!("{}\n{}", self.stdout.text, self.stderr.text),
        }
    }
}

/// Why a tool could not be run at all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolError {
    /// The process could not be created.
    #[error("`{program}` could not be started: {reason}")]
    Start {
        /// The program's file name.
        program: String,
        /// Why.
        reason: String,
    },
    /// Its capture files could not be created.
    #[error("the private capture files could not be created: {0}")]
    Capture(String),
}

/// Numbers each run's capture files, so no two runs share one.
static RUNS: AtomicU64 = AtomicU64::new(0);

fn capture(path: &Path, cap: usize) -> Captured {
    let bytes = std::fs::metadata(path).map_or(0, |metadata| metadata.len());
    let mut kept = Vec::new();
    if let Ok(file) = std::fs::File::open(path) {
        use std::io::Read as _;
        let limit = u64::try_from(cap).unwrap_or(u64::MAX);
        let _ = file.take(limit).read_to_end(&mut kept);
    }
    let truncated = bytes > kept.len() as u64;
    Captured {
        text: String::from_utf8_lossy(&kept).into_owned(),
        bytes,
        truncated,
    }
}

fn file_name(program: &Path) -> String {
    program.file_name().map_or_else(
        || "tool".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Run `run`, killing it at its deadline or as soon as `cancelled` answers
/// true.
///
/// # Errors
/// Returns [`ToolError`] when the tool or its capture files could not be
/// created; a tool that started always answers its [`ToolOutput`].
pub fn run(run: &ToolRun<'_>, cancelled: &dyn Fn() -> bool) -> Result<ToolOutput, ToolError> {
    let number = RUNS.fetch_add(1, Ordering::Relaxed);
    let stem = format!("tool-{}-{number}", std::process::id());
    let out_path: PathBuf = run.scratch.join(format!("{stem}.stdout"));
    let err_path: PathBuf = run.scratch.join(format!("{stem}.stderr"));
    let create = |path: &Path| {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| ToolError::Capture(error.to_string()))
    };
    let stdout = create(&out_path)?;
    let stderr = match create(&err_path) {
        Ok(file) => file,
        Err(error) => {
            let _ = std::fs::remove_file(&out_path);
            return Err(error);
        }
    };
    let started = Instant::now();
    let child = Command::new(run.program)
        .args(&run.arguments)
        .env_clear()
        .envs(run.environment.iter().cloned())
        .current_dir(run.directory)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            let _ = std::fs::remove_file(&out_path);
            let _ = std::fs::remove_file(&err_path);
            return Err(ToolError::Start {
                program: file_name(run.program),
                reason: error.to_string(),
            });
        }
    };
    let mut pause = Duration::from_millis(5);
    let exit = loop {
        match child.try_wait() {
            Ok(Some(status)) => break exit_of(status),
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                break ToolExit::Signaled(format!("unobservable ({error})"));
            }
        }
        let stop = if cancelled() {
            Some(ToolExit::Cancelled)
        } else if started.elapsed() >= run.deadline {
            Some(ToolExit::TimedOut)
        } else {
            None
        };
        if let Some(stop) = stop {
            let _ = child.kill();
            let _ = child.wait();
            break stop;
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(50));
    };
    let elapsed = started.elapsed();
    let output = ToolOutput {
        exit,
        stdout: capture(&out_path, run.output_cap),
        stderr: capture(&err_path, run.output_cap),
        elapsed,
    };
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(&err_path);
    Ok(output)
}

#[cfg(unix)]
fn exit_of(status: std::process::ExitStatus) -> ToolExit {
    use std::os::unix::process::ExitStatusExt as _;
    match (status.code(), status.signal()) {
        (Some(code), _) => ToolExit::Exited(code),
        (None, Some(signal)) => ToolExit::Signaled(signal.to_string()),
        (None, None) => ToolExit::Signaled("unknown".to_owned()),
    }
}

#[cfg(not(unix))]
fn exit_of(status: std::process::ExitStatus) -> ToolExit {
    status.code().map_or_else(
        || ToolExit::Signaled("unknown".to_owned()),
        ToolExit::Exited,
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn shell(scratch: &Path, script: &str, deadline: Duration, cap: usize) -> ToolOutput {
        run(
            &ToolRun {
                program: Path::new("/bin/sh"),
                arguments: vec!["-c".to_owned(), script.to_owned()],
                environment: vec![("ONLY".to_owned(), "this".to_owned())],
                directory: scratch,
                deadline,
                output_cap: cap,
                scratch,
            },
            &|| false,
        )
        .expect("the shell starts")
    }

    #[test]
    fn a_tool_sees_exactly_its_environment_and_its_streams_are_kept() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let output = shell(
            scratch.path(),
            "echo \"$ONLY ${HOME:-no-home}\"; echo oops >&2; exit 3",
            Duration::from_secs(30),
            1024,
        );
        assert_eq!(output.exit, ToolExit::Exited(3));
        assert_eq!(output.stdout.text, "this no-home\n");
        assert_eq!(output.stderr.text, "oops\n");
        assert!(!output.stdout.truncated);
        // The capture files are gone.
        assert_eq!(std::fs::read_dir(scratch.path()).expect("list").count(), 0);
    }

    #[test]
    fn output_past_the_cap_is_counted_and_marked_truncated() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let output = shell(
            scratch.path(),
            "i=0; while [ $i -lt 100 ]; do echo 0123456789; i=$((i+1)); done",
            Duration::from_secs(30),
            64,
        );
        assert!(output.exit.succeeded());
        assert_eq!(output.stdout.text.len(), 64);
        assert_eq!(output.stdout.bytes, 1100);
        assert!(output.stdout.truncated);
    }

    #[test]
    fn a_tool_past_its_deadline_is_killed_and_said_so() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let output = shell(
            scratch.path(),
            "echo started; exec sleep 30",
            Duration::from_millis(200),
            1024,
        );
        assert_eq!(output.exit, ToolExit::TimedOut);
        assert_eq!(output.stdout.text, "started\n");
        assert!(output.elapsed < Duration::from_secs(25));
    }

    #[test]
    fn a_cancelled_tool_is_killed() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let output = run(
            &ToolRun {
                program: Path::new("/bin/sh"),
                arguments: vec!["-c".to_owned(), "exec sleep 30".to_owned()],
                environment: Vec::new(),
                directory: scratch.path(),
                deadline: Duration::from_secs(60),
                output_cap: 16,
                scratch: scratch.path(),
            },
            &|| true,
        )
        .expect("starts");
        assert_eq!(output.exit, ToolExit::Cancelled);
    }

    #[test]
    fn a_missing_tool_is_an_error_not_an_exit() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let error = run(
            &ToolRun {
                program: &scratch.path().join("absent"),
                arguments: Vec::new(),
                environment: Vec::new(),
                directory: scratch.path(),
                deadline: Duration::from_secs(5),
                output_cap: 16,
                scratch: scratch.path(),
            },
            &|| false,
        )
        .expect_err("nothing to start");
        assert!(matches!(error, ToolError::Start { .. }));
        assert_eq!(std::fs::read_dir(scratch.path()).expect("list").count(), 0);
    }
}
