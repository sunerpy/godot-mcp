//! Todo 36 acceptance: the documentation checker, the security lint and the
//! executable quickstart.
//!
//! Two tests, matching `qa/tasks/36.json` exactly.
//!
//! `docs_link_check_happy_path` proves the committed documentation tree passes
//! every rule and that a scripted **clean temporary environment** can follow the
//! quickstart's install / configure / stdio-smoke steps **without interpretation**:
//! each step is executed exactly as the page prints it, in order, with only the
//! variables the page itself declares.
//!
//! `docs_link_check_failure_paths` proves the production code path rejects each
//! failure case. The pristine copy is asserted clean first, so every finding is
//! caused by the injection under test rather than by pre-existing content.
//!
//! POSIX-only: the quickstart's steps are `sh` commands. The QA gate runs on
//! Linux; on a non-POSIX host this file compiles to no test rather than to a
//! fabricated pass.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

use godot_mcp_cli::docs::{self, checker, generated, script, security};

const QUICKSTART: &str = "docs/operator/quickstart-stdio.md";
const SMOKE_BLOCK: &str = "stdio-smoke";

fn workspace_root() -> PathBuf {
    godot_mcp_cli::audit::default_workspace_root()
}

/// Build a temporary workspace whose `docs/` tree is a real writable copy and
/// whose every other top-level entry is a symlink to the committed one, so link
/// resolution and every canonical source behave exactly as in the repository.
fn temp_workspace() -> (tempfile::TempDir, PathBuf) {
    let real = workspace_root();
    let temp = tempfile::Builder::new()
        .prefix("godot-mcp-task36-")
        .tempdir()
        .expect("temp workspace");
    let root = temp.path().join("workspace");
    std::fs::create_dir_all(&root).expect("temp workspace root");
    for entry in std::fs::read_dir(&real).expect("read the workspace root") {
        let entry = entry.expect("workspace entry");
        let name = entry.file_name();
        let label = name.to_string_lossy().to_string();
        if label == docs::DOCS_DIR || label == "target" || label == ".git" {
            continue;
        }
        std::os::unix::fs::symlink(entry.path(), root.join(&name)).expect("symlink entry");
    }
    copy_tree(&real.join(docs::DOCS_DIR), &root.join(docs::DOCS_DIR));
    (temp, root)
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create destination directory");
    for entry in std::fs::read_dir(from).expect("read source directory") {
        let entry = entry.expect("source entry");
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

fn read_page(root: &Path, relative: &str) -> String {
    std::fs::read_to_string(root.join(relative))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

fn write_page(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create page directory");
    }
    std::fs::write(&path, text).unwrap_or_else(|error| panic!("write {relative}: {error}"));
}

/// Append one line to a page and return the rules the checker then reports.
fn rules_after_appending(relative: &str, line: &str) -> Vec<String> {
    let (_temp, root) = temp_workspace();
    let mut text = read_page(&root, relative);
    text.push_str(line);
    write_page(&root, relative, &text);
    rules(&root)
}

fn rules(root: &Path) -> Vec<String> {
    checker::check(root)
        .expect("the checker runs")
        .into_iter()
        .map(|finding| finding.rule)
        .collect()
}

fn assert_reports(rules: &[String], rule: &str, case: &str) {
    assert!(
        rules.iter().any(|found| found == rule),
        "{case} must be reported as {rule}; got {rules:?}"
    );
}

/// Extract the quickstart's smoke block from the committed page.
fn smoke_block(root: &Path) -> script::DocScript {
    let pages = docs::load(root).expect("the documentation tree loads");
    let page = pages
        .iter()
        .find(|page| page.relative == QUICKSTART)
        .unwrap_or_else(|| panic!("{QUICKSTART} must exist"));
    script::extract(page)
        .expect("the quickstart's runnable blocks extract")
        .into_iter()
        .find(|block| block.name == SMOKE_BLOCK)
        .unwrap_or_else(|| panic!("{QUICKSTART} must carry the {SMOKE_BLOCK} block"))
}

/// Stage an extracted package built from this test's own `godot-mcp` binary.
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
    godot_mcp_cli::package::archive::extract_tar_gz(&archive.bytes, into)
        .expect("the archive extracts");
    into.join(godot_mcp_cli::package::payload_root(
        env!("CARGO_PKG_VERSION"),
        target,
    ))
}

#[test]
fn docs_link_check_happy_path() {
    let root = workspace_root();

    // 1. Every documentation rule passes on the committed tree.
    let findings = checker::check(&root).expect("the checker runs");
    assert!(
        findings.is_empty(),
        "the committed documentation tree must be clean:\n{}",
        docs::render(&findings)
    );

    // 2. The generated pages are exactly what their canonical sources render, so
    //    no tool, error code or Godot version in them can be stale.
    let canonical = docs::canonical::build(&root).expect("the canonical inventory builds");
    for page in generated::PAGES {
        assert_eq!(
            read_page(&root, page.relative),
            generated::render(page.generator, &canonical).expect("the page renders"),
            "{} must equal its canonical rendering",
            page.relative
        );
    }

    // 3. The lint and the credential scanner are both proven non-vacuous.
    assert!(security::self_check().is_empty());
    assert!(docs::secrets::prove_detects_planted_credential().is_empty());

    // 4. A clean temporary environment follows the quickstart verbatim.
    let block = smoke_block(&root);
    assert!(
        block.steps.len() >= 4,
        "the smoke block must cover install, configure and a stdio request, got {} step(s)",
        block.steps.len()
    );
    let temp = tempfile::Builder::new()
        .prefix("godot-mcp-task36-quickstart-")
        .tempdir()
        .expect("temp quickstart environment");
    let package = staged_package(temp.path());
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).expect("project directory");
    // The quickstart's own variable names, bound to this temporary environment. None
    // uses the `GODOT_MCP_` prefix, because the configuration loader treats every
    // variable under that prefix as configuration and rejects an unknown one.
    let bindings: Vec<(&str, String)> = vec![
        ("PACKAGE", package.to_string_lossy().into_owned()),
        (
            "MCP_BIN",
            package
                .join("bin")
                .join("godot-mcp")
                .to_string_lossy()
                .into_owned(),
        ),
        ("PROJECT", project.to_string_lossy().into_owned()),
    ];
    for declared in &block.variables {
        assert!(
            bindings.iter().any(|(name, _)| name == declared),
            "the quickstart declares ${declared} but this environment binds none"
        );
    }
    for step in &block.steps {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(&step.command)
            .current_dir(temp.path())
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", temp.path());
        for (name, value) in &bindings {
            command.env(name, value);
        }
        let output = command
            .output()
            .unwrap_or_else(|error| panic!("run quickstart step at line {}: {error}", step.line));
        assert!(
            output.status.success(),
            "quickstart step at {QUICKSTART}:{} failed with {:?}\ncommand: {}\nstdout: {}\nstderr: {}",
            step.line,
            output.status.code(),
            step.command,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // 5. The install step really wrote the bridge assets into the project.
    assert!(
        project
            .join(godot_mcp_cli::package::installer::INSTALL_ROOT)
            .join("bridge")
            .join("bridge_codec.gd")
            .is_file(),
        "the quickstart's install step must place the bridge codec in the project"
    );
}

#[test]
fn docs_link_check_failure_paths() {
    // The pristine copy is clean, so every finding below is caused by its own
    // injection and not by pre-existing content.
    let (_pristine, pristine_root) = temp_workspace();
    assert!(
        rules(&pristine_root).is_empty(),
        "the copied documentation tree must start clean:\n{}",
        docs::render(&checker::check(&pristine_root).expect("the checker runs"))
    );

    let page = "docs/operator/troubleshooting.md";

    // --- the docs checker detects a stale tool / config / protocol name -------
    assert_reports(
        &rules_after_appending(page, "\nCall `scene.tree.list` to list nodes.\n"),
        "stale-name",
        "a renamed tool",
    );
    assert_reports(
        &rules_after_appending(page, "\nSet `limits.deadline` in the config file.\n"),
        "stale-name",
        "a renamed configuration key",
    );
    assert_reports(
        &rules_after_appending(page, "\nThe bridge opens with `bridge:handshake`.\n"),
        "stale-protocol-name",
        "a renamed bridge message kind",
    );
    assert_reports(
        &rules_after_appending(page, "\nThe envelope carries `envelope:request_id`.\n"),
        "stale-protocol-name",
        "a renamed bridge envelope field",
    );
    assert_reports(
        &rules_after_appending(page, "\nThe proxy sets `http-protocol:x-godot-proxy`.\n"),
        "stale-protocol-name",
        "a renamed HTTP protocol constant",
    );
    assert_reports(
        &rules_after_appending(page, "\nExport `GODOT_MCP_HTTP_PORT` first.\n"),
        "stale-env-var",
        "an environment variable the loader does not recognize",
    );
    assert_reports(
        &rules_after_appending(page, "\nRead `godot-mcp://resources/sessions` for state.\n"),
        "stale-resource-uri",
        "a resource URI outside the canonical set",
    );

    // --- the security lint rejects the containment term ----------------------
    assert_reports(
        &rules_after_appending(
            page,
            &format!("\nEach run is fully {}.\n", security::CONTAINMENT_TERMS[1]),
        ),
        "overstated-containment",
        "describing this project's isolation with a containment term it does not provide",
    );
    assert_reports(
        &rules_after_appending(
            "docs/security/threat-model.md",
            &format!(
                "\n{} godot-mcp is `completely {}` for you.\n",
                security::QUOTED_TERM_MARKER,
                security::CONTAINMENT_TERMS[1]
            ),
        ),
        "overstated-containment",
        "a quoted-term marker used to launder a claim outside the canonical posture constant",
    );

    // --- the security lint rejects an unauthenticated public example ----------
    assert_reports(
        &rules_after_appending(
            page,
            "\n```sh\ncurl https://godot-mcp.example.invalid/mcp\n```\n",
        ),
        "unauthenticated-public-example",
        "a public endpoint reached with no credential",
    );
    assert_reports(
        &rules_after_appending(page, "\n```toml\nbind = \"0.0.0.0:8765\"\n```\n"),
        "non-loopback-bind",
        "an example binding the unspecified address",
    );
    assert_reports(
        &rules_after_appending(
            page,
            "\n```sh\ncurl -H 'Authorization: Bearer eyJhbGciOiJSUzI1NiJ9.x.y' \
             https://godot-mcp.example.invalid/mcp\n```\n",
        ),
        "credential-in-docs",
        "a credential-shaped value pasted into an example",
    );

    // --- other honesty rules -------------------------------------------------
    assert_reports(
        &rules_after_appending(
            page,
            "\nRun `godot-mcp transaction repair --force` to fix it.\n",
        ),
        "forbidden-affordance",
        "a bypass flag that does not exist",
    );
    assert_reports(
        &rules_after_appending(page, "\nThe server exposes 175 tools.\n"),
        "unverified-tool-count",
        "an unverified third-party tool count",
    );
    assert_reports(
        &rules_after_appending(page, "\nRequires Godot 4.5 or newer.\n"),
        "godot-version-alias",
        "a major-minor Godot alias",
    );
    assert_reports(
        &rules_after_appending(page, "\nOpen /var/lib/godot-mcp/artifacts/frame.png.\n"),
        "opaque-artifact-only",
        "a raw artifact storage path",
    );
    assert_reports(
        &rules_after_appending(page, "\nSee [the missing page](./nowhere-at-all.md).\n"),
        "broken-link",
        "a relative link that resolves to nothing",
    );
    assert_reports(
        &rules_after_appending(page, "\nDeploy it as a multi-tenant hosted service.\n"),
        "service-model-claim",
        "advertising a service model this project does not offer",
    );

    // --- a configuration example must get its annotated verdict --------------
    assert_reports(
        &rules_after_appending(
            page,
            "\n<!-- docs-check:config expect=\"accept\" -->\n```toml\ntransport = \"http\"\n```\n",
        ),
        "config-example",
        "an example annotated accept that the production loader rejects",
    );
    assert_reports(
        &rules_after_appending(
            page,
            "\n<!-- docs-check:config expect=\"reject\" -->\n```toml\ntransport = \"stdio\"\n```\n",
        ),
        "config-example",
        "an example annotated reject that the production loader accepts",
    );
    assert_reports(
        &rules_after_appending(page, "\n```toml\ntransport = \"stdio\"\n```\n"),
        "config-example",
        "an unannotated configuration example",
    );

    // --- a generated page may not be hand-edited -----------------------------
    {
        let (_temp, root) = temp_workspace();
        let target = generated::PAGES[0].relative;
        let mut text = read_page(&root, target);
        text.push_str("\n* `hand-written-addition`\n");
        write_page(&root, target, &text);
        assert_reports(
            &rules(&root),
            "generated-page",
            "a hand-edited generated page",
        );
    }

    // --- an honesty-critical statement may not be deleted -------------------
    {
        let required = checker::REQUIRED_STATEMENTS[0];
        let (_temp, root) = temp_workspace();
        let text = read_page(&root, required.file).replace(required.needle, "is fully supported");
        write_page(&root, required.file, &text);
        assert_reports(
            &rules(&root),
            "required-statement",
            "deleting the statement that the editor plane is not shipped",
        );
    }

    // --- a runnable block must stay executable ------------------------------
    {
        let (_temp, root) = temp_workspace();
        let text = read_page(&root, QUICKSTART).replace(
            "<!-- docs-script:var PROJECT",
            "<!-- docs-script:removed PROJECT",
        );
        write_page(&root, QUICKSTART, &text);
        assert_reports(
            &rules(&root),
            "runnable-block",
            "a quickstart step expanding a variable the page no longer declares",
        );
    }

    // --- a missing documentation tree is an error, never a silent pass -------
    let empty = tempfile::tempdir().expect("temp dir");
    assert!(
        checker::check(empty.path()).is_err(),
        "a workspace with no documentation tree must fail rather than report clean"
    );
}
