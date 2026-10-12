//! Todo 34 packaging internals: the deterministic archive writers, the
//! lock-derived SBOM, and the Task-34 wrapper's four-step lifecycle contract.
//!
//! `package_and_repair.rs` owns the two acceptance cases `qa/tasks/34.json` names.
//! This file covers the pieces underneath them that the acceptance cases rely on
//! but cannot isolate:
//!
//! * the tar/gzip and zip writers, including the **Windows** zip layout, which is
//!   exercised on every host precisely because the Windows archive itself cannot
//!   be built here;
//! * archive determinism at the byte level, and the extraction guard against an
//!   escaping entry path;
//! * the lock parser's linked-crate closure, which is what the release-graph audit
//!   treats as the release binary's linked-crate list;
//! * that `make qa-task TASK=34`'s runner really implements the normative
//!   four-step setup / run-all / unconditional-cleanup / strict-exit order,
//!   asserted against the runner's own source rather than assumed;
//! * that the packaged crate's advertised `[package] description` names only
//!   command tokens the shipped binary actually defines, with the surface derived
//!   from the binary's own source, so a manifest can never advertise a subcommand
//!   or a transport that does not exist.

use std::path::{Path, PathBuf};

use godot_mcp_cli::audit;
use godot_mcp_cli::package::archive::{
    self, ArchiveEntry, extract_tar_gz, read_gzip, read_tar, read_zip, write_gzip, write_tar,
    write_tar_gz, write_zip,
};
use godot_mcp_cli::package::lock::CargoLock;
use godot_mcp_cli::package::{self, ReleaseTarget, sbom};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root above crates/godot-mcp-cli")
        .to_path_buf()
}

fn sample() -> Vec<ArchiveEntry> {
    vec![
        ArchiveEntry {
            path: "pkg/MANIFEST.json".to_string(),
            bytes: b"{}\n".to_vec(),
            mode: 0o644,
        },
        ArchiveEntry {
            path: "pkg/bin/godot-mcp".to_string(),
            bytes: vec![0x7F, b'E', b'L', b'F', 1, 2, 3, 4, 5],
            mode: 0o755,
        },
        ArchiveEntry {
            path: "pkg/schemas/nested/deep.json".to_string(),
            bytes: vec![b'x'; 70_000],
            mode: 0o644,
        },
    ]
}

#[test]
fn tar_gz_round_trips_every_entry_verbatim() {
    let entries = sample();
    let archive = write_tar_gz(&entries).expect("the archive is writable");
    let restored =
        read_tar(&read_gzip(&archive).expect("the member decodes")).expect("the tar stream parses");
    assert_eq!(restored, entries);
}

#[test]
fn gzip_spans_multiple_stored_blocks_for_a_large_payload() {
    // A stored deflate block carries at most 0xFFFF bytes, so a larger payload
    // must be split; a reader that only handled one block would silently truncate.
    let payload = vec![b'z'; 200_000];
    let member = write_gzip(&payload);
    assert_eq!(read_gzip(&member).expect("the member decodes"), payload);
}

#[test]
fn archive_bytes_are_a_pure_function_of_the_payload() {
    let entries = sample();
    assert_eq!(
        write_tar_gz(&entries).expect("first"),
        write_tar_gz(&entries).expect("second"),
        "no build clock, uid or compression choice may leak into the bytes"
    );
    assert_eq!(
        write_zip(&entries).expect("first"),
        write_zip(&entries).expect("second")
    );
}

#[test]
fn tar_records_no_owner_and_no_timestamp() {
    let stream = write_tar(&sample()).expect("the tar stream is writable");
    // uid, gid and mtime occupy bytes 108..116, 116..124 and 136..148 of the
    // header; all three must be zero for the archive to be reproducible.
    for range in [108_usize..116, 116..124, 136..148] {
        let field = &stream[range.clone()];
        let digits: String = field
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .map(|byte| char::from(*byte))
            .collect();
        assert_eq!(
            digits.trim_start_matches('0'),
            "",
            "header field {range:?} must be zero, got {digits:?}"
        );
    }
}

#[test]
fn the_windows_zip_layout_is_written_and_readable_on_every_host() {
    // The Windows archive cannot be built on a Linux host, but its container
    // format is platform-independent and is verified everywhere so a broken
    // writer is caught before a real runner ever uses it.
    let entries = sample();
    let archive = write_zip(&entries).expect("the zip is writable");
    assert_eq!(&archive[..4], b"PK\x03\x04");
    let restored = read_zip(&archive).expect("the zip parses");
    let names: Vec<&str> = restored.iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "pkg/MANIFEST.json",
            "pkg/bin/godot-mcp",
            "pkg/schemas/nested/deep.json"
        ]
    );
    for (original, parsed) in entries.iter().zip(restored.iter()) {
        assert_eq!(original.bytes, parsed.bytes);
    }
    assert_eq!(ReleaseTarget::WindowsX86_64Msvc.archive_extension(), "zip");
    assert_eq!(
        ReleaseTarget::WindowsX86_64Msvc.executable_name(),
        "godot-mcp.exe"
    );
}

#[test]
fn extraction_recreates_the_tree_and_refuses_an_escaping_entry() {
    let temp = tempfile::tempdir().expect("temp dir");
    let entries = sample();
    let archive = write_tar_gz(&entries).expect("the archive is writable");
    let written = extract_tar_gz(&archive, temp.path()).expect("the archive extracts");
    assert_eq!(written.len(), entries.len());
    assert_eq!(
        std::fs::read(temp.path().join("pkg/schemas/nested/deep.json")).expect("read"),
        vec![b'x'; 70_000]
    );

    for escaping in ["../escaped.txt", "pkg/../../escaped.txt", "/absolute.txt"] {
        let hostile = vec![ArchiveEntry {
            path: escaping.to_string(),
            bytes: b"hostile".to_vec(),
            mode: 0o644,
        }];
        let archive = write_tar_gz(&hostile).expect("the archive is writable");
        let refusal =
            extract_tar_gz(&archive, temp.path()).expect_err("an escaping entry path is refused");
        assert!(refusal.contains(escaping), "{refusal}");
    }
}

#[test]
fn a_compressed_gzip_member_is_refused_rather_than_mis_decoded() {
    // Only the stored-block flavour this module writes is supported. A dynamic
    // Huffman member must be reported, never partially decoded.
    let mut member = write_gzip(b"payload").to_vec();
    member[10] |= 0b0000_0010;
    let refusal = read_gzip(&member).expect_err("a compressed block is refused");
    assert!(refusal.contains("stored blocks"), "{refusal}");
}

#[test]
fn a_tampered_tar_header_checksum_is_refused() {
    let mut stream = write_tar(&sample()).expect("the tar stream is writable");
    stream[0] = b'X';
    let refusal = read_tar(&stream).expect_err("a bad checksum is refused");
    assert!(refusal.contains("checksum"), "{refusal}");
}

#[test]
fn the_linked_crate_closure_is_the_committed_lock_closure() {
    let locked =
        CargoLock::load(&workspace_root().join("Cargo.lock")).expect("the committed lock parses");
    let linked = locked
        .linked_crates(package::RELEASE_ROOT_CRATE)
        .expect("the closure resolves");
    assert!(linked.contains(package::RELEASE_ROOT_CRATE));
    for expected in [
        "godot-mcp-domain",
        "godot-mcp-server",
        "godot-mcp-engine",
        "serde",
        "clap",
    ] {
        assert!(
            linked.contains(expected),
            "{expected} must be in the linked closure"
        );
    }
    // A crate reachable only from a non-shipping member must not appear.
    assert!(!linked.contains("qualification-tools"));
    // Every locked crate name is a superset of the closure.
    assert!(linked.is_subset(&locked.names()));
    // An unknown root is an error, never an empty closure that would vacuously
    // satisfy the audit.
    assert!(locked.linked_crates("not-a-crate").is_err());
}

#[test]
fn a_lock_naming_an_unlocked_dependency_is_an_error() {
    let locked = CargoLock::parse(
        "version = 4\n\n[[package]]\nname = \"root\"\nversion = \"0.1.0\"\n\
         dependencies = [\n \"absent\",\n]\n",
    )
    .expect("the lock text parses");
    let refusal = locked
        .linked_crates("root")
        .expect_err("a hole in the closure is an error");
    assert!(refusal.contains("absent"), "{refusal}");
}

#[test]
fn the_sbom_describes_the_linked_closure_deterministically() {
    let locked =
        CargoLock::load(&workspace_root().join("Cargo.lock")).expect("the committed lock parses");
    let first = sbom::from_cargo_lock(&locked, package::RELEASE_ROOT_CRATE, "godot-mcp", "0.1.0")
        .expect("the SBOM is derivable");
    let second = sbom::from_cargo_lock(&locked, package::RELEASE_ROOT_CRATE, "godot-mcp", "0.1.0")
        .expect("the SBOM is derivable");
    assert_eq!(
        sbom::to_bytes(&first).expect("bytes"),
        sbom::to_bytes(&second).expect("bytes"),
        "the SBOM must carry no clock and no random serial number"
    );
    assert!(
        first["serialNumber"]
            .as_str()
            .expect("a serial number")
            .starts_with("urn:uuid:"),
        "{}",
        first["serialNumber"]
    );

    let components = sbom::component_names(&first);
    let linked = locked
        .linked_crates(package::RELEASE_ROOT_CRATE)
        .expect("the closure resolves");
    for name in &components {
        assert!(
            linked.contains(name),
            "the SBOM must not describe a crate outside the linked closure: {name}"
        );
    }
    for name in &linked {
        assert!(
            components.contains(name),
            "the SBOM must not omit a linked crate: {name}"
        );
    }
}

#[test]
fn only_the_host_platform_target_can_be_claimed_buildable() {
    let host = ReleaseTarget::host().expect("this host is a published target");
    for target in ReleaseTarget::ALL.iter().copied() {
        if target != host {
            assert!(
                !target.buildable_on_host(),
                "{} must not be claimed buildable on this host",
                target.triple()
            );
        }
        assert_eq!(ReleaseTarget::from_triple(target.triple()), Some(target));
    }
    assert_eq!(
        ReleaseTarget::from_triple("x86_64-unknown-linux-musl"),
        Some(ReleaseTarget::LinuxX86_64Musl)
    );
    assert_eq!(ReleaseTarget::from_triple("x86_64-unknown-linux-gnu"), None);
    assert_eq!(ReleaseTarget::from_triple("s390x-unknown-linux-gnu"), None);
}

#[test]
fn windows_release_builds_link_the_crt_statically() {
    let config = workspace_root().join(".cargo/config.toml");
    let text = std::fs::read_to_string(&config).unwrap_or_else(|error| {
        panic!(
            "{} must configure release linkage: {error}",
            config.display()
        )
    });
    let parsed: toml::Value = toml::from_str(&text).expect(".cargo/config.toml must be valid TOML");
    let rustflags = parsed["target"]["x86_64-pc-windows-msvc"]["rustflags"]
        .as_array()
        .expect("the Windows MSVC target must declare rustflags")
        .iter()
        .map(|flag| flag.as_str().expect("every rustflag is a string"))
        .collect::<Vec<_>>();
    assert_eq!(
        rustflags,
        ["-C", "target-feature=+crt-static"],
        "the Windows release must not depend on VCRUNTIME140.dll"
    );
}

#[test]
fn the_test_hook_binaries_are_derived_from_the_crate_manifest() {
    let hooks =
        package::test_hook_binary_names(&workspace_root()).expect("the crate manifest is readable");
    assert_eq!(
        hooks,
        vec![
            "docs-check".to_owned(),
            "qa-audit".to_owned(),
            "qa-fixture-34".to_owned(),
            "qa-host-invoke".to_owned(),
            "release-package".to_owned(),
        ],
        "every test-only binary must be feature-gated out of a release build"
    );
    assert!(
        !hooks.contains(&package::RELEASE_BINARY.to_owned()),
        "the shipping binary must not be feature-gated"
    );
}

/// The shipped binary's command surface, derived from its own source rather than
/// from a list kept beside it. The exact-equality assertion is the point: a new,
/// removed or renamed subcommand fails here and forces the declaration surface to
/// be re-checked in the same change.
#[test]
fn the_cli_command_surface_is_derived_from_the_binary_source() {
    let surface = audit::cli_subcommand_surface(&workspace_root())
        .expect("the binary source defines every subcommand enum");
    let found: Vec<&str> = surface.iter().map(String::as_str).collect();
    assert_eq!(
        found,
        vec![
            "assets",
            "assets install",
            "assets remove",
            "assets status",
            "assets update",
            "check-config",
            "self-check",
            "serve",
            "transaction",
            "transaction repair",
            "version",
        ],
        "the shipped `godot-mcp` surface is exactly these commands"
    );
}

/// The manifest description may advertise only commands that exist.
///
/// This is the regression guard for a defect the batch reviews rejected three
/// times: a shipped declaration surface advertising a cancelled or absent
/// capability (`project locate`/`init`/`configure`, an `addon install`, an HTTP
/// transport no subcommand reaches). The advertised set is compared against the
/// surface the binary itself defines, so the claim cannot drift from the code.
#[test]
fn the_crate_description_advertises_no_undefined_subcommand() {
    let root = workspace_root();
    let description =
        audit::cli_crate_description(&root).expect("the crate manifest declares a description");
    let surface = audit::cli_subcommand_surface(&root).expect("the binary source is readable");

    assert_eq!(
        audit::undefined_subcommands_advertised(&description, &surface),
        Vec::<String>::new(),
        "every backticked command token in {} must be a command the binary defines",
        audit::CLI_MANIFEST
    );

    for command in surface.iter().filter(|command| !command.contains(' ')) {
        assert!(
            description.contains(&format!("`{command}`"))
                || description.contains(&format!("`{command} ")),
            "{} must name the `{command}` subcommand it ships, alone or with its child",
            audit::CLI_MANIFEST
        );
    }
}

/// Every archive-producing build must compile the optional HTTP/auth path into
/// the shipped executable. Testing the Makefile itself prevents an all-features
/// workspace test from passing while `make dist` silently emits a stdio-only
/// binary.
#[test]
fn archive_builds_enable_the_shipping_transport_features() {
    let makefile = std::fs::read_to_string(workspace_root().join("Makefile"))
        .expect("the release Makefile is readable");
    for command in [
        "$(CARGO) build --locked --target $(RELEASE_TARGET) -p godot-mcp-cli --features http,auth --bin godot-mcp",
        "$(CARGO) build --locked --release --target $(RELEASE_TARGET) -p godot-mcp-cli --features http,auth --bin godot-mcp",
    ] {
        assert!(
            makefile.contains(command),
            "every host-package/dist/reproducibility executable build must include `{command}`"
        );
    }
    assert_eq!(
        makefile
            .matches(
                "--release --target $(RELEASE_TARGET) -p godot-mcp-cli --features http,auth --bin godot-mcp",
            )
            .count(),
        3,
        "dist and both clean reproducibility builds must use the shipping feature set"
    );
}

/// The same description, checked against a surface that deliberately omits the
/// advertised commands, must report every one of them — proving the check above
/// fails on a false claim rather than passing vacuously.
#[test]
fn an_advertised_subcommand_missing_from_the_surface_is_reported() {
    let description = audit::cli_crate_description(&workspace_root())
        .expect("the crate manifest declares a description");
    let empty = std::collections::BTreeSet::new();
    let reported = audit::undefined_subcommands_advertised(&description, &empty);
    assert!(
        reported.contains(&"serve".to_owned()) && reported.contains(&"assets install".to_owned()),
        "an empty surface must report every advertised command, got {reported:?}"
    );

    let surface = audit::cli_subcommand_surface(&workspace_root()).expect("the source is readable");
    assert_eq!(
        audit::undefined_subcommands_advertised(
            "advertises `addon install` and `project locate`",
            &surface
        ),
        vec!["addon install".to_owned(), "project locate".to_owned()],
        "a description naming an absent subcommand must be rejected by name"
    );
}

#[test]
fn collect_tree_is_ordered_and_skips_symlinks() {
    let temp = tempfile::tempdir().expect("temp dir");
    for relative in ["b.json", "a/nested.json", "a/b/deep.json"] {
        let path = temp.path().join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create");
        std::fs::write(&path, relative.as_bytes()).expect("write");
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(temp.path().join("b.json"), temp.path().join("link.json"))
        .expect("symlink");
    let entries = archive::collect_tree(temp.path(), "schemas").expect("the tree is collectable");
    let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "schemas/a/b/deep.json",
            "schemas/a/nested.json",
            "schemas/b.json"
        ],
        "the walk must be sorted and must not follow a symlink"
    );
}

/// The Task-34 wrapper lifecycle is normative, so it is asserted against the
/// runner's own source: setup first, every command run without an early stop,
/// cleanup unconditionally in a `finally`-equivalent path, and a final exit that
/// is nonzero if setup failed, any command mismatched, or cleanup failed.
#[test]
fn the_task_34_wrapper_runs_setup_then_every_command_then_cleanup_unconditionally() {
    let runner = std::fs::read_to_string(workspace_root().join("scripts/qa_task.py"))
        .expect("the qa-task runner is readable");

    let branch = runner
        .find("if d.get(\"wrapper\") == \"fixture-lifecycle\":")
        .expect("the runner has a fixture-lifecycle branch");
    let body = &runner[branch..];

    let setup = body.find("run(fl[\"setup\"])").expect("setup is run");
    let loop_start = body
        .find("for c in commands:")
        .expect("every descriptor command is run");
    let cleanup = body.find("run(fl[\"cleanup\"])").expect("cleanup is run");
    assert!(
        setup < loop_start && loop_start < cleanup,
        "the order must be setup, then every command, then cleanup"
    );

    // Cleanup sits at the branch's own indentation, so no failure above it can
    // skip it: it is not nested inside the `if setup_rc == 0` guard.
    let cleanup_line = body[..cleanup]
        .rfind('\n')
        .map(|index| &body[index + 1..cleanup])
        .expect("cleanup is on its own line");
    assert_eq!(
        cleanup_line.trim_start_matches(' ').len() + 8,
        cleanup_line.len(),
        "cleanup must run unconditionally, not inside the setup-success guard: {cleanup_line:?}"
    );

    // The loop records each result instead of returning early.
    let loop_body = &body[loop_start..cleanup];
    assert!(
        loop_body.contains("results.append(check("),
        "each command's result must be recorded: {loop_body}"
    );
    assert!(
        !loop_body.contains("break") && !loop_body.contains("sys.exit"),
        "the command loop must not stop early: {loop_body}"
    );

    // The final exit is the conjunction of all three phases.
    let final_exit = body
        .find("final_ok =")
        .expect("the runner computes its own final exit");
    let expression = &body[final_exit
        ..body[final_exit..]
            .find('\n')
            .map_or(body.len(), |offset| final_exit + offset)];
    for required in ["setup_rc == 0", "all(results)", "cleanup_rc == 0"] {
        assert!(
            expression.contains(required),
            "the final exit must account for {required}: {expression}"
        );
    }
}

/// `qa/tasks/34.json` still resolves the deterministic packaged binary for every
/// repair command, and the manifest path it names is the one the packager writes.
#[test]
fn every_task_34_repair_command_resolves_the_packaged_binary() {
    let descriptor = std::fs::read_to_string(workspace_root().join("qa/tasks/34.json"))
        .expect("the descriptor is readable");
    let value: serde_json::Value =
        serde_json::from_str(&descriptor).expect("the descriptor is valid json");
    let mut repair_commands = 0_usize;
    for key in ["happy", "failure"] {
        for entry in value[key].as_array().expect("a command array") {
            let command = entry["command"].as_str().expect("a command string");
            if !command.contains("transaction repair") {
                continue;
            }
            repair_commands += 1;
            assert!(
                command.contains("--bin qa-host-invoke"),
                "a repair command must go through the deterministic resolver: {command}"
            );
            assert!(
                command.contains(godot_mcp_cli::host_package::HOST_PACKAGE_MANIFEST_PATH),
                "a repair command must resolve the frozen manifest path: {command}"
            );
        }
    }
    // Six proven happy transitions and six composite refusals; the two remaining
    // descriptor commands are the in-process acceptance tests, not repair calls.
    assert_eq!(
        repair_commands, 12,
        "every packaged-binary repair command must be covered"
    );
}

/// A placeholder executable whose first bytes are the real container format the
/// target declares, so the shipped verifier's format check is exercised against a
/// payload this host can build without a cross toolchain.
fn binary_stub(target: ReleaseTarget) -> Vec<u8> {
    let mut bytes = vec![0_u8; 64];
    match target {
        ReleaseTarget::LinuxX86_64Musl => {
            bytes[..8].copy_from_slice(&[0x7F, b'E', b'L', b'F', 2, 1, 1, 0]);
            bytes[16] = 3;
            bytes[18] = 0x3E;
        }
        ReleaseTarget::WindowsX86_64Msvc => bytes[..2].copy_from_slice(b"MZ"),
        ReleaseTarget::MacOsAarch64 => {
            bytes[..8].copy_from_slice(&[0xCF, 0xFA, 0xED, 0xFE, 0x0C, 0x00, 0x00, 0x01]);
        }
    }
    bytes
}

/// Build a real archive for an arbitrary target from placeholder executables, so
/// the payload-surface assertions below cover the Windows and macOS layouts this
/// host cannot compile for.
fn archive_for(target: ReleaseTarget) -> package::ProducedArchive {
    package::build_archive(&package::PackageInputs {
        workspace_root: workspace_root(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        target,
        executable: binary_stub(target),
        supervisor: binary_stub(target),
        profile: "release".to_owned(),
    })
    .expect("the archive is buildable for any target")
}

fn payload_entry<'archive>(
    produced: &'archive package::ProducedArchive,
    payload_path: &str,
) -> &'archive ArchiveEntry {
    let prefix = format!(
        "{}/",
        package::payload_root(env!("CARGO_PKG_VERSION"), produced.target)
    );
    produced
        .entries
        .iter()
        .find(|entry| entry.path == format!("{prefix}{payload_path}"))
        .unwrap_or_else(|| panic!("the archive carries {payload_path}"))
}

/// The frozen `isolation-docker-clean-room` gate execs `/dist/smoke.sh`. Every
/// target's payload must therefore carry that script, at the committed bytes and
/// mode 0755, or the gate has nothing to run.
#[test]
fn every_target_payload_carries_the_executable_smoke_script() {
    let committed = std::fs::read(workspace_root().join(package::SMOKE_SOURCE))
        .expect("the committed smoke script is readable");
    assert!(
        !committed.is_empty(),
        "{} must not be empty",
        package::SMOKE_SOURCE
    );
    for target in ReleaseTarget::ALL.iter().copied() {
        let produced = archive_for(target);
        let entry = payload_entry(&produced, package::SMOKE_FILE);
        assert_eq!(
            entry.bytes,
            committed,
            "{}'s payload smoke.sh must be the committed {} verbatim",
            target.triple(),
            package::SMOKE_SOURCE
        );
        assert_eq!(
            entry.mode,
            0o755,
            "{}'s payload smoke.sh must be executable; the clean room execs it directly",
            target.triple()
        );
    }
}

/// `MANIFEST.json` must record `smoke.sh` like any other payload file, with its
/// real digest, size and mode — the script's own gate-time identity check reads
/// that record.
#[test]
fn the_payload_manifest_records_the_smoke_script_with_its_digest_and_mode() {
    let produced = archive_for(ReleaseTarget::LinuxX86_64Musl);
    let manifest: serde_json::Value =
        serde_json::from_slice(&payload_entry(&produced, "MANIFEST.json").bytes)
            .expect("MANIFEST.json is valid json");
    let smoke = payload_entry(&produced, package::SMOKE_FILE);
    let expected_digest: String = {
        use sha2::{Digest, Sha256};
        Sha256::digest(&smoke.bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    };
    let recorded = manifest["files"]
        .as_array()
        .expect("the manifest records a files array")
        .iter()
        .find(|file| file["path"] == serde_json::json!(package::SMOKE_FILE))
        .unwrap_or_else(|| panic!("MANIFEST.json records {}", package::SMOKE_FILE));
    assert_eq!(recorded["sha256"], serde_json::json!(expected_digest));
    assert_eq!(
        recorded["size"],
        serde_json::json!(u64::try_from(smoke.bytes.len()).expect("payload size fits in u64"))
    );
    assert_eq!(recorded["mode"], serde_json::json!("755"));
}

/// `write_dist` publishes the dist-level copy the gate's bind mount exposes, and
/// it must be byte-identical to the archive payload entry — that identity is what
/// the script's own D4 check asserts inside the container.
#[test]
fn write_dist_publishes_an_executable_smoke_script_beside_the_archives() {
    let dist = tempfile::tempdir().expect("temp dist");
    let produced = archive_for(ReleaseTarget::LinuxX86_64Musl);
    let stale_archive = dist.path().join(format!(
        "godot-mcp-{}-x86_64-unknown-linux-gnu.tar.gz",
        env!("CARGO_PKG_VERSION")
    ));
    std::fs::write(&stale_archive, b"stale archive").expect("write stale archive");
    let written = package::write_dist(
        &workspace_root(),
        dist.path(),
        env!("CARGO_PKG_VERSION"),
        std::slice::from_ref(&produced),
    )
    .expect("the dist set is publishable");

    assert!(
        !stale_archive.exists(),
        "write_dist must remove archives left by an obsolete target"
    );
    let smoke = dist.path().join(package::SMOKE_FILE);
    assert!(
        written.contains(&smoke),
        "write_dist must report the smoke script it published: {written:?}"
    );
    assert_eq!(
        std::fs::read(&smoke).expect("the published smoke script is readable"),
        payload_entry(&produced, package::SMOKE_FILE).bytes,
        "the dist-level smoke.sh must be byte-identical to the archive payload entry"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&smoke)
            .expect("the published smoke script is stattable")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "the gate execs /dist/smoke.sh directly, so it must be published executable"
        );
    }
    for expected in [
        package::CHECKSUM_FILE,
        package::PROVENANCE_FILE,
        &produced.file_name,
    ] {
        assert!(
            dist.path().join(expected).is_file(),
            "write_dist must still publish {expected}"
        );
    }
}

/// The shell verifier keeps its own copy of the required-payload list, because it
/// runs where no Rust is available. The two lists are asserted identical so
/// neither can drift: adding a required payload path in Rust without teaching the
/// container-side verifier about it fails here.
#[test]
fn the_shell_smoke_requires_exactly_the_rust_payload_paths() {
    let script = std::fs::read_to_string(workspace_root().join(package::SMOKE_SOURCE))
        .expect("the committed smoke script is readable");
    let start = script
        .find("# --- BEGIN REQUIRED_PAYLOAD_PATHS")
        .expect("the script delimits its required-path list");
    let end = script
        .find("# --- END REQUIRED_PAYLOAD_PATHS")
        .expect("the script closes its required-path list");
    let block = &script[start..end];
    let quoted = block
        .split_once("REQUIRED_PAYLOAD_PATHS='")
        .expect("the list is a single-quoted shell assignment")
        .1;
    let quoted = quoted.split_once('\'').expect("the assignment is closed").0;
    let declared: Vec<&str> = quoted.lines().collect();
    assert_eq!(
        declared,
        package::REQUIRED_PAYLOAD_PATHS.to_vec(),
        "{} must require exactly package::REQUIRED_PAYLOAD_PATHS",
        package::SMOKE_SOURCE
    );
}

/// The verifier must fail closed on a shell that has no `pipefail`, must never
/// assume a writable `/tmp`, and must not embed a credential marker — the last
/// one would make the packager refuse to stage the script at all.
#[test]
fn the_shell_smoke_is_portable_and_fails_closed() {
    let script = std::fs::read_to_string(workspace_root().join(package::SMOKE_SOURCE))
        .expect("the committed smoke script is readable");
    assert!(
        script.starts_with("#!/bin/sh\n"),
        "the clean-room image has no bash; the verifier must be POSIX shell"
    );
    assert!(
        script.contains("\nset -eu\n"),
        "the verifier must abort on an unset variable or a failed command"
    );
    assert!(
        script.contains("(set -o pipefail) 2>/dev/null && set -o pipefail || true"),
        "pipefail must be probed rather than assumed, so a shell without it still runs"
    );
    for forbidden in ["/tmp/", "mktemp"] {
        assert!(
            !script.contains(forbidden),
            "`--read-only` leaves no writable /tmp in the clean room, so the verifier \
             must not reference {forbidden}"
        );
    }
    for marker in package::credential_markers() {
        assert!(
            !script.contains(&marker),
            "the verifier must not embed the credential marker {marker:?}; the packager \
             would refuse to stage it"
        );
    }
}

/// The shipped verifier is executed, not merely published.
///
/// Gated to Linux because that is where the isolation gate runs it and because
/// macOS ships no `sha256sum`; the container-side behaviour is the contract, and a
/// verifier that were only ever read would repeat this project's worst defect
/// class. Both directions are asserted: a clean payload passes, and a payload with
/// one altered byte is rejected by name.
#[cfg(target_os = "linux")]
#[test]
fn the_shipped_smoke_script_verifies_an_extracted_payload_and_rejects_a_tampered_one() {
    let produced = archive_for(ReleaseTarget::LinuxX86_64Musl);
    let temp = tempfile::tempdir().expect("temp extraction");
    extract_tar_gz(&produced.bytes, temp.path()).expect("the archive extracts");
    let root = temp.path().join(package::payload_root(
        env!("CARGO_PKG_VERSION"),
        produced.target,
    ));
    let script = root.join(package::SMOKE_FILE);
    assert!(script.is_file(), "the extracted payload carries smoke.sh");

    let clean = std::process::Command::new("sh")
        .arg(&script)
        .output()
        .expect("the shipped verifier is runnable");
    assert!(
        clean.status.success(),
        "the shipped verifier must accept an untouched payload\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&clean.stdout),
        String::from_utf8_lossy(&clean.stderr)
    );
    let report = String::from_utf8_lossy(&clean.stdout).to_string();
    for expected in [
        "recorded payload files match their SHA-256",
        "carries nothing beyond its recorded files",
        "every required payload path is recorded",
        "bin/ holds exactly bin/godot-mcp bin/godot-mcp-supervisor",
        "is a 64-bit little-endian x86-64 ELF image",
        "PASS: every check performed above passed",
    ] {
        assert!(
            report.contains(expected),
            "the verifier's report must state {expected:?}: {report}"
        );
    }

    let victim = root.join("licenses").join("LICENSE-MIT");
    let mut bytes = std::fs::read(&victim).expect("a recorded payload file is readable");
    bytes.push(b'\n');
    std::fs::write(&victim, &bytes).expect("alter one recorded payload file");
    let tampered = std::process::Command::new("sh")
        .arg(&script)
        .output()
        .expect("the shipped verifier is runnable");
    assert!(
        !tampered.status.success(),
        "one altered byte must be rejected, not tolerated\nstdout:\n{}",
        String::from_utf8_lossy(&tampered.stdout)
    );
    let complaint = String::from_utf8_lossy(&tampered.stderr).to_string();
    assert!(
        complaint.contains("licenses/LICENSE-MIT hashes to"),
        "the verifier must name the file whose digest drifted: {complaint}"
    );
}
