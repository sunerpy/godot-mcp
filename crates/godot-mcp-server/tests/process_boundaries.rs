//! M7 acceptance: the process boundary the `[process]` configuration selects
//! is the one every launch gets, over the real stdio assembly.
//!
//! The production server runs out of process with the non-shipping
//! `qa-fake-game` stand-in as its Godot executable. An untrusted project with
//! no strong boundary must start nothing and say why. A configured container
//! adapter — the protocol fixture the engine's own suite uses — must contain
//! an untrusted project's launch, and the reference namespace adapter must keep
//! a launch that tries to move itself out inside. A server started inside
//! a delegated cgroup v2 sub-tree (`systemd-run --user --scope -p
//! Delegate=yes`, which the delegated-cgroup case probes for) must contain a
//! one-shot run and a managed session in their own launch cgroups, and the
//! next server — started in another delegated scope — must terminate what a
//! killed one left running there.
#![cfg(target_os = "linux")]

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::{ProjectTree, StdioSession, error_code};

/// Set to `1` to make the delegated-cgroup case fail instead of reporting
/// that it did not run when this host cannot delegate a cgroup. It carries no
/// `GODOT_MCP_` prefix: a server loading a configuration refuses every
/// variable of that prefix it does not know.
const REQUIRE_DELEGATION: &str = "QA_REQUIRE_DELEGATED_CGROUP";

fn fake_game() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_qa-fake-game"))
}

fn config(tree: &ProjectTree, text: &str) -> PathBuf {
    let path = tree.base.join("serve.toml");
    std::fs::write(&path, text).expect("configuration");
    path
}

fn server(tree: &ProjectTree, config: &Path, delegated: bool) -> StdioSession {
    let qa = env!("CARGO_BIN_EXE_qa-journal-foundation");
    let mut command = if delegated {
        let mut command = Command::new("systemd-run");
        command.args([
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            "-p",
            "Delegate=yes",
            "--",
            qa,
        ]);
        command
    } else {
        Command::new(qa)
    };
    command
        .arg("--project")
        .arg(&tree.root)
        .arg("--profile")
        .arg("full")
        .arg("--godot-binary")
        .arg(fake_game())
        .arg("--config")
        .arg(config)
        .env_remove(REQUIRE_DELEGATION)
        .env_remove("GODOT_MCP_TEST_LAUNCH_CRASH_STAGE");
    StdioSession::handshake(StdioSession::start_command(command)).0
}

fn structured(response: &Value) -> Value {
    assert!(
        response.get("error").is_none() && response["result"]["isError"] != json!(true),
        "the call must succeed: {response}"
    );
    response["result"]["structuredContent"].clone()
}

fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.split_whitespace()
            .nth(2)
            .is_none_or(|state| state != "Z")
    })
}

fn game_pid(tree: &ProjectTree) -> u32 {
    let path = tree.root.join("fake-game.pid");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(pid) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "the game never recorded its pid");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The cgroup v2 directory `pid` belongs to.
fn cgroup_of(pid: u32) -> PathBuf {
    let membership = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).expect("membership");
    let relative = membership
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .expect("a cgroup v2 membership");
    Path::new("/sys/fs/cgroup").join(relative.trim().trim_start_matches('/'))
}

/// Whether this host can start a process in a delegated user scope.
fn delegation_available() -> bool {
    Command::new("systemd-run")
        .args([
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            "-p",
            "Delegate=yes",
            "--",
            "true",
        ])
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn an_untrusted_project_without_a_strong_boundary_starts_nothing() {
    let tree = ProjectTree::new("m7-untrusted");
    tree.write("fake-game.conf", "exit:0\n");
    let config = config(&tree, "[process]\ntrust = \"untrusted\"\n");
    let mut session = server(&tree, &config, false);

    let run = session.call_tool(
        2,
        "project.run",
        json!({ "headless": true, "deadline_ms": 30_000 }),
    );
    assert_eq!(error_code(&run), json!("unavailable"), "{run}");
    assert!(run.to_string().contains("strong boundary"), "{run}");
    let start = session.call_tool(
        3,
        "project.run.start",
        json!({ "idempotencyId": "m7-untrusted", "display": "headless" }),
    );
    assert_eq!(error_code(&start), json!("unavailable"), "{start}");
    assert!(
        !tree.root.join("fake-game.pid").exists(),
        "nothing was launched"
    );

    let health = structured(&session.call_tool(4, "health.get", json!({})));
    let godot = health["components"]
        .as_array()
        .and_then(|components| {
            components
                .iter()
                .find(|component| component["component"] == json!("godot"))
        })
        .expect("a godot component")
        .clone();
    assert_ne!(godot["status"], json!("ready"), "{health}");
    assert!(
        godot["message"]
            .as_str()
            .is_some_and(|message| message.contains("strong boundary")),
        "the godot component says why: {health}"
    );
    let _ = session.finish();
}

#[test]
fn a_delegated_cgroup_contains_every_launch_and_what_a_killed_server_left() {
    if !delegation_available() {
        assert!(
            std::env::var_os(REQUIRE_DELEGATION).is_none(),
            "{REQUIRE_DELEGATION} is set but this host cannot start a delegated user scope"
        );
        eprintln!(
            "this host cannot start a delegated user scope; the delegated-cgroup check did not run"
        );
        return;
    }
    let tree = ProjectTree::new("m7-cgroup");
    // A delegated cgroup is weak containment, for trusted projects only; the
    // boundary kind below proves the launch did not fall back to the locator.
    let config = config(&tree, "[process]\nboundary = \"cgroup\"\n");
    tree.write("fake-game.conf", "exit:0\n");
    let mut first = server(&tree, &config, true);

    let run = structured(&first.call_tool(
        2,
        "project.run",
        json!({ "headless": true, "deadline_ms": 30_000 }),
    ));
    assert_eq!(
        run["execution"]["boundary"]["kind"],
        json!("cgroup_v2"),
        "{run}"
    );
    assert_eq!(run["execution"]["boundary"]["strength"], json!("weak"));
    assert_eq!(
        run["execution"]["boundary"]["membership"],
        json!("confirmed")
    );
    assert_eq!(
        run["execution"]["boundary"]["empty_after_exit"],
        json!(true)
    );
    std::fs::remove_file(tree.root.join("fake-game.pid")).expect("first run's pid");

    // A session that will not stop by itself, so a killed server leaves it.
    tree.write("fake-game.conf", "stubborn\n");
    let started = structured(&first.call_tool(
        3,
        "project.run.start",
        json!({ "idempotencyId": "m7-session", "display": "headless" }),
    ));
    assert_eq!(
        started["execution"]["boundary_kind"],
        json!("cgroup_v2"),
        "{started}"
    );
    assert_eq!(started["execution"]["boundary_strength"], json!("weak"));
    let session = started["session"]["session"]
        .as_str()
        .expect("session id")
        .to_owned();
    let pid = game_pid(&tree);
    let launch_cgroup = cgroup_of(pid);
    assert!(
        launch_cgroup
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "godot-mcp-launches"),
        "the game runs in its own launch cgroup: {}",
        launch_cgroup.display()
    );
    assert!(
        std::fs::read_to_string(launch_cgroup.join("pids.max"))
            .is_ok_and(|limit| limit.trim() != "max"),
        "the launch cgroup is limited"
    );
    first.kill();
    assert!(alive(pid), "the stubborn game outlives its killed server");

    let mut second = server(&tree, &config, true);
    let status =
        structured(&second.call_tool(2, "project.run.status", json!({ "session": session })));
    assert_eq!(status["state"], json!("reconciled"), "{status}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(Instant::now() < deadline, "the survivor was not terminated");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !launch_cgroup.exists(),
        "the survivor's launch cgroup was removed: {}",
        launch_cgroup.display()
    );
    let _ = second.finish();
}

/// A delegated cgroup the namespace adapter can make boundaries in: a user scope
/// whose own process waits in a leaf beside them, so the scope itself holds no
/// process and its sub-tree can use controllers.
struct AdapterRoot {
    holder: std::process::Child,
    root: PathBuf,
}

impl AdapterRoot {
    fn start() -> Self {
        let mut holder = Command::new("systemd-run")
            .args([
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "-p",
                "Delegate=yes",
                "--",
                "sleep",
                "600",
            ])
            .spawn()
            .expect("a delegated scope starts");
        let pid = holder.id();
        let deadline = Instant::now() + Duration::from_secs(10);
        let root = loop {
            let cgroup = cgroup_of(pid);
            // systemd-run execs the program itself once the scope exists.
            let running = std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|argv| {
                argv.split(|byte| *byte == 0)
                    .next()
                    .is_some_and(|program| program.ends_with(b"sleep"))
            });
            if running
                && cgroup
                    .extension()
                    .is_some_and(|extension| extension == "scope")
            {
                break cgroup;
            }
            if Instant::now() >= deadline {
                let _ = holder.kill();
                panic!("the holder never ran in its own scope");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let leaf = root.join("holder");
        std::fs::create_dir(&leaf).expect("the holder's leaf");
        std::fs::write(leaf.join("cgroup.procs"), pid.to_string()).expect("the holder moves");
        Self { holder, root }
    }
}

impl Drop for AdapterRoot {
    fn drop(&mut self) {
        let _ = self.holder.kill();
        let _ = self.holder.wait();
    }
}

/// The reference adapter: every launch of an untrusted project runs in its own
/// cgroup and in bubblewrap's namespaces, and a member that tries to move
/// itself into another cgroup of the delegated sub-tree cannot even see one.
#[test]
fn the_reference_adapter_keeps_an_untrusted_projects_launch_inside() {
    let bwrap = Path::new("/usr/bin/bwrap");
    if !delegation_available() || !bwrap.exists() {
        assert!(
            std::env::var_os(REQUIRE_DELEGATION).is_none(),
            "{REQUIRE_DELEGATION} is set but this host cannot start a delegated scope or has no \
             bubblewrap"
        );
        eprintln!(
            "no delegated scope or no bubblewrap here; the namespace adapter check did not run"
        );
        return;
    }
    let tree = ProjectTree::new("m7-namespaces");
    let adapter_root = AdapterRoot::start();
    // Where a breakaway would go: the holder's leaf, beside the boundaries.
    let escape = adapter_root.root.join("holder").join("cgroup.procs");
    tree.write("fake-game.conf", "escape\n");
    tree.write("fake-game.escape", &escape.display().to_string());
    let adapter = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/adapters/linux-namespace-adapter.sh")
        .canonicalize()
        .expect("the reference adapter");
    let config = config(
        &tree,
        &format!(
            "[process]\nboundary = \"adapter\"\ntrust = \"untrusted\"\n\n[process.adapter]\n\
             program = \"{}\"\nroot = \"{}\"\n",
            adapter.display(),
            adapter_root.root.display()
        ),
    );
    // cgroup v2 moves a process between cgroups only inside the user's own
    // delegated tree, so the server runs in a user scope of its own, as an
    // operator would start it.
    let mut session = server(&tree, &config, true);
    let run = structured(&session.call_tool(
        2,
        "project.run",
        json!({ "headless": true, "deadline_ms": 30_000 }),
    ));
    assert_eq!(
        run["execution"]["boundary"]["kind"],
        json!("container_adapter"),
        "{run}"
    );
    assert_eq!(run["execution"]["boundary"]["strength"], json!("strong"));
    assert_eq!(
        run["execution"]["boundary"]["membership"],
        json!("confirmed")
    );
    assert_eq!(
        run["execution"]["boundary"]["empty_after_exit"],
        json!(true)
    );
    let report = std::fs::read_to_string(tree.root.join("fake-game.escape-report"))
        .expect("the game reported its escape attempt");
    assert_eq!(report.trim(), "moved=false cgroupfs_entries=0", "{report}");
    assert_eq!(
        std::fs::read_to_string(&escape)
            .expect("the holder's leaf")
            .trim(),
        adapter_root.holder.id().to_string(),
        "nothing joined the cgroup beside the boundary"
    );
    let _ = session.finish();
}

#[test]
fn a_configured_adapter_contains_an_untrusted_projects_launch() {
    let tree = ProjectTree::new("m7-adapter");
    tree.write("fake-game.conf", "exit:0\n");
    let adapter = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/launch/container-adapter.sh")
        .canonicalize()
        .expect("the reference adapter");
    let root = tree.base.join("boundaries");
    std::fs::create_dir_all(&root).expect("boundary root");
    let config = config(
        &tree,
        &format!(
            "[process]\nboundary = \"adapter\"\ntrust = \"untrusted\"\n\n[process.adapter]\n\
             program = \"{}\"\nroot = \"{}\"\n",
            adapter.display(),
            root.display()
        ),
    );
    let mut session = server(&tree, &config, false);
    let run = structured(&session.call_tool(
        2,
        "project.run",
        json!({ "headless": true, "deadline_ms": 30_000 }),
    ));
    assert_eq!(
        run["execution"]["boundary"]["kind"],
        json!("container_adapter"),
        "{run}"
    );
    assert_eq!(run["execution"]["boundary"]["strength"], json!("strong"));
    assert_eq!(
        run["execution"]["boundary"]["membership"],
        json!("confirmed")
    );
    assert_eq!(
        run["execution"]["boundary"]["empty_after_exit"],
        json!(true)
    );
    let health = structured(&session.call_tool(3, "health.get", json!({})));
    let godot = health["components"]
        .as_array()
        .and_then(|components| {
            components
                .iter()
                .find(|component| component["component"] == json!("godot"))
        })
        .expect("a godot component")
        .clone();
    assert_eq!(godot["status"], json!("ready"), "{health}");
    assert!(
        godot["message"]
            .as_str()
            .is_some_and(|message| message.contains("container_adapter (strong)")),
        "the godot component names the boundary: {health}"
    );
    let _ = session.finish();
}
