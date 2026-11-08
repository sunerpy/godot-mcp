//! An editor mutation whose editor exited before it answered, against real
//! Godot editors (`#[ignore]`d; `GODOT_MCP_TEST_EDITORS` or
//! `GODOT_MCP_LOCKED_ARTIFACT` names them).
//!
//! `editor.script.run` outlasts its deadline, so the request is unanswered
//! and the project is quarantined. While the editor still runs,
//! `godot-mcp transaction repair` refuses to settle it. Once the editor has
//! been killed, `reconcile` reports and `acknowledge-safe-state` commits the
//! project's files as they are; the next server opens its write gate, and a
//! replay of the request still answers `uncertain`.
//!
//! The server is the `godot-mcp` binary itself, driven over stdio, and the
//! editor is headless with private user directories.

#![cfg(all(feature = "test-hooks", any(target_os = "linux", target_os = "macos")))]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use godot_mcp_server::test_support::Task34FixtureJournal;
use serde_json::{Value, json};

const EDITORS: &str = "GODOT_MCP_TEST_EDITORS";
const ARTIFACT: &str = "GODOT_MCP_LOCKED_ARTIFACT";

/// How long one MCP answer may take.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);

/// The editor binaries this run checks, with the scratch directory an
/// unpacked archive lives in.
fn editors() -> (Vec<PathBuf>, Option<tempfile::TempDir>) {
    if let Ok(listed) = std::env::var(EDITORS) {
        let binaries: Vec<PathBuf> = listed
            .split(',')
            .filter(|binary| !binary.is_empty())
            .map(PathBuf::from)
            .collect();
        assert!(!binaries.is_empty(), "{EDITORS} lists no editor");
        return (binaries, None);
    }
    let archive = PathBuf::from(
        std::env::var_os(ARTIFACT)
            .unwrap_or_else(|| panic!("set {EDITORS} or {ARTIFACT} to run the live-editor suite")),
    );
    let name = archive
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".zip"))
        .unwrap_or_else(|| panic!("{} is not a zip archive", archive.display()))
        .to_owned();
    let work = tempfile::tempdir().expect("scratch directory for the editor");
    let status = Command::new("unzip")
        .arg("-q")
        .arg("-o")
        .arg(&archive)
        .arg("-d")
        .arg(work.path())
        .status()
        .expect("run unzip");
    assert!(
        status.success(),
        "unzip {} failed: {status}",
        archive.display()
    );
    let binary = work.path().join(&name);
    assert!(
        binary.is_file(),
        "{} does not unpack to {name}",
        archive.display()
    );
    (vec![binary], Some(work))
}

/// One headless editor with private user directories, killed when dropped.
struct Editor {
    child: Child,
    log: PathBuf,
    binary: String,
}

impl Editor {
    /// The editor finds no user session: its runtime directory is a private
    /// one and its session bus address names a socket nothing listens on, so
    /// it reaches neither the user's audio server nor the user's desktop.
    fn launch(binary: &Path, project: &Path, home: &Path) -> Self {
        for directory in ["config", "data", "cache", "runtime"] {
            std::fs::create_dir_all(home.join(directory)).expect("private user directory");
        }
        let runtime = home.join("runtime");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
                .expect("a private runtime directory");
        }
        let log = home.join("editor.log");
        let output = std::fs::File::create(&log).expect("editor log");
        let child = Command::new(binary)
            .args(["--headless", "--editor", "--path"])
            .arg(project)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("XDG_CACHE_HOME", home.join("cache"))
            .env("XDG_RUNTIME_DIR", &runtime)
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", runtime.join("no-session-bus").display()),
            )
            .stdin(Stdio::null())
            .stdout(output.try_clone().expect("editor log"))
            .stderr(output)
            .spawn()
            .unwrap_or_else(|error| panic!("launch {}: {error}", binary.display()));
        Self {
            child,
            log,
            binary: binary.display().to_string(),
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn context(&self) -> String {
        format!(
            "{}\n{}",
            self.binary,
            std::fs::read_to_string(&self.log).unwrap_or_default()
        )
    }

    /// Kill the editor, as a crash ends it, and reap it.
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        self.kill();
    }
}

/// One `godot-mcp serve` over stdio, with scripts allowed.
struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    stdout_drain: Option<JoinHandle<()>>,
    stderr: Arc<Mutex<String>>,
    stderr_drain: Option<JoinHandle<()>>,
    next_id: u64,
}

impl Server {
    fn start(project: &Path) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_godot-mcp"));
        for (name, _) in std::env::vars().filter(|(name, _)| name.starts_with("GODOT_MCP_")) {
            command.env_remove(name);
        }
        let mut child = command
            .args(["serve", "--project"])
            .arg(project)
            .args(["--profile", "full", "--allow-scripts"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start godot-mcp serve");
        let stdout = child.stdout.take().expect("piped stdout");
        let (sender, lines) = channel();
        let stdout_drain = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let mut pipe = child.stderr.take().expect("piped stderr");
        let sink = Arc::clone(&stderr);
        let stderr_drain = std::thread::spawn(move || {
            use std::io::Read as _;
            let mut buffer = [0_u8; 8192];
            while let Ok(read) = pipe.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                sink.lock()
                    .expect("stderr buffer")
                    .push_str(&String::from_utf8_lossy(&buffer[..read]));
            }
        });
        let mut server = Self {
            stdin: child.stdin.take(),
            child,
            lines,
            stdout_drain: Some(stdout_drain),
            stderr,
            stderr_drain: Some(stderr_drain),
            next_id: 0,
        };
        let initialized = server.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "live-editor-exit", "version": "0.1.0" }
            }),
        );
        assert!(initialized.get("result").is_some(), "{initialized}");
        server.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        server
    }

    fn send(&mut self, frame: &Value) {
        let stdin = self.stdin.as_mut().expect("the server's stdin is open");
        let mut line = serde_json::to_string(frame).expect("frame");
        line.push('\n');
        stdin.write_all(line.as_bytes()).expect("write a frame");
        stdin.flush().expect("flush a frame");
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let started = Instant::now();
        loop {
            let remaining = ANSWER_TIMEOUT.saturating_sub(started.elapsed());
            let line = match self.lines.recv_timeout(remaining) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => {
                    panic!(
                        "no answer to {method} within {ANSWER_TIMEOUT:?}: {}",
                        self.stderr()
                    )
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!(
                        "the server ended before answering {method}: {}",
                        self.stderr()
                    )
                }
            };
            let frame: Value = serde_json::from_str(&line).unwrap_or_else(|error| {
                panic!("stdout carries JSON frames only: {line:?} ({error})")
            });
            if frame["id"] == json!(id) {
                return frame;
            }
        }
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.request(
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
        )
    }

    fn stderr(&self) -> String {
        self.stderr.lock().expect("stderr buffer").clone()
    }

    /// Close the server's stdin, which ends the session, and wait for it to
    /// stop; answer what it wrote to stderr.
    fn finish(mut self) -> String {
        drop(self.stdin.take());
        let started = Instant::now();
        while self
            .child
            .try_wait()
            .expect("the server's status")
            .is_none()
        {
            if started.elapsed() > ANSWER_TIMEOUT {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("the server did not stop: {}", self.stderr());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        for drain in [self.stdout_drain.take(), self.stderr_drain.take()]
            .into_iter()
            .flatten()
        {
            let _ = drain.join();
        }
        self.stderr()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn ok(response: &Value, context: &str) -> Value {
    assert!(
        response.get("error").is_none() && response["result"]["isError"] != json!(true),
        "the call must succeed: {response}\n{context}"
    );
    response["result"]["structuredContent"].clone()
}

fn error_code(response: &Value) -> Value {
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

/// Wait until the add-on in `editor` has connected, at most 90 seconds.
fn wait_connected(server: &mut Server, editor: &Editor) {
    let started = Instant::now();
    loop {
        let status = ok(
            &server.call("editor.status.get", json!({})),
            &editor.context(),
        );
        if status["connected"] == json!(true) {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(90),
            "the add-on never connected: {status}\n{}",
            editor.context()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Run `godot-mcp transaction repair` and answer its exit status, stdout and
/// stderr.
fn repair(
    project: &Path,
    transaction_id: &str,
    operation: &str,
    proof: &Path,
) -> (bool, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_godot-mcp"));
    for (name, _) in std::env::vars().filter(|(name, _)| name.starts_with("GODOT_MCP_")) {
        command.env_remove(name);
    }
    let output = command
        .args(["transaction", "repair", "--project"])
        .arg(project)
        .args(["--transaction-id", transaction_id, "--operation", operation])
        .arg("--proof-file")
        .arg(proof)
        .output()
        .expect("run transaction repair");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn report_value<'a>(report: &'a str, key: &str) -> &'a str {
    report
        .lines()
        .find_map(|line| {
            let (candidate, value) = line.split_once(' ')?;
            (candidate == key).then_some(value)
        })
        .unwrap_or_else(|| panic!("the report has no {key:?} field: {report}"))
}

/// The source of a run that outlasts any deadline this case sets.
const OUTLASTING: &str =
    "extends Node\n\nfunc run():\n\tawait get_tree().create_timer(600.0).timeout\n\treturn 1\n";

#[test]
#[ignore = "needs real Godot editors: GODOT_MCP_TEST_EDITORS or GODOT_MCP_LOCKED_ARTIFACT"]
fn an_editor_that_exits_before_it_answers_releases_its_quarantine_through_repair() {
    let (binaries, _unpacked) = editors();
    for binary in &binaries {
        let started = Instant::now();
        an_editor_that_exits_before_it_answers_releases_its_quarantine_through_repair_in(binary);
        eprintln!(
            "live-editor-exit: {} passed in {:.1} s",
            binary.display(),
            started.elapsed().as_secs_f64()
        );
    }
}

fn an_editor_that_exits_before_it_answers_releases_its_quarantine_through_repair_in(binary: &Path) {
    let scratch = tempfile::Builder::new()
        .prefix("godot-mcp-live-editor-exit-")
        .tempdir()
        .expect("scratch directory");
    let base = scratch.path().canonicalize().expect("canonical scratch");
    let project = base.join("game");
    std::fs::create_dir_all(&project).expect("project directory");
    std::fs::write(
        project.join("project.godot"),
        "config_version=5\n\n[application]\n\nconfig/name=\"Exit\"\n",
    )
    .expect("project.godot");

    let mut server = Server::start(&project);
    let installed = server.call(
        "editor.addon.install",
        json!({ "enable": true, "idempotencyId": "live-install" }),
    );
    assert!(installed.get("error").is_none(), "{installed}");
    let identity = ok(
        &server.call("project.info.get", json!({})),
        "project.info.get",
    )["project"]
        .clone();
    let mut editor = Editor::launch(binary, &project, &base.join("home"));
    wait_connected(&mut server, &editor);

    // The run outlasts its deadline: the request reached the editor and was
    // never answered.
    let arguments = json!({
        "source": OUTLASTING,
        "deadline_ms": 1000,
        "idempotencyId": "outlasting",
    });
    let unanswered = server.call("editor.script.run", arguments.clone());
    let report = format!("{unanswered}\n{}", editor.context());
    assert_eq!(error_code(&unanswered), json!("uncertain"), "{report}");
    let blocked = server.call(
        "file.create",
        json!({ "path": "before.txt", "content": "x\n", "idempotencyId": "before" }),
    );
    assert!(
        blocked.get("error").is_some() || blocked["result"]["isError"] == json!(true),
        "a write after the unanswered run is refused: {blocked}"
    );
    let stderr = server.finish();

    // The command takes the project's lease, so the server is stopped first.
    let journal = Task34FixtureJournal::open(&project).expect("the project's journal");
    let quarantined = journal.quarantined().expect("the quarantine list");
    assert_eq!(quarantined.len(), 1, "{quarantined:?}\n{stderr}");
    let transaction_id = quarantined[0].clone();
    let record = journal.record_json(&transaction_id).expect("the record");
    drop(journal);
    assert_eq!(record["tool_name"], json!("editor.script.run"), "{record}");
    assert_eq!(
        record["proof"]["editor_process"]["pid"],
        json!(editor.pid()),
        "the record names the editor process the run was sent to: {record}\n{}",
        editor.context()
    );
    assert!(
        blocked.to_string().contains(&transaction_id),
        "the refused write names the quarantined transaction: {blocked}"
    );

    let proof = base.join("proof.json");
    std::fs::write(
        &proof,
        serde_json::to_vec_pretty(&json!({
            "effect_kind": "editor_runtime",
            "transaction_id": transaction_id,
            "project_object_identity": identity["object_id"],
            "project_generation": identity["generation"],
            "bridge_reachable": false,
            "claimed_disposition": "writer-ended",
        }))
        .expect("proof json"),
    )
    .expect("write the proof file");

    // While the editor runs, nothing is settled.
    let still_running = format!(
        "the editor that ran this request is still running (pid {}): wait for it to answer, \
         or close it",
        editor.pid()
    );
    let (succeeded, stdout, stderr) =
        repair(&project, &transaction_id, "acknowledge-safe-state", &proof);
    assert!(!succeeded, "{stdout}");
    assert!(stderr.contains(&still_running), "{stderr}");
    let (succeeded, stdout, stderr) = repair(&project, &transaction_id, "reconcile", &proof);
    assert!(succeeded, "{stderr}");
    assert_eq!(
        report_value(&stdout, "proven-disposition"),
        "none",
        "{stdout}"
    );
    assert!(
        report_value(&stdout, "detail").contains(&still_running),
        "{stdout}"
    );

    // The editor ends, as a crash ends it.
    editor.kill();
    let (succeeded, stdout, stderr) = repair(&project, &transaction_id, "reconcile", &proof);
    assert!(succeeded, "{stderr}");
    assert_eq!(
        report_value(&stdout, "proven-disposition"),
        "writer-ended",
        "{stdout}"
    );
    assert_eq!(report_value(&stdout, "transition"), "none", "{stdout}");
    let (succeeded, stdout, stderr) =
        repair(&project, &transaction_id, "acknowledge-safe-state", &proof);
    assert!(succeeded, "{stderr}");
    assert_eq!(
        report_value(&stdout, "phase-after"),
        "committed_fsynced",
        "{stdout}"
    );
    assert_eq!(report_value(&stdout, "transition"), "written", "{stdout}");

    // The next server serves writes again.
    let mut server = Server::start(&project);
    let written = server.call(
        "file.create",
        json!({ "path": "after.txt", "content": "x\n", "idempotencyId": "after" }),
    );
    ok(&written, &server.stderr());
    assert_eq!(
        std::fs::read_to_string(project.join("after.txt")).expect("after.txt"),
        "x\n"
    );

    // An editor mutation is answered only while an editor is connected, so a
    // new editor takes the place of the one that ended. The request still
    // answers that its result is unknown, and is not sent again.
    let editor = Editor::launch(binary, &project, &base.join("home-again"));
    wait_connected(&mut server, &editor);
    let replayed = server.call("editor.script.run", arguments);
    let report = format!("{replayed}\n{}", editor.context());
    assert_eq!(error_code(&replayed), json!("uncertain"), "{report}");
    assert!(
        replayed.to_string().contains(&transaction_id),
        "the replay names the settled transaction: {report}"
    );
    let stderr = server.finish();
    assert!(!stderr.contains("write gate closed"), "{stderr}");
    drop(editor);
}
