//! The real-editor driver the live suites share: which editors a run checks,
//! one launched editor with private user directories, and the MCP calls every
//! case starts with.
//!
//! A suite includes it with `#[path = "support/live.rs"] mod live;` next to
//! `mod support;`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::support::{ProjectTree, Spawn, StdioSession};

pub const EDITORS: &str = "GODOT_MCP_TEST_EDITORS";
pub const ARTIFACT: &str = "GODOT_MCP_LOCKED_ARTIFACT";

/// The editor binaries this run checks, with the scratch directory an
/// unpacked archive lives in.
pub fn editors() -> (Vec<PathBuf>, Option<tempfile::TempDir>) {
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

/// One running editor, killed when dropped.
pub struct Editor {
    child: Child,
    log: PathBuf,
    pub binary: String,
}

impl Editor {
    /// Launch `binary` as a headless editor on `project`, with private user
    /// directories under `home`.
    pub fn launch(binary: &Path, project: &Path, home: &Path) -> Self {
        Self::launch_with(binary, project, home, &["--headless", "--editor", "--path"])
    }

    /// Launch `binary` as an editor with a window on the display the test
    /// runs under, drawn by the OpenGL renderer.
    pub fn launch_windowed(binary: &Path, project: &Path, home: &Path) -> Self {
        Self::launch_with(
            binary,
            project,
            home,
            &["--editor", "--rendering-driver", "opengl3", "--path"],
        )
    }

    /// Launch `binary` with `arguments` on `project`, with private user
    /// directories under `home`. The editor finds no user session either: its
    /// runtime directory is a private one, so it reaches no audio server, and
    /// its session bus address names a socket nothing listens on. An editor
    /// that finds no address autolaunches a bus, whose document portal mounts
    /// a file system under `home` until the X server exits; one that finds the
    /// user's bus talks to the user's desktop session. Every host then runs
    /// the editor as one without a desktop session does.
    pub fn launch_with(binary: &Path, project: &Path, home: &Path, arguments: &[&str]) -> Self {
        for directory in ["config", "data", "cache", "runtime"] {
            std::fs::create_dir_all(home.join(directory)).expect("private user directory");
        }
        let runtime = home.join("runtime");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
                .expect("a private runtime directory");
        }
        let log = home.join("editor.log");
        let output = std::fs::File::create(&log).expect("editor log");
        let child = Command::new(binary)
            .args(arguments)
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

    pub fn output(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// The editor's process id.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Wait for a run that ends by itself (`--script`) to end, at most `limit`.
    pub fn wait(&mut self, limit: Duration) -> std::process::ExitStatus {
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("wait for Godot") {
                return status;
            }
            assert!(
                started.elapsed() < limit,
                "{}: the run did not end within {limit:?}\n{}",
                self.binary,
                self.output()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Where the editor's output ends now.
    pub fn mark(&self) -> usize {
        self.output().len()
    }

    /// Fail when the editor printed an engine or script error after `mark`:
    /// serving a request must not raise one, and every error also lands in
    /// the user's Output panel. An error the headless dummy renderer raises
    /// is not counted: Godot 4.4 asks it for a texture while saving a scene's
    /// thumbnail, which a windowed editor's renderer has. Nor are two errors
    /// of a game the editor runs, which shares the editor's output: an X
    /// `BadWindow` error, which a game whose window is embedded in the
    /// editor's (Godot 4.4.1 starts it with `--wid`) reports under Xvfb, and
    /// the ALSA driver's failure to open a sound device on a host without one,
    /// after which the game falls back to the dummy audio driver.
    pub fn assert_no_errors_since(&self, mark: usize, during: &str) {
        let output = self.output();
        let lines: Vec<&str> = output.get(mark..).unwrap_or_default().lines().collect();
        let errors: Vec<&str> = lines
            .iter()
            .enumerate()
            .filter(|(index, line)| {
                line.contains("ERROR:")
                    && !line.contains("Unhandled XServer error: BadWindow")
                    && !lines.get(index + 1).is_some_and(|at| {
                        at.contains("servers/rendering/dummy/") || at.contains("drivers/alsa/")
                    })
            })
            .map(|(_, line)| *line)
            .collect();
        assert!(
            errors.is_empty(),
            "{}: {during} printed {} engine errors, the first: {:?}\n{}",
            self.binary,
            errors.len(),
            errors.iter().take(3).collect::<Vec<_>>(),
            output.get(mark..).unwrap_or_default()
        );
    }

    /// What a failure names: the build and everything the editor printed.
    pub fn context(&self) -> String {
        format!("{}\n{}", self.binary, self.output())
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn open(tree: &ProjectTree) -> StdioSession {
    StdioSession::initialized(&Spawn {
        project: &tree.root,
        current_dir: None,
        profile: "full",
        godot: None,
    })
}

pub fn ok(response: &Value, context: &str) -> Value {
    assert!(
        response.get("error").is_none() && response["result"]["isError"] != json!(true),
        "the call must succeed: {response}\n{context}"
    );
    response["result"]["structuredContent"].clone()
}

/// The next request id of one session.
pub struct Ids(pub u64);

impl Ids {
    pub fn next(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

/// Wait until the add-on in `editor` has connected, at most 90 seconds.
pub fn wait_connected(session: &mut StdioSession, ids: &mut Ids, editor: &Editor) -> Value {
    let started = Instant::now();
    loop {
        let status = ok(
            &session.call_tool(ids.next(), "editor.status.get", json!({})),
            &editor.context(),
        );
        if status["connected"] == json!(true) {
            return status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(90),
            "{}: the add-on never connected: {status}\n{}",
            editor.binary,
            editor.output()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// A project whose only editor plugin is godot_mcp, installed through the
/// file path before any editor runs.
pub fn project_with_addon(label: &str, files: &[(&str, &str)]) -> (ProjectTree, StdioSession, Ids) {
    let tree = ProjectTree::new(label);
    tree.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Live\"\n",
    );
    for (path, contents) in files {
        tree.write(path, contents);
    }
    let mut session = open(&tree);
    let mut ids = Ids(1);
    let installed = session.call_tool(
        ids.next(),
        "editor.addon.install",
        json!({ "enable": true, "idempotencyId": "live-install" }),
    );
    assert!(installed.get("error").is_none(), "{installed}");
    (tree, session, ids)
}

/// The semantic digest of the scene the editor is editing.
pub fn digest_of(session: &mut StdioSession, ids: &mut Ids, context: &str) -> String {
    ok(
        &session.call_tool(ids.next(), "editor.scene.tree.get", json!({})),
        context,
    )["digest"]
        .as_str()
        .expect("digest")
        .to_owned()
}

/// The bytes of one `<property> = PackedByteArray(...)` line of a text scene,
/// in either the base64 or the comma-separated spelling.
pub fn packed_bytes(line: &str) -> Option<Vec<u8>> {
    let inner = line.split_once("PackedByteArray(")?.1.strip_suffix(')')?;
    if let Some(encoded) = inner
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        return Some(decode_base64(encoded));
    }
    inner
        .split(',')
        .map(|byte| byte.trim().parse().ok())
        .collect()
}

pub fn decode_base64(text: &str) -> Vec<u8> {
    let value = |byte: u8| -> u32 {
        u32::from(match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => panic!("not base64: {byte}"),
        })
    };
    let mut out = Vec::new();
    for quad in text.as_bytes().chunks(4) {
        let padding = quad.iter().rev().take_while(|byte| **byte == b'=').count();
        let mut word = 0_u32;
        for (position, byte) in quad.iter().enumerate() {
            word = (word << 6)
                | if position >= 4 - padding {
                    0
                } else {
                    value(*byte)
                };
        }
        out.extend_from_slice(&[(word >> 16) as u8, (word >> 8) as u8, word as u8][..3 - padding]);
    }
    out
}
