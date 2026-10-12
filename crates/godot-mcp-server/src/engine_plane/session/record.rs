//! The durable record of one managed Godot session.
//!
//! A session's record lives in its private directory under the server's state
//! root, next to its probe scripts and archived output. It is rewritten
//! atomically on every state change, so after a crash the restarted server
//! knows exactly which sessions it may have left running and which boundary
//! each one was contained in. The boundary identity, PIDs and paths stay in
//! this private record; the public projection is [`SessionRecord::public`].

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The record file inside a session directory.
pub(super) const RECORD_FILE: &str = "session.json";
/// The archived stdout inside a session directory.
pub(super) const STDOUT_LOG: &str = "stdout.log";
/// The archived stderr inside a session directory.
pub(super) const STDERR_LOG: &str = "stderr.log";

pub(crate) use crate::effect::boundary::SessionKind;

/// Where a session is in its life.
///
/// ```text
/// starting -> running -> stopping -> exited | cancelled (an export)
///          \-> failed
/// (left by a server that stopped) -> reconciled | quarantined
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionState {
    Starting,
    Running,
    Stopping,
    Exited,
    Failed,
    Reconciled,
    Quarantined,
    Cancelled,
}

impl SessionState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Exited => "exited",
            Self::Failed => "failed",
            Self::Reconciled => "reconciled",
            Self::Quarantined => "quarantined",
            Self::Cancelled => "cancelled",
        }
    }

    /// Whether nothing of the session can still be running.
    pub(crate) const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Exited | Self::Failed | Self::Reconciled | Self::Quarantined | Self::Cancelled
        )
    }
}

/// How a session's process ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExitFacts {
    pub(crate) termination: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) signal: Option<String>,
    pub(crate) timed_out: bool,
    pub(crate) boundary_empty: Option<bool>,
}

/// The complete private record of one session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SessionRecord {
    pub(crate) schema_version: String,
    pub(crate) session_id: String,
    pub(crate) kind: SessionKind,
    pub(crate) state: SessionState,
    pub(crate) scene: Option<String>,
    pub(crate) display: String,
    pub(crate) created_at_unix_ms: u64,
    pub(crate) started_at_unix_ms: Option<u64>,
    pub(crate) ended_at_unix_ms: Option<u64>,
    pub(crate) lifetime_ms: u64,
    pub(crate) start_transaction_id: Option<String>,
    pub(crate) launch_transaction_id: Option<String>,
    /// The containment boundary's durable identity facts, private.
    pub(crate) boundary: Option<BTreeMap<String, Value>>,
    pub(crate) boundary_kind: Option<String>,
    pub(crate) boundary_strength: Option<String>,
    pub(crate) binary_sha256: Option<String>,
    pub(crate) argv_sha256: Option<String>,
    pub(crate) godot_version: Option<String>,
    pub(crate) lsp_port: Option<u16>,
    pub(crate) dap_port: Option<u16>,
    pub(crate) exit: Option<ExitFacts>,
    pub(crate) reason: Option<String>,
    /// What an export session exports and produced.
    #[serde(default)]
    pub(crate) export: Option<super::export::ExportRecord>,
}

impl SessionRecord {
    pub(crate) fn new(
        session_id: String,
        kind: SessionKind,
        scene: Option<String>,
        display: String,
        lifetime_ms: u64,
        created_at_unix_ms: u64,
    ) -> Self {
        Self {
            schema_version: "1".to_owned(),
            session_id,
            kind,
            state: SessionState::Starting,
            scene,
            display,
            created_at_unix_ms,
            started_at_unix_ms: None,
            ended_at_unix_ms: None,
            lifetime_ms,
            start_transaction_id: None,
            launch_transaction_id: None,
            boundary: None,
            boundary_kind: None,
            boundary_strength: None,
            binary_sha256: None,
            argv_sha256: None,
            godot_version: None,
            lsp_port: None,
            dap_port: None,
            exit: None,
            reason: None,
            export: None,
        }
    }

    /// The path-free, PID-free projection a client may see.
    pub(crate) fn public(&self) -> Value {
        json!({
            "session": self.session_id,
            "kind": self.kind.as_str(),
            "state": self.state.as_str(),
            "scene": self.scene,
            "display": self.display,
            "created_at_unix_ms": self.created_at_unix_ms,
            "started_at_unix_ms": self.started_at_unix_ms,
            "ended_at_unix_ms": self.ended_at_unix_ms,
            "lifetime_ms": self.lifetime_ms,
            "start_transaction_id": self.start_transaction_id,
            "boundary_kind": self.boundary_kind,
            "boundary_strength": self.boundary_strength,
            "binary_sha256": self.binary_sha256,
            "argv_sha256": self.argv_sha256,
            "godot_version": self.godot_version,
            "exit": self.exit.as_ref().map(|exit| json!({
                "termination": exit.termination,
                "exit_code": exit.exit_code,
                "signal": exit.signal,
                "timed_out": exit.timed_out,
                "boundary_empty": exit.boundary_empty,
            })),
            "reason": self.reason,
            "export": self.export.as_ref().map(|export| export.public(None, super::now_unix_ms())),
        })
    }
}

/// Write `record` into `directory` atomically: a private temporary file,
/// fsynced, renamed over the record, then the directory fsynced.
pub(super) fn write(directory: &Path, record: &SessionRecord) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(record).map_err(|error| error.to_string())?;
    let target = directory.join(RECORD_FILE);
    let staging = directory.join(format!(".{RECORD_FILE}.staging"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&staging)
        .map_err(|error| format!("the session record cannot be written: {error}"))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("the session record cannot be written: {error}"))?;
    drop(file);
    std::fs::rename(&staging, &target)
        .map_err(|error| format!("the session record cannot be written: {error}"))?;
    if let Ok(handle) = std::fs::File::open(directory) {
        let _ = handle.sync_all();
    }
    Ok(())
}

/// Every readable session record under `root`, with its directory.
pub(super) fn load_all(root: &Path) -> Vec<(PathBuf, SessionRecord)> {
    let Ok(listing) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut records: Vec<(PathBuf, SessionRecord)> = listing
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter_map(|directory| {
            let bytes = std::fs::read(directory.join(RECORD_FILE)).ok()?;
            let record = serde_json::from_slice::<SessionRecord>(&bytes).ok()?;
            Some((directory, record))
        })
        .collect();
    records.sort_by(|left, right| left.1.session_id.cmp(&right.1.session_id));
    records
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips_and_its_public_view_carries_no_private_fact() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut record = SessionRecord::new(
            format!("run_{}", "a".repeat(32)),
            SessionKind::Game,
            Some("res://main.tscn".to_owned()),
            "window".to_owned(),
            60_000,
            1,
        );
        record.boundary = Some(
            [(
                "boundary_identity".to_owned(),
                json!("/private/state/launches/locator-x"),
            )]
            .into_iter()
            .collect(),
        );
        let session = directory.path().join(&record.session_id);
        std::fs::create_dir(&session).expect("session directory");
        write(&session, &record).expect("write");
        let loaded = load_all(directory.path());
        assert_eq!(loaded, vec![(session, record.clone())]);
        let public = record.public().to_string();
        assert!(!public.contains("/private/state"), "{public}");
        assert!(!public.contains("pid"), "{public}");
    }
}
