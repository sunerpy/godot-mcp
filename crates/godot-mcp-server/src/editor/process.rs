//! Which editor process a bridge session runs in, when the server can prove it.
//!
//! The add-on's `bridge.describe_session` reports the editor's process id and
//! executable. The server keeps an identity only when it can check both against
//! the operating system: the id names a live process, the platform reports that
//! process's start time, and the process runs the executable the editor named.
//! The start time is what keeps a later process that reuses the id from passing
//! for the editor. Anything less keeps nothing: an editor in another pid
//! namespace (a container, a Flatpak sandbox) reports an id this host does not
//! resolve to the same program, and an add-on older than the `executable`
//! field names none.
//!
//! The identity is what lets an operator settle a request the editor never
//! answered once that editor has ended (`transaction repair --operation
//! acknowledge-safe-state`): [`has_ended`] proves the writer is gone, never
//! what it wrote.

use std::io;
use std::path::Path;

use godot_mcp_engine::supervision::procfs::{self, ProcessIdentity};
use serde_json::Value;

use crate::journal::EditorProcess;

/// The editor process `description` reports, when this host can prove it.
pub(crate) fn observe(description: &Value) -> Option<EditorProcess> {
    if !procfs::observations_available() {
        return None;
    }
    let (pid, executable) = reported(description)?;
    identify(pid, executable, procfs::process_identity, runs)
}

/// The process id and executable a session description names.
fn reported(description: &Value) -> Option<(u32, &Path)> {
    let pid = description
        .get("pid")?
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid > 0)?;
    let executable = description
        .get("executable")?
        .as_str()
        .filter(|executable| !executable.is_empty())?;
    Some((pid, Path::new(executable)))
}

/// The identity of `pid` when it has a start time and runs `executable`. The
/// process is observed on both sides of the executable check, so an id that
/// changes hands in between is not recorded for either process.
fn identify(
    pid: u32,
    executable: &Path,
    mut observed: impl FnMut(u32) -> io::Result<Option<ProcessIdentity>>,
    runs: impl FnOnce(u32, &Path) -> bool,
) -> Option<EditorProcess> {
    let start_time = |identity: io::Result<Option<ProcessIdentity>>| {
        identity
            .ok()
            .flatten()
            .filter(|identity| identity.pid == pid)
            .and_then(|identity| identity.start_time)
            .filter(|start_time| !start_time.is_empty())
    };
    let before = start_time(observed(pid))?;
    if !runs(pid, executable) {
        return None;
    }
    let after = start_time(observed(pid))?;
    (before == after).then_some(EditorProcess {
        pid,
        start_time: before,
    })
}

/// Whether `pid` runs the file at `executable`: the image the process runs and
/// the file the path names are one file.
#[cfg(unix)]
fn runs(pid: u32, executable: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    let Ok(Some(image)) = procfs::executable(pid) else {
        return false;
    };
    match (std::fs::metadata(image), std::fs::metadata(executable)) {
        (Ok(running), Ok(named)) => running.dev() == named.dev() && running.ino() == named.ino(),
        _ => false,
    }
}

/// No platform without Unix file identities observes process identity yet.
#[cfg(not(unix))]
const fn runs(_pid: u32, _executable: &Path) -> bool {
    false
}

/// Whether the recorded editor process has ended: no live process has its id,
/// or the one that has it started at another time.
///
/// # Errors
/// Returns why when the platform cannot observe the process, so the caller
/// keeps the transaction as it is rather than assuming the editor is gone.
pub(crate) fn has_ended(recorded: &EditorProcess) -> Result<bool, String> {
    if !procfs::observations_available() {
        return Err("this platform cannot observe process identity".to_owned());
    }
    match procfs::process_identity(recorded.pid) {
        Ok(Some(identity)) => {
            Ok(!identity.matches_recorded(recorded.pid, Some(&recorded.start_time), None))
        }
        Ok(None) => Ok(true),
        Err(error) => Err(format!(
            "pid {} could not be observed: {error}",
            recorded.pid
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::Path;

    use godot_mcp_engine::supervision::procfs::{self, ProcessIdentity};
    use serde_json::json;

    use super::*;

    fn identity(pid: u32, start_time: Option<&str>) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            start_time: start_time.map(str::to_owned),
            process_group: None,
            argv: Vec::new(),
        }
    }

    #[test]
    fn an_identity_is_recorded_only_when_the_start_time_and_the_executable_check_out() {
        let editor = Path::new("/opt/godot/Godot_v4.7.2-stable_linux.x86_64");
        let observed = |pid: u32| Ok(Some(identity(pid, Some("12345"))));
        assert_eq!(
            identify(4242, editor, observed, |_, _| true),
            Some(EditorProcess {
                pid: 4242,
                start_time: "12345".to_owned()
            })
        );
        // The platform reports no start time, so a later process reusing the
        // id could not be told apart.
        assert_eq!(
            identify(
                4242,
                editor,
                |pid| Ok(Some(identity(pid, None))),
                |_, _| true
            ),
            None
        );
        // The process runs another executable than the one the editor named.
        assert_eq!(identify(4242, editor, observed, |_, _| false), None);
        // No such process, or one that cannot be observed.
        assert_eq!(identify(4242, editor, |_| Ok(None), |_, _| true), None);
        assert_eq!(
            identify(
                4242,
                editor,
                |_| Err(io::Error::other("no process table")),
                |_, _| true
            ),
            None
        );
        // The id changed hands while it was being checked.
        let mut starts = ["12345", "67890"].into_iter();
        assert_eq!(
            identify(
                4242,
                editor,
                |pid| Ok(Some(identity(pid, starts.next()))),
                |_, _| true
            ),
            None
        );
    }

    #[test]
    fn a_description_without_a_pid_or_an_executable_names_no_process() {
        let editor = "/opt/godot/Godot_v4.7.2-stable_linux.x86_64";
        assert_eq!(reported(&json!({ "pid": 4242 })), None, "an older add-on");
        assert_eq!(reported(&json!({ "executable": editor })), None);
        assert_eq!(reported(&json!({ "pid": 0, "executable": editor })), None);
        assert_eq!(reported(&json!({ "pid": -1, "executable": editor })), None);
        assert_eq!(reported(&json!({ "pid": 4242, "executable": "" })), None);
        assert_eq!(
            reported(&json!({ "pid": 4242, "executable": editor })),
            Some((4242, Path::new(editor)))
        );
    }

    /// The host's own observation, against this test process.
    #[test]
    fn this_process_is_observed_under_its_own_executable_and_no_other() {
        let own = std::process::id();
        let executable = std::env::current_exe().expect("this test's executable");
        let described = json!({ "pid": own, "executable": executable.display().to_string() });
        if !procfs::observations_available() {
            assert_eq!(observe(&described), None);
            return;
        }
        let start_time = procfs::process_identity(own)
            .expect("this process is observable")
            .expect("this process is live")
            .start_time
            .expect("a start time is reported");
        assert_eq!(
            observe(&described),
            Some(EditorProcess {
                pid: own,
                start_time
            })
        );
        let elsewhere = tempfile::NamedTempFile::new().expect("another file");
        assert_eq!(
            observe(&json!({ "pid": own, "executable": elsewhere.path().display().to_string() })),
            None,
            "another file is not the image this process runs"
        );
        assert_eq!(
            observe(&json!({ "pid": own })),
            None,
            "an editor that names no executable is not identified"
        );
    }

    #[test]
    fn a_recorded_process_has_ended_once_its_pid_is_gone_or_reused() {
        if !procfs::observations_available() {
            return;
        }
        let own = std::process::id();
        let start_time = procfs::process_identity(own)
            .expect("this process is observable")
            .expect("this process is live")
            .start_time
            .expect("a start time is reported");
        let running = EditorProcess {
            pid: own,
            start_time: start_time.clone(),
        };
        assert_eq!(has_ended(&running), Ok(false), "this process still runs");
        let reused = EditorProcess {
            pid: own,
            start_time: format!("{start_time}0"),
        };
        assert_eq!(
            has_ended(&reused),
            Ok(true),
            "a live process with another start time is not the one recorded"
        );
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("a short-lived child");
        let pid = child.id();
        child.wait().expect("the child exits");
        let gone = EditorProcess {
            pid,
            start_time: "1".to_owned(),
        };
        assert_eq!(has_ended(&gone), Ok(true), "a reaped child has ended");
    }
}
