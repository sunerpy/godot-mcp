//! Todo 8 acceptance tests: `project.godot` parsing and canonical project
//! identity.
//!
//! Exactly two test cases, matching the `qa/tasks/8.json` descriptor:
//! `project_identity_happy_path` and `project_identity_failure_paths`.
//!
//! Both cases run against a *copy* of the committed fixture project so the
//! repository tree is never mutated and so deliberate aliases (and a deliberate
//! retarget) can be built without touching version-controlled files.
//!
//! The whole identity feature is built on Todo 2's held-object ownership layer,
//! which is Unix-object-identity based on this host; the tests therefore assert
//! the real behaviour on Unix and are compiled out elsewhere rather than
//! silently weakened.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use godot_mcp_analysis::project::{
    PROJECT_MANIFEST as PROJECT_MARKER_NAME, ProjectDiagnosticKind, ProjectError, ProjectResolver,
    ResourceTarget, SettingInterpretation,
};
use godot_mcp_domain::contracts::ProvenanceSource;
use godot_mcp_domain::lease::LeaseManager;
use godot_mcp_domain::{Confidence, ErrorKind};

/// Absolute path of the committed fixture project. The analysis layer must never
/// need a sibling `codegraph-rust` checkout or a Godot install at runtime.
fn fixture_project() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("project")
        .join("sample")
}

/// Recursively copy `from` into `to`, creating `to`.
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

/// A writable, symlink-free copy of the fixture project plus the *path* of a
/// not-yet-created private lease state directory.
struct Fixture {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    state: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Canonicalize so the base is symlink-free (macOS `/var` -> `/private/var`),
    // making every alias built below a deliberate alias rather than an artefact
    // of the temp base.
    let base = tmp.path().canonicalize().expect("canonicalize temp base");
    let root = base.join("game");
    let state = base.join("state");
    copy_tree(&fixture_project(), &root);
    Fixture {
        _tmp: tmp,
        base,
        root,
        state,
    }
}

fn is_denied_or_conflict(kind: ErrorKind) -> bool {
    matches!(kind, ErrorKind::PermissionDenied | ErrorKind::Conflict)
}

// ===========================================================================
// Descriptor-owned happy case
// ===========================================================================

#[test]
fn project_identity_happy_path() {
    let fx = fixture();
    // The engine cache directory exists in a real project but is never source.
    // A real Godot 4 project keeps an engine-written `project.godot` copy in
    // there; it must never be mistaken for a project root.
    let cache = fx.root.join(".godot");
    fs::create_dir(&cache).expect("mkdir .godot");
    fs::write(cache.join("uid_cache.bin"), b"\x00binary").expect("write engine cache file");
    fs::write(
        cache.join("project.godot"),
        b"[application]\nconfig/name=\"Engine Cache Decoy\"\n",
    )
    .expect("write the engine-written marker copy");
    fs::create_dir(cache.join("imported")).expect("mkdir .godot/imported");

    // Separate managers do not share their in-process convergence registry, so
    // production must pass its held lease into analysis instead of reacquiring it.
    let preexisting_lease = LeaseManager::new(&fx.state)
        .acquire(&fx.root)
        .expect("acquire the production-shaped lease first");
    let resolver = ProjectResolver::new(&fx.state);
    let conflict = resolver
        .resolve(&fx.root, 8)
        .expect_err("a separate manager must observe the held advisory lock");
    assert!(matches!(conflict, ProjectError::Conflict { .. }));
    drop(preexisting_lease);

    // (1) Upward discovery from a *nested script* reached through a symlink-free
    // lexical alias resolves the one fixture project identity.
    let nested_script = fx
        .root
        .join(".")
        .join("scripts")
        .join("..")
        .join("scripts")
        .join("player.gd");
    let project = resolver
        .resolve(&nested_script, 8)
        .expect("nested script resolves after the prior owner releases the lease");
    assert_eq!(project.display_root(), fx.root.as_path());
    assert_eq!(project.identity().generation(), 8);

    // A differently spelled, still symlink-free path to the same objects — and a
    // plain nested directory — converge onto the identical project identity and
    // the identical held lease.
    let alias_dir = fx.root.join("scripts").join("..");
    let by_alias = resolver
        .resolve(&alias_dir, 8)
        .expect("lexical alias converges");
    let by_subdir = resolver
        .resolve(&fx.root.join("scripts"), 8)
        .expect("upward discovery from a subdirectory converges");
    assert_eq!(project.lease_key(), by_alias.lease_key());
    assert_eq!(project.lease_key(), by_subdir.lease_key());
    assert_eq!(project.identity(), by_alias.identity());
    assert_eq!(project.identity(), by_subdir.identity());
    assert!(project.same_project(&by_alias) && project.same_project(&by_subdir));
    // The public project id is the stable OS-object identity, never a path.
    assert!(
        project
            .identity()
            .object_id()
            .as_str()
            .starts_with("project_"),
        "public project id must be the opaque object identity"
    );
    assert!(
        !project
            .identity()
            .object_id()
            .as_str()
            .contains(&fx.root.display().to_string()),
        "a display path must never leak into the project identity"
    );
    // A *different* project object is a different identity.
    let other_root = fx.base.join("other");
    copy_tree(&fixture_project(), &other_root);
    let other = resolver
        .resolve(&other_root, 8)
        .expect("a second project resolves");
    assert_ne!(project.identity(), other.identity());
    assert_ne!(project.lease_key(), other.lease_key());

    // A nested project.godot wins over its ancestor: discovery stops at the
    // nearest marker.
    let inner_root = fx.root.join("subproject");
    copy_tree(&fixture_project(), &inner_root);
    let inner = resolver
        .resolve(&inner_root.join("scripts"), 8)
        .expect("nested project resolves to itself");
    assert_eq!(inner.display_root(), inner_root.as_path());
    assert_ne!(inner.identity(), project.identity());

    // The engine cache is not a project root even though the engine writes a
    // `project.godot` copy into it: discovery must ascend past `.godot/` and
    // land on the real project, never adopt the cached copy's settings.
    for from in [
        cache.clone(),
        cache.join("imported"),
        cache.join("uid_cache.bin"),
    ] {
        let through_cache = resolver
            .resolve(&from, 8)
            .expect("a path inside the engine cache still resolves the real project");
        assert_eq!(
            through_cache.display_root(),
            fx.root.as_path(),
            "{} must resolve the real root, not the engine cache",
            from.display()
        );
        assert_eq!(through_cache.lease_key(), project.lease_key());
        assert_eq!(
            through_cache.settings().application().name(),
            Some("Fixture Quest"),
            "the engine-written cache copy must never supply project settings"
        );
    }

    // Discovery never traverses above the resolved root: the temp base holds no
    // marker, so the marker that *is* found is the nearest one at or below it.
    assert!(
        !fx.base.join(PROJECT_MARKER_NAME).exists(),
        "the fixture base must not itself contain a marker"
    );
    assert!(project.display_root().starts_with(&fx.base));

    // (2) INI semantics: application metadata.
    let settings = project.settings();
    assert_eq!(
        settings.provenance().source(),
        ProvenanceSource::ProjectMetadata
    );
    assert_eq!(settings.provenance().confidence(), Confidence::Exact);
    assert_eq!(settings.provenance().project_generation(), 8);
    let application = settings.application();
    assert_eq!(application.name(), Some("Fixture Quest"));
    assert_eq!(
        application.description(),
        Some("A committed fixture project used by the Todo 8 golden tests.")
    );
    assert_eq!(application.version(), Some("2.1.0"));
    assert_eq!(application.icon(), Some("res://icon.svg"));
    assert_eq!(application.features(), ["4.3", "Forward Plus"]);
    assert_eq!(settings.config_version(), Some(5));

    // (3) Main scene.
    let main_scene = settings.main_scene().expect("main scene is configured");
    assert_eq!(
        main_scene.target(),
        &ResourceTarget::ProjectRelative {
            res_uri: "res://main.tscn".to_owned(),
            path: "main.tscn".to_owned(),
        }
    );
    assert_eq!(main_scene.line(), 12);
    assert_eq!(
        main_scene.provenance().source(),
        ProvenanceSource::ProjectMetadata
    );
    assert_eq!(main_scene.provenance().confidence(), Confidence::Exact);

    // (4) Autoloads, in source order, with the `*` singleton marker honoured and
    // a `uid://` target reported as unresolved rather than guessed.
    let autoloads = settings.autoloads();
    assert_eq!(
        autoloads.iter().map(|a| a.name()).collect::<Vec<_>>(),
        vec!["GameState", "Music", "ComboUi"]
    );
    assert!(autoloads[0].is_singleton());
    assert_eq!(
        autoloads[0].target(),
        &ResourceTarget::ProjectRelative {
            res_uri: "res://scripts/game_state.gd".to_owned(),
            path: "scripts/game_state.gd".to_owned(),
        }
    );
    assert_eq!(autoloads[0].line(), 19);
    assert_eq!(autoloads[0].provenance().confidence(), Confidence::Exact);
    assert!(
        !autoloads[1].is_singleton(),
        "a value without a leading `*` is a plain preload, not a singleton"
    );
    assert_eq!(
        autoloads[2].target(),
        &ResourceTarget::Uid("uid://bddoi8q2q2olp".to_owned()),
        "a uid target is not resolvable without the Todo 11 index"
    );
    assert_eq!(
        autoloads[2].provenance().source(),
        ProvenanceSource::Unresolved
    );
    assert_eq!(
        autoloads[2].provenance().confidence(),
        Confidence::Unresolved
    );

    // (5) Input actions: names in source order, multi-line dictionary values
    // retained verbatim and never mistaken for new keys.
    let actions = settings.input_actions();
    assert_eq!(
        actions.iter().map(|a| a.name()).collect::<Vec<_>>(),
        vec!["jump", "crouch"]
    );
    assert!(
        actions[0].raw_value().contains("\"deadzone\": 0.5"),
        "the dictionary value must be retained verbatim: {:?}",
        actions[0].raw_value()
    );
    assert_eq!(actions[0].line(), 25);
    assert_eq!(actions[0].provenance().confidence(), Confidence::Exact);

    // (6) Enabled editor plugins.
    let plugins = settings.enabled_plugins();
    assert_eq!(plugins.len(), 1);
    assert_eq!(
        plugins[0].target(),
        &ResourceTarget::ProjectRelative {
            res_uri: "res://addons/pixel_tools/plugin.cfg".to_owned(),
            path: "addons/pixel_tools/plugin.cfg".to_owned(),
        }
    );
    assert_eq!(
        plugins[0].provenance().source(),
        ProvenanceSource::ProjectMetadata
    );

    // (7) Comments and unknown keys survive, remain queryable and carry honest
    // provenance.
    assert!(
        settings
            .comments()
            .iter()
            .any(|c| c.text().contains("Comments and unknown keys must survive")),
        "comments must be preserved: {:?}",
        settings.comments()
    );
    let unknown_key = settings
        .setting(Some("application"), "config/unknown_future_key")
        .expect("an unrecognized key inside a known section is retained");
    assert_eq!(unknown_key.raw_value(), "\"keep me\"");
    assert_eq!(
        unknown_key.interpretation(),
        SettingInterpretation::RetainedUnknown
    );
    assert_eq!(
        unknown_key.provenance().source(),
        ProvenanceSource::ProjectMetadata,
        "the raw key/value text is exactly known even when its meaning is not"
    );
    let unknown_section = settings
        .setting(Some("future_engine_section"), "some/unknown_setting")
        .expect("a key inside an unknown section is retained");
    assert_eq!(unknown_section.raw_value(), "42");
    assert_eq!(
        unknown_section.interpretation(),
        SettingInterpretation::RetainedUnknown
    );
    assert!(
        settings
            .unknown_settings()
            .iter()
            .any(|s| s.key() == "config/unknown_future_key"),
        "unknown keys must be enumerable"
    );
    // A recognized key is marked as interpreted.
    assert_eq!(
        settings
            .setting(Some("application"), "run/main_scene")
            .expect("main scene setting is retained")
            .interpretation(),
        SettingInterpretation::Interpreted
    );
    // Every raw entry keeps its source line so an agent can point at it.
    assert!(
        settings.settings().iter().all(|s| s.line() > 0),
        "every retained setting must carry its 1-based source line"
    );

    // (8) In-root source reads go through the held lease, not a display path.
    let script = project
        .read_source_file(Path::new("scripts/player.gd"))
        .expect("in-root descriptor-relative read");
    assert!(String::from_utf8_lossy(&script).contains("extends CharacterBody2D"));

    // (9) The parser never invokes Godot: no process-spawning API appears in the
    // project module at all.
    let module = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("project");
    let mut sources = Vec::new();
    for entry in fs::read_dir(&module).expect("project module directory") {
        let path = entry.expect("module entry").path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            sources.push(fs::read_to_string(&path).expect("module source"));
        }
    }
    assert!(!sources.is_empty(), "the project module must have sources");
    for source in &sources {
        for forbidden in ["std::process", "Command::new", "std::env::var"] {
            assert!(
                !source.contains(forbidden),
                "the project parser must not reference {forbidden}"
            );
        }
    }
}

// ===========================================================================
// Descriptor-owned failure case
// ===========================================================================

#[test]
fn project_identity_failure_paths() {
    // --- (1) a symlink component resolving to the SAME object is rejected ----
    {
        let fx = fixture();
        let resolver = ProjectResolver::new(&fx.state);
        let held = resolver
            .resolve(&fx.root, 8)
            .expect("the real root resolves");

        let link_root = fx.base.join("linkroot");
        symlink(&fx.root, &link_root).expect("create same-object symlink alias");
        let error = resolver
            .resolve(&link_root, 8)
            .expect_err("a symlink component is rejected even for the already-held object");
        assert_eq!(
            error.error_kind(),
            ErrorKind::PermissionDenied,
            "symlink component => permission_denied, got {error:?}"
        );
        assert!(matches!(error, ProjectError::SymlinkComponent { .. }));

        // A symlinked *ancestor* is equally rejected, and so is a symlinked
        // nested script path.
        let link_base = fx.base.join("linkbase");
        symlink(&fx.base, &link_base).expect("symlink ancestor");
        let ancestor = resolver
            .resolve(&link_base.join("game"), 8)
            .expect_err("a symlinked ancestor component is rejected");
        assert!(matches!(ancestor, ProjectError::SymlinkComponent { .. }));
        assert_eq!(ancestor.error_kind(), ErrorKind::PermissionDenied);
        let nested = resolver
            .resolve(&link_root.join("scripts").join("player.gd"), 8)
            .expect_err("a nested script under a symlink component is rejected");
        assert!(matches!(nested, ProjectError::SymlinkComponent { .. }));

        // A symlink cancelled by a later `..` is still rejected: normalization
        // never erases a component before it has been inspected.
        let detour = fx.base.join("detour");
        fs::create_dir(&detour).expect("mkdir detour");
        let elided = fx.base.join("elided-link");
        symlink(&detour, &elided).expect("symlink cancelled by parent traversal");
        let cancelled = resolver
            .resolve(&elided.join("..").join("game"), 8)
            .expect_err("a symlink cancelled by `..` is still rejected");
        assert!(matches!(cancelled, ProjectError::SymlinkComponent { .. }));

        // The held project is untouched by the rejections.
        assert_eq!(
            resolver
                .resolve(&fx.root, 8)
                .expect("the real spelling still resolves")
                .lease_key(),
            held.lease_key()
        );
    }

    // --- (2) a symlinked project.godot is rejected before lease acquisition --
    {
        let fx = fixture();
        let outside = fx.base.join("project.godot");
        fs::write(&outside, b"[application]\n").expect("outside marker file");
        fs::remove_file(fx.root.join("project.godot")).expect("remove real marker");
        symlink(&outside, fx.root.join("project.godot")).expect("symlink the marker");

        let resolver = ProjectResolver::new(&fx.state);
        let error = resolver
            .resolve(&fx.root, 8)
            .expect_err("a symlinked project.godot is rejected");
        assert!(matches!(error, ProjectError::SymlinkComponent { .. }));
        assert_eq!(error.error_kind(), ErrorKind::PermissionDenied);
        assert!(
            !fx.state.exists(),
            "rejection must precede lease-state creation"
        );
    }

    // --- (3) a retargeted display path is detected before any file read ------
    {
        let fx = fixture();
        let resolver = ProjectResolver::new(&fx.state);
        let project = resolver
            .resolve(&fx.root, 8)
            .expect("resolve before the retarget");

        // Swap the display path to name a *different* directory object that also
        // contains a project.godot — the classic retarget.
        let evil = fx.base.join("evil");
        copy_tree(&fixture_project(), &evil);
        let aside = fx.base.join("game-old");
        fs::rename(&fx.root, &aside).expect("move the real root aside");
        fs::rename(&evil, &fx.root).expect("retarget the display path");

        let error = project
            .read_source_file(Path::new("scripts/player.gd"))
            .expect_err("a retargeted display path must be rejected");
        assert!(matches!(error, ProjectError::IdentityChanged));
        assert!(
            is_denied_or_conflict(error.error_kind()),
            "retarget => conflict/permission_denied, got {:?}",
            error.error_kind()
        );
        // Re-parsing through the retargeted path is refused too: the static DTO
        // never reacquires an effect handle by display path.
        let reread = project
            .reload_settings(8)
            .expect_err("re-reading project.godot after a retarget must be rejected");
        assert!(matches!(reread, ProjectError::IdentityChanged));
    }

    // --- (4) an outside-root path is rejected before any lease/file read -----
    {
        let fx = fixture();
        let resolver = ProjectResolver::new(&fx.state);
        let project = resolver
            .resolve(&fx.root, 8)
            .expect("resolve for in-root API");
        let secret = fx.base.join("secret.txt");
        fs::write(&secret, b"top secret").expect("write an out-of-root file");

        for outside in ["../secret.txt", "/etc/hostname", "scripts/../project.godot"] {
            let error = project
                .read_source_file(Path::new(outside))
                .expect_err("an outside-root path must be rejected");
            assert_eq!(
                error.error_kind(),
                ErrorKind::PermissionDenied,
                "{outside} => permission_denied, got {error:?}"
            );
        }

        // An in-root symlink escaping the root is not followed.
        symlink(&secret, fx.root.join("leak.gd")).expect("symlink inside the root");
        let leak = project
            .read_source_file(Path::new("leak.gd"))
            .expect_err("an in-root symlink escaping the root is rejected");
        assert!(is_denied_or_conflict(leak.error_kind()));
        assert_eq!(
            fs::read(&secret).expect("outside file untouched"),
            b"top secret"
        );

        // `.godot/` is engine cache, never project source.
        fs::create_dir(fx.root.join(".godot")).expect("mkdir .godot");
        fs::write(fx.root.join(".godot").join("uid_cache.bin"), b"cache")
            .expect("write engine cache");
        let cache = project
            .read_source_file(Path::new(".godot/uid_cache.bin"))
            .expect_err("the engine cache directory is not source");
        assert!(matches!(cache, ProjectError::EngineCache { .. }));
        assert_eq!(cache.error_kind(), ErrorKind::PermissionDenied);
    }

    // --- (5) no project, and a marker that is not a regular file -------------
    {
        let fx = fixture();
        let resolver = ProjectResolver::new(&fx.state);
        let empty = fx.base.join("no-project");
        fs::create_dir(&empty).expect("mkdir an empty directory");
        // The temp base has no `project.godot` above it, so discovery exhausts.
        let error = resolver
            .resolve(&empty, 8)
            .expect_err("a directory with no project.godot above it fails");
        assert!(matches!(error, ProjectError::NotFound { .. }));
        assert_eq!(error.error_kind(), ErrorKind::NotFound);

        let fx2 = fixture();
        let resolver2 = ProjectResolver::new(&fx2.state);
        fs::remove_file(fx2.root.join("project.godot")).expect("remove marker");
        fs::create_dir(fx2.root.join("project.godot")).expect("directory named project.godot");
        let not_regular = resolver2
            .resolve(&fx2.root, 8)
            .expect_err("a directory named project.godot is not a marker");
        assert!(
            !matches!(not_regular, ProjectError::Parse { .. }),
            "the marker type must be rejected before parsing, got {not_regular:?}"
        );
        assert!(
            !fx2.state.exists(),
            "marker validation must precede lease-state creation"
        );
    }

    // --- (6) malformed project.godot is reported, never silently accepted ----
    {
        let fx = fixture();
        fs::write(
            fx.root.join("project.godot"),
            b"[application\nconfig/name=\"Broken\"\n=novalue\nstray line without equals\n",
        )
        .expect("write a malformed manifest");
        let resolver = ProjectResolver::new(&fx.state);
        let project = resolver
            .resolve(&fx.root, 8)
            .expect("a malformed manifest still yields an identity");
        let settings = project.settings();
        assert!(
            !settings.diagnostics().is_empty(),
            "malformed lines must produce diagnostics"
        );
        assert_eq!(
            settings.provenance().confidence(),
            Confidence::Inferred,
            "a manifest with diagnostics may not claim exactness"
        );
        assert_eq!(
            settings.provenance().source(),
            ProvenanceSource::Heuristic,
            "partial parse evidence is heuristic"
        );
        assert!(
            settings.diagnostics().iter().all(|d| d.line() > 0),
            "every diagnostic must point at a 1-based source line"
        );
        // Each defect is classified, not lumped into one generic bucket.
        let kinds: Vec<ProjectDiagnosticKind> =
            settings.diagnostics().iter().map(|d| d.kind()).collect();
        assert!(kinds.contains(&ProjectDiagnosticKind::MalformedSectionHeader));
        assert!(kinds.contains(&ProjectDiagnosticKind::EmptyKey));
        assert!(kinds.contains(&ProjectDiagnosticKind::MissingAssignment));

        // A key repeated inside one section is reported; the first declaration
        // wins, matching the engine, and the duplicate is never silently merged.
        let fx3 = fixture();
        fs::write(
            fx3.root.join("project.godot"),
            b"[autoload]\nGameState=\"res://scripts/game_state.gd\"\nGameState=\"res://scripts/music.gd\"\n",
        )
        .expect("write a duplicated key");
        let resolver3 = ProjectResolver::new(&fx3.state);
        let duplicated = resolver3
            .resolve(&fx3.root, 8)
            .expect("a duplicated key still yields an identity");
        let duplicated = duplicated.settings();
        assert_eq!(duplicated.autoloads().len(), 1, "first declaration wins");
        assert_eq!(
            duplicated.autoloads()[0].target(),
            &ResourceTarget::ProjectRelative {
                res_uri: "res://scripts/game_state.gd".to_owned(),
                path: "scripts/game_state.gd".to_owned(),
            }
        );
        assert!(
            duplicated
                .diagnostics()
                .iter()
                .any(|d| d.kind() == ProjectDiagnosticKind::DuplicateKey && d.line() == 3),
            "the duplicate must be reported at its own line: {:?}",
            duplicated.diagnostics()
        );
        assert_eq!(duplicated.provenance().confidence(), Confidence::Inferred);
        assert_eq!(
            duplicated.settings().len(),
            2,
            "both declarations stay queryable as raw settings"
        );

        // Non-UTF-8 bytes are rejected rather than lossily reinterpreted.
        let fx2 = fixture();
        fs::write(fx2.root.join("project.godot"), [0x5b, 0xff, 0xfe, 0x5d])
            .expect("write invalid UTF-8");
        let resolver2 = ProjectResolver::new(&fx2.state);
        let error = resolver2
            .resolve(&fx2.root, 8)
            .expect_err("a non-UTF-8 manifest is rejected");
        assert!(matches!(error, ProjectError::NotUtf8 { .. }));
        assert_eq!(error.error_kind(), ErrorKind::InvalidInput);
    }

    // --- (7) a res:// value escaping the project root is never mapped --------
    {
        let fx = fixture();
        fs::write(
            fx.root.join("project.godot"),
            b"[application]\nrun/main_scene=\"res://../outside/main.tscn\"\n\n[autoload]\nEscape=\"*res://../evil.gd\"\n",
        )
        .expect("write an escaping manifest");
        let resolver = ProjectResolver::new(&fx.state);
        let project = resolver
            .resolve(&fx.root, 8)
            .expect("resolve escaping project");
        let settings = project.settings();
        let main_scene = settings.main_scene().expect("main scene present");
        assert!(
            matches!(main_scene.target(), ResourceTarget::Unsupported(_)),
            "a res:// value escaping the root must not be mapped: {:?}",
            main_scene.target()
        );
        assert_eq!(
            main_scene.provenance().source(),
            ProvenanceSource::Unresolved
        );
        assert!(matches!(
            settings.autoloads()[0].target(),
            ResourceTarget::Unsupported(_)
        ));
    }
}
