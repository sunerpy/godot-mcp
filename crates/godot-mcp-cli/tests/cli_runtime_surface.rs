//! Every `godot-mcp` argument's advertised behaviour, asserted by **running the
//! built binary**.
//!
//! This suite exists because a source-text assertion is structurally incapable of
//! catching the defect it guards. The `--stdio` help text once documented
//! "`--stdio=false` exits 2", and the observed exit code was indeed 2 — but it came
//! from `clap`'s generic usage error, because `stdio: bool` with only
//! `default_value_t = true` derives `ArgAction::SetTrue`, which accepts no value at
//! all. The value never reached `serve`, so its `if !stdio` refusal branch was
//! unreachable production code and the documented mechanism never ran. The source
//! text and the runtime behaviour disagreed, and only executing the binary could
//! tell them apart.
//!
//! So every test here spawns the real executable and asserts on its observed exit
//! code and stderr, never on the shape of the source. Two classes are covered:
//!
//! * an argument whose documented mechanism must actually execute — bare `--stdio`
//!   still selects stdio (load-bearing: `docs/operator/quickstart-stdio.md` runs it
//!   verbatim and `qa/tasks/36.json` executes that page), while an explicit
//!   `--stdio=false` is *accepted by the parser* and then refused by `serve` itself,
//!   so the refusal is the program's own decision rather than a parse accident;
//! * a help string whose stated rule must match what the code really does — the
//!   `assets install` conflict rule, which refuses a foreign or user-edited file but
//!   not a path already holding this installer's own unmodified payload.

use std::io::Read as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::prelude::*;

/// The exact refusal `serve` prints when a transport other than stdio is asked
/// for. Pinned here so a reworded message fails this suite rather than silently
/// making the assertion vacuous.
const REFUSAL: &str = "godot-mcp serve: only --stdio is available in this build";

fn cli() -> Command {
    let mut command = Command::cargo_bin("godot-mcp").expect("godot-mcp binary");
    for (name, _) in std::env::vars().filter(|(name, _)| name.starts_with("GODOT_MCP_")) {
        command.env_remove(name);
    }
    command
}

/// A project path that certainly holds no `project.godot`, so a run that gets
/// *past* argument parsing and past the transport decision fails in startup with
/// its own distinct exit code (69) instead of 0 or 2. That separation is what
/// makes "the flag was accepted" and "the flag was refused" distinguishable.
fn no_project() -> std::path::PathBuf {
    std::env::temp_dir()
}

/// Both spellings of an explicit `false` must parse and then be refused by
/// `serve`, exiting 2 with the refusal on stderr.
///
/// This is the test that would have failed before the fix: with `ArgAction::SetTrue`
/// the parser rejected the value, stderr carried clap's "unexpected value" usage
/// error instead of this message, and `if !stdio` never ran.
#[test]
fn an_explicit_false_is_parsed_and_then_refused_by_serve() {
    for form in [
        vec!["serve".to_string(), "--stdio=false".to_string()],
        vec![
            "serve".to_string(),
            "--stdio".to_string(),
            "false".to_string(),
        ],
    ] {
        cli()
            .args(&form)
            .arg("--project")
            .arg(no_project())
            .assert()
            .code(2)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(REFUSAL))
            .stderr(predicate::str::contains("unexpected value").not())
            .stderr(predicate::str::contains("Usage:").not());
    }
}

/// Bare `--stdio` keeps meaning `true`: it must get past the transport decision,
/// which the distinct startup exit code 69 proves, and must never print the
/// refusal.
#[test]
fn a_bare_stdio_flag_still_selects_the_stdio_transport() {
    cli()
        .arg("serve")
        .arg("--stdio")
        .arg("--project")
        .arg(no_project())
        .assert()
        .code(69)
        .stderr(predicate::str::contains(REFUSAL).not());
}

/// An explicit `true`, and omitting the argument entirely, must both behave
/// exactly like the bare flag — so the optional value did not turn the default
/// into a required argument.
#[test]
fn an_explicit_true_and_an_omitted_flag_both_select_stdio() {
    for extra in [vec!["--stdio=true"], vec![]] {
        cli()
            .arg("serve")
            .args(&extra)
            .arg("--project")
            .arg(no_project())
            .assert()
            .code(69)
            .stderr(predicate::str::contains(REFUSAL).not());
    }
}

/// A value that is neither `true` nor `false` is a parse error, not a silent
/// coercion into one of them — otherwise `--stdio=no` would quietly select stdio.
#[test]
fn a_non_boolean_stdio_value_is_a_parse_error() {
    cli()
        .arg("serve")
        .arg("--stdio=maybe")
        .arg("--project")
        .arg(no_project())
        .assert()
        .code(2)
        .stderr(predicate::str::contains(REFUSAL).not())
        .stderr(predicate::str::contains("invalid value"));
}

/// `--help` must advertise the optional value, so the help text and the parser
/// agree on the argument's shape. `[<STDIO>]` is `clap`'s own rendering of an
/// optional value; its presence is proof the value is accepted, taken from the
/// binary's real output rather than from the source.
#[test]
fn the_serve_help_advertises_the_optional_stdio_value() {
    cli()
        .arg("serve")
        .arg("--help")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("--stdio [<STDIO>]"));
}

/// The shipped binary must expose the configuration-file selector that makes the
/// already-validated HTTP transport reachable. This is asserted from real help
/// output so a field added only to an internal options struct cannot satisfy it.
#[test]
fn the_serve_help_advertises_the_config_file_selector() {
    cli()
        .arg("serve")
        .arg("--help")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("--config <CONFIG>"));
}

/// A supplied stdio configuration must be parsed and routed to the same startup
/// path as omission. The missing project marker deliberately produces startup's
/// exit 69; clap's exit 2 would prove `--config` never reached `serve`.
#[test]
fn a_stdio_config_routes_to_the_stdio_runtime() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = temp.path().join("stdio.toml");
    std::fs::write(&config, "transport = \"stdio\"\n").expect("write stdio config");

    cli()
        .arg("serve")
        .arg("--config")
        .arg(config)
        .arg("--project")
        .arg(no_project())
        .assert()
        .code(69)
        .stderr(predicate::str::contains("unexpected argument '--config'").not());
}

/// A supplied HTTP configuration must reach the real listener binding path, not
/// merely pass `check-config`. `trusted_local` publishes its fresh per-launch
/// secret only after `ProjectRuntime` has opened and `bind_http_transport` has
/// successfully bound the loopback socket, so observing that file proves the
/// shipped executable crossed the missing runtime edge.
#[test]
fn an_http_config_routes_to_the_authenticated_http_runtime() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = empty_project(temp.path());
    let config = temp.path().join("http.toml");
    let probe = TcpListener::bind("127.0.0.1:0").expect("reserve a loopback port");
    let address = probe.local_addr().expect("reserved address");
    drop(probe);
    std::fs::write(
        &config,
        format!(
            r#"transport = "http"

[http]
bind = "{address}"
mode = "trusted_local"
host = "{address}"
origin = "http://{address}"

[http.local_secret.env]
var = "GODOT_MCP_TEST_LOCAL_SECRET"
"#
        ),
    )
    .expect("write HTTP config");

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_godot-mcp"));
    for (name, _) in std::env::vars().filter(|(name, _)| name.starts_with("GODOT_MCP_")) {
        command.env_remove(name);
    }
    command
        .arg("serve")
        .arg("--config")
        .arg(&config)
        .arg("--project")
        .arg(&project)
        .env(
            "GODOT_MCP_TEST_LOCAL_SECRET",
            "config-indirection-is-not-the-launch-secret",
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn configured HTTP server");
    let secret = project.join(".godot-mcp").join("http-launch-secret");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !secret.is_file() && Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("inspect child") {
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .expect("captured stderr")
                .read_to_string(&mut stderr)
                .expect("read stderr");
            panic!("configured HTTP server exited as {status} before binding: {stderr}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        secret.is_file(),
        "the configured HTTP runtime must publish its launch secret at {}",
        secret.display()
    );
    child.kill().expect("stop configured HTTP server");
    let _ = child.wait();
}

/// The four proof-driven repair operations are the whole `--operation` surface,
/// and `--force` is not an argument. Asserted through the binary because the
/// absence of a bypass is a security claim the docs make in the present tense.
#[test]
fn transaction_repair_rejects_force_and_an_unknown_operation() {
    cli()
        .args([
            "transaction",
            "repair",
            "--project",
            "/tmp",
            "--transaction-id",
            "x",
            "--operation",
            "compensate",
            "--proof-file",
            "/tmp/absent.json",
            "--force",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unexpected argument '--force'"));

    cli()
        .args([
            "transaction",
            "repair",
            "--project",
            "/tmp",
            "--transaction-id",
            "x",
            "--operation",
            "bogus",
            "--proof-file",
            "/tmp/absent.json",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "expected one of reconcile, compensate, terminate, acknowledge-safe-state",
        ));
}

/// Every profile `serve`'s help names is accepted by the parser, and anything else
/// is refused with the same list. An accepted profile proceeds to startup (69)
/// rather than being rejected here.
#[test]
fn every_profile_the_help_names_is_accepted_and_others_are_refused() {
    for profile in ["core", "editor", "runtime", "full"] {
        cli()
            .arg("serve")
            .arg("--project")
            .arg(no_project())
            .arg("--profile")
            .arg(profile)
            .assert()
            .code(69)
            .stderr(predicate::str::contains("unknown profile").not());
    }
    cli()
        .arg("serve")
        .arg("--project")
        .arg(no_project())
        .arg("--profile")
        .arg("bogus")
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "expected core, editor, runtime or full",
        ));
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root above crates/godot-mcp-cli")
        .to_path_buf()
}

/// Stage a real extracted release package from the binary this test run built, and
/// return its root. `assets` resolves its payload from the package it was launched
/// out of, so the packaged executable — not the bare target binary — is what these
/// tests must invoke.
fn staged_package(into: &Path) -> PathBuf {
    let target = godot_mcp_cli::package::ReleaseTarget::host().expect("this host is a target");
    let executable = std::fs::read(env!("CARGO_BIN_EXE_godot-mcp")).expect("read the built binary");
    let archive = godot_mcp_cli::package::build_archive(&godot_mcp_cli::package::PackageInputs {
        workspace_root: workspace_root(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        target,
        executable,
        supervisor: b"supervisor-test-binary".to_vec(),
        profile: "debug".to_owned(),
    })
    .expect("the host archive builds");
    // The host's own archive format: a zip for Windows, a tar.gz elsewhere.
    if archive.file_name.ends_with(".zip") {
        godot_mcp_cli::package::archive::extract_zip(&archive.bytes, into)
    } else {
        godot_mcp_cli::package::archive::extract_tar_gz(&archive.bytes, into)
    }
    .expect("the archive extracts");
    into.join(godot_mcp_cli::package::payload_root(
        env!("CARGO_PKG_VERSION"),
        target,
    ))
}

fn packaged_cli(package: &Path) -> Command {
    let target = godot_mcp_cli::package::ReleaseTarget::host().expect("this host is a target");
    let executable = package.join("bin").join(target.executable_name());
    let mut command = Command::new(executable);
    for (name, _) in std::env::vars().filter(|(name, _)| name.starts_with("GODOT_MCP_")) {
        command.env_remove(name);
    }
    command
}

fn empty_project(root: &Path) -> PathBuf {
    let project = root.join("project");
    std::fs::create_dir_all(&project).expect("create the project directory");
    std::fs::write(project.join("project.godot"), b"config_version=5\n").expect("write project");
    project
}

/// `assets install`'s help states the exact conflict rule, so all three of its
/// outcomes are asserted against the running packaged binary: a clean first
/// install succeeds, a repeat over this installer's own unmodified files is **not**
/// a conflict and succeeds, and an owned file the user edited refuses.
///
/// The middle case is the one the help used to get wrong by claiming it "refuses
/// any occupied path"; it exits 0.
#[test]
fn assets_install_refuses_only_a_foreign_or_edited_file_not_its_own_clean_payload() {
    let temp = tempfile::tempdir().expect("temp dir");
    let package = staged_package(temp.path());
    let project = empty_project(temp.path());

    packaged_cli(&package)
        .args(["assets", "install", "--project"])
        .arg(&project)
        .assert()
        .code(0);

    let installed = project
        .join("godot_mcp")
        .join("bridge")
        .join("bridge_codec.gd");
    assert!(
        installed.is_file(),
        "the payload installs under godot_mcp/, never an addons/ tree"
    );
    assert!(
        !project.join("addons").exists(),
        "no addons/ tree may be created: no editor plug-in ships"
    );

    packaged_cli(&package)
        .args(["assets", "install", "--project"])
        .arg(&project)
        .assert()
        .code(0);

    let mut edited = std::fs::read(&installed).expect("read the installed payload");
    edited.extend_from_slice(b"\n# a user edit\n");
    std::fs::write(&installed, &edited).expect("edit the owned file");

    packaged_cli(&package)
        .args(["assets", "install", "--project"])
        .arg(&project)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("refused before writing anything"));
}

/// A payload path held by a file no manifest records is foreign, and the install
/// refuses before writing a byte.
#[test]
fn assets_install_refuses_a_foreign_file_at_a_payload_path() {
    let temp = tempfile::tempdir().expect("temp dir");
    let package = staged_package(temp.path());
    let project = empty_project(temp.path());

    let occupied = project.join("godot_mcp").join("bridge");
    std::fs::create_dir_all(&occupied).expect("create the occupied directory");
    std::fs::write(occupied.join("bridge_codec.gd"), b"not ours\n").expect("write foreign");

    packaged_cli(&package)
        .args(["assets", "install", "--project"])
        .arg(&project)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not owned by this installer"));
}
