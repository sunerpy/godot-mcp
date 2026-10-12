//! Deliberately-corrupted fixture builders for Todo 1's Red gate.
//!
//! Each function materializes a minimal, self-contained workspace tree in a
//! caller-provided directory that violates exactly one frozen invariant, so the
//! corresponding audit fails **by name**. These back the `qa-audit` test-hooks
//! binary (invoked by `qa/tasks/1.json`'s failure commands, which expect a
//! nonzero exit) and the in-process Red tests.
//!
//! The builders never touch the real workspace; they only write under `dir`.

use std::io;
use std::path::Path;

/// Named corrupt fixtures. Each maps to exactly one audit that must reject it,
/// and each shipping-artifact class has its own distinct fixture so no single
/// fixture stands in for the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorruptFixture {
    /// `godot-mcp-domain` declares a dependency on `godot-mcp-server`
    /// (a forbidden lower-layer -> transport/UI edge).
    DomainImportsServer,
    /// A shipping member pins production `godot = 0.5.4`.
    ShippingGodot,
    /// A shipping member declares a `gdext-bridge` dependency.
    ShippingGdextBridge,
    /// A shipping `.gdextension` descriptor is present in the tree.
    ShippingGdextension,
    /// A native bridge library (`*.so`/`*.dylib`/`*.dll`) ships inside the
    /// addon tree.
    ShippingNativeBridgeLib,
    /// A benign native library that is NOT addon/bridge shipping content; the
    /// audit must NOT flag it (guards against false positives on ordinary
    /// dependency/build libraries).
    OrdinaryDependencyLibrary,
    /// `tooling.lock.json` is present but unpopulated (empty fields).
    UnpopulatedToolingLock,
    /// `qa/tasks/17.json` is deleted/emptied.
    MissingDescriptor,
    /// `qa/tasks/34.json` command carries an angle-bracket transaction token.
    Task34AngleBracket,
    /// `qa/tasks/34.json` repair command invokes an ambient-PATH `godot-mcp
    /// transaction ...` instead of the deterministic packaged-binary resolver.
    Task34AmbientInvocation,
    /// Crate `godot` is added as a **supplied** dependency of the shipping CLI:
    /// it is declared in `[workspace.dependencies]`, declared by a shipping
    /// member, locked in `Cargo.lock`, and therefore inside the release binary's
    /// linked-crate list. Every one of those surfaces must reject it, which is
    /// what makes the first-release native-dependency guard falsifiable.
    SuppliedGodotDependency,
}

impl CorruptFixture {
    /// The audit-check name this fixture is expected to fail by.
    #[must_use]
    pub const fn expected_failing_check(self) -> &'static str {
        match self {
            Self::DomainImportsServer => "no_transport_ui_imports",
            Self::ShippingGodot
            | Self::ShippingGdextBridge
            | Self::ShippingGdextension
            | Self::ShippingNativeBridgeLib
            | Self::OrdinaryDependencyLibrary => "no_shipping_gdext_artifact",
            Self::UnpopulatedToolingLock => "tooling_lock_populated",
            Self::MissingDescriptor | Self::Task34AngleBracket | Self::Task34AmbientInvocation => {
                "all_37_descriptors_present_and_valid"
            }
            Self::SuppliedGodotDependency => "no_supplied_native_dependency",
        }
    }

    /// Whether the audit is expected to REJECT this fixture. Every fixture
    /// rejects except [`Self::OrdinaryDependencyLibrary`], which must be
    /// accepted (it is a false-positive guard).
    #[must_use]
    pub const fn expects_rejection(self) -> bool {
        !matches!(self, Self::OrdinaryDependencyLibrary)
    }

    /// Parse a fixture name (as used on the `qa-audit --fixture <name>` CLI).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "domain-imports-server" => Self::DomainImportsServer,
            "shipping-godot" => Self::ShippingGodot,
            "shipping-gdext-bridge" => Self::ShippingGdextBridge,
            "shipping-gdextension" => Self::ShippingGdextension,
            "shipping-native-bridge-lib" => Self::ShippingNativeBridgeLib,
            "ordinary-dependency-library" => Self::OrdinaryDependencyLibrary,
            "unpopulated-tooling-lock" => Self::UnpopulatedToolingLock,
            "missing-descriptor" => Self::MissingDescriptor,
            "task34-angle-bracket" => Self::Task34AngleBracket,
            "task34-ambient-invocation" => Self::Task34AmbientInvocation,
            "supplied-godot-dependency" => Self::SuppliedGodotDependency,
            _ => return None,
        })
    }
}

fn write(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)
}

/// Minimal but valid member manifest text.
fn member_manifest(name: &str, extra_dep: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nserde = \"1\"\n{extra_dep}\n"
    )
}

/// Write the six shipping member manifests into `<root>/crates/<name>/Cargo.toml`,
/// applying `mutate` to each `(name, extra_dep)` pair so a caller can inject a
/// forbidden dependency into one member.
fn write_shipping_members(root: &Path, mutate: impl Fn(&str) -> String) -> io::Result<()> {
    for member in crate::audit::SHIPPING_MEMBERS {
        let extra = mutate(member);
        let manifest = root.join("crates").join(member).join("Cargo.toml");
        write(&manifest, &member_manifest(member, &extra))?;
    }
    // A minimal workspace root manifest so find_workspace_root succeeds.
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/godot-mcp-domain\"]\n",
    )
}

/// Build the named corrupt fixture under `dir`. Returns the workspace root to
/// pass to the audit.
///
/// # Errors
/// Returns an I/O error if any fixture file cannot be written.
pub fn build(fixture: CorruptFixture, dir: &Path) -> io::Result<std::path::PathBuf> {
    let root = dir.to_path_buf();
    match fixture {
        CorruptFixture::DomainImportsServer => {
            write_shipping_members(&root, |member| {
                if member == "godot-mcp-domain" {
                    "godot-mcp-server = { path = \"../godot-mcp-server\" }".to_string()
                } else {
                    String::new()
                }
            })?;
        }
        CorruptFixture::ShippingGodot => {
            write_shipping_members(&root, |member| {
                if member == "godot-mcp-engine" {
                    "godot = \"=0.5.4\"".to_string()
                } else {
                    String::new()
                }
            })?;
        }
        CorruptFixture::ShippingGdextBridge => {
            write_shipping_members(&root, |member| {
                if member == "godot-mcp-bridge" {
                    "gdext-bridge = \"=0.1.0\"".to_string()
                } else {
                    String::new()
                }
            })?;
        }
        CorruptFixture::ShippingGdextension => {
            // Clean shipping manifests plus a planted `.gdextension` descriptor
            // in the shipping addon tree.
            write_shipping_members(&root, |_| String::new())?;
            write(
                &root
                    .join("addons")
                    .join("godot_mcp")
                    .join("godot_mcp.gdextension"),
                "[configuration]\nentry_symbol = \"gdext_rust_init\"\n",
            )?;
        }
        CorruptFixture::ShippingNativeBridgeLib => {
            // Clean shipping manifests plus a planted native bridge library
            // inside the shipping addon tree.
            write_shipping_members(&root, |_| String::new())?;
            write(
                &root
                    .join("addons")
                    .join("godot_mcp")
                    .join("bin")
                    .join("libgodot_mcp_bridge.so"),
                "\x7fELF fake native bridge library bytes",
            )?;
        }
        CorruptFixture::OrdinaryDependencyLibrary => {
            // Clean shipping manifests plus a benign native library that is NOT
            // addon/bridge shipping content — it lives outside every addon
            // subtree, so the audit must accept this fixture.
            write_shipping_members(&root, |_| String::new())?;
            write(
                &root.join("vendor").join("libsomedependency.so"),
                "\x7fELF ordinary dependency library bytes",
            )?;
        }
        CorruptFixture::UnpopulatedToolingLock => {
            write(
                &root.join("tooling.lock.json"),
                "{\n  \"schema_version\": \"1.0.0\",\n  \"tools\": {}\n}\n",
            )?;
        }
        CorruptFixture::MissingDescriptor
        | CorruptFixture::Task34AngleBracket
        | CorruptFixture::Task34AmbientInvocation => {
            build_descriptor_fixture(fixture, &root)?;
        }
        CorruptFixture::SuppliedGodotDependency => {
            build_supplied_godot_dependency(&root)?;
        }
    }
    Ok(root)
}

/// Materialize a workspace in which crate `godot` is genuinely supplied to the
/// shipping CLI: named in `[workspace.dependencies]`, declared by a shipping
/// member, locked, and reachable from `godot-mcp-cli` in the lock graph. It is
/// self-contained under `dir` and is never built, so nothing is downloaded and no
/// native dependency ever enters this workspace's own release graph.
fn build_supplied_godot_dependency(root: &Path) -> io::Result<()> {
    write_shipping_members(root, |member| {
        if member == "godot-mcp-cli" {
            "godot = { workspace = true }".to_string()
        } else {
            String::new()
        }
    })?;
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/godot-mcp-cli\"]\n\n\
         [workspace.dependencies]\ngodot = \"=0.5.4\"\nserde = \"=1.0.229\"\n",
    )?;
    write(&root.join("Cargo.lock"), &supplied_godot_lock())
}

/// A minimal but structurally valid lock in which `godot-mcp-cli` depends on
/// `godot`, so the linked-crate closure genuinely contains it.
fn supplied_godot_lock() -> String {
    let mut lock = String::from("version = 4\n");
    for member in crate::audit::SHIPPING_MEMBERS {
        let dependencies = if *member == "godot-mcp-cli" {
            "dependencies = [\n \"godot\",\n \"serde\",\n]\n"
        } else {
            "dependencies = [\n \"serde\",\n]\n"
        };
        lock.push_str(&format!(
            "\n[[package]]\nname = \"{member}\"\nversion = \"0.1.0\"\n{dependencies}"
        ));
    }
    lock.push_str(
        "\n[[package]]\nname = \"godot\"\nversion = \"0.5.4\"\n\
         source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
         dependencies = [\n \"godot-core\",\n]\n\
         \n[[package]]\nname = \"godot-core\"\nversion = \"0.5.4\"\n\
         source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
         \n[[package]]\nname = \"serde\"\nversion = \"1.0.229\"\n\
         source = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    );
    lock
}

/// Write a full 1..=37 descriptor set, then corrupt one entry.
fn build_descriptor_fixture(fixture: CorruptFixture, root: &Path) -> io::Result<()> {
    let qa = root.join("qa").join("tasks");
    std::fs::create_dir_all(&qa)?;
    for n in 1..=crate::descriptors::TASK_COUNT {
        let body = if n == 34 {
            minimal_task34()
        } else {
            minimal_simple_descriptor(n)
        };
        write(&qa.join(format!("{n}.json")), &body)?;
    }
    match fixture {
        CorruptFixture::MissingDescriptor => {
            // Empty out qa/tasks/17.json.
            write(&qa.join("17.json"), "")?;
        }
        CorruptFixture::Task34AngleBracket => {
            // Replace task 34 with a version whose command uses <transaction-id>.
            write(&qa.join("34.json"), &task34_with_angle_bracket())?;
        }
        CorruptFixture::Task34AmbientInvocation => {
            // Replace task 34 with a version whose repair command invokes an
            // ambient-PATH `godot-mcp transaction ...` instead of the
            // deterministic packaged-binary resolver.
            write(&qa.join("34.json"), &task34_with_ambient_invocation())?;
        }
        _ => {}
    }
    Ok(())
}

fn minimal_simple_descriptor(n: u32) -> String {
    format!(
        r#"{{
  "task": {n},
  "wrapper": "simple",
  "happy": [
    {{ "command": "cargo test -p godot-mcp-domain --test placeholder_{n} -- --exact happy_{n}", "expected_exit": 0, "assertions": ["exits 0"] }}
  ],
  "failure": [
    {{ "command": "cargo test -p godot-mcp-domain --test placeholder_{n} -- --exact failure_{n}", "expected_exit": 0, "assertions": ["asserts rejection"] }}
  ],
  "evidence": "<attemptDir>/task-{n}-godot-mcp-server-architecture.txt"
}}
"#
    )
}

fn minimal_task34() -> String {
    let ids = crate::descriptors::TASK34_FIXED_IDS;
    let manifest = crate::descriptors::TASK34_HOST_PACKAGE_MANIFEST;
    let mut happy = String::new();
    for id in ids {
        happy.push_str(&format!(
            "    {{ \"command\": \"cargo +1.96.0 run --locked -p godot-mcp-cli --features test-hooks --bin qa-host-invoke -- --manifest {manifest} -- transaction repair --project tests/fixtures/quarantined --transaction-id {id} --operation reconcile --proof-file tests/fixtures/repair/p.json\", \"expected_exit\": 0, \"assertions\": [\"observes disposition\"] }},\n"
        ));
    }
    let happy = happy.trim_end_matches(",\n");
    format!(
        r#"{{
  "task": 34,
  "wrapper": "fixture-lifecycle",
  "fixture_lifecycle": {{
    "setup": "cargo +1.96.0 run --locked -p godot-mcp-cli --features test-hooks --bin qa-fixture-34 -- setup --scenario task34",
    "cleanup": "cargo +1.96.0 run --locked -p godot-mcp-cli --features test-hooks --bin qa-fixture-34 -- cleanup --scenario task34"
  }},
  "happy": [
{happy}
  ],
  "failure": [
    {{ "command": "cargo test -p godot-mcp-cli --test package_and_repair -- --exact package_and_repair_failure_paths", "expected_exit": 0, "assertions": ["asserts rejection"] }}
  ],
  "evidence": "<attemptDir>/task-34-godot-mcp-server-architecture.txt"
}}
"#
    )
}

fn task34_with_angle_bracket() -> String {
    r#"{
  "task": 34,
  "wrapper": "fixture-lifecycle",
  "fixture_lifecycle": {
    "setup": "cargo +1.96.0 run --locked -p godot-mcp-cli --features test-hooks --bin qa-fixture-34 -- setup --scenario task34",
    "cleanup": "cargo +1.96.0 run --locked -p godot-mcp-cli --features test-hooks --bin qa-fixture-34 -- cleanup --scenario task34"
  },
  "happy": [
    { "command": "godot-mcp transaction repair --project tests/fixtures/quarantined --transaction-id <transaction-id> --operation reconcile --proof-file p.json", "expected_exit": 0, "assertions": ["uses a token"] }
  ],
  "failure": [
    { "command": "cargo test -p godot-mcp-cli --test package_and_repair -- --exact package_and_repair_failure_paths", "expected_exit": 0, "assertions": ["asserts rejection"] }
  ],
  "evidence": "<attemptDir>/task-34-godot-mcp-server-architecture.txt"
}
"#
    .to_string()
}

/// A Task-34 descriptor whose repair command invokes an ambient-PATH
/// `godot-mcp transaction ...` instead of the deterministic packaged-binary
/// resolver. The fixed IDs and fixture lifecycle are otherwise well-formed, so
/// only the ambient-invocation contract violation triggers rejection.
fn task34_with_ambient_invocation() -> String {
    let ids = crate::descriptors::TASK34_FIXED_IDS;
    let mut happy = String::new();
    for id in ids {
        happy.push_str(&format!(
            "    {{ \"command\": \"godot-mcp transaction repair --project tests/fixtures/quarantined --transaction-id {id} --operation reconcile --proof-file tests/fixtures/repair/p.json\", \"expected_exit\": 0, \"assertions\": [\"observes disposition\"] }},\n"
        ));
    }
    let happy = happy.trim_end_matches(",\n");
    format!(
        r#"{{
  "task": 34,
  "wrapper": "fixture-lifecycle",
  "fixture_lifecycle": {{
    "setup": "cargo +1.96.0 run --locked -p godot-mcp-cli --features test-hooks --bin qa-fixture-34 -- setup --scenario task34",
    "cleanup": "cargo +1.96.0 run --locked -p godot-mcp-cli --features test-hooks --bin qa-fixture-34 -- cleanup --scenario task34"
  }},
  "happy": [
{happy}
  ],
  "failure": [
    {{ "command": "cargo test -p godot-mcp-cli --test package_and_repair -- --exact package_and_repair_failure_paths", "expected_exit": 0, "assertions": ["asserts rejection"] }}
  ],
  "evidence": "<attemptDir>/task-34-godot-mcp-server-architecture.txt"
}}
"#
    )
}
