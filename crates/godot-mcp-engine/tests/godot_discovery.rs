//! Todo 13 acceptance suite: discover, pin and feature-negotiate Godot
//! executables.
//!
//! The suite never touches the network and never needs a real Godot
//! installation: every probe runs against a committed fake executable under
//! `tests/fixtures/godot-binaries/`, and every compatibility fact comes from the
//! committed `tests/fixtures/godot-releases.lock.json`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use godot_mcp_domain::ErrorKind;
use godot_mcp_engine::discovery::{
    CandidateOrigin, DiscoveryError, EnvironmentSnapshot, HostPlatform, discover,
    platform_candidates, resolution_order,
};
use godot_mcp_engine::probe::{
    BuildKind, CacheOutcome, ProbeCache, ProbeError, ProbeRequest, executable_digest, hex_digest,
};
use godot_mcp_engine::releases::{
    ArtifactKind, ExactVersion, Platform, ReleaseLock, ReleaseLockError,
};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Shared fixture helpers
// ---------------------------------------------------------------------------

/// Repository root, derived from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the engine crate lives at <repo>/crates/godot-mcp-engine")
        .to_path_buf()
}

fn committed_lock_bytes() -> Vec<u8> {
    let path = repo_root().join(ReleaseLock::COMMITTED_PATH);
    std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn committed_lock() -> ReleaseLock {
    ReleaseLock::from_json_slice(&committed_lock_bytes()).expect("the committed lock file is valid")
}

/// Copy one committed fake-Godot scenario into `dir` under `name`, returning the
/// executable path. On Unix the shell fake is used; on Windows the batch fake.
fn install_fake(dir: &Path, scenario: &str, name: &str) -> PathBuf {
    let (subdir, extension) = if cfg!(windows) {
        ("windows", "cmd")
    } else {
        ("unix", "sh")
    };
    let source = repo_root()
        .join("tests/fixtures/godot-binaries")
        .join(subdir)
        .join(format!("{scenario}.{extension}"));
    let bytes = std::fs::read(&source).unwrap_or_else(|e| panic!("read {}: {e}", source.display()));
    let target = dir.join(format!("{name}.{extension}"));
    std::fs::write(&target, &bytes).expect("write the fake executable");
    make_executable(&target);
    settle_executable(&target);
    target
}

/// Wait until `path` can be executed. A child another test thread forks while
/// the fake is still open for writing inherits that descriptor until it execs,
/// and exec'ing a file some process holds open for writing fails with
/// `ETXTBSY`. The write is closed by now, so no new writer can appear: one
/// successful exec (every fake exits at once without arguments) proves every
/// later one succeeds too. Only a Unix exec can be refused this way.
#[cfg(unix)]
fn settle_executable(path: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match std::process::Command::new(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Ok(_) => return,
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("execute the fake {}: {error}", path.display()),
        }
    }
}

#[cfg(not(unix))]
fn settle_executable(_path: &Path) {}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)
        .expect("stat the fake")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("chmod the fake");
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

fn probe_request(binary: &Path) -> ProbeRequest {
    ProbeRequest {
        binary: binary.to_path_buf(),
        deadline: Duration::from_secs(20),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_digest(&hasher.finalize().into())
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn godot_discovery_happy_path() {
    // -- 1. The committed lock file is the sole compatibility source. --------
    let lock = committed_lock();
    let versions: Vec<String> = lock
        .releases()
        .iter()
        .map(|release| release.version.to_string())
        .collect();
    assert_eq!(
        versions,
        vec![
            "4.4.1".to_string(),
            "4.5.1".to_string(),
            "4.6.1".to_string(),
            "4.7.1".to_string()
        ],
        "the lock file pins exactly the four supported patch releases"
    );
    assert_eq!(
        lock.minimum_supported_version(),
        ExactVersion::new(4, 4, 0),
        "the supported floor is 4.4"
    );

    // Every required platform artifact appears exactly once per release, with a
    // 64-hex lowercase SHA-256 and a unique download URL.
    let mut urls: BTreeSet<&str> = BTreeSet::new();
    for release in lock.releases() {
        for platform in lock.required_platforms() {
            let matching: Vec<_> = release
                .artifacts
                .iter()
                .filter(|artifact| artifact.platform == *platform)
                .collect();
            assert_eq!(
                matching.len(),
                1,
                "{} must pin exactly one {:?} artifact",
                release.version,
                platform
            );
            let artifact = matching[0];
            assert_eq!(artifact.sha256.len(), 64, "{} sha256 length", artifact.name);
            assert!(
                artifact
                    .sha256
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{} sha256 must be lowercase hex",
                artifact.name
            );
            assert!(
                artifact.url.contains(&release.git_tag),
                "{} URL must name its own release tag",
                artifact.name
            );
            assert!(
                urls.insert(artifact.url.as_str()),
                "duplicate artifact URL {}",
                artifact.url
            );
        }
    }
    assert_eq!(urls.len(), 16, "4 releases x 4 required platform artifacts");
    assert_eq!(
        lock.artifact("4.7.1", Platform::AllPlatforms)
            .map(|artifact| artifact.kind),
        Some(ArtifactKind::ExportTemplates),
        "the .tpz artifact is classified as export templates"
    );
    assert_eq!(
        lock.artifact("4.7.1", Platform::LinuxX86_64)
            .map(|artifact| artifact.kind),
        Some(ArtifactKind::Editor)
    );

    // A major.minor alias is not a lock entry and is never resolvable.
    assert!(lock.release("4.7").is_none(), "4.7 is an alias, not a pin");
    assert!(lock.release("4.7.1").is_some());

    // -- 2. Explicit configuration resolves before platform candidates. ------
    let workspace = tempfile::tempdir().expect("tempdir");
    let explicit = install_fake(workspace.path(), "godot-4.7.1-editor", "explicit-godot");
    let env = EnvironmentSnapshot {
        home: Some(workspace.path().join("home")),
        path_dirs: vec![workspace.path().join("bin")],
        program_files: Some(workspace.path().join("Program Files")),
        local_app_data: Some(workspace.path().join("AppData/Local")),
    };
    let order = resolution_order(Some(explicit.as_path()), HostPlatform::Linux, &env);
    assert_eq!(order[0].origin, CandidateOrigin::ExplicitConfig);
    assert_eq!(order[0].path, explicit);
    assert!(
        order[1..]
            .iter()
            .all(|candidate| candidate.origin == CandidateOrigin::PlatformCandidate),
        "everything after the explicit entry is a platform candidate"
    );
    let resolved = discover(Some(explicit.as_path()), HostPlatform::Linux, &env)
        .expect("the explicitly configured binary resolves");
    assert_eq!(resolved.origin, CandidateOrigin::ExplicitConfig);
    assert_eq!(resolved.path, explicit);

    // Platform candidate shapes exist for all three host platforms.
    for (platform, expected) in [
        (HostPlatform::Linux, "/usr/local/bin/godot"),
        (
            HostPlatform::MacOs,
            "/Applications/Godot.app/Contents/MacOS/Godot",
        ),
        (HostPlatform::Windows, r"Godot\Godot.exe"),
    ] {
        let candidates = platform_candidates(platform, &env);
        assert!(
            candidates
                .iter()
                .any(|path| path.to_string_lossy().contains(expected)),
            "{platform:?} candidates must include {expected}: {candidates:?}"
        );
    }

    // -- 3. Probing the fake 4.7.1 editor advertises the expected commands. --
    let mut cache = ProbeCache::new(lock.clone());
    let (snapshot, outcome) = cache
        .snapshot(&probe_request(&explicit))
        .await
        .expect("the fake 4.7.1 editor probes cleanly");
    assert_eq!(outcome, CacheOutcome::Fresh);
    assert_eq!(snapshot.version, ExactVersion::new(4, 7, 1));
    assert_eq!(snapshot.build_kind, BuildKind::Editor);
    assert!(
        snapshot.version_line.starts_with("4.7.1.stable"),
        "the raw --version line is retained: {}",
        snapshot.version_line
    );
    for command in [
        "--help",
        "--version",
        "--verbose",
        "--headless",
        "--path",
        "--quit",
        "--quit-after",
        "--script",
        "--check-only",
        "--editor",
        "--import",
        "--export-release",
        "--export-debug",
        "--export-pack",
        "--doctool",
        "--debug-server",
    ] {
        assert!(
            snapshot.advertises(command),
            "the 4.7.1 editor must advertise {command}: {:?}",
            snapshot.capabilities
        );
    }
    assert!(
        !snapshot.advertises("--main-pack"),
        "editor help does not advertise the template-only --main-pack flag"
    );

    // The snapshot is keyed by the binary's own content hash plus its version.
    let observed = sha256_hex(&std::fs::read(&explicit).expect("read the fake"));
    assert_eq!(snapshot.key.binary_sha256, observed);
    assert_eq!(
        snapshot.key.binary_sha256,
        hex_digest(&executable_digest(&explicit).expect("handle-safe digest"))
    );
    assert_eq!(snapshot.key.version, ExactVersion::new(4, 7, 1));

    // -- 4. The snapshot is cached by hash+version and invalidated by bytes. -
    let spawns_after_first = cache.spawn_count();
    let (again, outcome) = cache
        .snapshot(&probe_request(&explicit))
        .await
        .expect("the cached snapshot is reused");
    assert_eq!(outcome, CacheOutcome::Hit);
    assert_eq!(again.key, snapshot.key);
    assert_eq!(
        cache.spawn_count(),
        spawns_after_first,
        "a cache hit must not spawn the binary again"
    );

    // Replacing the binary bytes in place invalidates the snapshot: the key is
    // derived from content, so the stale entry can never be served.
    let replacement = install_fake(workspace.path(), "godot-4.6.1-editor", "replacement");
    std::fs::copy(&replacement, &explicit).expect("overwrite the fake in place");
    make_executable(&explicit);
    settle_executable(&explicit);
    let (fresh, outcome) = cache
        .snapshot(&probe_request(&explicit))
        .await
        .expect("the changed binary is re-probed");
    assert_eq!(
        outcome,
        CacheOutcome::Fresh,
        "a changed binary invalidates its snapshot"
    );
    assert_ne!(fresh.key.binary_sha256, snapshot.key.binary_sha256);
    assert_eq!(fresh.version, ExactVersion::new(4, 6, 1));
    assert!(cache.spawn_count() > spawns_after_first);

    // -- 5. The whole supported matrix probes as supported editors. ----------
    for (scenario, expected) in [
        ("godot-4.4.1-editor", ExactVersion::new(4, 4, 1)),
        ("godot-4.5.1-editor", ExactVersion::new(4, 5, 1)),
        ("godot-4.6.1-editor", ExactVersion::new(4, 6, 1)),
        ("godot-4.7.1-editor", ExactVersion::new(4, 7, 1)),
    ] {
        let binary = install_fake(workspace.path(), scenario, scenario);
        let (snapshot, _) = cache
            .snapshot(&probe_request(&binary))
            .await
            .unwrap_or_else(|error| panic!("{scenario} must probe cleanly: {error}"));
        assert_eq!(snapshot.version, expected);
        assert_eq!(snapshot.build_kind, BuildKind::Editor);
        assert!(
            lock.supports(&snapshot.version),
            "{expected} is in the pinned matrix"
        );
        assert!(
            lock.release(&expected.to_string()).is_some(),
            "the probed version resolves to its exact lock entry"
        );
    }

    // -- 6. An export-template build is classified as such, not as an editor. -
    let template = install_fake(workspace.path(), "godot-4.7.1-template", "template");
    let template_snapshot = cache
        .probe_uncached(&probe_request(&template))
        .await
        .expect("the template probes cleanly as a template");
    assert_eq!(template_snapshot.build_kind, BuildKind::ExportTemplate);
    assert!(template_snapshot.advertises("--main-pack"));
    assert!(!template_snapshot.advertises("--editor"));

    // -- 7. A matching matrix download verifies against the pinned digest. ---
    let artifact = lock
        .artifact("4.7.1", Platform::LinuxX86_64)
        .expect("4.7.1 linux artifact");
    let payload = b"pretend these are the official release bytes";
    let mut hasher = Sha256::new();
    hasher.update(payload);
    let payload_digest = hex_digest(&hasher.finalize().into());
    assert_ne!(
        payload_digest, artifact.sha256,
        "the placeholder payload is deliberately not the official artifact"
    );
    let verified = lock.verify_download("4.7.1", Platform::LinuxX86_64, artifact.sha256.as_bytes());
    assert!(
        verified.is_err(),
        "arbitrary bytes never satisfy the pinned digest"
    );
}

// ---------------------------------------------------------------------------
// Failure paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn godot_discovery_failure_paths() {
    let lock = committed_lock();
    let workspace = tempfile::tempdir().expect("tempdir");
    let mut cache = ProbeCache::new(lock.clone());

    // -- 1. A template that silently ignores --editor is `unsupported`. ------
    let template = install_fake(workspace.path(), "godot-4.7.1-template", "template");
    // The fake really does accept the editor flag with a success status, so
    // exit codes prove nothing about the capability.
    let ignored = tokio::process::Command::new(&template)
        .arg("--editor")
        .output()
        .await
        .expect("run the template fake with --editor");
    assert!(
        ignored.status.success() && ignored.stdout.is_empty(),
        "the fixture must silently ignore --editor with a success status"
    );
    let error = cache
        .require_editor(&probe_request(&template))
        .await
        .expect_err("an export template can never satisfy an editor requirement");
    assert_eq!(
        error.error_kind(),
        ErrorKind::Unsupported,
        "a missing editor capability is `unsupported`, not a success"
    );
    assert!(
        matches!(error, ProbeError::MissingCapability { .. }),
        "the probe names the missing capability: {error:?}"
    );
    assert!(
        error.remediation().contains("editor"),
        "the remediation names the editor build requirement: {}",
        error.remediation()
    );

    // -- 2. A build below the 4.4 floor is a clear `unsupported`. ------------
    let old = install_fake(workspace.path(), "godot-4.3.0-editor", "old");
    let error = cache
        .snapshot(&probe_request(&old))
        .await
        .expect_err("4.3.0 is below the supported floor");
    assert_eq!(error.error_kind(), ErrorKind::Unsupported);
    assert!(
        matches!(error, ProbeError::UnsupportedVersion { .. }),
        "{error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains("4.3.0") && message.contains("4.4"),
        "the message names both the found version and the floor: {message}"
    );

    // -- 3. A malformed --version line is never trusted. --------------------
    let malformed = install_fake(workspace.path(), "godot-malformed-version", "malformed");
    let error = cache
        .snapshot(&probe_request(&malformed))
        .await
        .expect_err("an unparsable version string cannot be accepted");
    assert_eq!(error.error_kind(), ErrorKind::Unsupported);
    assert!(
        matches!(error, ProbeError::UnparsableVersion { .. }),
        "{error:?}"
    );

    // -- 4. A hanging --version fails by deadline, not by hanging forever. --
    let hanging = install_fake(workspace.path(), "godot-hanging-version", "hanging");
    let error = cache
        .snapshot(&ProbeRequest {
            binary: hanging.clone(),
            deadline: Duration::from_millis(750),
        })
        .await
        .expect_err("a hanging probe must time out");
    assert_eq!(error.error_kind(), ErrorKind::Timeout);
    assert!(
        matches!(error, ProbeError::ProbeTimeout { .. }),
        "{error:?}"
    );

    // -- 5. A build that rejects --help cannot be feature-negotiated. -------
    let unknown = install_fake(workspace.path(), "godot-unknown-flag", "unknown");
    let error = cache
        .snapshot(&probe_request(&unknown))
        .await
        .expect_err("capability negotiation requires a usable --help");
    assert_eq!(error.error_kind(), ErrorKind::Unsupported);
    assert!(
        matches!(error, ProbeError::HelpRejected { .. }),
        "{error:?}"
    );

    // -- 6. An absent binary is `not_found` and nothing is downloaded. ------
    let env = EnvironmentSnapshot {
        home: Some(workspace.path().join("home")),
        path_dirs: vec![workspace.path().join("bin")],
        program_files: Some(workspace.path().join("Program Files")),
        local_app_data: Some(workspace.path().join("AppData/Local")),
    };
    let missing = workspace.path().join("definitely-absent-godot");
    let error = discover(Some(missing.as_path()), HostPlatform::Linux, &env)
        .expect_err("an absent binary cannot be discovered");
    assert_eq!(error.error_kind(), ErrorKind::NotFound);
    assert!(
        matches!(error, DiscoveryError::NotFound { .. }),
        "{error:?}"
    );
    let remediation = error.remediation().to_ascii_lowercase();
    assert!(
        remediation.contains("install") && remediation.contains("configure"),
        "the remediation tells the operator to install/configure Godot: {remediation}"
    );
    assert!(
        !remediation.contains("download"),
        "the engine never offers to download Godot: {remediation}"
    );
    // Mechanical proof that no automatic download is even possible: the engine
    // manifest declares no HTTP/download client.
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .expect("read the engine manifest");
    for forbidden in ["reqwest", "hyper", "ureq", "curl", "isahc", "attohttpc"] {
        assert!(
            !manifest.contains(forbidden),
            "the engine must not depend on {forbidden}; a silent download must be impossible"
        );
    }

    // -- 7. A non-regular / symlinked target is refused before execution. ---
    let directory = workspace.path().join("a-directory");
    std::fs::create_dir(&directory).expect("create the directory");
    let error = executable_digest(&directory).expect_err("a directory is not an executable");
    assert!(
        matches!(error, ProbeError::NotRegularFile { .. }),
        "{error:?}"
    );

    #[cfg(unix)]
    {
        let real = install_fake(workspace.path(), "godot-4.7.1-editor", "symlink-target");
        let link = workspace.path().join("godot-symlink");
        std::os::unix::fs::symlink(&real, &link).expect("create the symlink");
        let error = executable_digest(&link)
            .expect_err("a symlinked final component is never followed for a probe");
        assert_eq!(error.error_kind(), ErrorKind::PermissionDenied);
        assert!(
            matches!(error, ProbeError::SymlinkRejected { .. }),
            "{error:?}"
        );
    }

    // -- 8. A corrupted matrix download fails before any execution. ---------
    let artifact = lock
        .artifact("4.7.1", Platform::LinuxX86_64)
        .expect("4.7.1 linux artifact");
    let corrupt = b"corrupted archive bytes";
    let error = lock
        .verify_download("4.7.1", Platform::LinuxX86_64, corrupt)
        .expect_err("a corrupted download must fail closed");
    assert_eq!(error.error_kind(), ErrorKind::Conflict);
    match &error {
        ReleaseLockError::CorruptDownload {
            expected, actual, ..
        } => {
            assert_eq!(expected, &artifact.sha256);
            assert_eq!(actual, &sha256_hex(corrupt));
        }
        other => panic!("expected a corrupt-download error, got {other:?}"),
    }
    // The corrupt bytes never produce an execution plan, so nothing can run.
    assert!(
        lock.plan_execution("4.7.1", Platform::LinuxX86_64, corrupt)
            .is_err(),
        "a corrupt download can never be promoted into an execution plan"
    );

    // -- 9. Alias, duplicate and unknown lock entries are all rejected. -----
    let text = String::from_utf8(committed_lock_bytes()).expect("utf-8 lock file");
    let aliased = text.replace("\"version\": \"4.7.1\"", "\"version\": \"4.7\"");
    assert_ne!(aliased, text, "the alias mutation applied");
    let error = ReleaseLock::from_json_slice(aliased.as_bytes())
        .expect_err("a major.minor alias is never a lock entry");
    assert_eq!(error.error_kind(), ErrorKind::InvalidInput);
    assert!(
        matches!(error, ReleaseLockError::AliasVersion { .. }),
        "{error:?}"
    );

    let duplicated = text.replace(
        "https://github.com/godotengine/godot/releases/download/4.6.1-stable/Godot_v4.6.1-stable_linux.x86_64.zip",
        "https://github.com/godotengine/godot/releases/download/4.7.1-stable/Godot_v4.7.1-stable_linux.x86_64.zip",
    );
    assert_ne!(duplicated, text, "the duplicate-URL mutation applied");
    let error = ReleaseLock::from_json_slice(duplicated.as_bytes())
        .expect_err("two lock entries may never share a download URL");
    assert!(
        matches!(error, ReleaseLockError::DuplicateArtifactUrl { .. }),
        "{error:?}"
    );

    let truncated = text.replace(
        &lock
            .artifact("4.5.1", Platform::WindowsX86_64)
            .expect("4.5.1 windows artifact")
            .sha256
            .clone(),
        "abc123",
    );
    assert_ne!(truncated, text, "the short-digest mutation applied");
    let error = ReleaseLock::from_json_slice(truncated.as_bytes())
        .expect_err("a shortened digest is never accepted");
    assert!(
        matches!(error, ReleaseLockError::MalformedDigest { .. }),
        "{error:?}"
    );

    let error = lock
        .verify_download("4.2.2", Platform::LinuxX86_64, b"anything")
        .expect_err("an unpinned version has no artifact to verify against");
    assert_eq!(error.error_kind(), ErrorKind::Unsupported);
    assert!(
        matches!(error, ReleaseLockError::UnknownVersion { .. }),
        "{error:?}"
    );

    // -- 10. `ExactVersion` refuses anything that is not an exact patch. ----
    for alias in ["4.4", "4", "4.4.x", "", "4.4.1-stable", "v4.4.1"] {
        assert!(
            ExactVersion::parse(alias).is_err(),
            "{alias:?} is not an exact patch version"
        );
    }
    assert_eq!(
        ExactVersion::parse("4.4.1").expect("4.4.1 parses"),
        ExactVersion::new(4, 4, 1)
    );
}
