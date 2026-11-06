//! ADR 0014: `godot-mcp serve --http <ADDR>` starts the open HTTP mode without
//! a configuration file. Every case runs the built binary: the lines it prints
//! on stderr, the URL a client on this host dials, the token when one is
//! named, and the refusals before anything opens.

#![cfg(feature = "http")]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{IpAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use predicates::prelude::*;

const PREFIX: &str = "godot-mcp serve: ";

fn empty_project(root: &Path) -> PathBuf {
    let project = root.join("project");
    std::fs::create_dir_all(&project).expect("create the project directory");
    std::fs::write(project.join("project.godot"), b"config_version=5\n")
        .expect("write project.godot");
    project
}

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_godot-mcp"));
    for (name, _) in std::env::vars().filter(|(name, _)| name.starts_with("GODOT_MCP_")) {
        command.env_remove(name);
    }
    command
}

fn assert_cli() -> assert_cmd::Command {
    let mut command = assert_cmd::Command::cargo_bin("godot-mcp").expect("godot-mcp binary");
    for (name, _) in std::env::vars().filter(|(name, _)| name.starts_with("GODOT_MCP_")) {
        command.env_remove(name);
    }
    command
}

/// A running `serve`, and the lines it prints on stderr as they come.
struct Server {
    child: Child,
    lines: Receiver<String>,
}

impl Server {
    fn spawn(project: &Path, arguments: &[&str], environment: &[(&str, &str)]) -> Self {
        let mut command = command();
        command
            .args(["serve", "--project"])
            .arg(project)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for (name, value) in environment {
            command.env(name, value);
        }
        let mut child = command.spawn().expect("spawn serve");
        let stderr = child.stderr.take().expect("captured stderr");
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self { child, lines }
    }

    /// The banner lines (`godot-mcp serve: ...`, not the JSON log records), up
    /// to and including the first one `last` accepts.
    fn banner_until(&self, last: impl Fn(&str) -> bool) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut banner = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(left).unwrap_or_else(|error| {
                panic!("serve printed no further line ({error}); so far: {banner:#?}")
            });
            if !line.starts_with(PREFIX) {
                continue;
            }
            let done = last(&line);
            banner.push(line);
            if done {
                return banner;
            }
        }
    }

    /// The banner lines printed within `quiet` after `banner_until` stopped.
    fn further_banner(&self, quiet: Duration) -> Vec<String> {
        let deadline = Instant::now() + quiet;
        let mut further = Vec::new();
        while let Ok(line) = self
            .lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            if line.starts_with(PREFIX) {
                further.push(line);
            }
        }
        further
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// POST an MCP `initialize` to `url`; answer the status and the body read.
fn initialize(url: &str, authorization: Option<&str>) -> (u16, String) {
    let rest = url.strip_prefix("http://").expect("an http URL");
    let (authority, path) = rest.split_once('/').expect("a URL with a path");
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"serve-http-smoke","version":"0"}}}"#;
    let mut request = format!(
        "POST /{path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\
         Connection: close\r\n",
        body.len()
    );
    if let Some(authorization) = authorization {
        request.push_str(&format!("Authorization: {authorization}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    let mut stream = TcpStream::connect(authority).expect("connect to the listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("read timeout");
    stream
        .write_all(request.as_bytes())
        .expect("send the request");
    let mut response = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                response.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&response);
                if text.contains("\"result\"") || text.contains("\"error\"") {
                    break;
                }
            }
        }
    }
    let text = String::from_utf8_lossy(&response).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no HTTP status line: {text:?}"));
    (status, text)
}

/// The port of a banner line naming a URL (`... at http://A:P/mcp`) or a
/// wildcard address (`... on 0.0.0.0:P, every interface`).
fn port_of(listening: &str) -> u16 {
    let address = listening
        .strip_suffix(", every interface")
        .or_else(|| listening.strip_suffix("/mcp"))
        .unwrap_or_else(|| panic!("a URL or a wildcard address: {listening:?}"));
    address
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
        .unwrap_or_else(|| panic!("no port in {listening:?}"))
}

#[test]
fn serve_http_on_every_interface_prints_where_to_connect() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = empty_project(temp.path());
    let server = Server::spawn(&project, &["--http", "0.0.0.0:0"], &[]);
    let banner = server.banner_until(|line| line.contains("warning: no token"));

    let listening = banner[0]
        .strip_prefix("godot-mcp serve: HTTP (open) listening on ")
        .unwrap_or_else(|| panic!("the first line names the listener: {banner:#?}"));
    assert!(
        listening.starts_with("0.0.0.0:") && listening.ends_with(", every interface"),
        "a wildcard is named as an address, not as a URL to dial: {listening}"
    );
    let port = port_of(listening);
    assert_ne!(port, 0);
    assert_eq!(
        banner[1],
        format!("godot-mcp serve: this host: http://127.0.0.1:{port}/mcp")
    );
    let other = banner[2]
        .strip_prefix("godot-mcp serve: other hosts: ")
        .unwrap_or_else(|| panic!("the third line names other hosts' URL: {banner:#?}"));
    let placeholder = format!("http://<this host's LAN address>:{port}/mcp");
    if other != placeholder {
        let address = other
            .strip_prefix("http://")
            .and_then(|rest| rest.strip_suffix(&format!(":{port}/mcp")))
            .and_then(|address| address.parse::<IpAddr>().ok())
            .unwrap_or_else(|| panic!("a URL or the placeholder: {other:?}"));
        assert!(
            !address.is_loopback() && !address.is_unspecified(),
            "{address}"
        );
    }
    assert_eq!(
        banner[3],
        "godot-mcp serve: warning: no token; anyone who can reach this port can use every \
         tool of the core profile"
    );
    // No script warning: script execution is off.
    assert!(
        server.further_banner(Duration::from_millis(300)).is_empty(),
        "nothing follows the warning"
    );

    let (status, body) = initialize(&format!("http://127.0.0.1:{port}/mcp"), None);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"protocolVersion\""), "{body}");
}

#[test]
fn serve_http_with_a_token_needs_it_and_does_not_warn() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = empty_project(temp.path());
    let server = Server::spawn(
        &project,
        &[
            "--http",
            "0.0.0.0:0",
            "--http-token-env",
            "SERVE_HTTP_SMOKE_TOKEN",
            "--allow-scripts",
        ],
        &[("SERVE_HTTP_SMOKE_TOKEN", "smoke-token-1")],
    );
    let banner = server.banner_until(|line| line.contains("other hosts:"));
    assert_eq!(banner.len(), 3, "{banner:#?}");
    assert!(
        server.further_banner(Duration::from_millis(300)).is_empty(),
        "a token silences the warning, scripts or not"
    );
    let port = port_of(&banner[0]);
    let url = format!("http://127.0.0.1:{port}/mcp");
    assert_eq!(initialize(&url, None).0, 401);
    assert_eq!(initialize(&url, Some("Bearer smoke-token-2")).0, 401);
    let (status, body) = initialize(&url, Some("Bearer smoke-token-1"));
    assert_eq!(status, 200, "{body}");
}

#[test]
fn serve_http_names_script_execution_when_it_is_on() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = empty_project(temp.path());
    let server = Server::spawn(
        &project,
        &[
            "--http",
            "0.0.0.0:0",
            "--profile",
            "full",
            "--allow-scripts",
        ],
        &[],
    );
    let banner = server.banner_until(|line| line.contains("script execution"));
    assert!(
        banner
            .iter()
            .any(|line| line.ends_with("every tool of the full profile")),
        "{banner:#?}"
    );
    assert_eq!(
        banner.last().map(String::as_str),
        Some(
            "godot-mcp serve: warning: script execution is on, so they can also run any \
             GDScript with this user's privileges"
        )
    );
}

#[test]
fn serve_http_on_loopback_prints_one_line() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = empty_project(temp.path());
    let server = Server::spawn(&project, &["--http", "127.0.0.1:0"], &[]);
    let banner = server.banner_until(|line| line.contains("listening at"));
    assert!(
        banner[0].starts_with("godot-mcp serve: HTTP (open) listening at http://127.0.0.1:"),
        "{banner:#?}"
    );
    assert!(
        server.further_banner(Duration::from_millis(300)).is_empty(),
        "only this host reaches a loopback listener: no other URL, no warning"
    );
}

#[test]
fn serve_http_is_refused_with_a_config_a_workspace_or_an_unset_token() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = empty_project(temp.path());
    let config = temp.path().join("serve.toml");
    std::fs::write(&config, "transport = \"stdio\"\n").expect("write config");

    assert_cli()
        .args(["serve", "--http", "0.0.0.0:0", "--config"])
        .arg(&config)
        .arg("--project")
        .arg(&project)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot be used with"));
    assert_cli()
        .args(["serve", "--http", "0.0.0.0:0", "--workspace"])
        .arg(temp.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot be used with"));
    assert_cli()
        .args(["serve", "--http-token-env", "LAN_TOKEN", "--project"])
        .arg(&project)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--http <ADDR>"));
    assert_cli()
        .args(["serve", "--http", "every-interface", "--project"])
        .arg(&project)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("invalid value"));
    assert_cli()
        .args([
            "serve",
            "--http",
            "0.0.0.0:0",
            "--http-token-env",
            "SERVE_HTTP_SMOKE_TOKEN_UNSET",
            "--project",
        ])
        .arg(&project)
        .env_remove("SERVE_HTTP_SMOKE_TOKEN_UNSET")
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("configuration rejected")
                .and(predicate::str::contains("SERVE_HTTP_SMOKE_TOKEN_UNSET"))
                .and(predicate::str::contains("listening").not()),
        );
}

#[test]
fn the_serve_help_advertises_the_open_mode_flags() {
    assert_cli()
        .args(["serve", "--help"])
        .assert()
        .code(0)
        .stdout(
            predicate::str::contains("--http <ADDR>")
                .and(predicate::str::contains("--http-token-env <VAR>")),
        );
}
