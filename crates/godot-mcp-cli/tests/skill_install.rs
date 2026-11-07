//! `godot-mcp skill install|update|uninstall|status`.
//!
//! Every case builds its own home directory, agent configuration directories
//! and working directory under a fresh temporary directory. Library cases pass
//! them in a `SkillContext`; binary cases set `HOME`, remove `USERPROFILE`,
//! `CLAUDE_CONFIG_DIR` and `CODEX_HOME`, and set the working directory. No case
//! reads or writes a real user directory, so the cases run in parallel.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use godot_mcp_cli::audit;
use godot_mcp_cli::skill::{
    self, Action, Location, MARKER_FILE, Request, SKILL_FILE, SKILLS, SkillContext, Summary,
    Target, TargetSelection, VERSION, parse_target_selection, resolve, sha256_hex,
};
use predicates::str::contains;
use serde_json::{Value, json};
use tempfile::TempDir;

/// One isolated set of directories.
struct Sandbox {
    _guard: TempDir,
    root: PathBuf,
    home: PathBuf,
    cwd: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let guard = TempDir::new().expect("a temporary directory");
        // Canonical, so a path the command reports compares equal to ours.
        let root = as_reported(
            guard
                .path()
                .canonicalize()
                .expect("the temporary directory resolves"),
        );
        let home = root.join("home");
        let cwd = root.join("work");
        fs::create_dir_all(&home).expect("create the home directory");
        fs::create_dir_all(&cwd).expect("create the working directory");
        Self {
            _guard: guard,
            root,
            home,
            cwd,
        }
    }

    fn context(&self) -> SkillContext {
        SkillContext {
            home: self.home.clone(),
            cwd: self.cwd.clone(),
            claude_config_dir: None,
            codex_home: None,
        }
    }

    fn claude_skills(&self) -> PathBuf {
        self.home.join(".claude").join("skills")
    }

    fn skill_dir(&self, name: &str) -> PathBuf {
        self.claude_skills().join(name)
    }

    fn run(&self, request: &Request) -> (Summary, String) {
        run_with(&self.context(), request)
    }

    /// The installed binary, isolated to this sandbox.
    fn binary(&self) -> Command {
        let mut command = Command::cargo_bin("godot-mcp").expect("the godot-mcp binary is built");
        command
            .env("HOME", &self.home)
            .env_remove("USERPROFILE")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .current_dir(&self.cwd);
        command
    }
}

/// `path` as a process reports its working directory. On Windows a canonical
/// path carries the `\\?\` verbatim prefix and a process's current directory
/// does not, so a drive path drops it.
fn as_reported(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    if let Some(rest) = path.to_str().and_then(|text| text.strip_prefix(r"\\?\"))
        && rest.as_bytes().get(1) == Some(&b':')
    {
        return PathBuf::from(rest);
    }
    path
}

fn run_with(context: &SkillContext, request: &Request) -> (Summary, String) {
    let mut out = Vec::new();
    let summary = skill::run(request, context, &mut out).expect("the report is written");
    (
        summary,
        String::from_utf8(out).expect("the report is UTF-8"),
    )
}

fn request(action: Action, target: &str) -> Request {
    Request::new(
        action,
        parse_target_selection(target).expect("a valid target"),
        Location::Global,
    )
}

fn forced(action: Action, target: &str) -> Request {
    Request {
        force: true,
        ..request(action, target)
    }
}

fn embedded(name: &str) -> &'static str {
    SKILLS
        .iter()
        .find(|skill| skill.name == name)
        .expect("an embedded skill")
        .content
}

fn sha_of(name: &str) -> String {
    sha256_hex(embedded(name).as_bytes())
}

fn read_marker(dir: &Path) -> Value {
    serde_json::from_slice(&fs::read(dir.join(MARKER_FILE)).expect("the marker exists"))
        .expect("the marker is JSON")
}

fn put_marker(dir: &Path, sha256: &str, version: &str) {
    let marker = json!({ "sha256": sha256, "version": version, "installed_at": 1 });
    fs::write(
        dir.join(MARKER_FILE),
        serde_json::to_vec(&marker).expect("serialize"),
    )
    .expect("write the marker");
}

/// Put `content` in place as a skill an older godot-mcp wrote: a marker naming
/// its SHA-256 and version 0.0.1.
fn put_older_version(dir: &Path, content: &str) {
    fs::create_dir_all(dir).expect("create the skill directory");
    fs::write(dir.join(SKILL_FILE), content).expect("write SKILL.md");
    put_marker(dir, &sha256_hex(content.as_bytes()), "0.0.1");
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs()
}

/// The one report line for `skill` of `target`.
fn line_for<'a>(report: &'a str, target: &str, skill: &str) -> &'a str {
    let prefix = format!("{target}: {skill}: ");
    let lines: Vec<&str> = report
        .lines()
        .filter(|line| line.starts_with(&prefix))
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "exactly one line for {prefix:?} in:\n{report}"
    );
    lines[0]
}

/// What one directory entry is and holds.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    Directory,
    File(Vec<u8>),
    Link(PathBuf),
}

/// Every entry under `root`, without following links.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Entry> {
    let mut entries = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read a directory") {
            let path = entry.expect("a directory entry").path();
            let metadata = fs::symlink_metadata(&path).expect("inspect an entry");
            let kind = if metadata.file_type().is_symlink() {
                Entry::Link(fs::read_link(&path).expect("read a link"))
            } else if metadata.is_dir() {
                stack.push(path.clone());
                Entry::Directory
            } else {
                Entry::File(fs::read(&path).expect("read a file"))
            };
            entries.insert(path, kind);
        }
    }
    entries
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("read a directory")
        .map(|entry| {
            entry
                .expect("a directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root resolves")
}

#[test]
fn the_embedded_skills_are_exactly_the_repository_skills_directory() {
    let skills = workspace_root().join("skills");
    let mut on_disk = names_in(&skills);
    on_disk.retain(|name| skills.join(name).is_dir());
    let mut embedded_names: Vec<String> =
        SKILLS.iter().map(|skill| skill.name.to_owned()).collect();
    embedded_names.sort();
    embedded_names.dedup();
    assert_eq!(
        embedded_names.len(),
        SKILLS.len(),
        "no skill is embedded twice"
    );
    assert_eq!(
        embedded_names, on_disk,
        "every skills/<name>/ directory is embedded and nothing else is"
    );
    for skill in SKILLS {
        assert_eq!(
            fs::read(skills.join(skill.name).join(SKILL_FILE)).expect("read SKILL.md"),
            skill.content.as_bytes(),
            "{} is embedded byte for byte",
            skill.name
        );
    }
}

#[test]
fn a_fresh_install_writes_every_skill_and_its_marker() {
    let sandbox = Sandbox::new();
    let before = now();
    let (summary, report) = sandbox.run(&request(Action::Install, "claude"));
    let after = now();
    assert_eq!(
        summary,
        Summary {
            written: SKILLS.len(),
            ..Summary::default()
        }
    );
    assert_eq!(summary.exit_status(), 0);
    for skill in SKILLS {
        let dir = sandbox.skill_dir(skill.name);
        assert_eq!(
            fs::read(dir.join(SKILL_FILE)).expect("SKILL.md is written"),
            skill.content.as_bytes()
        );
        let marker = read_marker(&dir);
        assert_eq!(marker["sha256"], sha256_hex(skill.content.as_bytes()));
        assert_eq!(marker["version"], VERSION);
        let installed_at = marker["installed_at"]
            .as_u64()
            .expect("installed_at is Unix seconds");
        assert!((before..=after).contains(&installed_at));
        assert_eq!(
            names_in(&dir),
            [MARKER_FILE, SKILL_FILE],
            "no temporary file is left behind"
        );
        assert_eq!(
            line_for(&report, "claude", skill.name),
            format!("claude: {}: installed {}", skill.name, dir.display())
        );
    }
    assert!(
        report.ends_with("install: written 3, up to date 0, skipped 0, errors 0\n"),
        "{report}"
    );
}

#[test]
fn installing_again_changes_nothing() {
    let sandbox = Sandbox::new();
    sandbox.run(&request(Action::Install, "claude"));
    // A marker from an earlier second: rewriting it would change its bytes.
    for skill in SKILLS {
        put_marker(&sandbox.skill_dir(skill.name), &sha_of(skill.name), VERSION);
    }
    let before = snapshot(&sandbox.root);
    for action in [Action::Install, Action::Update] {
        let (summary, report) = sandbox.run(&request(action, "claude"));
        assert_eq!(
            summary,
            Summary {
                unchanged: SKILLS.len(),
                ..Summary::default()
            }
        );
        for skill in SKILLS {
            assert_eq!(
                line_for(&report, "claude", skill.name),
                format!(
                    "claude: {}: up to date {}",
                    skill.name,
                    sandbox.skill_dir(skill.name).display()
                )
            );
        }
        assert_eq!(snapshot(&sandbox.root), before);
    }
}

#[test]
fn install_and_update_refresh_a_skill_an_older_godot_mcp_wrote() {
    for action in [Action::Install, Action::Update] {
        let sandbox = Sandbox::new();
        for skill in SKILLS {
            put_older_version(&sandbox.skill_dir(skill.name), "An older skill.\n");
        }
        let (summary, report) = sandbox.run(&request(action, "claude"));
        assert_eq!(
            summary,
            Summary {
                written: SKILLS.len(),
                ..Summary::default()
            }
        );
        for skill in SKILLS {
            let dir = sandbox.skill_dir(skill.name);
            assert_eq!(
                fs::read(dir.join(SKILL_FILE)).expect("SKILL.md"),
                skill.content.as_bytes()
            );
            let marker = read_marker(&dir);
            assert_eq!(marker["sha256"], sha_of(skill.name));
            assert_eq!(marker["version"], VERSION);
            assert_eq!(
                line_for(&report, "claude", skill.name),
                format!(
                    "claude: {}: updated {} (0.0.1 -> {VERSION})",
                    skill.name,
                    dir.display()
                )
            );
        }
    }
}

#[test]
fn a_changed_skill_is_skipped_and_force_overwrites_it() {
    for action in [Action::Install, Action::Update] {
        let sandbox = Sandbox::new();
        sandbox.run(&request(Action::Install, "claude"));
        let dir = sandbox.skill_dir("godot-mcp");
        let changed = format!("{}A line the user added.\n", embedded("godot-mcp"));
        fs::write(dir.join(SKILL_FILE), &changed).expect("change SKILL.md");
        let marker = fs::read(dir.join(MARKER_FILE)).expect("the marker");

        let (summary, report) = sandbox.run(&request(action, "claude"));
        assert_eq!(
            summary,
            Summary {
                unchanged: 2,
                skipped: 1,
                ..Summary::default()
            }
        );
        assert_eq!(summary.exit_status(), 0, "a skipped skill is not an error");
        assert_eq!(fs::read_to_string(dir.join(SKILL_FILE)).unwrap(), changed);
        assert_eq!(fs::read(dir.join(MARKER_FILE)).unwrap(), marker);
        assert_eq!(
            line_for(&report, "claude", "godot-mcp"),
            format!(
                "claude: godot-mcp: skipped {}: its SKILL.md was changed after godot-mcp \
                 wrote it; --force overwrites it",
                dir.display()
            )
        );

        let (summary, report) = sandbox.run(&forced(action, "claude"));
        assert_eq!(
            summary,
            Summary {
                written: SKILLS.len(),
                ..Summary::default()
            },
            "--force overwrites every installed skill"
        );
        assert_eq!(
            fs::read(dir.join(SKILL_FILE)).unwrap(),
            embedded("godot-mcp").as_bytes()
        );
        assert_eq!(read_marker(&dir)["sha256"], sha_of("godot-mcp"));
        assert_eq!(
            line_for(&report, "claude", "godot-mcp"),
            format!("claude: godot-mcp: overwrote {} (--force)", dir.display())
        );
    }
}

#[test]
fn a_differing_skill_without_a_marker_is_skipped_like_a_changed_one() {
    let sandbox = Sandbox::new();
    let dir = sandbox.skill_dir("godot-mcp");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(SKILL_FILE), "Somebody else's skill.\n").unwrap();
    for action in [Action::Install, Action::Update] {
        let (summary, report) = sandbox.run(&request(action, "claude"));
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.errors, 0);
        assert_eq!(
            fs::read_to_string(dir.join(SKILL_FILE)).unwrap(),
            "Somebody else's skill.\n"
        );
        assert!(!dir.join(MARKER_FILE).exists(), "no marker is invented");
        assert_eq!(
            line_for(&report, "claude", "godot-mcp"),
            format!(
                "claude: godot-mcp: skipped {}: its SKILL.md differs and it has no marker, \
                 so godot-mcp cannot tell it wrote it; --force overwrites it",
                dir.display()
            )
        );
    }
    let (summary, _) = sandbox.run(&forced(Action::Install, "claude"));
    assert_eq!(summary.written, SKILLS.len());
    assert_eq!(
        fs::read(dir.join(SKILL_FILE)).unwrap(),
        embedded("godot-mcp").as_bytes()
    );
    assert_eq!(read_marker(&dir)["sha256"], sha_of("godot-mcp"));
}

/// The ways `<skill dir>/SKILL.md` can be something other than a regular file.
#[derive(Debug, Clone, Copy)]
enum Irregular {
    Directory,
    #[cfg(unix)]
    LinkToFile,
    #[cfg(unix)]
    LinkToDirectory,
}

impl Irregular {
    const ALL: &[Self] = &[
        Self::Directory,
        #[cfg(unix)]
        Self::LinkToFile,
        #[cfg(unix)]
        Self::LinkToDirectory,
    ];

    /// Put this kind of entry at `dir/SKILL.md`, with whatever it points at
    /// under `outside`, and return the reason the report gives.
    fn make(self, dir: &Path, outside: &Path) -> &'static str {
        fs::create_dir_all(dir).unwrap();
        fs::create_dir_all(outside).unwrap();
        let skill_file = dir.join(SKILL_FILE);
        match self {
            Self::Directory => {
                fs::create_dir(&skill_file).unwrap();
                fs::write(skill_file.join("inner.txt"), "inside\n").unwrap();
                "is a directory, not a regular file"
            }
            #[cfg(unix)]
            Self::LinkToFile => {
                // The link's target holds exactly the embedded skill: following
                // it would make the skill look up to date.
                let target = outside.join("SKILL.md");
                fs::write(&target, embedded("godot-mcp")).unwrap();
                std::os::unix::fs::symlink(&target, &skill_file).unwrap();
                "is a symbolic link, not a regular file"
            }
            #[cfg(unix)]
            Self::LinkToDirectory => {
                let target = outside.join("a-directory");
                fs::create_dir_all(&target).unwrap();
                std::os::unix::fs::symlink(&target, &skill_file).unwrap();
                "is a symbolic link, not a regular file"
            }
        }
    }
}

#[test]
fn a_skill_md_that_is_not_a_regular_file_is_an_error_even_with_force() {
    for kind in Irregular::ALL {
        for (label, request) in [
            ("install", request(Action::Install, "claude")),
            ("install --force", forced(Action::Install, "claude")),
            ("update", request(Action::Update, "claude")),
            ("update --force", forced(Action::Update, "claude")),
            ("uninstall", request(Action::Uninstall, "claude")),
        ] {
            let sandbox = Sandbox::new();
            if request.action == Action::Uninstall {
                sandbox.run(&request_install());
                fs::remove_file(sandbox.skill_dir("godot-mcp").join(SKILL_FILE)).unwrap();
                fs::remove_file(sandbox.skill_dir("godot-mcp").join(MARKER_FILE)).unwrap();
            }
            let dir = sandbox.skill_dir("godot-mcp");
            let outside = sandbox.root.join("outside");
            let reason = kind.make(&dir, &outside);
            let bad_before = snapshot(&dir);
            let outside_before = snapshot(&outside);

            let (summary, report) = sandbox.run(&request);
            assert_eq!(summary.errors, 1, "{kind:?} with {label}:\n{report}");
            assert_eq!(summary.exit_status(), 1);
            if request.action == Action::Uninstall {
                assert_eq!(summary.removed, 2, "the other skills are still removed");
            } else {
                assert_eq!(summary.written, 2, "the other skills are still written");
            }
            assert_eq!(snapshot(&dir), bad_before, "{kind:?} with {label}");
            assert_eq!(snapshot(&outside), outside_before, "{kind:?} with {label}");
            assert_eq!(
                line_for(&report, "claude", "godot-mcp"),
                format!(
                    "claude: godot-mcp: error {}: {} {reason}",
                    dir.display(),
                    dir.join(SKILL_FILE).display()
                ),
                "{kind:?} with {label}"
            );
        }
    }
}

fn request_install() -> Request {
    request(Action::Install, "claude")
}

#[test]
fn a_skill_directory_position_that_is_not_a_directory_is_an_error() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.claude_skills()).unwrap();
    let position = sandbox.skill_dir("godot-mcp");
    fs::write(&position, "a file where a directory belongs\n").unwrap();
    for request in [
        request_install(),
        forced(Action::Install, "claude"),
        request(Action::Uninstall, "claude"),
        request(Action::Status, "claude"),
    ] {
        let (summary, report) = sandbox.run(&request);
        assert_eq!(summary.errors, 1, "{report}");
        assert_eq!(
            line_for(&report, "claude", "godot-mcp"),
            format!(
                "claude: godot-mcp: error {}: {} exists and is not a directory",
                position.display(),
                position.display()
            )
        );
        assert_eq!(
            fs::read_to_string(&position).unwrap(),
            "a file where a directory belongs\n"
        );
    }
}

#[cfg(unix)]
#[test]
fn linked_skill_directories_are_followed() {
    use std::os::unix::fs::symlink;

    // `~/.claude/skills` is a link to another skills directory.
    let sandbox = Sandbox::new();
    let shared = sandbox.root.join("agents-skills");
    fs::create_dir_all(&shared).unwrap();
    fs::create_dir_all(sandbox.home.join(".claude")).unwrap();
    symlink(&shared, sandbox.claude_skills()).unwrap();
    let (summary, _) = sandbox.run(&request_install());
    assert_eq!(summary.written, SKILLS.len());
    for skill in SKILLS {
        assert_eq!(
            fs::read(shared.join(skill.name).join(SKILL_FILE)).unwrap(),
            skill.content.as_bytes()
        );
    }
    let (summary, _) = sandbox.run(&request(Action::Uninstall, "claude"));
    assert_eq!(summary.removed, SKILLS.len());
    assert!(
        names_in(&shared).is_empty(),
        "the skill directories are pruned"
    );
    assert!(
        fs::symlink_metadata(sandbox.claude_skills())
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link to the skills directory stays"
    );

    // A skill directory that is itself a link to a directory.
    let sandbox = Sandbox::new();
    let elsewhere = sandbox.root.join("elsewhere").join("godot-mcp");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::create_dir_all(sandbox.claude_skills()).unwrap();
    let linked = sandbox.skill_dir("godot-mcp");
    symlink(&elsewhere, &linked).unwrap();
    let (summary, _) = sandbox.run(&request_install());
    assert_eq!(summary.written, SKILLS.len());
    assert_eq!(
        fs::read(elsewhere.join(SKILL_FILE)).unwrap(),
        embedded("godot-mcp").as_bytes()
    );
    let (summary, report) = sandbox.run(&request(Action::Uninstall, "claude"));
    assert_eq!(summary.removed, SKILLS.len());
    assert_eq!(
        line_for(&report, "claude", "godot-mcp"),
        format!(
            "claude: godot-mcp: removed {}; kept the symbolic link it is reached through",
            linked.display()
        )
    );
    assert!(
        fs::symlink_metadata(&linked)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(names_in(&elsewhere).is_empty());

    // A skill directory that is a link to a file, or a dangling link.
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.claude_skills()).unwrap();
    let file = sandbox.root.join("a-file");
    fs::write(&file, "not a directory\n").unwrap();
    symlink(&file, sandbox.skill_dir("godot-mcp")).unwrap();
    symlink(
        sandbox.root.join("missing"),
        sandbox.skill_dir("godot-mcp-setup"),
    )
    .unwrap();
    let (summary, report) = sandbox.run(&forced(Action::Install, "claude"));
    assert_eq!(summary.errors, 2, "{report}");
    assert_eq!(summary.written, 1);
    assert!(
        line_for(&report, "claude", "godot-mcp")
            .ends_with("is a symbolic link to something that is not a directory")
    );
    assert!(
        line_for(&report, "claude", "godot-mcp-setup")
            .contains("is a symbolic link that cannot be followed")
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "not a directory\n");
    assert!(!sandbox.root.join("missing").exists());
}

#[cfg(unix)]
#[test]
fn an_unreadable_skill_md_is_an_error_even_with_force() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new();
    sandbox.run(&request_install());
    let dir = sandbox.skill_dir("godot-mcp");
    let skill_file = dir.join(SKILL_FILE);
    let marker = fs::read(dir.join(MARKER_FILE)).unwrap();
    fs::set_permissions(&skill_file, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&skill_file).is_ok() {
        fs::set_permissions(&skill_file, fs::Permissions::from_mode(0o644)).unwrap();
        eprintln!(
            "skipped: this process reads a file it has no read permission for (it runs as \
             root), so an unreadable SKILL.md cannot be arranged"
        );
        return;
    }
    for request in [
        request_install(),
        forced(Action::Install, "claude"),
        forced(Action::Update, "claude"),
        request(Action::Status, "claude"),
        request(Action::Uninstall, "claude"),
    ] {
        let (summary, report) = sandbox.run(&request);
        assert_eq!(summary.errors, 1, "{report}");
        assert_eq!(summary.exit_status(), 1);
        let line = line_for(&report, "claude", "godot-mcp");
        assert!(
            line.starts_with(&format!(
                "claude: godot-mcp: error {}: cannot read {}: ",
                dir.display(),
                skill_file.display()
            )),
            "{line}"
        );
    }
    fs::set_permissions(&skill_file, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        fs::read(&skill_file).unwrap(),
        embedded("godot-mcp").as_bytes()
    );
    assert_eq!(fs::read(dir.join(MARKER_FILE)).unwrap(), marker);

    // An unreadable marker is no usable marker, not an error.
    let sandbox = Sandbox::new();
    sandbox.run(&request_install());
    let dir = sandbox.skill_dir("godot-mcp");
    let marker_file = dir.join(MARKER_FILE);
    fs::set_permissions(&marker_file, fs::Permissions::from_mode(0o000)).unwrap();
    let (summary, report) = sandbox.run(&request_install());
    assert_eq!(summary.errors, 0, "{report}");
    let line = line_for(&report, "claude", "godot-mcp");
    assert!(
        line.starts_with(&format!(
            "claude: godot-mcp: up to date {}; rewrote its marker (its marker cannot be read: ",
            dir.display()
        )),
        "{line}"
    );
    assert_eq!(read_marker(&dir)["sha256"], sha_of("godot-mcp"));
}

const NOT_UTF8: &[u8] = b"\xff\xfe not UTF-8 \x80\n";

#[test]
fn a_skill_md_that_is_not_utf8_is_skipped_without_force_and_overwritten_with_force() {
    let sandbox = Sandbox::new();
    sandbox.run(&request_install());
    let dir = sandbox.skill_dir("godot-mcp");
    fs::write(dir.join(SKILL_FILE), NOT_UTF8).unwrap();
    for action in [Action::Install, Action::Update] {
        let (summary, report) = sandbox.run(&request(action, "claude"));
        assert_eq!(
            summary,
            Summary {
                unchanged: 2,
                skipped: 1,
                ..Summary::default()
            },
            "{report}"
        );
        assert_eq!(summary.exit_status(), 0);
        assert_eq!(fs::read(dir.join(SKILL_FILE)).unwrap(), NOT_UTF8);
    }
    fs::remove_file(dir.join(MARKER_FILE)).unwrap();
    let (summary, _) = sandbox.run(&request(Action::Update, "claude"));
    assert_eq!(summary.skipped, 1);
    assert_eq!(fs::read(dir.join(SKILL_FILE)).unwrap(), NOT_UTF8);

    let (summary, _) = sandbox.run(&forced(Action::Install, "claude"));
    assert_eq!(summary.written, SKILLS.len());
    assert_eq!(
        fs::read(dir.join(SKILL_FILE)).unwrap(),
        embedded("godot-mcp").as_bytes()
    );
}

#[test]
fn update_diff_on_a_non_utf8_skill_prints_one_line_with_both_sizes_and_digests() {
    for dry_run in [false, true] {
        let sandbox = Sandbox::new();
        sandbox.run(&request_install());
        let dir = sandbox.skill_dir("godot-mcp");
        fs::write(dir.join(SKILL_FILE), NOT_UTF8).unwrap();
        let before = snapshot(&sandbox.root);
        let (summary, report) = sandbox.run(&Request {
            diff: true,
            dry_run,
            ..request(Action::Update, "claude")
        });
        assert_eq!(summary.exit_status(), 0);
        assert_eq!(summary.skipped, 1);
        assert_eq!(snapshot(&sandbox.root), before, "dry_run {dry_run}");
        let skip = if dry_run { "would skip" } else { "skipped" };
        assert_eq!(
            line_for(&report, "claude", "godot-mcp"),
            format!(
                "claude: godot-mcp: {skip} {}: its SKILL.md was changed after godot-mcp wrote \
                 it; --force overwrites it; the installed SKILL.md is not UTF-8 text, so there \
                 is no diff (installed: {} bytes, sha256 {}; embedded: {} bytes, sha256 {})",
                dir.display(),
                NOT_UTF8.len(),
                sha256_hex(NOT_UTF8),
                embedded("godot-mcp").len(),
                sha_of("godot-mcp"),
            )
        );
        assert!(
            report.lines().all(|line| !line.starts_with("--- ")
                && !line.starts_with("+++ ")
                && !line.starts_with("@@ ")),
            "no text diff is printed:\n{report}"
        );
    }
}

#[test]
fn a_marker_that_is_not_valid_json_counts_as_no_marker() {
    let sandbox = Sandbox::new();
    sandbox.run(&request_install());
    let dir = sandbox.skill_dir("godot-mcp");
    fs::write(dir.join(SKILL_FILE), "An older text.\n").unwrap();
    fs::write(dir.join(MARKER_FILE), "{not json").unwrap();
    for action in [Action::Install, Action::Update] {
        let (summary, report) = sandbox.run(&request(action, "claude"));
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.errors, 0);
        assert_eq!(
            line_for(&report, "claude", "godot-mcp"),
            format!(
                "claude: godot-mcp: skipped {}: its SKILL.md differs and its marker is not \
                 valid JSON, so godot-mcp cannot tell it wrote it; --force overwrites it",
                dir.display()
            )
        );
        assert_eq!(
            fs::read_to_string(dir.join(SKILL_FILE)).unwrap(),
            "An older text.\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join(MARKER_FILE)).unwrap(),
            "{not json"
        );
    }

    // With matching content the unusable marker is replaced.
    fs::write(dir.join(SKILL_FILE), embedded("godot-mcp")).unwrap();
    let (summary, report) = sandbox.run(&request_install());
    assert_eq!(summary.unchanged, SKILLS.len());
    assert_eq!(
        line_for(&report, "claude", "godot-mcp"),
        format!(
            "claude: godot-mcp: up to date {}; rewrote its marker (its marker is not valid JSON)",
            dir.display()
        )
    );
    assert_eq!(read_marker(&dir)["sha256"], sha_of("godot-mcp"));
}

#[test]
fn a_matching_skill_gets_its_marker_rewritten() {
    let sandbox = Sandbox::new();
    sandbox.run(&request_install());
    let missing = sandbox.skill_dir("godot-mcp");
    let stale = sandbox.skill_dir("godot-mcp-playtest");
    fs::remove_file(missing.join(MARKER_FILE)).unwrap();
    put_marker(&stale, &sha256_hex(b"something else"), "0.0.1");

    let (summary, report) = sandbox.run(&Request {
        dry_run: true,
        ..request(Action::Update, "claude")
    });
    assert_eq!(summary.unchanged, SKILLS.len());
    assert_eq!(
        line_for(&report, "claude", "godot-mcp"),
        format!(
            "claude: godot-mcp: up to date {}; would rewrite its marker (it has no marker)",
            missing.display()
        )
    );
    assert!(
        !missing.join(MARKER_FILE).exists(),
        "a dry run writes no marker"
    );

    let (summary, report) = sandbox.run(&request_install());
    assert_eq!(
        summary,
        Summary {
            unchanged: SKILLS.len(),
            ..Summary::default()
        }
    );
    assert_eq!(
        line_for(&report, "claude", "godot-mcp"),
        format!(
            "claude: godot-mcp: up to date {}; rewrote its marker (it has no marker)",
            missing.display()
        )
    );
    assert_eq!(
        line_for(&report, "claude", "godot-mcp-playtest"),
        format!(
            "claude: godot-mcp-playtest: up to date {}; rewrote its marker (its marker named \
             another SHA-256)",
            stale.display()
        )
    );
    for (dir, name) in [(&missing, "godot-mcp"), (&stale, "godot-mcp-playtest")] {
        let marker = read_marker(dir);
        assert_eq!(marker["sha256"], sha_of(name));
        assert_eq!(marker["version"], VERSION);
        assert_eq!(
            fs::read(dir.join(SKILL_FILE)).unwrap(),
            embedded(name).as_bytes()
        );
    }
}

#[test]
fn update_dry_run_writes_nothing() {
    let sandbox = Sandbox::new();
    // godot-mcp is absent; godot-mcp-playtest is an older version godot-mcp
    // wrote; godot-mcp-setup was changed by its user.
    let older = sandbox.skill_dir("godot-mcp-playtest");
    put_older_version(&older, "An older skill.\n");
    let changed = sandbox.skill_dir("godot-mcp-setup");
    fs::create_dir_all(&changed).unwrap();
    fs::write(
        changed.join(SKILL_FILE),
        format!("{}Mine.\n", embedded("godot-mcp-setup")),
    )
    .unwrap();
    put_marker(&changed, &sha_of("godot-mcp-setup"), VERSION);
    let before = snapshot(&sandbox.root);

    let (summary, report) = sandbox.run(&Request {
        dry_run: true,
        ..request(Action::Update, "claude")
    });
    assert_eq!(snapshot(&sandbox.root), before);
    assert_eq!(
        summary,
        Summary {
            written: 2,
            skipped: 1,
            ..Summary::default()
        }
    );
    assert_eq!(
        line_for(&report, "claude", "godot-mcp"),
        format!(
            "claude: godot-mcp: would install {}",
            sandbox.skill_dir("godot-mcp").display()
        )
    );
    assert_eq!(
        line_for(&report, "claude", "godot-mcp-playtest"),
        format!(
            "claude: godot-mcp-playtest: would update {} (0.0.1 -> {VERSION})",
            older.display()
        )
    );
    assert!(
        line_for(&report, "claude", "godot-mcp-setup").starts_with(&format!(
            "claude: godot-mcp-setup: would skip {}",
            changed.display()
        ))
    );
    assert!(
        report.ends_with(
            "update --dry-run wrote nothing: would write 2, up to date 0, would skip 1, errors 0\n"
        ),
        "{report}"
    );

    let (summary, report) = sandbox.run(&Request {
        dry_run: true,
        ..forced(Action::Update, "claude")
    });
    assert_eq!(snapshot(&sandbox.root), before);
    assert_eq!(summary.written, SKILLS.len());
    assert_eq!(
        line_for(&report, "claude", "godot-mcp-setup"),
        format!(
            "claude: godot-mcp-setup: would overwrite {} (--force)",
            changed.display()
        )
    );
}

#[test]
fn update_diff_prints_the_diff_from_the_installed_file_to_the_embedded_one() {
    let sandbox = Sandbox::new();
    sandbox.run(&request_install());
    let older = sandbox.skill_dir("godot-mcp");
    put_older_version(&older, "An older skill.\n");
    fs::remove_dir_all(sandbox.skill_dir("godot-mcp-setup")).unwrap();
    let before = snapshot(&sandbox.root);

    let (summary, report) = sandbox.run(&Request {
        diff: true,
        dry_run: true,
        ..request(Action::Update, "claude")
    });
    assert_eq!(
        snapshot(&sandbox.root),
        before,
        "--diff --dry-run writes nothing"
    );
    assert_eq!(summary.exit_status(), 0);
    let update = godot_mcp_server::diff::unified(
        Some("godot-mcp/SKILL.md"),
        Some("godot-mcp/SKILL.md"),
        "An older skill.\n",
        embedded("godot-mcp"),
    );
    let install = godot_mcp_server::diff::unified(
        None,
        Some("godot-mcp-setup/SKILL.md"),
        "",
        embedded("godot-mcp-setup"),
    );
    assert!(!update.truncated && !install.truncated);
    assert!(
        report.contains(&format!(
            "{}\n{}",
            line_for(&report, "claude", "godot-mcp"),
            update.text
        )),
        "the diff follows the skill's line:\n{report}"
    );
    assert!(
        update
            .text
            .starts_with("--- a/godot-mcp/SKILL.md\n+++ b/godot-mcp/SKILL.md\n@@ -1,1 +1,")
    );
    assert!(update.text.contains("\n-An older skill.\n"));
    assert!(report.contains(&format!(
        "{}\n{}",
        line_for(&report, "claude", "godot-mcp-setup"),
        install.text
    )));
    assert!(
        install
            .text
            .starts_with("--- /dev/null\n+++ b/godot-mcp-setup/SKILL.md\n")
    );
    assert!(
        !report.contains("godot-mcp-playtest/SKILL.md"),
        "an up-to-date skill has no diff"
    );

    // Without --dry-run the same diff is printed and the files are written.
    let (summary, report) = sandbox.run(&Request {
        diff: true,
        ..request(Action::Update, "claude")
    });
    assert_eq!(summary.written, 2);
    assert!(report.contains(&update.text));
    assert_eq!(
        fs::read(older.join(SKILL_FILE)).unwrap(),
        embedded("godot-mcp").as_bytes()
    );
}

#[test]
fn uninstall_removes_what_godot_mcp_wrote_and_keeps_changed_files() {
    let sandbox = Sandbox::new();
    sandbox.run(&request_install());
    let changed = sandbox.skill_dir("godot-mcp");
    let changed_text = format!("{}Mine.\n", embedded("godot-mcp"));
    fs::write(changed.join(SKILL_FILE), &changed_text).unwrap();
    let changed_marker = fs::read(changed.join(MARKER_FILE)).unwrap();
    let older = sandbox.skill_dir("godot-mcp-playtest");
    put_older_version(&older, "An older skill.\n");
    let crowded = sandbox.skill_dir("godot-mcp-setup");
    fs::write(crowded.join("notes.txt"), "the user's notes\n").unwrap();

    let (summary, report) = sandbox.run(&request(Action::Uninstall, "claude"));
    assert_eq!(
        summary,
        Summary {
            removed: 2,
            kept: 1,
            ..Summary::default()
        }
    );
    assert_eq!(summary.exit_status(), 0, "a kept file is not an error");
    assert_eq!(
        line_for(&report, "claude", "godot-mcp"),
        format!(
            "claude: godot-mcp: kept {}: its SKILL.md was changed after godot-mcp wrote it",
            changed.display()
        )
    );
    assert_eq!(
        fs::read_to_string(changed.join(SKILL_FILE)).unwrap(),
        changed_text
    );
    assert_eq!(fs::read(changed.join(MARKER_FILE)).unwrap(), changed_marker);
    assert_eq!(
        line_for(&report, "claude", "godot-mcp-playtest"),
        format!("claude: godot-mcp-playtest: removed {}", older.display())
    );
    assert!(!older.exists(), "the emptied skill directory is removed");
    assert_eq!(
        line_for(&report, "claude", "godot-mcp-setup"),
        format!(
            "claude: godot-mcp-setup: removed {}; kept the directory, which holds other files",
            crowded.display()
        )
    );
    assert_eq!(names_in(&crowded), ["notes.txt"]);
    assert!(
        sandbox.claude_skills().is_dir(),
        "the agent's skills directory stays"
    );
    assert!(
        report.ends_with("uninstall: removed 2, kept 1, not installed 0, errors 0\n"),
        "{report}"
    );

    // A differing SKILL.md without a marker is kept too; a leftover marker goes.
    fs::remove_file(changed.join(MARKER_FILE)).unwrap();
    fs::create_dir_all(&older).unwrap();
    put_marker(&older, &sha_of("godot-mcp-playtest"), VERSION);
    let (summary, report) = sandbox.run(&request(Action::Uninstall, "claude"));
    assert_eq!(
        summary,
        Summary {
            kept: 1,
            absent: 2,
            ..Summary::default()
        }
    );
    assert_eq!(
        line_for(&report, "claude", "godot-mcp"),
        format!(
            "claude: godot-mcp: kept {}: its SKILL.md differs and it has no marker, so \
             godot-mcp cannot tell it wrote it",
            changed.display()
        )
    );
    assert_eq!(
        line_for(&report, "claude", "godot-mcp-playtest"),
        format!(
            "claude: godot-mcp-playtest: not installed {}; removed its leftover marker",
            older.display()
        )
    );
    assert!(!older.exists());
    assert_eq!(
        line_for(&report, "claude", "godot-mcp-setup"),
        format!(
            "claude: godot-mcp-setup: not installed {}",
            crowded.display()
        )
    );
    assert_eq!(
        fs::read_to_string(changed.join(SKILL_FILE)).unwrap(),
        changed_text
    );
}

#[test]
fn status_reports_each_state_and_changes_nothing() {
    let sandbox = Sandbox::new();
    sandbox.run(&request(Action::Install, "claude,codex"));
    let claude = sandbox.claude_skills();
    let codex = sandbox.home.join(".agents").join("skills");
    // claude: not installed, up to date, update available.
    fs::remove_dir_all(claude.join("godot-mcp")).unwrap();
    put_older_version(&claude.join("godot-mcp-setup"), "An older skill.\n");
    // codex: modified, error, modified without a marker.
    fs::write(codex.join("godot-mcp").join(SKILL_FILE), "Mine.\n").unwrap();
    let playtest = codex.join("godot-mcp-playtest").join(SKILL_FILE);
    fs::remove_file(&playtest).unwrap();
    fs::create_dir(&playtest).unwrap();
    fs::write(codex.join("godot-mcp-setup").join(SKILL_FILE), "Mine.\n").unwrap();
    fs::remove_file(codex.join("godot-mcp-setup").join(MARKER_FILE)).unwrap();
    let before = snapshot(&sandbox.root);

    let (summary, report) = sandbox.run(&request(Action::Status, "claude,codex"));
    assert_eq!(snapshot(&sandbox.root), before, "status changes nothing");
    assert_eq!(
        summary,
        Summary {
            absent: 1,
            unchanged: 1,
            outdated: 1,
            modified: 2,
            errors: 1,
            ..Summary::default()
        }
    );
    assert_eq!(summary.exit_status(), 1);
    let expected = [
        format!(
            "claude: godot-mcp: not installed {}",
            claude.join("godot-mcp").display()
        ),
        format!(
            "claude: godot-mcp-playtest: up to date {} ({VERSION})",
            claude.join("godot-mcp-playtest").display()
        ),
        format!(
            "claude: godot-mcp-setup: update available {} (0.0.1 -> {VERSION})",
            claude.join("godot-mcp-setup").display()
        ),
        format!(
            "codex: godot-mcp: modified {}: its SKILL.md was changed after godot-mcp wrote it",
            codex.join("godot-mcp").display()
        ),
        format!(
            "codex: godot-mcp-playtest: error {}: {} is a directory, not a regular file",
            codex.join("godot-mcp-playtest").display(),
            playtest.display()
        ),
        format!(
            "codex: godot-mcp-setup: modified {}: its SKILL.md differs and it has no marker, \
             so godot-mcp cannot tell it wrote it",
            codex.join("godot-mcp-setup").display()
        ),
        "status: not installed 1, up to date 1, update available 1, modified 2, errors 1"
            .to_owned(),
    ];
    assert_eq!(report.lines().collect::<Vec<_>>(), expected);
}

#[test]
fn every_target_writes_where_the_table_says() {
    let sandbox = Sandbox::new();
    let (summary, _) = sandbox.run(&request(Action::Install, "all"));
    assert_eq!(summary.written, 6 * SKILLS.len());
    let global = [
        sandbox.home.join(".claude/skills"),
        sandbox.home.join(".agents/skills"),
        sandbox.home.join(".cursor/skills"),
        sandbox.home.join(".gemini/skills"),
        sandbox.home.join(".kiro/skills"),
        sandbox.home.join(".config/opencode/skills"),
    ];
    for (target, dir) in Target::ALL.iter().zip(&global) {
        assert_eq!(
            target.skills_dir(&sandbox.context(), Location::Global),
            *dir
        );
        for skill in SKILLS {
            assert!(dir.join(skill.name).join(SKILL_FILE).is_file(), "{dir:?}");
        }
    }

    let sandbox = Sandbox::new();
    let home_before = snapshot(&sandbox.home);
    let (summary, _) = sandbox.run(&Request::new(
        Action::Install,
        TargetSelection::All,
        Location::Local,
    ));
    assert_eq!(summary.written, 6 * SKILLS.len());
    let local = [
        ".claude/skills",
        ".agents/skills",
        ".cursor/skills",
        ".gemini/skills",
        ".kiro/skills",
        ".opencode/skills",
    ];
    for (target, dir) in Target::ALL.iter().zip(local) {
        let dir = sandbox.cwd.join(dir);
        assert_eq!(target.skills_dir(&sandbox.context(), Location::Local), dir);
        for skill in SKILLS {
            assert!(dir.join(skill.name).join(SKILL_FILE).is_file(), "{dir:?}");
        }
    }
    assert_eq!(
        snapshot(&sandbox.home),
        home_before,
        "--local leaves home alone"
    );

    // CLAUDE_CONFIG_DIR replaces ~/.claude, relative to the current directory.
    let sandbox = Sandbox::new();
    let context = SkillContext {
        claude_config_dir: Some(PathBuf::from("claude-config")),
        ..sandbox.context()
    };
    let (summary, _) = run_with(&context, &request(Action::Install, "claude"));
    assert_eq!(summary.written, SKILLS.len());
    assert!(
        sandbox
            .cwd
            .join("claude-config/skills/godot-mcp")
            .join(SKILL_FILE)
            .is_file()
    );
    assert!(!sandbox.home.join(".claude").exists());
}

#[test]
fn auto_selects_every_agent_whose_configuration_directory_exists() {
    let sandbox = Sandbox::new();
    for dir in [".codex", ".gemini", ".config/opencode"] {
        fs::create_dir_all(sandbox.home.join(dir)).unwrap();
    }
    let resolved = resolve(&TargetSelection::Auto, &sandbox.context(), Location::Global);
    assert_eq!(
        resolved.targets,
        [Target::Codex, Target::Gemini, Target::Opencode]
    );
    assert!(!resolved.fell_back);
    let (summary, report) = sandbox.run(&request(Action::Install, "auto"));
    assert_eq!(summary.written, 3 * SKILLS.len());
    assert!(!report.contains("auto:"), "{report}");
    assert!(sandbox.home.join(".agents/skills/godot-mcp").is_dir());
    assert!(sandbox.home.join(".gemini/skills/godot-mcp").is_dir());
    assert!(
        sandbox
            .home
            .join(".config/opencode/skills/godot-mcp")
            .is_dir()
    );
    assert!(!sandbox.home.join(".claude").exists());

    // CLAUDE_CONFIG_DIR and CODEX_HOME count when they exist.
    let sandbox = Sandbox::new();
    let claude_config = sandbox.root.join("claude-config");
    let codex_home = sandbox.root.join("codex-home");
    fs::create_dir_all(&claude_config).unwrap();
    fs::create_dir_all(&codex_home).unwrap();
    let context = SkillContext {
        claude_config_dir: Some(claude_config.clone()),
        codex_home: Some(codex_home),
        ..sandbox.context()
    };
    let resolved = resolve(&TargetSelection::Auto, &context, Location::Global);
    assert_eq!(resolved.targets, [Target::Claude, Target::Codex]);
    let (summary, _) = run_with(&context, &request(Action::Install, "auto"));
    assert_eq!(summary.written, 2 * SKILLS.len());
    assert!(claude_config.join("skills/godot-mcp").is_dir());
    assert!(sandbox.home.join(".agents/skills/godot-mcp").is_dir());

    // An unset CLAUDE_CONFIG_DIR's directory that does not exist counts for nothing.
    let sandbox = Sandbox::new();
    let context = SkillContext {
        claude_config_dir: Some(sandbox.root.join("missing")),
        ..sandbox.context()
    };
    fs::create_dir_all(sandbox.home.join(".kiro")).unwrap();
    let resolved = resolve(&TargetSelection::Auto, &context, Location::Global);
    assert_eq!(resolved.targets, [Target::Kiro]);

    // --local looks in the current directory.
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.cwd.join(".codex")).unwrap();
    fs::create_dir_all(sandbox.cwd.join(".cursor")).unwrap();
    fs::create_dir_all(sandbox.home.join(".claude")).unwrap();
    let resolved = resolve(&TargetSelection::Auto, &sandbox.context(), Location::Local);
    assert_eq!(resolved.targets, [Target::Codex, Target::Cursor]);
    let (summary, _) = sandbox.run(&Request::new(
        Action::Install,
        TargetSelection::Auto,
        Location::Local,
    ));
    assert_eq!(summary.written, 2 * SKILLS.len());
    assert!(sandbox.cwd.join(".agents/skills/godot-mcp").is_dir());
    assert!(sandbox.cwd.join(".cursor/skills/godot-mcp").is_dir());
}

#[test]
fn auto_falls_back_to_claude_when_no_agent_is_configured() {
    for location in [Location::Global, Location::Local] {
        let sandbox = Sandbox::new();
        let resolved = resolve(&TargetSelection::Auto, &sandbox.context(), location);
        assert_eq!(resolved.targets, [Target::Claude]);
        assert!(resolved.fell_back);
        let (summary, report) = sandbox.run(&Request::new(
            Action::Install,
            TargetSelection::Auto,
            location,
        ));
        assert_eq!(summary.written, SKILLS.len());
        assert!(
            report.starts_with("auto: no agent configuration directory was found; using claude\n"),
            "{report}"
        );
        let base = match location {
            Location::Global => &sandbox.home,
            Location::Local => &sandbox.cwd,
        };
        assert_eq!(names_in(base), [".claude"]);
        assert!(
            base.join(".claude/skills/godot-mcp")
                .join(SKILL_FILE)
                .is_file()
        );
    }
}

#[test]
fn a_target_list_is_parsed_strictly() {
    assert_eq!(parse_target_selection("auto"), Ok(TargetSelection::Auto));
    assert_eq!(parse_target_selection("all"), Ok(TargetSelection::All));
    assert_eq!(
        parse_target_selection(" claude , codex,claude "),
        Ok(TargetSelection::Only(vec![Target::Claude, Target::Codex]))
    );
    let error = parse_target_selection("bogus").expect_err("an unknown target");
    assert!(
        error.starts_with("unknown target bogus; expected auto, all, or"),
        "{error}"
    );
    for id in ["claude", "codex", "cursor", "gemini", "kiro", "opencode"] {
        assert!(error.contains(id), "{error} names {id}");
        assert_eq!(Target::from_id(id).map(Target::id), Some(id));
    }
    for value in ["", "claude,", "claude,bogus", "auto,claude", "codex,all"] {
        assert!(
            parse_target_selection(value).is_err(),
            "{value:?} is refused"
        );
    }
}

#[test]
fn the_context_reads_home_and_the_agent_directories_from_the_environment() {
    fn lookup(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }
    let cwd = PathBuf::from("/work");
    let context = SkillContext::from_variables(
        cwd.clone(),
        lookup(&[("HOME", "/home/a"), ("USERPROFILE", "/profile")]),
    )
    .unwrap();
    assert_eq!(context.home, PathBuf::from("/home/a"));
    assert_eq!(context.claude_config_dir, None);
    assert_eq!(context.codex_home, None);
    let context =
        SkillContext::from_variables(cwd.clone(), lookup(&[("HOME", " "), ("USERPROFILE", "/p")]))
            .unwrap();
    assert_eq!(
        context.home,
        PathBuf::from("/p"),
        "a blank HOME counts as unset"
    );
    assert!(SkillContext::from_variables(cwd.clone(), lookup(&[])).is_err());
    let context = SkillContext::from_variables(
        cwd.clone(),
        lookup(&[
            ("HOME", "/home/a"),
            ("CLAUDE_CONFIG_DIR", "conf"),
            ("CODEX_HOME", ""),
        ]),
    )
    .unwrap();
    assert_eq!(context.claude_config_dir, Some(PathBuf::from("conf")));
    assert_eq!(
        context.codex_home, None,
        "a blank CODEX_HOME counts as unset"
    );
    assert_eq!(
        Target::Claude.skills_dir(&context, Location::Global),
        PathBuf::from("/work/conf/skills")
    );
}

#[test]
fn the_binary_reports_and_exits_by_the_rules() {
    let sandbox = Sandbox::new();
    let local = sandbox.cwd.join(".claude").join("skills");
    sandbox
        .binary()
        .args(["skill", "install", "--target", "claude", "--local"])
        .assert()
        .code(0)
        .stdout(contains(format!(
            "claude: godot-mcp: installed {}",
            local.join("godot-mcp").display()
        )));
    for skill in SKILLS {
        assert_eq!(
            fs::read(local.join(skill.name).join(SKILL_FILE)).unwrap(),
            skill.content.as_bytes()
        );
    }
    assert!(
        !sandbox.home.join(".claude").exists(),
        "--local stays out of home"
    );
    sandbox
        .binary()
        .args(["skill", "status", "--target", "claude", "--local"])
        .assert()
        .code(0)
        .stdout(contains("claude: godot-mcp-setup: up to date"));

    // A skipped skill exits 0; an error exits 1 after every other skill ran.
    fs::write(local.join("godot-mcp").join(SKILL_FILE), "Mine.\n").unwrap();
    sandbox
        .binary()
        .args(["skill", "update", "--target", "claude", "--local", "--diff"])
        .assert()
        .code(0)
        .stdout(contains("claude: godot-mcp: skipped"))
        .stdout(contains("-Mine."));
    let playtest = local.join("godot-mcp-playtest").join(SKILL_FILE);
    fs::remove_file(&playtest).unwrap();
    fs::create_dir(&playtest).unwrap();
    sandbox
        .binary()
        .args(["skill", "uninstall", "--target", "claude", "--local"])
        .assert()
        .code(1)
        .stdout(contains("claude: godot-mcp-playtest: error"))
        .stdout(contains("claude: godot-mcp-setup: removed"))
        .stdout(contains("claude: godot-mcp: kept"));
    assert!(!local.join("godot-mcp-setup").exists());

    // The default is --global under HOME.
    sandbox
        .binary()
        .args(["skill", "install", "--target", "codex"])
        .assert()
        .code(0);
    assert!(
        sandbox
            .home
            .join(".agents/skills/godot-mcp")
            .join(SKILL_FILE)
            .is_file()
    );

    // Argument errors are clap's, exit 2.
    for arguments in [
        &["skill"][..],
        &["skill", "install", "--target", "bogus"],
        &["skill", "install", "--global", "--local"],
        &["skill", "install", "--dry-run"],
        &["skill", "install", "--diff"],
        &["skill", "uninstall", "--force"],
        &["skill", "status", "--force"],
        &["skill", "status", "--dry-run"],
    ] {
        sandbox.binary().args(arguments).assert().code(2);
    }
    sandbox
        .binary()
        .args(["skill", "install", "--target", "claude,bogus"])
        .assert()
        .code(2)
        .stderr(contains("unknown target bogus"))
        .stderr(contains("opencode"));

    // Without a home directory nothing can be resolved.
    sandbox
        .binary()
        .env_remove("HOME")
        .args(["skill", "status"])
        .assert()
        .code(1)
        .stderr(contains("neither HOME nor USERPROFILE is set"));
}

/// The documentation security lint lets `--force` appear only with the two
/// commands that accept it. This holds that exemption to the binary: every
/// command of the shipped surface is asked for `--force --help`, and only those
/// two accept the flag.
#[test]
fn only_skill_install_and_skill_update_accept_force() {
    let surface =
        audit::cli_subcommand_surface(&workspace_root()).expect("the binary source is readable");
    let accepting: Vec<String> = surface
        .iter()
        .filter(|command| {
            let mut arguments: Vec<&str> = command.split(' ').collect();
            arguments.extend(["--force", "--help"]);
            let sandbox = Sandbox::new();
            let output = sandbox.binary().args(&arguments).output().expect("run");
            output.status.success()
        })
        .cloned()
        .collect();
    assert_eq!(accepting, ["skill install", "skill update"]);
    assert_eq!(
        godot_mcp_cli::docs::security::FORCE_FLAG_COMMANDS,
        ["godot-mcp skill install", "godot-mcp skill update"]
    );
}
