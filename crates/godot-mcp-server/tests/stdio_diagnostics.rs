use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use godot_mcp_domain::config::{LogSink, ServerConfig};
use godot_mcp_server::stdio::{StdioError, install_stdio_diagnostics};

const CHILD_ENV: &str = "GODOT_MCP_STDIO_DIAGNOSTICS_CHILD";
const TEST_NAME: &str = "repeated_install_reports_that_the_configured_sink_was_not_installed";

fn wait_with_output_or_kill(mut child: Child, timeout: Duration) -> Output {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().expect("poll diagnostics child") {
            Some(_) => {
                return child
                    .wait_with_output()
                    .expect("collect diagnostics child output");
            }
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            None => break,
        }
    }

    let pid = child.id();
    child.kill().expect("kill timed-out diagnostics child");
    let output = child
        .wait_with_output()
        .expect("collect timed-out diagnostics child output");
    panic!(
        "diagnostics child timed out after {timeout:?}: pid={pid} status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn repeated_install_reports_that_the_configured_sink_was_not_installed() {
    if std::env::var_os(CHILD_ENV).is_none() {
        let child = Command::new(std::env::current_exe().expect("current test binary"))
            .arg(TEST_NAME)
            .arg("--exact")
            .arg("--nocapture")
            .env(CHILD_ENV, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run diagnostics child with separate streams");
        let output = wait_with_output_or_kill(child, Duration::from_secs(3));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("running 1 test") && stdout.contains(TEST_NAME),
            "diagnostics child filter did not match exactly one existing test: stdout={stdout}"
        );
        assert!(
            output.status.success(),
            "diagnostics child failed: status={} stdout={} stderr={}",
            output.status,
            stdout,
            String::from_utf8_lossy(&output.stderr),
        );
        return;
    }

    // Given one process-global subscriber installed from the server config.
    let temp = tempfile::tempdir().expect("temp dir");
    let config = ServerConfig {
        log_sink: LogSink::File {
            path: temp
                .path()
                .join("diagnostics.jsonl")
                .to_string_lossy()
                .into_owned(),
        },
        ..ServerConfig::default()
    };
    install_stdio_diagnostics(&config).expect("first diagnostics install succeeds");

    // When the public library entry point is invoked again in the same process.
    let error = install_stdio_diagnostics(&config)
        .expect_err("second diagnostics install must not report success");

    // Then the caller is told that its requested sink was not installed.
    match error {
        StdioError::Diagnostics(message) => {
            assert_eq!(message, "a global tracing subscriber is already installed")
        }
        other => panic!("unexpected repeated-install error: {other}"),
    }
}
