//! `health.get` must never name a profile the caller did not select.
//!
//! The defect this pins was a real one: the three plane components carried
//! hardcoded literals naming `core`, so a caller who selected `full` was told
//! that "the core profile never starts a Godot process". Every message here is
//! therefore checked *behaviourally* — the tool is driven through the kernel
//! under each of the four profiles and the returned JSON is inspected. A
//! source-text scan would pass again the moment someone reintroduced the literal
//! through a different spelling.
//!
//! Two further properties are pinned because both drifted in the same way:
//!
//! * the `godot` component's status follows the *observed* engine preconditions,
//!   so a resolvable executable is not reported `unavailable`; and
//! * the static-validation limitation names no profile at all, because
//!   `project.validate` is static under every profile.
//!
//! Nothing here starts a Godot process. The "resolvable" case uses an ordinary
//! temporary file, because discovery only stats a candidate — the capability
//! probe that would execute it is deliberately not part of `health.get`.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use godot_mcp_analysis::index::FileInventory;
use godot_mcp_analysis::project::ProjectResolver;
use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::contracts::PrincipalId;
use godot_mcp_engine::discovery::{DiscoveryError, EnvironmentSnapshot, HostPlatform, discover};
use godot_mcp_server::core::{CoreLimits, CoreSession, register_core_tools};
use godot_mcp_server::kernel::{AccessContext, ProgressReporter, ServiceKernel};
use godot_mcp_server::test_support::OutcomeRouterHarness;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

const GENERATION: u64 = 12;
const OBSERVED_AT_UNIX_MS: u64 = 1_700_000_000_000;

const INVENTORY: &[&str] = &[
    "project.godot",
    "resources/buff.tres",
    "resources/legacy.res",
    "scenes/hud.tscn",
    "scenes/legacy.scn",
    "scenes/main.tscn",
    "scripts/dynamic.gd",
    "scripts/game_state.gd",
    "scripts/hud.gd",
    "scripts/player.gd",
];

const PROFILES: [CapabilityProfile; 4] = [
    CapabilityProfile::Core,
    CapabilityProfile::Editor,
    CapabilityProfile::Runtime,
    CapabilityProfile::Full,
];

fn committed_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("core")
        .join("project")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create destination directory");
    for entry in fs::read_dir(from).expect("read fixture directory") {
        let entry = entry.expect("fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("fixture entry type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}

fn install_fixture_executable(base: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;

    let executable = base.join("configured-godot");
    fs::write(&executable, b"#!/bin/sh\nexit 0\n").expect("write the fixture executable");
    let mut permissions = fs::metadata(&executable)
        .expect("stat the fixture executable")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).expect("chmod the fixture executable");
    executable
}

struct Probe {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
}

impl Probe {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize temp base");
        let root = base.join("game");
        copy_tree(&committed_fixture(), &root);
        Self {
            _tmp: tmp,
            base,
            root,
        }
    }

    /// Drive `health.get` through the kernel under one profile, with the engine
    /// binary the operator would have configured.
    async fn health(&self, profile: CapabilityProfile, engine_binary: Option<PathBuf>) -> Value {
        let resolver = ProjectResolver::new(&self.base.join(format!("lease-{profile:?}")));
        let project = resolver
            .resolve(&self.root, GENERATION)
            .expect("resolve the fixture project");
        let identity = project.identity().clone();
        let session = Arc::new(
            CoreSession::new(
                project,
                self.base.join(format!("index-{profile:?}")),
                FileInventory::from_paths(INVENTORY).expect("fixture inventory"),
                CoreLimits::default(),
            )
            .with_engine_binary(engine_binary),
        );
        let mut kernel = ServiceKernel::new(profile);
        register_core_tools(&mut kernel, session)
            .expect("every profile advertising core tools accepts their handlers");
        let access = AccessContext::new(
            identity,
            PrincipalId::parse("principal_0123456789abcdef").expect("principal"),
            None,
            OBSERVED_AT_UNIX_MS,
        );
        kernel
            .call_tool(
                "health.get",
                Map::new(),
                access,
                CancellationToken::new(),
                ProgressReporter::noop(),
            )
            .await
            .expect("health.get must answer")
    }
}

fn components(answer: &Value) -> BTreeMap<String, (String, String)> {
    answer
        .get("components")
        .and_then(Value::as_array)
        .expect("health carries a component array")
        .iter()
        .map(|component| {
            let name = component
                .get("component")
                .and_then(Value::as_str)
                .expect("a named component")
                .to_owned();
            let status = component
                .get("status")
                .and_then(Value::as_str)
                .expect("a component status")
                .to_owned();
            let message = component
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            (name, (status, message))
        })
        .collect()
}

/// No component message may name a profile other than the selected one.
#[tokio::test]
async fn every_health_component_names_only_the_selected_profile() {
    let probe = Probe::new();
    for selected in PROFILES {
        let answer = probe.health(selected, None).await;
        let observed = components(&answer);
        for component in ["godot", "editor", "runtime"] {
            let (_, message) = observed
                .get(component)
                .unwrap_or_else(|| panic!("the {component} component is reported, never omitted"));
            assert!(
                message.contains(selected.manifest_name()),
                "the {component} component under {selected:?} must name the selected profile: \
                 {message}"
            );
            for other in PROFILES {
                if other == selected {
                    continue;
                }
                assert!(
                    !message.contains(&format!("{} profile", other.manifest_name())),
                    "the {component} component under {selected:?} names the unselected \
                     {other:?} profile: {message}"
                );
            }
        }
    }
}

/// The four profiles must not share one boilerplate reason: a message that is
/// byte-identical across profiles is how the original defect hid.
#[tokio::test]
async fn the_godot_reason_differs_per_profile() {
    let probe = Probe::new();
    let mut seen: Vec<String> = Vec::new();
    for selected in PROFILES {
        let answer = probe.health(selected, None).await;
        let (_, message) = components(&answer)
            .remove("godot")
            .expect("the godot component is reported");
        assert!(
            !seen.contains(&message),
            "{selected:?} repeats another profile's godot reason verbatim: {message}"
        );
        seen.push(message);
    }
}

/// `core` grants `read` only, so no tool it advertises reaches the process
/// plane. That, and not the absence of an engine, is the reason it reports
/// `unavailable`.
#[tokio::test]
async fn core_reports_that_it_advertises_no_process_tool() {
    let probe = Probe::new();
    let answer = probe.health(CapabilityProfile::Core, None).await;
    let (status, message) = components(&answer)
        .remove("godot")
        .expect("the godot component is reported");

    assert_eq!(status, "unavailable");
    assert!(
        message.contains("no tool whose effect reaches the process plane"),
        "core must give the profile-membership reason, not an engine-absence reason: {message}"
    );
    assert!(
        !message.contains("was found in this host's"),
        "core must not blame host discovery for a plane it never advertises: {message}"
    );
}

/// A resolvable executable is never reported `unavailable`, and an explicitly
/// configured one that does not resolve is reported as *configured*, not as a
/// failed host search.
#[tokio::test]
async fn the_godot_status_follows_the_observed_engine_preconditions() {
    let probe = Probe::new();
    let resolvable = probe.base.join("configured-godot");
    fs::write(&resolvable, b"not executed, only stat'ed").expect("write the configured candidate");
    let missing = probe.base.join("absent-godot");

    for selected in [
        CapabilityProfile::Editor,
        CapabilityProfile::Runtime,
        CapabilityProfile::Full,
    ] {
        let answer = probe.health(selected, Some(resolvable.clone())).await;
        let (status, message) = components(&answer)
            .remove("godot")
            .expect("the godot component is reported");
        assert_ne!(
            status, "unavailable",
            "a resolved executable must not be reported unavailable under {selected:?}: {message}"
        );
        assert!(
            message.contains("resolved"),
            "the reason must state that an executable resolved: {message}"
        );

        let answer = probe.health(selected, Some(missing.clone())).await;
        let (status, message) = components(&answer)
            .remove("godot")
            .expect("the godot component is reported");
        assert_eq!(status, "unavailable");
        assert!(
            message.contains("explicitly configured"),
            "a configured path that does not resolve must be reported as configured: {message}"
        );
    }
}

/// The production assembly must give `health.get` the same explicit binary the
/// engine plane receives.
#[tokio::test]
async fn production_health_honours_the_configured_godot_binary() {
    {
        let probe = Probe::new();
        let configured_directory = probe.base.join("configured-godot-directory");
        fs::create_dir(&configured_directory).expect("create the configured directory");
        let ambient_bin = probe.base.join("ambient-bin");
        fs::create_dir(&ambient_bin).expect("create the ambient bin directory");
        fs::write(ambient_bin.join("godot"), b"ambient candidate")
            .expect("write the ambient candidate");
        let error = discover(
            Some(&configured_directory),
            HostPlatform::Linux,
            &EnvironmentSnapshot {
                path_dirs: vec![ambient_bin],
                ..EnvironmentSnapshot::default()
            },
        )
        .expect_err("an explicit directory must short-circuit before ambient discovery");
        assert!(matches!(error, DiscoveryError::NotRegularFile { .. }));

        let harness =
            OutcomeRouterHarness::open_with_godot_binary(&probe.root, configured_directory.clone())
                .expect("open the production runtime with a configured Godot directory");
        assert!(harness.assembled_through_production_path());
        let answer = harness
            .call_tool("health.get", json!({}))
            .await
            .expect("production health.get must answer");
        let (status, message) = components(&answer)
            .remove("godot")
            .expect("the godot component is reported");

        // Load-bearing discriminator: only `ConfiguredUnresolved` can emit this phrase, while an
        // explicit directory short-circuits before any ambient Godot candidate can satisfy discovery.
        assert!(
            message.contains(
                "explicitly configured Godot executable could not be resolved to a regular file"
            ),
            "production health.get must identify the configured-directory failure instead of an \
             ambient discovery outcome: {message}"
        );
        assert_eq!(status, "unavailable");
    }

    {
        let probe = Probe::new();
        let configured = install_fixture_executable(&probe.base);
        let harness = OutcomeRouterHarness::open_with_godot_binary(&probe.root, configured.clone())
            .expect("open the production runtime with a configured Godot binary");
        let answer = harness
            .call_tool("health.get", json!({}))
            .await
            .expect("production health.get must answer");
        let (status, message) = components(&answer)
            .remove("godot")
            .expect("the godot component is reported");

        assert_ne!(
            status,
            "unavailable",
            "production health.get must resolve the operator's configured Godot binary at {}: \
             {message}",
            configured.display()
        );
    }
}

/// The `editor` and `runtime` components say what this server attaches. A bare
/// kernel attaches neither the editor bridge nor managed game sessions, so both
/// are `unavailable` under every profile: a profile that advertises the plane's
/// tools is told the boundary is not attached here, and one that does not is
/// told so. Neither ever claims the plane is not part of the product.
#[tokio::test]
async fn the_plane_components_say_what_this_server_attaches() {
    let probe = Probe::new();
    for selected in PROFILES {
        let answer = probe.health(selected, None).await;
        let observed = components(&answer);
        for (component, advertising) in [
            (
                "editor",
                [CapabilityProfile::Editor, CapabilityProfile::Full],
            ),
            (
                "runtime",
                [CapabilityProfile::Runtime, CapabilityProfile::Full],
            ),
        ] {
            let (status, message) = observed
                .get(component)
                .unwrap_or_else(|| panic!("the {component} component is reported"));
            assert_eq!(status, "unavailable");
            let expected = if advertising.contains(&selected) {
                "but this server attaches no"
            } else {
                "advertises none of"
            };
            assert!(
                message.contains(expected),
                "the {component} component under {selected:?} must say `{expected}`: {message}"
            );
            for stale in ["not shipped", "Todos 20"] {
                assert!(
                    !message.contains(stale),
                    "the {component} component under {selected:?} must not claim the plane is \
                     missing from the product: {message}"
                );
            }
        }
        assert!(
            observed
                .get("editor")
                .is_some_and(|(_, message)| !message.contains("EditorPlugin")),
            "no message may present an unattached capability as present"
        );
    }
}

/// The static-validation limitation is a property of the validator, so it must
/// name no profile at all.
#[tokio::test]
async fn the_static_validation_limitation_names_no_profile() {
    let probe = Probe::new();
    for selected in PROFILES {
        let resolver =
            ProjectResolver::new(&probe.base.join(format!("validate-lease-{selected:?}")));
        let project = resolver
            .resolve(&probe.root, GENERATION)
            .expect("resolve the fixture project");
        let identity = project.identity().clone();
        let session = Arc::new(CoreSession::new(
            project,
            probe.base.join(format!("validate-index-{selected:?}")),
            FileInventory::from_paths(INVENTORY).expect("fixture inventory"),
            CoreLimits::default(),
        ));
        let mut kernel = ServiceKernel::new(selected);
        register_core_tools(&mut kernel, session).expect("register the core handlers");
        let access = AccessContext::new(
            identity,
            PrincipalId::parse("principal_0123456789abcdef").expect("principal"),
            None,
            OBSERVED_AT_UNIX_MS,
        );
        let answer = kernel
            .call_tool(
                "project.validate",
                Map::new(),
                access,
                CancellationToken::new(),
                ProgressReporter::noop(),
            )
            .await
            .expect("project.validate must answer");
        let limitations = answer
            .get("data")
            .and_then(|data| data.get("limitations"))
            .and_then(Value::as_array)
            .expect("static validation declares its limitations")
            .clone();
        let engine = limitations
            .iter()
            .find(|limitation| {
                limitation.get("code") == Some(&json!("engine_validation_not_performed"))
            })
            .unwrap_or_else(|| panic!("{selected:?} must disclose that no engine ran"));
        let message = engine
            .get("message")
            .and_then(Value::as_str)
            .expect("the limitation carries a message");
        for profile in PROFILES {
            assert!(
                !message.contains(&format!("{} profile", profile.manifest_name())),
                "the static-validation limitation must name no profile, found {profile:?}: \
                 {message}"
            );
        }
        assert!(
            message.contains("static validator never starts a Godot process"),
            "the limitation must still state that no engine ran: {message}"
        );
    }
}
