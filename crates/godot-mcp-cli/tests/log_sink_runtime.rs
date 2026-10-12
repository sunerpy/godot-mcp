use std::path::Path;

use assert_cmd::Command;
use serde_json::{Value, json};

const STDIO_HANDSHAKE: &str = concat!(
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"log-sink-test","version":"0.1.0"}}}"#,
    "\n",
    r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
    "\n",
);

fn cli() -> Command {
    let mut command = Command::cargo_bin("godot-mcp").expect("godot-mcp binary");
    for (name, _) in std::env::vars().filter(|(name, _)| name.starts_with("GODOT_MCP_")) {
        command.env_remove(name);
    }
    command
}

fn std_cli() -> std::process::Command {
    let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin!("godot-mcp"));
    for (name, _) in std::env::vars().filter(|(name, _)| name.starts_with("GODOT_MCP_")) {
        command.env_remove(name);
    }
    command
}

#[cfg(unix)]
unsafe extern "C" {
    fn posix_openpt(flags: std::ffi::c_int) -> std::ffi::c_int;
    fn grantpt(fd: std::ffi::c_int) -> std::ffi::c_int;
    fn unlockpt(fd: std::ffi::c_int) -> std::ffi::c_int;
    fn ptsname_r(
        fd: std::ffi::c_int,
        buffer: *mut std::ffi::c_char,
        length: usize,
    ) -> std::ffi::c_int;
}

#[cfg(unix)]
fn open_pty() -> (std::fs::File, std::fs::File) {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStringExt as _;

    const O_RDWR: std::ffi::c_int = 2;
    // SAFETY: `posix_openpt` returns a newly owned descriptor on success.
    let master_fd = unsafe { posix_openpt(O_RDWR) };
    assert!(
        master_fd >= 0,
        "posix_openpt failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: ownership of the successful descriptor is transferred exactly once.
    let master = unsafe { std::fs::File::from_raw_fd(master_fd) };
    // SAFETY: `master` owns a live PTY master descriptor for both calls.
    assert_eq!(
        unsafe { grantpt(master.as_raw_fd()) },
        0,
        "grantpt failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: `master` owns a live PTY master descriptor for both calls.
    assert_eq!(
        unsafe { unlockpt(master.as_raw_fd()) },
        0,
        "unlockpt failed: {}",
        std::io::Error::last_os_error()
    );

    let mut name = vec![0_u8; 1024];
    // SAFETY: the buffer is writable for its declared length and the descriptor
    // remains live for the duration of the call.
    let result = unsafe {
        ptsname_r(
            master.as_raw_fd(),
            name.as_mut_ptr().cast::<std::ffi::c_char>(),
            name.len(),
        )
    };
    assert_eq!(result, 0, "ptsname_r failed with {result}");
    let end = name
        .iter()
        .position(|byte| *byte == 0)
        .expect("PTY slave name terminator");
    let slave_path = std::path::PathBuf::from(std::ffi::OsString::from_vec(name[..end].to_vec()));
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&slave_path)
        .unwrap_or_else(|error| panic!("open PTY slave {}: {error}", slave_path.display()));
    (master, slave)
}

#[cfg(unix)]
fn wait_or_kill(
    child: &mut std::process::Child,
    timeout: std::time::Duration,
    context: &str,
) -> std::process::ExitStatus {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait().expect("poll PTY CLI child") {
            Some(status) => return status,
            None if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            None => break,
        }
    }

    let pid = child.id();
    let wchan = std::fs::read_to_string(format!("/proc/{pid}/wchan"))
        .unwrap_or_else(|error| format!("unavailable: {error}"));
    child.kill().expect("kill timed-out PTY CLI child");
    let status = child.wait().expect("wait for killed PTY CLI child");
    panic!(
        "{context} timed out after {timeout:?}: pid={pid} wchan={} status={status}",
        wchan.trim(),
    );
}

#[cfg(unix)]
fn read_pty(mut master: std::fs::File) -> Vec<u8> {
    use std::io::Read as _;

    let mut output = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        match master.read(&mut buffer) {
            Ok(0) => break,
            Ok(length) => output.extend_from_slice(&buffer[..length]),
            // Linux reports EIO after the final PTY slave closes; other Unix
            // platforms commonly report EOF instead.
            Err(error) if error.raw_os_error() == Some(5) => break,
            Err(error) => panic!("read PTY master: {error}"),
        }
    }
    output
}

#[cfg(unix)]
fn run_cli_on_shared_pty(
    configure: impl FnOnce(&mut std::process::Command),
    input: &[u8],
    context: &str,
) -> (std::process::ExitStatus, Vec<u8>) {
    let (master, slave) = open_pty();
    let mut command = std_cli();
    configure(&mut command);
    let mut child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::from(
            slave.try_clone().expect("clone PTY slave for stdout"),
        ))
        .stderr(std::process::Stdio::from(slave))
        .spawn()
        .unwrap_or_else(|error| panic!("spawn {context}: {error}"));
    drop(command);
    // The master is read while the child runs: macOS discards what a PTY still
    // buffers once its last slave descriptor closes, so reading only after the
    // child exited would find nothing there.
    let reader = std::thread::spawn(move || read_pty(master));
    std::io::Write::write_all(child.stdin.as_mut().expect("piped PTY CLI stdin"), input)
        .unwrap_or_else(|error| panic!("write {context} stdin: {error}"));
    drop(child.stdin.take());
    let status = wait_or_kill(&mut child, std::time::Duration::from_secs(3), context);
    (status, reader.join().expect("the PTY reader finishes"))
}

#[cfg(unix)]
fn wait_with_output_or_kill(
    mut child: std::process::Child,
    timeout: std::time::Duration,
    context: &str,
) -> std::process::Output {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait().expect("poll CLI child") {
            Some(_) => return child.wait_with_output().expect("collect CLI output"),
            None if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            None => break,
        }
    }

    let pid = child.id();
    let wchan = std::fs::read_to_string(format!("/proc/{pid}/wchan"))
        .unwrap_or_else(|error| format!("unavailable: {error}"));
    child.kill().expect("kill timed-out CLI child");
    let output = child
        .wait_with_output()
        .expect("collect timed-out CLI output");
    panic!(
        "{context} timed out after {timeout:?}: pid={pid} wchan={} status={} stdout={} stderr={}",
        wchan.trim(),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Offer the stdio handshake to a CLI that is expected to refuse before it
/// reads a byte. It may already have exited, so a closed pipe is no failure
/// here: the exit status and the output the caller asserts are the verdict.
fn offer_handshake(child: &mut std::process::Child) {
    match std::io::Write::write_all(
        child.stdin.as_mut().expect("piped CLI stdin"),
        STDIO_HANDSHAKE.as_bytes(),
    ) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
        Err(error) => panic!("write stdio handshake: {error}"),
    }
    drop(child.stdin.take());
}

fn empty_project(root: &Path) -> std::path::PathBuf {
    let project = root.join("project");
    std::fs::create_dir_all(&project).expect("create project directory");
    std::fs::write(project.join("project.godot"), b"config_version=5\n")
        .expect("write project marker");
    project
}

fn json_lines(bytes: &[u8], source: &str) -> Vec<Value> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|error| panic!("{source} line was not JSON: {line:?} ({error})"))
        })
        .collect()
}

fn stdout_line_counts(bytes: &[u8]) -> (usize, usize) {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .fold(
            (0, 0),
            |(frames, non_frames), line| match serde_json::from_str::<Value>(line) {
                Ok(value) if value["jsonrpc"] == json!("2.0") => (frames + 1, non_frames),
                Ok(_) | Err(_) => (frames, non_frames + 1),
            },
        )
}

fn assert_protocol_stdout(bytes: &[u8]) {
    let (frame_lines, non_frame_lines) = stdout_line_counts(bytes);
    assert!(frame_lines > 0, "protocol stdout had no frames");
    assert_eq!(
        non_frame_lines,
        0,
        "protocol stdout contamination: frame_lines={frame_lines}, non_frame_lines={non_frame_lines}; stdout={}",
        String::from_utf8_lossy(bytes),
    );
}

#[test]
fn merged_default_stderr_and_stdout_are_rejected_before_protocol_startup() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = empty_project(temp.path());
    let merged_path = temp.path().join("merged-output");
    let merged = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&merged_path)
        .expect("open merged output");
    // This deliberately covers duplicated/inherited handles. The Windows-only
    // companion below independently opens the same path twice so it cannot pass
    // merely because `CompareObjectHandles` recognizes a duplicated handle.
    let merged_stderr = merged.try_clone().expect("clone merged output");

    let mut child = std_cli()
        .env("RUST_LOG", "info")
        .args(["serve", "--stdio", "--project"])
        .arg(&project)
        .arg("--profile")
        .arg("core")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::from(merged))
        .stderr(std::process::Stdio::from(merged_stderr))
        .spawn()
        .expect("spawn merged-stream CLI");
    offer_handshake(&mut child);
    let status = child.wait().expect("wait for merged-stream CLI");
    let merged = std::fs::read_to_string(&merged_path).expect("read merged output");

    assert_eq!(
        status.code(),
        Some(70),
        "merged default streams returned the wrong status: {status}; output={merged}"
    );
    assert!(
        !merged.contains("\"jsonrpc\"") && !merged.contains("stdio_serving"),
        "protocol or tracing output started before merged streams were rejected: {merged}"
    );
    assert!(
        merged.contains("stdio serving requires separate stdout and stderr")
            && !merged.contains("[log_sink.file]"),
        "merged-stream refusal was not actionable: {merged}"
    );
}

#[test]
fn merged_file_sink_is_rejected_while_same_config_with_separate_streams_succeeds() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = empty_project(temp.path());
    let log_path = temp.path().join("logs").join("godot-mcp.log");
    let config_path = temp.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!("[log_sink.file]\npath = {:?}\n", log_path.to_string_lossy()),
    )
    .expect("write file-sink config");
    let merged_path = temp.path().join("merged-output");
    let merged = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&merged_path)
        .expect("open merged output");
    let merged_stderr = merged.try_clone().expect("clone merged output");

    let mut child = std_cli()
        .env("RUST_LOG", "info")
        .args(["serve", "--stdio", "--config"])
        .arg(&config_path)
        .arg("--project")
        .arg(&project)
        .args(["--profile", "core"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::from(merged))
        .stderr(std::process::Stdio::from(merged_stderr))
        .spawn()
        .expect("spawn merged file-sink CLI");
    offer_handshake(&mut child);
    let status = child.wait().expect("wait for merged file-sink CLI");
    let merged = std::fs::read(&merged_path).expect("read merged output");
    let (frame_lines, non_frame_lines) = stdout_line_counts(&merged);

    assert_eq!(
        frame_lines,
        0,
        "file-sink stdio wrote protocol frames before rejecting merged streams: non_frame_lines={non_frame_lines}; output={}",
        String::from_utf8_lossy(&merged),
    );
    assert_eq!(
        status.code(),
        Some(70),
        "merged file-sink streams returned the wrong status: {status}; frame_lines={frame_lines}, non_frame_lines={non_frame_lines}; output={}",
        String::from_utf8_lossy(&merged),
    );
    assert!(
        String::from_utf8_lossy(&merged)
            .contains("stdio serving requires separate stdout and stderr"),
        "merged file-sink refusal was not actionable: {}",
        String::from_utf8_lossy(&merged),
    );
    assert!(
        !log_path.exists(),
        "file-sink tracing was installed before merged streams were rejected"
    );

    let output = cli()
        .env("RUST_LOG", "info")
        .args(["serve", "--stdio", "--config"])
        .arg(&config_path)
        .arg("--project")
        .arg(&project)
        .args(["--profile", "core"])
        .write_stdin(STDIO_HANDSHAKE)
        .output()
        .expect("run separate-stream file-sink CLI");
    assert!(
        output.status.success(),
        "the same file-sink config must work with separate streams: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_protocol_stdout(&output.stdout);
    assert!(
        output.stderr.is_empty(),
        "accepted file-sink stdio unexpectedly wrote to stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let diagnostics = std::fs::read(&log_path).expect("read separate-stream diagnostics");
    let events = json_lines(&diagnostics, "separate-stream file-sink diagnostics");
    assert!(
        events
            .iter()
            .any(|event| event["fields"]["event"] == json!("stdio_serving")),
        "the accepted separate-stream control did not reach stdio serving: {events:?}"
    );
}

#[cfg(unix)]
#[test]
fn shared_pty_check_config_succeeds_while_stdio_serve_still_refuses() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = empty_project(temp.path());
    let config_path = temp.path().join("config.toml");
    std::fs::write(&config_path, "transport = \"stdio\"\n").expect("write valid config");

    let (check_status, check_output) = run_cli_on_shared_pty(
        |command| {
            command.args(["check-config", "--config"]).arg(&config_path);
        },
        b"",
        "check-config on shared PTY",
    );
    assert_eq!(
        check_status.code(),
        Some(0),
        "check-config must validate a valid config even when stdout and stderr share a PTY: status={check_status}; output={}",
        String::from_utf8_lossy(&check_output),
    );
    assert!(
        String::from_utf8_lossy(&check_output).contains("check-config: OK"),
        "check-config did not reach validation on the shared PTY: {}",
        String::from_utf8_lossy(&check_output),
    );

    let (serve_status, serve_output) = run_cli_on_shared_pty(
        |command| {
            command
                .args(["serve", "--stdio", "--project"])
                .arg(&project)
                .args(["--profile", "core"]);
        },
        STDIO_HANDSHAKE.as_bytes(),
        "serve --stdio on shared PTY",
    );
    let (frame_lines, _) = stdout_line_counts(&serve_output);
    assert_eq!(
        frame_lines,
        0,
        "serve wrote protocol frames before rejecting the shared PTY: {}",
        String::from_utf8_lossy(&serve_output),
    );
    assert_eq!(
        serve_status.code(),
        Some(70),
        "serve --stdio must still reject a shared PTY: status={serve_status}; output={}",
        String::from_utf8_lossy(&serve_output),
    );
    assert!(
        String::from_utf8_lossy(&serve_output)
            .contains("stdio serving requires separate stdout and stderr"),
        "serve --stdio used the wrong shared-PTY refusal: {}",
        String::from_utf8_lossy(&serve_output),
    );
    assert!(
        !String::from_utf8_lossy(&serve_output).contains("stdio_serving"),
        "tracing started before the shared PTY was rejected: {}",
        String::from_utf8_lossy(&serve_output),
    );
}

#[cfg(windows)]
#[test]
fn independently_opened_same_disk_file_for_stdout_and_stderr_is_rejected() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = empty_project(temp.path());
    let merged_path = temp.path().join("independently-opened-output");
    let stdout_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&merged_path)
        .expect("open stdout file");
    // Open the path again instead of cloning `stdout_file`: on Windows this is a
    // distinct kernel file object that still names the same physical file.
    let stderr_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&merged_path)
        .expect("independently open stderr file");

    let mut child = std_cli()
        .env("RUST_LOG", "info")
        .args(["serve", "--stdio", "--project"])
        .arg(&project)
        .args(["--profile", "core"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::from(stdout_file))
        .stderr(std::process::Stdio::from(stderr_file))
        .spawn()
        .expect("spawn independently-opened merged-stream CLI");
    offer_handshake(&mut child);
    let status = child
        .wait()
        .expect("wait for independently-opened merged-stream CLI");
    let merged = std::fs::read_to_string(&merged_path).expect("read merged output");

    assert_eq!(
        status.code(),
        Some(70),
        "independent handles for the same disk file returned the wrong status: {status}; output={merged}"
    );
    assert!(
        !merged.contains("\"jsonrpc\"") && !merged.contains("stdio_serving"),
        "protocol or tracing output started before independent same-file handles were rejected: {merged}"
    );
    assert!(
        merged.contains("stdio serving requires separate stdout and stderr"),
        "independent same-file handles were not rejected by physical file identity: {merged}"
    );
}

#[cfg(unix)]
#[test]
fn stdout_alias_file_sinks_are_rejected_without_protocol_contamination() {
    for sink_path in ["/dev/./stdout", "/dev/../dev/stdout"] {
        // Given a valid project and a file sink path that names process stdout
        // without matching the cheap string blacklist.
        let temp = tempfile::tempdir().expect("temp dir");
        let project = empty_project(temp.path());
        let config_path = temp.path().join("config.toml");
        let stdout_path = temp.path().join("protocol.stdout");
        let stdout_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&stdout_path)
            .expect("open regular protocol stdout file");
        std::fs::write(
            &config_path,
            format!("[log_sink.file]\npath = {sink_path:?}\n"),
        )
        .expect("write stdout-alias config");

        // When the shipped CLI attempts a real stdio handshake.
        let mut child = std_cli()
            .env("RUST_LOG", "info")
            .args(["serve", "--stdio", "--config"])
            .arg(&config_path)
            .arg("--project")
            .arg(&project)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::from(
                stdout_file.try_clone().expect("clone protocol stdout"),
            ))
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn stdout-alias CLI");
        offer_handshake(&mut child);
        let output = child.wait_with_output().expect("run stdout-alias CLI");

        // Then rejection happens before either diagnostics or protocol data can
        // contaminate stdout. Count contamination first so the historical bug
        // fails with the observed frame/non-frame totals rather than only status.
        let stdout = std::fs::read(&stdout_path).expect("read protocol stdout");
        let (frame_lines, non_frame_lines) = stdout_line_counts(&stdout);
        assert_eq!(
            non_frame_lines,
            0,
            "stdout contamination for sink {sink_path}: frame_lines={frame_lines}, non_frame_lines={non_frame_lines}; stdout={}",
            String::from_utf8_lossy(&stdout),
        );
        assert_eq!(
            frame_lines, 0,
            "stdout alias {sink_path} must be rejected before protocol startup"
        );
        assert_eq!(
            output.status.code(),
            Some(70),
            "stdout alias {sink_path} returned the wrong status: status={} stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("protocol stdout is not a diagnostics sink"),
            "stdout alias {sink_path} was not rejected by sink identity: stderr={}",
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

#[cfg(unix)]
#[test]
fn non_regular_file_sinks_are_rejected_without_protocol_contamination() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = empty_project(temp.path());
    let sink_path = Path::new("/dev/null");
    // Given a character device selected as a rolling-file sink.
    let config_path = temp.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[log_sink.file]\npath = {:?}\n",
            sink_path.to_string_lossy()
        ),
    )
    .expect("write non-regular sink config");

    // When the shipped CLI attempts a real stdio handshake.
    let output = cli()
        .env("RUST_LOG", "info")
        .args(["serve", "--stdio", "--config"])
        .arg(&config_path)
        .arg("--project")
        .arg(&project)
        .write_stdin(STDIO_HANDSHAKE)
        .output()
        .expect("run non-regular-sink CLI");

    // Then inspect contamination before status so a regression reports the
    // frame/non-frame totals instead of only an exit-code mismatch.
    let (frame_lines, non_frame_lines) = stdout_line_counts(&output.stdout);
    assert_eq!(
        non_frame_lines,
        0,
        "stdout contamination for sink {}: frame_lines={frame_lines}, non_frame_lines={non_frame_lines}; stdout={}",
        sink_path.display(),
        String::from_utf8_lossy(&output.stdout),
    );
    assert_eq!(
        frame_lines,
        0,
        "non-regular sink {} must be rejected before protocol startup",
        sink_path.display(),
    );
    assert_eq!(
        output.status.code(),
        Some(70),
        "non-regular sink {} returned the wrong status: status={} stdout={} stderr={}",
        sink_path.display(),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("the object is not a regular file"),
        "non-regular sink {} used the wrong refusal: stderr={}",
        sink_path.display(),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[cfg(unix)]
#[test]
fn readerless_fifo_file_sink_is_rejected_without_hanging_or_protocol_contamination() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = empty_project(temp.path());
    let fifo_path = temp.path().join("diagnostics.fifo");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo_path)
        .status()
        .expect("run mkfifo");
    assert!(status.success(), "mkfifo failed with {status}");
    let config_path = temp.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[log_sink.file]\npath = {:?}\n",
            fifo_path.to_string_lossy()
        ),
    )
    .expect("write reader-less FIFO sink config");

    // Deliberately do not open the FIFO from this process: supplying a reader
    // would mask a blocking write-only open in the CLI.
    let mut child = std_cli()
        .env("RUST_LOG", "info")
        .args(["serve", "--stdio", "--config"])
        .arg(&config_path)
        .arg("--project")
        .arg(&project)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn reader-less FIFO CLI");
    offer_handshake(&mut child);
    let output = wait_with_output_or_kill(
        child,
        std::time::Duration::from_secs(3),
        "reader-less FIFO diagnostics sink",
    );

    // Count contamination before status so a regression reports both totals.
    let (frame_lines, non_frame_lines) = stdout_line_counts(&output.stdout);
    assert_eq!(
        non_frame_lines,
        0,
        "stdout contamination for reader-less FIFO: frame_lines={frame_lines}, non_frame_lines={non_frame_lines}; stdout={}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert_eq!(
        frame_lines, 0,
        "reader-less FIFO must be rejected before protocol startup"
    );
    assert_eq!(
        output.status.code(),
        Some(70),
        "reader-less FIFO returned the wrong status: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("the diagnostics sink has no reader (ENXIO)"),
        "reader-less FIFO used the wrong refusal: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[cfg(unix)]
#[test]
fn readerless_fifo_audit_sink_is_rejected_without_hanging_or_protocol_contamination() {
    use std::os::unix::fs::DirBuilderExt;

    let temp = tempfile::tempdir().expect("temp dir");
    let project = empty_project(temp.path());
    let state_dir = project.join(".godot-mcp");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&state_dir)
        .expect("create private state directory");
    let audit_dir = state_dir.join("audit");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&audit_dir)
        .expect("create audit directory");
    let fifo_path = audit_dir.join("audit.jsonl");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo_path)
        .status()
        .expect("run mkfifo");
    assert!(status.success(), "mkfifo failed with {status}");

    // Deliberately do not open the FIFO from this process: supplying a reader
    // would mask a blocking write-only open in the runtime's audit assembly.
    let mut child = std_cli()
        .args(["serve", "--stdio", "--project"])
        .arg(&project)
        .arg("--profile")
        .arg("core")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn reader-less FIFO audit CLI");
    offer_handshake(&mut child);
    let output = wait_with_output_or_kill(
        child,
        std::time::Duration::from_secs(3),
        "reader-less FIFO audit sink",
    );

    // Count contamination before status so a regression reports both totals.
    let (frame_lines, non_frame_lines) = stdout_line_counts(&output.stdout);
    assert_eq!(
        non_frame_lines,
        0,
        "stdout contamination for reader-less audit FIFO: frame_lines={frame_lines}, non_frame_lines={non_frame_lines}; stdout={}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert_eq!(
        frame_lines, 0,
        "reader-less audit FIFO must be rejected before protocol startup"
    );
    assert_eq!(
        output.status.code(),
        Some(70),
        "reader-less audit FIFO returned the wrong status: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("the diagnostics sink has no reader (ENXIO)"),
        "reader-less audit FIFO used the wrong refusal: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[cfg(unix)]
#[test]
fn relative_conout_name_is_an_ordinary_file_sink() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = empty_project(temp.path());
    let config_path = temp.path().join("config.toml");
    std::fs::write(&config_path, "[log_sink.file]\npath = \"conout$\"\n")
        .expect("write relative conout config");

    let output = cli()
        .current_dir(temp.path())
        .env("RUST_LOG", "info")
        .args(["serve", "--stdio", "--config"])
        .arg(&config_path)
        .arg("--project")
        .arg(&project)
        .write_stdin(STDIO_HANDSHAKE)
        .output()
        .expect("run relative conout CLI");

    assert!(
        output.status.success(),
        "relative conout sink must start: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_protocol_stdout(&output.stdout);
    let diagnostics = std::fs::read(temp.path().join("conout$")).expect("read conout sink");
    let events = json_lines(&diagnostics, "relative conout diagnostics");
    assert!(
        events
            .iter()
            .any(|event| event["fields"]["event"] == json!("stdio_serving")),
        "relative conout sink did not receive stdio_serving: {events:?}"
    );
}

#[test]
fn configured_file_sink_receives_json_diagnostics_from_cli() {
    // Given a valid project and a server config selecting a file sink.
    let temp = tempfile::tempdir().expect("temp dir");
    let project = empty_project(temp.path());
    let log_path = temp.path().join("logs").join("godot-mcp.log");
    let config_path = temp.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!("[log_sink.file]\npath = {:?}\n", log_path.to_string_lossy()),
    )
    .expect("write file-sink config");

    // When the shipped CLI completes a real stdio handshake.
    let output = cli()
        .env("RUST_LOG", "info")
        .args(["serve", "--stdio", "--config"])
        .arg(&config_path)
        .arg("--project")
        .arg(&project)
        .write_stdin(STDIO_HANDSHAKE)
        .output()
        .expect("run configured CLI");
    assert!(
        output.status.success(),
        "configured CLI must exit successfully: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_protocol_stdout(&output.stdout);

    // Then its configured file contains structured diagnostics from that run.
    let diagnostics = std::fs::read_to_string(&log_path).unwrap_or_else(|error| {
        panic!(
            "configured diagnostics file {} could not be read: {error}",
            log_path.display()
        )
    });
    assert!(
        !diagnostics.is_empty(),
        "configured diagnostics file {} was empty after a successful CLI run; stderr={}",
        log_path.display(),
        String::from_utf8_lossy(&output.stderr),
    );
    let events = json_lines(diagnostics.as_bytes(), "configured diagnostics");
    assert!(
        events
            .iter()
            .any(|event| event["fields"]["event"] == json!("stdio_serving")),
        "configured diagnostics did not contain the stdio_serving event: {events:?}"
    );
}

#[test]
fn default_sink_writes_json_diagnostics_to_stderr() {
    // Given a valid project and a config with no log_sink key.
    let temp = tempfile::tempdir().expect("temp dir");
    let project = empty_project(temp.path());
    let config_path = temp.path().join("config.toml");
    std::fs::write(&config_path, "transport = \"stdio\"\n").expect("write default config");

    // When the shipped CLI completes a real stdio handshake.
    let output = cli()
        .env("RUST_LOG", "info")
        .args(["serve", "--stdio", "--config"])
        .arg(&config_path)
        .arg("--project")
        .arg(&project)
        .write_stdin(STDIO_HANDSHAKE)
        .output()
        .expect("run default CLI");
    assert!(
        output.status.success(),
        "default CLI must exit successfully: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    // Then stdout is protocol-only and structured diagnostics use stderr.
    assert_protocol_stdout(&output.stdout);
    let events = json_lines(&output.stderr, "default stderr diagnostics");
    assert!(
        events
            .iter()
            .any(|event| event["fields"]["event"] == json!("stdio_serving")),
        "default stderr diagnostics did not contain stdio_serving: {events:?}"
    );
}
