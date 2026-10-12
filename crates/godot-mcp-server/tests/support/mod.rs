//! Shared out-of-process driver for the post-0.1 acceptance suites.
//!
//! Every suite that uses this module drives the **production** stdio assembly
//! (`ProjectRuntime::open` + `serve_stdio`) through the non-shipping
//! `qa-journal-foundation` binary, which contains no product logic of its own.
//! Nothing here reaches into crate internals: a response is only ever what an MCP
//! client would read off stdout.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// A fresh project tree: `<base>/game/project.godot` plus a nested
/// `<base>/game/sub/deeper` directory, under a canonicalized temp base so every
/// spelling a test builds is a deliberate alias rather than a temp-dir artefact.
pub struct ProjectTree {
    _temp: tempfile::TempDir,
    pub base: PathBuf,
    pub root: PathBuf,
}

impl ProjectTree {
    pub fn new(label: &str) -> Self {
        let temp = tempfile::Builder::new()
            .prefix(&format!("godot-mcp-{label}-"))
            .tempdir()
            .expect("temp project base");
        let base = temp.path().canonicalize().expect("canonical temp base");
        let root = base.join("game");
        std::fs::create_dir_all(root.join("sub").join("deeper")).expect("nested project dirs");
        std::fs::write(
            root.join("project.godot"),
            "config_version=5\n\n[application]\nconfig/name=\"m0\"\n",
        )
        .expect("write project.godot");
        Self {
            _temp: temp,
            base,
            root,
        }
    }

    pub fn write(&self, relative: &str, contents: &str) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent directories");
        }
        std::fs::write(path, contents).expect("write project file");
    }

    /// Every directory named `.godot-mcp` anywhere under the base, sorted.
    pub fn state_dirs(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![self.base.clone()];
        while let Some(directory) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    if path.file_name().is_some_and(|name| name == ".godot-mcp") {
                        found.push(path.clone());
                    }
                    pending.push(path);
                }
            }
        }
        found.sort();
        found
    }
}

/// Options for one spawned server.
pub struct Spawn<'a> {
    pub project: &'a Path,
    pub current_dir: Option<&'a Path>,
    pub profile: &'a str,
    pub godot: Option<&'a Path>,
}

impl<'a> Spawn<'a> {
    pub fn core(project: &'a Path) -> Self {
        Self {
            project,
            current_dir: None,
            profile: "core",
            godot: None,
        }
    }
}

pub struct StdioSession {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
    stderr: std::sync::Arc<std::sync::Mutex<String>>,
    stderr_drain: Option<std::thread::JoinHandle<()>>,
    pub transcript: String,
}

pub struct Exited {
    pub code: Option<i32>,
    pub stderr: String,
}

impl StdioSession {
    pub fn spawn(spawn: &Spawn<'_>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_qa-journal-foundation"));
        command
            .arg("--project")
            .arg(spawn.project)
            .arg("--profile")
            .arg(spawn.profile)
            .env_remove("GODOT_MCP_TEST_LAUNCH_CRASH_STAGE");
        if let Some(directory) = spawn.current_dir {
            command.current_dir(directory);
        }
        if let Some(godot) = spawn.godot {
            command.arg("--godot-binary").arg(godot);
        }
        Self::start_command(command)
    }

    /// Spawn a server in bootstrap state over a workspace directory.
    pub fn spawn_workspace(workspace: &Path, profile: &str) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_qa-journal-foundation"));
        command
            .arg("--workspace")
            .arg(workspace)
            .arg("--profile")
            .arg(profile)
            .env_remove("GODOT_MCP_TEST_LAUNCH_CRASH_STAGE");
        Self::start_command(command)
    }

    /// Spawn the server described by `command` with piped stdio.
    pub fn start_command(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("stdio server starts");
        let reader = BufReader::new(child.stdout.take().expect("stdout is piped"));
        // Diagnostics are drained continuously: a server that logs more than a
        // pipe buffer's worth would otherwise block on stderr mid-request.
        let stderr = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let mut pipe = child.stderr.take().expect("stderr is piped");
        let sink = std::sync::Arc::clone(&stderr);
        let stderr_drain = std::thread::spawn(move || {
            use std::io::Read as _;
            let mut buffer = [0_u8; 8192];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => sink
                        .lock()
                        .expect("stderr buffer")
                        .push_str(&String::from_utf8_lossy(&buffer[..read])),
                }
            }
        });
        Self {
            child,
            reader,
            stderr,
            stderr_drain: Some(stderr_drain),
            transcript: String::new(),
        }
    }

    fn collected_stderr(&mut self) -> String {
        if let Some(drain) = self.stderr_drain.take() {
            let _ = drain.join();
        }
        self.stderr.lock().expect("stderr buffer").clone()
    }

    /// Spawn and complete the MCP initialization handshake.
    pub fn initialized(spawn: &Spawn<'_>) -> Self {
        Self::handshake(Self::spawn(spawn)).0
    }

    /// Complete the MCP initialization handshake, returning the initialize
    /// response alongside the session.
    pub fn handshake(mut session: Self) -> (Self, Value) {
        let initialized = session.request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "post01-acceptance", "version": "0.2.0" }
            }),
        );
        assert_eq!(
            initialized["result"]["serverInfo"]["name"], "godot-mcp-server",
            "initialize must answer: {initialized}"
        );
        session.notify("notifications/initialized", json!({}));
        (session, initialized)
    }

    pub fn send(&mut self, frame: &Value) {
        let stdin = self.child.stdin.as_mut().expect("stdin is piped");
        let mut line = serde_json::to_string(frame).expect("frame serializes");
        line.push('\n');
        stdin.write_all(line.as_bytes()).expect("write frame");
        stdin.flush().expect("flush frame");
    }

    pub fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }));
        loop {
            let mut line = String::new();
            let read = self.reader.read_line(&mut line).expect("read frame");
            if read == 0 {
                // The server ended: its exit status and stderr say why.
                let status = self.child.wait().map(|status| status.to_string());
                let stderr = self.collected_stderr();
                panic!(
                    "the server closed stdout before answering {method} ({status:?}); \
                     transcript: {}; stderr: {stderr}",
                    self.transcript
                );
            }
            self.transcript.push_str(&line);
            let frame: Value = serde_json::from_str(line.trim())
                .unwrap_or_else(|error| panic!("stdout frame must be JSON: {line:?} ({error})"));
            if frame["id"] == json!(id) {
                return frame;
            }
        }
    }

    pub fn call_tool(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.request(
            id,
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Wait for the process to exit on its own and collect stderr.
    pub fn wait_exit(mut self, timeout: Duration) -> Exited {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("query server status") {
                return Exited {
                    code: status.code(),
                    stderr: self.collected_stderr(),
                };
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("stdio server did not exit within {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Kill the server with SIGKILL, as a crash would: nothing of it runs any
    /// cleanup.
    pub fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Close stdin and collect the remaining output.
    pub fn finish(mut self) -> Exited {
        drop(self.child.stdin.take());
        let mut remaining = String::new();
        while self
            .reader
            .read_line(&mut remaining)
            .expect("drain remaining stdout")
            > 0
        {
            self.transcript.push_str(&remaining);
            remaining.clear();
        }
        let status = self.child.wait().expect("server exits");
        Exited {
            code: status.code(),
            stderr: self.collected_stderr(),
        }
    }
}

/// The domain error code of a refused `tools/call`, whether it was answered as
/// a protocol error or as a structured tool error.
pub fn error_code(response: &Value) -> Value {
    if response.get("error").is_some() {
        response["error"]["data"]["code"].clone()
    } else {
        assert_eq!(
            response["result"]["isError"],
            json!(true),
            "expected a refusal: {response}"
        );
        response["result"]["structuredContent"]["code"].clone()
    }
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The structured payload of a successful `tools/call`, failing loudly otherwise.
pub fn structured(response: &Value) -> &Value {
    assert!(
        response.get("error").is_none(),
        "tools/call must not be a protocol error: {response}"
    );
    &response["result"]["structuredContent"]
}

/// A Linux shell script standing in for a Godot editor build. It answers the
/// capability probe like a real 4.7.1 editor, records every launch argv (one
/// argument per line) into `argv.log`, then runs `body`.
#[cfg(target_os = "linux")]
pub fn fake_godot(directory: &Path, body: &str) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt as _;

    let binary = directory.join("fake-godot");
    let argv_log = directory.join("argv.log");
    let script = format!(
        "#!/bin/sh\n\
         if [ \"$1\" = \"--version\" ]; then\n\
           printf '%s\\n' '4.7.1.stable.official.b09f793f5'\n\
           exit 0\n\
         fi\n\
         if [ \"$1\" = \"--help\" ]; then\n\
           for flag in --help --version --verbose --headless --path --quit --quit-after --script --check-only --editor --import --export-release --export-debug --export-pack --doctool --debug-server --main-pack --remote-debug; do\n\
             printf '  %s  fixture capability\\n' \"$flag\"\n\
           done\n\
           exit 0\n\
         fi\n\
         for argument in \"$@\"; do printf '%s\\n' \"$argument\" >> '{}'; done\n\
         printf '%s\\n' '--end--' >> '{}'\n\
         {body}\n",
        argv_log.display(),
        argv_log.display()
    );
    std::fs::write(&binary, script).expect("write fake Godot");
    let mut permissions = std::fs::metadata(&binary)
        .expect("fake Godot metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&binary, permissions).expect("make fake Godot executable");
    (binary, argv_log)
}
