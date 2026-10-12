//! What `make godot-test` checks against one exact locked Godot artifact.
//!
//! The Makefile verifies the download against the lock and names it in
//! `GODOT_MCP_LOCKED_ARTIFACT`. This test finds the artifact in the committed lock
//! by its SHA-256 and checks it the way the engine plane uses it:
//!
//! * an editor archive is unpacked and probed as a configured build is (its exact
//!   version, an editor build, every capability the lock requires), and then
//!   checks a broken script and a sound one with `--check-only`, so the
//!   diagnostic parser is proven against that build's own output;
//! * with `GODOT_MCP_DISPLAY_REQUIRED=1` the editor also runs windowed, and must
//!   report a display server other than `headless` and render frames;
//! * an export-templates archive must declare the build's exact template label
//!   and hold the template every desktop preset this server exports resolves to.
//!
//! Without the variable there is nothing to check, so the test is ignored; the
//! release-gate matrix runs it through `make godot-test` for every locked
//! artifact. It unpacks archives with `unzip`, as the CI runners provide.

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use godot_mcp_engine::export::presets::parse_presets;
use godot_mcp_engine::export::templates::{TemplateSet, target_for, template_label};
use godot_mcp_engine::probe::{BuildKind, ProbeCache, ProbeRequest};
use godot_mcp_engine::releases::{
    ArtifactKind, PinnedArtifact, PinnedRelease, Platform, ReleaseLock,
};
use godot_mcp_engine::validation::diagnostic::{
    has_error, located_errors, parse_engine_diagnostics,
};
use godot_mcp_engine::validation::runner::headless_project_args;
use sha2::{Digest, Sha256};

const ARTIFACT: &str = "GODOT_MCP_LOCKED_ARTIFACT";
const DISPLAY_REQUIRED: &str = "GODOT_MCP_DISPLAY_REQUIRED";
const RUN_DEADLINE: Duration = Duration::from_secs(180);

const PROJECT: &str = "config_version=5\n\n[application]\n\nconfig/name=\"locked artifact\"\n";
const SOUND: &str = "extends Node\n\nfunc _ready() -> void:\n\tprint(\"sound\")\n";
/// The syntax error sits on line 4.
const BROKEN: &str =
    "extends Node\n\nfunc _ready() -> void:\n\tgodot_mcp_locked_syntax_error not_a_statement\n";
const BROKEN_LINE: u32 = 4;
/// Prints the display server, then the root viewport's size once frames have
/// been drawn, and quits.
const DISPLAY: &str = r#"extends SceneTree

var frames := 0

func _initialize() -> void:
	print("godot-mcp-display=", DisplayServer.get_name())

func _process(_delta: float) -> bool:
	frames += 1
	if frames < 3:
		return false
	var image := root.get_texture().get_image()
	print("godot-mcp-frame=", image.get_width(), "x", image.get_height())
	quit()
	return false
"#;
/// One preset per desktop platform the server exports.
const DESKTOP_PRESETS: &str = r#"[preset.0]

name="Linux"
platform="Linux"

[preset.0.options]

binary_format/architecture="x86_64"

[preset.1]

name="Windows"
platform="Windows Desktop"

[preset.1.options]

binary_format/architecture="x86_64"

[preset.2]

name="macOS"
platform="macOS"

[preset.2.options]

binary_format/architecture="universal"
"#;

#[test]
#[ignore = "needs GODOT_MCP_LOCKED_ARTIFACT; make godot-test runs it for each locked artifact"]
fn the_locked_artifact_serves_the_engine_plane() {
    let archive = PathBuf::from(
        std::env::var_os(ARTIFACT)
            .unwrap_or_else(|| panic!("{ARTIFACT} must name the verified download")),
    );
    let lock = committed_lock();
    let (release, artifact) = locked(&lock, &archive);
    let display_required = std::env::var(DISPLAY_REQUIRED).is_ok_and(|value| value == "1");
    match artifact.kind {
        ArtifactKind::Editor => {
            assert_eq!(
                artifact.platform,
                Platform::LinuxX86_64,
                "{} is an editor for another platform; this host runs Linux x86_64 builds only",
                artifact.name
            );
            check_editor(&lock, &release, &artifact, &archive, display_required);
        }
        ArtifactKind::ExportTemplates => {
            assert!(
                !display_required,
                "{DISPLAY_REQUIRED}=1 needs an editor archive, and {} holds export templates",
                artifact.name
            );
            check_templates(&release, &archive);
        }
    }
}

fn committed_lock() -> ReleaseLock {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(ReleaseLock::COMMITTED_PATH);
    let bytes = std::fs::read(&path).expect("read the committed lock");
    ReleaseLock::from_json_slice(&bytes).expect("the committed lock parses")
}

/// The lock entry whose SHA-256 is the archive's, so a stray file is refused.
fn locked(lock: &ReleaseLock, archive: &Path) -> (PinnedRelease, PinnedArtifact) {
    let digest = file_sha256(archive);
    lock.releases()
        .iter()
        .find_map(|release| {
            release
                .artifacts
                .iter()
                .find(|artifact| artifact.sha256 == digest)
                .map(|artifact| (release.clone(), artifact.clone()))
        })
        .unwrap_or_else(|| {
            panic!(
                "{} (SHA-256 {digest}) is not an artifact of the lock",
                archive.display()
            )
        })
}

fn file_sha256(path: &Path) -> String {
    let mut file = std::fs::File::open(path)
        .unwrap_or_else(|error| panic!("open {}: {error}", path.display()));
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer).expect("read the artifact");
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn check_editor(
    lock: &ReleaseLock,
    release: &PinnedRelease,
    artifact: &PinnedArtifact,
    archive: &Path,
    display_required: bool,
) {
    let work = tempfile::tempdir().expect("scratch directory");
    unzip(archive, work.path());
    let name = artifact
        .name
        .strip_suffix(".zip")
        .unwrap_or_else(|| panic!("{} is not a zip archive", artifact.name));
    let binary = work.path().join(name);
    assert!(
        binary.is_file(),
        "{} does not unpack to {name}",
        artifact.name
    );

    // Probed the way a configured build is.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("probe runtime");
    let mut cache = ProbeCache::new(lock.clone());
    let (snapshot, _) = runtime
        .block_on(cache.snapshot(&ProbeRequest {
            binary: binary.clone(),
            deadline: Duration::from_secs(60),
        }))
        .unwrap_or_else(|error| panic!("{name} does not probe: {error}"));
    assert_eq!(
        snapshot.version, release.version,
        "{}",
        snapshot.version_line
    );
    assert!(snapshot.pinned, "{name} is not an exact entry of the lock");
    assert_eq!(snapshot.build_kind, BuildKind::Editor, "{name}");
    for capability in &release.required_editor_capabilities {
        assert!(
            snapshot.advertises(capability),
            "{name} does not advertise {capability}"
        );
    }

    let project = work.path().join("project");
    std::fs::create_dir_all(project.join("scripts")).expect("project directory");
    std::fs::write(project.join("project.godot"), PROJECT).expect("project.godot");
    std::fs::write(project.join("scripts/sound.gd"), SOUND).expect("sound script");
    std::fs::write(project.join("scripts/broken.gd"), BROKEN).expect("broken script");

    // The check validation.check_script issues, on a broken script: the parser
    // must find the error, at its file and line, in this build's output.
    let broken = run(
        &binary,
        &check_arguments(&project, "res://scripts/broken.gd"),
    );
    let diagnostics = parse_engine_diagnostics(&broken.combined());
    assert!(
        located_errors(&diagnostics).iter().any(|diagnostic| {
            diagnostic.location.as_ref().is_some_and(|location| {
                location.file.ends_with("scripts/broken.gd") && location.line == Some(BROKEN_LINE)
            })
        }),
        "{name} reported no error at scripts/broken.gd:{BROKEN_LINE} that the parser reads: \
         {diagnostics:#?}\n{}",
        broken.combined()
    );

    // And on a sound one: no error, and a clean exit.
    let sound = run(
        &binary,
        &check_arguments(&project, "res://scripts/sound.gd"),
    );
    let diagnostics = parse_engine_diagnostics(&sound.combined());
    assert!(
        !has_error(&diagnostics),
        "{name} reported an error on a sound script: {diagnostics:#?}\n{}",
        sound.combined()
    );
    assert_eq!(sound.status, Some(0), "{}", sound.combined());

    if display_required {
        check_display(&binary, &project, name);
    }
}

fn check_arguments(project: &Path, script: &str) -> Vec<String> {
    let mut arguments = headless_project_args(project);
    arguments.extend([
        "--check-only".to_owned(),
        "--script".to_owned(),
        script.to_owned(),
    ]);
    arguments
}

/// Run the editor windowed, as a display check must: never `--headless`.
fn check_display(binary: &Path, project: &Path, name: &str) {
    let display = std::env::var_os("DISPLAY")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("WAYLAND_DISPLAY").filter(|value| !value.is_empty()));
    assert!(
        display.is_some(),
        "{DISPLAY_REQUIRED}=1 needs a display, and neither DISPLAY nor WAYLAND_DISPLAY is set"
    );
    std::fs::write(project.join("scripts/display.gd"), DISPLAY).expect("display script");
    let arguments = vec![
        "--path".to_owned(),
        project.display().to_string(),
        "--script".to_owned(),
        "res://scripts/display.gd".to_owned(),
    ];
    let windowed = run(binary, &arguments);
    let output = windowed.combined();
    let server = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("godot-mcp-display="))
        .unwrap_or_else(|| panic!("{name} never reported its display server:\n{output}"));
    assert_ne!(
        server, "headless",
        "{name} ran without a display:\n{output}"
    );
    let frame = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("godot-mcp-frame="))
        .unwrap_or_else(|| panic!("{name} rendered no frame:\n{output}"));
    let (width, height) = frame
        .split_once('x')
        .and_then(|(width, height)| Some((width.parse::<u32>().ok()?, height.parse::<u32>().ok()?)))
        .unwrap_or_else(|| panic!("{name} reported an unreadable frame size `{frame}`"));
    assert!(
        width > 0 && height > 0,
        "{name} rendered an empty frame: {frame}"
    );
    assert_eq!(windowed.status, Some(0), "{output}");
}

fn check_templates(release: &PinnedRelease, archive: &Path) {
    let label = template_label(release.version, &release.channel);
    let root = tempfile::tempdir().expect("templates root");
    let directory = root.path().join(&label);
    std::fs::create_dir_all(&directory).expect("template directory");
    let version = Command::new("unzip")
        .arg("-p")
        .arg(archive)
        .arg("templates/version.txt")
        .output()
        .expect("run unzip");
    assert!(
        version.status.success(),
        "{} holds no templates/version.txt",
        archive.display()
    );
    std::fs::write(directory.join("version.txt"), &version.stdout).expect("version.txt");
    TemplateSet::resolve(root.path(), &label).unwrap_or_else(|error| {
        panic!(
            "{} does not declare the template label {label}: {error}",
            archive.display()
        )
    });

    let listing = Command::new("unzip")
        .arg("-Z1")
        .arg(archive)
        .output()
        .expect("run unzip");
    assert!(
        listing.status.success(),
        "unzip cannot list {}",
        archive.display()
    );
    let entries: BTreeSet<String> = String::from_utf8_lossy(&listing.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    let presets = parse_presets(DESKTOP_PRESETS).expect("the desktop presets parse");
    assert_eq!(presets.len(), 3);
    for preset in &presets {
        for debug in [false, true] {
            let target = target_for(preset, debug)
                .unwrap_or_else(|error| panic!("{}: {error}", preset.name()));
            let entry = format!("templates/{}", target.template);
            assert!(
                entries.contains(&entry),
                "{} lacks {entry}, the {} template the {} preset resolves to",
                archive.display(),
                if debug { "debug" } else { "release" },
                preset.name()
            );
        }
    }
}

fn unzip(archive: &Path, into: &Path) {
    let status = Command::new("unzip")
        .arg("-q")
        .arg("-o")
        .arg(archive)
        .arg("-d")
        .arg(into)
        .status()
        .expect("run unzip");
    assert!(
        status.success(),
        "unzip {} failed: {status}",
        archive.display()
    );
}

struct Run {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    fn combined(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }
}

/// Run the build with a deadline; a run that outlives it is killed and fails the
/// test rather than hanging the job.
fn run(binary: &Path, arguments: &[String]) -> Run {
    let mut child = Command::new(binary)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("start {}: {error}", binary.display()));
    let mut stdout = child.stdout.take().expect("stdout pipe");
    let mut stderr = child.stderr.take().expect("stderr pipe");
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for Godot") {
            break status.code();
        }
        if started.elapsed() > RUN_DEADLINE {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "{} {arguments:?} did not finish within {RUN_DEADLINE:?}",
                binary.display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Run {
        status,
        stdout: out.join().expect("stdout reader"),
        stderr: err.join().expect("stderr reader"),
    }
}
