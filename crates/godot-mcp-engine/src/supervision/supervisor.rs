//! The bundled minimal supervisor/barrier program's logic.
//!
//! One small binary (`godot-mcp-supervisor`) provides both roles the launch
//! protocol needs. Keeping them tiny is what makes "no user code runs before its
//! gate" inspectable rather than merely asserted.
//!
//! ## `barrier` — the exec barrier (both platform paths)
//!
//! Created with [`BARRIER_ARGV_MARKER`] in its own argument vector, it
//!
//! 1. joins its containment boundary when the mechanism requires the member to
//!    join itself (cgroup v2's `cgroup.procs`), so it is inside the boundary
//!    before it could run anything;
//! 2. blocks until its gate holds [`GATE_OPEN`](super::gate::GATE_OPEN);
//! 3. replaces its own image with the requested program.
//!
//! Because step 3 is an `exec` in place, the PID and start time bound into the
//! durable receipt are the same ones user code later runs under, and while the
//! marker is still visible in `argv` the process provably has not started user
//! code.
//!
//! ## `supervise` — the trusted-Unix weak-path supervisor
//!
//! Spawned with the pre-bound control socket inherited as file descriptor 0 and
//! the control nonce in its environment, it
//!
//! 1. calls `setsid`, so it leads a brand-new process group (which is why the
//!    protocol never claims to have pre-created an empty process group);
//! 2. announces `announce pid=… pgid=… nonce=…` and **blocks** on the control
//!    channel — it cannot create anything before the parent has durably recorded
//!    that announcement as `staged_fsynced`;
//! 3. on `create`, creates the barrier-gated child (whose own gate is still
//!    closed), reports `created pid=…` and blocks again;
//! 4. on an authenticated `release <nonce>`, and only then, opens the gate;
//! 5. reports the child's `code=…` or `signal=…` once it ends.
//!
//! The child never inherits the control socket or the nonce, so it can never be
//! mistaken for the supervisor when reconciliation looks for the endpoint's
//! authenticated peer.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::boundary::BARRIER_ARGV_MARKER;
use super::gate::{self, GateState};

/// Exit status for a usage error.
pub const USAGE_EXIT: u8 = 64;

/// Exit status for a protocol violation on the control channel.
pub const PROTOCOL_EXIT: u8 = 65;

/// Exit status for an unauthenticated release attempt.
pub const UNAUTHENTICATED_EXIT: u8 = 66;

/// Exit status when the control channel or the gate fails.
pub const CHANNEL_EXIT: u8 = 70;

/// How long the barrier waits for its gate before giving up.
pub const BARRIER_TIMEOUT: Duration = Duration::from_secs(300);

/// The two roles the bundled binary can take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// Join the boundary, block on the gate, then exec the program.
    Barrier {
        /// The one-shot gate artifact.
        gate: PathBuf,
        /// A cgroup v2 directory whose `cgroup.procs` this process must join.
        join_cgroup: Option<PathBuf>,
        /// The program and its arguments, already an array.
        argv: Vec<String>,
    },
    /// Drive the trusted-Unix weak-path control protocol.
    Supervise {
        /// The one-shot gate this supervisor opens on an authenticated release.
        gate: PathBuf,
        /// Where the supervised child's stdout is captured.
        child_stdout: PathBuf,
        /// Where the supervised child's stderr is captured.
        child_stderr: PathBuf,
        /// The child's argument array (itself a `barrier` invocation).
        argv: Vec<String>,
    },
}

/// A malformed invocation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("godot-mcp-supervisor: {0}")]
pub struct UsageError(pub String);

/// Parse one argument array into a role.
///
/// The program to run always follows a `--` terminator, so it can never be
/// re-parsed as an option of the supervisor itself, and a shell command string is
/// never accepted anywhere.
///
/// # Errors
/// Returns [`UsageError`] when the role, the gate or the argument array is missing
/// or malformed.
pub fn parse_role(arguments: &[String]) -> Result<Role, UsageError> {
    let mut arguments = arguments.iter();
    let role = arguments
        .next()
        .ok_or_else(|| UsageError("a role (barrier|supervise) is required".to_owned()))?;
    let mut gate = None;
    let mut join_cgroup = None;
    let mut child_stdout = None;
    let mut child_stderr = None;
    let mut argv: Vec<String> = Vec::new();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            BARRIER_ARGV_MARKER => {}
            "--gate" => gate = arguments.next().map(PathBuf::from),
            "--join-cgroup" => join_cgroup = arguments.next().map(PathBuf::from),
            "--child-stdout" => child_stdout = arguments.next().map(PathBuf::from),
            "--child-stderr" => child_stderr = arguments.next().map(PathBuf::from),
            "--" => {
                argv = arguments.map(std::borrow::ToOwned::to_owned).collect();
                break;
            }
            other => {
                return Err(UsageError(format!("unexpected argument {other}")));
            }
        }
    }
    let gate = gate.ok_or_else(|| UsageError("--gate is required".to_owned()))?;
    if argv.is_empty() {
        return Err(UsageError(
            "an argument array after `--` is required".to_owned(),
        ));
    }
    match role.as_str() {
        "barrier" => Ok(Role::Barrier {
            gate,
            join_cgroup,
            argv,
        }),
        "supervise" => Ok(Role::Supervise {
            gate,
            child_stdout: child_stdout
                .ok_or_else(|| UsageError("--child-stdout is required".to_owned()))?,
            child_stderr: child_stderr
                .ok_or_else(|| UsageError("--child-stderr is required".to_owned()))?,
            argv,
        }),
        other => Err(UsageError(format!("unknown role {other}"))),
    }
}

/// Run one role. Returns the process exit status to use.
///
/// The `barrier` role only ever *returns* on failure: on success it has replaced
/// this process's image with the requested program.
///
/// # Errors
/// Returns [`UsageError`] when the invocation is malformed.
pub fn run(role: Role) -> Result<u8, UsageError> {
    match role {
        Role::Barrier {
            gate,
            join_cgroup,
            argv,
        } => Ok(run_barrier(&gate, join_cgroup.as_deref(), &argv)),
        Role::Supervise {
            gate,
            child_stdout,
            child_stderr,
            argv,
        } => Ok(imp::run_supervisor(
            &gate,
            &child_stdout,
            &child_stderr,
            &argv,
        )),
    }
}

fn run_barrier(gate: &Path, join_cgroup: Option<&Path>, argv: &[String]) -> u8 {
    if let Some(cgroup) = join_cgroup {
        // Joining before blocking is what guarantees the member is inside the
        // boundary before it could ever run user code.
        let pid = std::process::id().to_string();
        if let Err(error) = std::fs::write(cgroup.join("cgroup.procs"), &pid) {
            eprintln!(
                "godot-mcp-supervisor: could not join {}: {error}",
                cgroup.display()
            );
            return CHANNEL_EXIT;
        }
    }

    let expires = std::time::Instant::now() + BARRIER_TIMEOUT;
    loop {
        match gate::state(gate) {
            GateState::Open => break,
            GateState::Closed | GateState::Unknown(_) => {}
        }
        if std::time::Instant::now() >= expires {
            eprintln!("godot-mcp-supervisor: the gate never opened");
            return CHANNEL_EXIT;
        }
        std::thread::sleep(gate::GATE_POLL_INTERVAL);
    }

    imp::exec_in_place(argv)
}

#[cfg(unix)]
mod imp {
    use std::os::fd::BorrowedFd;

    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    use super::super::weak_unix::NONCE_ENVIRONMENT_VARIABLE;
    use super::{CHANNEL_EXIT, PROTOCOL_EXIT, Path, UNAUTHENTICATED_EXIT, gate};

    /// The inherited control socket. Descriptor 0 is the *only* channel the parent
    /// and the supervisor share, and it is both readable and writable because it is
    /// a socket. `rustix::stdio::stdin` hands it over as a borrowed descriptor with
    /// no `unsafe` at this call site.
    fn control() -> BorrowedFd<'static> {
        rustix::stdio::stdin()
    }

    fn announce(message: &str) -> bool {
        rustix::io::write(control(), message.as_bytes()).is_ok()
    }

    struct Control(BufReader<std::fs::File>);

    impl Control {
        /// Buffered reader over an owned duplicate of the control endpoint, so
        /// buffering never closes the inherited original `announce` writes to.
        ///
        /// The duplicate must be close-on-exec: a descriptor that survived `exec`
        /// would leak the endpoint into the supervised child, leaving the endpoint
        /// with a live holder after this supervisor dies. The parent could then not
        /// distinguish a lost supervisor from a slow one, and reconciliation would
        /// find an endpoint holder that is not the supervisor.
        fn open() -> Option<Self> {
            let duplicated = rustix::io::fcntl_dupfd_cloexec(control(), 0).ok()?;
            Some(Self(BufReader::new(std::fs::File::from(duplicated))))
        }

        fn read_line(&mut self) -> Option<String> {
            let mut line = String::new();
            match self.0.read_line(&mut line) {
                Ok(0) | Err(_) => None,
                Ok(_) => Some(line.trim_end().to_owned()),
            }
        }
    }

    pub(super) fn exec_in_place(argv: &[String]) -> u8 {
        use std::os::unix::process::CommandExt as _;
        let error = Command::new(&argv[0]).args(&argv[1..]).exec();
        eprintln!("godot-mcp-supervisor: could not exec {}: {error}", argv[0]);
        CHANNEL_EXIT
    }

    pub(super) fn run_supervisor(
        gate: &Path,
        child_stdout: &Path,
        child_stderr: &Path,
        argv: &[String],
    ) -> u8 {
        let Ok(nonce) = std::env::var(NONCE_ENVIRONMENT_VARIABLE) else {
            eprintln!("godot-mcp-supervisor: no control nonce in the environment");
            return PROTOCOL_EXIT;
        };

        // A brand-new process group. An *empty* process group cannot be created in
        // advance, which is exactly why the durable locator — not this group — is
        // what `intent_fsynced` records.
        let pgid = match rustix::process::setsid() {
            Ok(pid) => pid.as_raw_nonzero().get().cast_unsigned(),
            Err(_) => rustix::process::getpid()
                .as_raw_nonzero()
                .get()
                .cast_unsigned(),
        };

        let Some(mut channel) = Control::open() else {
            eprintln!("godot-mcp-supervisor: the control channel is not usable");
            return CHANNEL_EXIT;
        };
        if !announce(&format!(
            "announce pid={} pgid={pgid} nonce={nonce}\n",
            std::process::id()
        )) {
            return CHANNEL_EXIT;
        }

        // Gate 1. No child of any kind exists until this read returns.
        match channel.read_line().as_deref() {
            Some("create") => {}
            Some(other) => {
                eprintln!("godot-mcp-supervisor: unexpected control command {other}");
                return PROTOCOL_EXIT;
            }
            None => return CHANNEL_EXIT,
        }

        let (Ok(out), Ok(err)) = (
            std::fs::File::create(child_stdout),
            std::fs::File::create(child_stderr),
        ) else {
            eprintln!("godot-mcp-supervisor: could not create the child capture files");
            return CHANNEL_EXIT;
        };
        let mut child = match Command::new(&argv[0])
            .args(&argv[1..])
            // The child never inherits the control socket or the nonce, so it can
            // never be mistaken for the endpoint's authenticated peer.
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .env_remove(NONCE_ENVIRONMENT_VARIABLE)
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                eprintln!("godot-mcp-supervisor: could not create the gated child: {error}");
                return CHANNEL_EXIT;
            }
        };
        if !announce(&format!("created pid={}\n", child.id())) {
            let _ = child.kill();
            return CHANNEL_EXIT;
        }

        // Gate 2. The child is created but blocked on its own gate.
        match channel.read_line() {
            Some(line) if line == format!("release {nonce}") => {}
            Some(_) => {
                eprintln!("godot-mcp-supervisor: unauthenticated release token");
                let _ = child.kill();
                return UNAUTHENTICATED_EXIT;
            }
            None => {
                let _ = child.kill();
                return CHANNEL_EXIT;
            }
        }
        if let Err(error) = gate::open(gate) {
            eprintln!("godot-mcp-supervisor: could not open the gate: {error}");
            let _ = child.kill();
            return CHANNEL_EXIT;
        }

        let status = match child.wait() {
            Ok(status) => status,
            Err(error) => {
                eprintln!("godot-mcp-supervisor: could not wait for the child: {error}");
                return CHANNEL_EXIT;
            }
        };
        use std::os::unix::process::ExitStatusExt as _;
        let report = match (status.code(), status.signal()) {
            (Some(code), _) => format!("exited code={code}\n"),
            (None, Some(signal)) => format!("exited signal={signal}\n"),
            (None, None) => "exited code=unknown\n".to_owned(),
        };
        let _ = announce(&report);
        let mut stderr = std::io::stderr();
        let _ = stderr.flush();
        0
    }
}

#[cfg(not(unix))]
mod imp {
    use std::process::Command;

    use super::{CHANNEL_EXIT, Path};

    pub(super) fn exec_in_place(argv: &[String]) -> u8 {
        // Windows has no `exec`, so the barrier runs the program as a child and
        // mirrors its status. The Job Object still contains it, because a job member
        // cannot break away and its children join the same job.
        match Command::new(&argv[0]).args(&argv[1..]).status() {
            Ok(status) => u8::try_from(status.code().unwrap_or(1)).unwrap_or(1),
            Err(error) => {
                eprintln!("godot-mcp-supervisor: could not run {}: {error}", argv[0]);
                CHANNEL_EXIT
            }
        }
    }

    pub(super) fn run_supervisor(
        _gate: &Path,
        _child_stdout: &Path,
        _child_stderr: &Path,
        _argv: &[String],
    ) -> u8 {
        eprintln!("godot-mcp-supervisor: the weak anchor protocol exists only on Unix hosts");
        super::USAGE_EXIT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn the_barrier_role_parses_its_marker_gate_and_program_array() {
        let role = parse_role(&arguments(&[
            "barrier",
            BARRIER_ARGV_MARKER,
            "--gate",
            "/tmp/gate",
            "--",
            "/bin/godot",
            "--headless",
        ]))
        .expect("a well-formed barrier invocation");
        assert_eq!(
            role,
            Role::Barrier {
                gate: PathBuf::from("/tmp/gate"),
                join_cgroup: None,
                argv: arguments(&["/bin/godot", "--headless"]),
            }
        );
    }

    #[test]
    fn the_supervise_role_requires_both_capture_files() {
        assert!(matches!(
            parse_role(&arguments(&[
                "supervise",
                "--gate",
                "/tmp/gate",
                "--",
                "/bin/barrier"
            ])),
            Err(UsageError(_))
        ));
        let role = parse_role(&arguments(&[
            "supervise",
            "--gate",
            "/tmp/gate",
            "--child-stdout",
            "/tmp/out",
            "--child-stderr",
            "/tmp/err",
            "--",
            "/bin/barrier",
            "--gate",
            "/tmp/gate",
        ]))
        .expect("a well-formed supervise invocation");
        assert!(matches!(role, Role::Supervise { .. }));
    }

    #[test]
    fn a_program_after_the_terminator_is_never_parsed_as_an_option() {
        let role = parse_role(&arguments(&[
            "barrier",
            "--gate",
            "/tmp/gate",
            "--",
            "/bin/godot",
            "--gate",
            "not-a-supervisor-option",
        ]))
        .expect("the terminator ends option parsing");
        match role {
            Role::Barrier { gate, argv, .. } => {
                assert_eq!(gate, PathBuf::from("/tmp/gate"));
                assert_eq!(
                    argv,
                    arguments(&["/bin/godot", "--gate", "not-a-supervisor-option"])
                );
            }
            other => panic!("expected a barrier role, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_or_unknown_role_is_refused() {
        assert!(matches!(parse_role(&[]), Err(UsageError(_))));
        assert!(matches!(
            parse_role(&arguments(&[
                "dance",
                "--gate",
                "/tmp/gate",
                "--",
                "/bin/true"
            ])),
            Err(UsageError(_))
        ));
    }
}
