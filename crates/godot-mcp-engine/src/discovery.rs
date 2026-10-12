//! Resolving *which* Godot executable to probe.
//!
//! Resolution order is fixed and explicit-first:
//!
//! 1. the operator's explicitly configured `godot_binary`, then
//! 2. the platform's conventional install locations, in a deterministic order.
//!
//! An explicit configuration therefore always wins over any ambient discovery,
//! matching the "command-line/config override beats convention" precedence the
//! rest of the system uses. Discovery never downloads, installs or fabricates a
//! Godot build: if nothing on the candidate list exists as a regular file, the
//! result is [`DiscoveryError::NotFound`] with operator remediation.
//!
//! Candidate *paths* are display metadata only. The authority for a candidate is
//! the handle-checked digest taken in [`crate::probe`]; a path that names a
//! symlink or a non-regular object is rejected there rather than followed.

use std::path::{Path, PathBuf};

use godot_mcp_domain::ErrorKind;

/// Which host platform's conventional locations to enumerate.
///
/// Passed explicitly rather than read from `cfg!` so the suite can assert all
/// three platform shapes from one host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostPlatform {
    /// Linux hosts.
    Linux,
    /// macOS hosts.
    MacOs,
    /// Windows hosts.
    Windows,
}

impl HostPlatform {
    /// The platform this build is running on.
    #[must_use]
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Linux
        }
    }
}

/// Ambient environment values discovery is allowed to consult.
///
/// Captured explicitly instead of read from the process environment inside the
/// resolver, so candidate enumeration is a pure function of its inputs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentSnapshot {
    /// The user's home directory, if known.
    pub home: Option<PathBuf>,
    /// `PATH` entries, in order.
    pub path_dirs: Vec<PathBuf>,
    /// Windows `%ProgramFiles%`, if known.
    pub program_files: Option<PathBuf>,
    /// Windows `%LocalAppData%`, if known.
    pub local_app_data: Option<PathBuf>,
}

impl EnvironmentSnapshot {
    /// Capture the current process environment.
    #[must_use]
    pub fn from_process_env() -> Self {
        let path_dirs = std::env::var_os("PATH")
            .map(|value| std::env::split_paths(&value).collect())
            .unwrap_or_default();
        Self {
            home: std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from),
            path_dirs,
            program_files: std::env::var_os("ProgramFiles").map(PathBuf::from),
            local_app_data: std::env::var_os("LocalAppData").map(PathBuf::from),
        }
    }
}

/// Where a candidate came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CandidateOrigin {
    /// The operator's explicit `godot_binary` configuration.
    ExplicitConfig,
    /// A conventional platform install location.
    PlatformCandidate,
}

/// One candidate executable location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Display path of the candidate. Never an authority by itself.
    pub path: PathBuf,
    /// Where the candidate came from.
    pub origin: CandidateOrigin,
}

/// A candidate that exists as a regular file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBinary {
    /// Display path of the resolved executable.
    pub path: PathBuf,
    /// Where the resolution came from.
    pub origin: CandidateOrigin,
}

/// Why discovery failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiscoveryError {
    /// No candidate exists as a regular file.
    #[error(
        "no Godot executable was found; searched {searched} candidate location(s) starting with {first}"
    )]
    NotFound {
        /// How many candidates were inspected.
        searched: usize,
        /// The first candidate inspected, for operator orientation.
        first: String,
    },
    /// The explicit configuration named something that is not a regular file.
    #[error("the configured Godot path `{path}` is not a regular file")]
    NotRegularFile {
        /// The rejected display path.
        path: String,
    },
}

impl DiscoveryError {
    /// Stable error classification.
    #[must_use]
    pub fn error_kind(&self) -> ErrorKind {
        match self {
            Self::NotFound { .. } => ErrorKind::NotFound,
            Self::NotRegularFile { .. } => ErrorKind::InvalidInput,
        }
    }

    /// Operator-facing remediation. Never offers to fetch Godot.
    #[must_use]
    pub fn remediation(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => {
                "Install a supported Godot build yourself, then configure godot_binary (or \
                 GODOT_MCP_GODOT_BINARY) with its full path. godot-mcp never fetches an engine \
                 build on your behalf."
            }
            Self::NotRegularFile { .. } => {
                "Configure godot_binary with the full path of the Godot executable file itself, \
                 not a directory or an application bundle."
            }
        }
    }
}

/// Conventional install locations for `platform`, in search order.
#[must_use]
pub fn platform_candidates(platform: HostPlatform, env: &EnvironmentSnapshot) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    match platform {
        HostPlatform::Linux => {
            candidates.push(PathBuf::from("/usr/local/bin/godot"));
            candidates.push(PathBuf::from("/usr/bin/godot"));
            candidates.push(PathBuf::from("/opt/godot/godot"));
            candidates.push(PathBuf::from(
                "/var/lib/flatpak/exports/bin/org.godotengine.Godot",
            ));
            if let Some(home) = &env.home {
                candidates.push(home.join(".local/bin/godot"));
                candidates.push(home.join("Applications/godot"));
            }
        }
        HostPlatform::MacOs => {
            candidates.push(PathBuf::from(
                "/Applications/Godot.app/Contents/MacOS/Godot",
            ));
            candidates.push(PathBuf::from("/usr/local/bin/godot"));
            candidates.push(PathBuf::from("/opt/homebrew/bin/godot"));
            if let Some(home) = &env.home {
                candidates.push(home.join("Applications/Godot.app/Contents/MacOS/Godot"));
                candidates.push(home.join(".local/bin/godot"));
            }
        }
        HostPlatform::Windows => {
            if let Some(program_files) = &env.program_files {
                candidates.push(program_files.join("Godot").join("Godot.exe"));
            }
            candidates.push(PathBuf::from(r"C:\Program Files\Godot\Godot.exe"));
            if let Some(local_app_data) = &env.local_app_data {
                candidates.push(
                    local_app_data
                        .join("Programs")
                        .join("Godot")
                        .join("Godot.exe"),
                );
            }
            if let Some(home) = &env.home {
                candidates.push(home.join("Godot").join("Godot.exe"));
            }
        }
    }
    for dir in &env.path_dirs {
        candidates.push(dir.join(executable_leaf(platform)));
    }
    candidates
}

const fn executable_leaf(platform: HostPlatform) -> &'static str {
    match platform {
        HostPlatform::Windows => "Godot.exe",
        HostPlatform::Linux | HostPlatform::MacOs => "godot",
    }
}

/// The full resolution order: the explicit configuration first (when present),
/// then the platform candidates.
#[must_use]
pub fn resolution_order(
    explicit: Option<&Path>,
    platform: HostPlatform,
    env: &EnvironmentSnapshot,
) -> Vec<Candidate> {
    let mut order: Vec<Candidate> = Vec::new();
    if let Some(path) = explicit {
        order.push(Candidate {
            path: path.to_path_buf(),
            origin: CandidateOrigin::ExplicitConfig,
        });
    }
    order.extend(
        platform_candidates(platform, env)
            .into_iter()
            .map(|path| Candidate {
                path,
                origin: CandidateOrigin::PlatformCandidate,
            }),
    );
    order
}

/// The executable a command-line client drives for `resolved`. Godot's
/// Windows builds are GUI programs that only attach to a console their parent
/// already has; every official archive ships a console executable beside the
/// windowed one (`<name>_console.exe`), which owns a console, runs the engine
/// attached to it and relays its standard streams. That one is driven whenever
/// it is there.
#[must_use]
pub fn command_line_executable(
    platform: HostPlatform,
    resolved: &Path,
    is_file: &dyn Fn(&Path) -> bool,
) -> PathBuf {
    if platform != HostPlatform::Windows {
        return resolved.to_path_buf();
    }
    let Some(stem) = resolved.file_stem().and_then(|stem| stem.to_str()) else {
        return resolved.to_path_buf();
    };
    if stem.ends_with("_console") {
        return resolved.to_path_buf();
    }
    let console = resolved.with_file_name(format!("{stem}_console.exe"));
    if is_file(&console) {
        console
    } else {
        resolved.to_path_buf()
    }
}

/// Resolve the first candidate that exists as a regular file.
///
/// # Errors
/// Returns [`DiscoveryError::NotRegularFile`] when the explicit configuration
/// names a non-regular object, or [`DiscoveryError::NotFound`] when no candidate
/// exists. Nothing is downloaded in either case.
pub fn discover(
    explicit: Option<&Path>,
    platform: HostPlatform,
    env: &EnvironmentSnapshot,
) -> Result<ResolvedBinary, DiscoveryError> {
    let order = resolution_order(explicit, platform, env);
    for candidate in &order {
        // `symlink_metadata` deliberately does not follow a final symlink: a
        // candidate is only accepted when the named entry is itself a regular
        // file. The authoritative check happens again, handle-relative, in the
        // probe.
        match std::fs::symlink_metadata(&candidate.path) {
            Ok(metadata) if metadata.is_file() => {
                return Ok(ResolvedBinary {
                    path: command_line_executable(platform, &candidate.path, &|path| {
                        std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
                    }),
                    origin: candidate.origin,
                });
            }
            Ok(_) if candidate.origin == CandidateOrigin::ExplicitConfig => {
                return Err(DiscoveryError::NotRegularFile {
                    path: candidate.path.display().to_string(),
                });
            }
            _ => {}
        }
    }
    Err(DiscoveryError::NotFound {
        searched: order.len(),
        first: order
            .first()
            .map(|candidate| candidate.path.display().to_string())
            .unwrap_or_else(|| "<no candidates>".to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_windowed_windows_build_is_driven_through_its_console_executable() {
        let windowed = Path::new(r"C:\godot\Godot_v4.7.1-stable_win64.exe");
        let console = Path::new(r"C:\godot\Godot_v4.7.1-stable_win64_console.exe");
        let both = |path: &Path| path == windowed || path == console;
        assert_eq!(
            command_line_executable(HostPlatform::Windows, windowed, &both),
            console
        );
        assert_eq!(
            command_line_executable(HostPlatform::Windows, console, &both),
            console,
            "the console executable is already the one to drive"
        );
        let only_windowed = |path: &Path| path == windowed;
        assert_eq!(
            command_line_executable(HostPlatform::Windows, windowed, &only_windowed),
            windowed,
            "without a console executable beside it the configured one stays"
        );
        let linux = Path::new("/opt/godot/godot");
        assert_eq!(
            command_line_executable(HostPlatform::Linux, linux, &|_| true),
            linux
        );
    }

    fn env() -> EnvironmentSnapshot {
        EnvironmentSnapshot {
            home: Some(PathBuf::from("/home/tester")),
            path_dirs: vec![PathBuf::from("/custom/bin")],
            program_files: Some(PathBuf::from(r"C:\Program Files")),
            local_app_data: Some(PathBuf::from(r"C:\Users\tester\AppData\Local")),
        }
    }

    #[test]
    fn explicit_configuration_is_always_first() {
        let order = resolution_order(
            Some(Path::new("/explicit/godot")),
            HostPlatform::Linux,
            &env(),
        );
        assert_eq!(order[0].origin, CandidateOrigin::ExplicitConfig);
        assert_eq!(order[0].path, PathBuf::from("/explicit/godot"));
        assert!(order.len() > 1);
    }

    #[test]
    fn every_platform_enumerates_path_entries_last() {
        for platform in [
            HostPlatform::Linux,
            HostPlatform::MacOs,
            HostPlatform::Windows,
        ] {
            let candidates = platform_candidates(platform, &env());
            let last = candidates.last().expect("candidates are non-empty");
            assert!(
                last.starts_with("/custom/bin"),
                "{platform:?} must search PATH last, got {last:?}"
            );
        }
    }

    #[test]
    fn absent_candidates_report_not_found_without_downloading() {
        let error = discover(
            Some(Path::new("/definitely/absent/godot")),
            HostPlatform::Linux,
            &EnvironmentSnapshot::default(),
        )
        .expect_err("nothing exists");
        assert_eq!(error.error_kind(), ErrorKind::NotFound);
        assert!(
            !error
                .remediation()
                .to_ascii_lowercase()
                .contains("download")
        );
    }
}
