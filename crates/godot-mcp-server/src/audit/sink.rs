//! The append-only, size-bounded, deterministically rotatable audit sink.
//!
//! ## Append-only by construction
//!
//! The sink opens its file with `append(true)` and never seeks, truncates or
//! rewrites. A recorded record's bytes are therefore a permanent prefix of the
//! live file until rotation moves the whole file aside, which the acceptance
//! suite asserts by comparing successive reads.
//!
//! ## Deterministic rotation
//!
//! Rotation depends on exactly two inputs: the live file's current length and the
//! configured byte bound. It never consults a clock, a random source or the host
//! locale, so the same sequence of records against the same bounds always rotates
//! the same number of times. Retention is bounded by `max_files`, and the oldest
//! rotation is deleted rather than kept, so total retained bytes are bounded.
//!
//! ## Fail-closed policy
//!
//! [`AuditFailurePolicy`] is a *configuration* decision, never an inference. The
//! sink itself only reports success or failure; refusing a high-risk mutation
//! when the sink is unavailable is the caller's enforcement, driven by this
//! configured value. A fail-open configuration still reports that nothing was
//! recorded, so a permitted mutation is never silently unaudited.

use std::fs::{self};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::observability::{OpenedFileSinkError, open_validated_append};

use super::event::{AuditError, AuditEvent};

/// What the caller must do when the audit sink cannot record an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditFailurePolicy {
    /// Refuse a high-risk mutation whose audit record cannot be written.
    FailClosed,
    /// Permit the operation, still reporting that nothing was recorded.
    FailOpen,
}

impl AuditFailurePolicy {
    /// Whether an unrecordable high-risk mutation must be refused.
    #[must_use]
    pub const fn refuses_high_risk_mutation(self) -> bool {
        matches!(self, Self::FailClosed)
    }
}

/// Where and how bounded the audit sink is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditSinkConfig {
    /// Live audit file. Rotations are siblings suffixed `.1`, `.2`, ….
    pub path: PathBuf,
    /// Rotate before a record would grow the live file past this size.
    pub max_bytes: u64,
    /// How many rotated files to retain.
    pub max_files: u8,
    /// What the caller must do when a record cannot be written.
    pub failure_policy: AuditFailurePolicy,
}

/// The append-only audit log.
#[derive(Debug)]
pub struct AuditLog {
    config: AuditSinkConfig,
    next_sequence: AtomicU64,
    rotations: AtomicUsize,
    write_failure: AtomicBool,
    record_failure: AtomicBool,
    append_lock: Mutex<()>,
}

impl AuditLog {
    /// Create the sink directory and open the live file for appending.
    ///
    /// # Errors
    /// Returns [`AuditError::InvalidSink`] for a zero bound, a path with no file
    /// name, or an opened object that is not a regular file or is protocol stdout.
    /// Returns [`AuditError::Io`] when the directory or file cannot be created or
    /// the opened object's identity cannot be inspected.
    pub fn open(config: AuditSinkConfig) -> Result<Self, AuditError> {
        if config.max_bytes == 0 || config.max_files == 0 {
            return Err(AuditError::InvalidSink("audit bounds must be non-zero"));
        }
        if config.path.file_name().is_none() {
            return Err(AuditError::InvalidSink("audit path has no file name"));
        }
        if let Some(parent) = config.path.parent() {
            fs::create_dir_all(parent).map_err(AuditError::Io)?;
        }
        open_audit_append(&config.path)?;
        Ok(Self {
            config,
            next_sequence: AtomicU64::new(1),
            rotations: AtomicUsize::new(0),
            write_failure: AtomicBool::new(false),
            record_failure: AtomicBool::new(false),
            append_lock: Mutex::new(()),
        })
    }

    /// The configured failure policy.
    #[must_use]
    pub const fn failure_policy(&self) -> AuditFailurePolicy {
        self.config.failure_policy
    }

    /// Reserve and return the next monotonic record sequence.
    #[must_use]
    pub fn next_sequence(&self) -> u64 {
        self.next_sequence.fetch_add(1, Ordering::SeqCst)
    }

    /// Whether the sink can currently accept a record.
    ///
    /// The probe is what lets a caller refuse a high-risk mutation *before* it
    /// dispatches, rather than discovering afterwards that the record was lost.
    ///
    /// # Errors
    /// Returns [`AuditError::Io`] when the live file cannot be opened for append.
    pub fn probe_writable(&self) -> Result<(), AuditError> {
        if self.write_failure.load(Ordering::SeqCst) {
            return Err(AuditError::Io(io::Error::other(
                "audit sink is configured unwritable",
            )));
        }
        open_audit_append(&self.config.path).map(|_file| ())
    }

    /// Append one sealed record, rotating first when it would exceed the bound.
    ///
    /// # Errors
    /// Returns [`AuditError::Serialization`] when the record cannot be encoded,
    /// [`AuditError::TooLarge`] when it exceeds its fixed bound, and
    /// [`AuditError::Io`] when the append or rotation fails.
    pub fn record(&self, event: &AuditEvent) -> Result<(), AuditError> {
        let line = event.canonical_line()?;
        if line.len() > super::event::MAX_AUDIT_EVENT_BYTES + 1 {
            return Err(AuditError::TooLarge);
        }
        let _append_guard = self
            .append_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.write_failure.load(Ordering::SeqCst) || self.record_failure.load(Ordering::SeqCst) {
            return Err(AuditError::Io(io::Error::other(
                "audit sink is configured unwritable",
            )));
        }
        let current = fs::metadata(&self.config.path).map_or(0, |metadata| metadata.len());
        if current > 0 && current.saturating_add(line.len() as u64) > self.config.max_bytes {
            self.rotate()?;
        }
        let mut file = open_audit_append(&self.config.path)?;
        file.write_all(&line).map_err(AuditError::Io)?;
        file.sync_all().map_err(AuditError::Io)
    }

    /// The live file's current bytes.
    ///
    /// # Errors
    /// Returns [`AuditError::Io`] when the live file cannot be read.
    pub fn current_bytes(&self) -> Result<Vec<u8>, AuditError> {
        match fs::read(&self.config.path) {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(AuditError::Io(error)),
        }
    }

    /// How many times the sink has rotated.
    #[must_use]
    pub fn rotations(&self) -> usize {
        self.rotations.load(Ordering::SeqCst)
    }

    /// How many files the sink currently retains, live file included.
    ///
    /// # Errors
    /// Returns [`AuditError::Io`] when a retained file cannot be inspected.
    pub fn retained_files(&self) -> Result<usize, AuditError> {
        let mut retained = usize::from(self.config.path.exists());
        for index in 1..=self.config.max_files {
            if rotated_path(&self.config.path, index).exists() {
                retained += 1;
            }
        }
        Ok(retained)
    }

    /// How many bytes the sink currently retains across every retained file.
    ///
    /// # Errors
    /// Returns [`AuditError::Io`] when a retained file cannot be inspected.
    pub fn retained_bytes(&self) -> Result<u64, AuditError> {
        let mut total = file_len(&self.config.path)?;
        for index in 1..=self.config.max_files {
            total = total.saturating_add(file_len(&rotated_path(&self.config.path, index))?);
        }
        Ok(total)
    }

    /// Force the sink to report failure, so a caller's fail-closed enforcement
    /// can be exercised without corrupting a real filesystem.
    pub fn set_write_failure(&self, failing: bool) {
        self.write_failure.store(failing, Ordering::SeqCst);
    }

    /// Force only the record write to fail while the readiness probe still
    /// succeeds, modeling a sink that becomes unavailable after dispatch.
    pub fn set_record_failure(&self, failing: bool) {
        self.record_failure.store(failing, Ordering::SeqCst);
    }

    fn rotate(&self) -> Result<(), AuditError> {
        let oldest = rotated_path(&self.config.path, self.config.max_files);
        if oldest.exists() {
            fs::remove_file(&oldest).map_err(AuditError::Io)?;
        }
        for index in (1..self.config.max_files).rev() {
            let source = rotated_path(&self.config.path, index);
            if source.exists() {
                fs::rename(&source, rotated_path(&self.config.path, index + 1))
                    .map_err(AuditError::Io)?;
            }
        }
        if self.config.path.exists() {
            fs::rename(&self.config.path, rotated_path(&self.config.path, 1))
                .map_err(AuditError::Io)?;
        }
        self.rotations.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn file_len(path: &Path) -> Result<u64, AuditError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(AuditError::Io(error)),
    }
}

fn rotated_path(path: &Path, index: u8) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

fn open_audit_append(path: &Path) -> Result<fs::File, AuditError> {
    open_validated_append(path).map_err(|error| match error {
        OpenedFileSinkError::Invalid(reason) => AuditError::InvalidSink(reason),
        OpenedFileSinkError::Io(error) => AuditError::Io(error),
    })
}

#[cfg(test)]
mod sink_audit {
    use super::*;
    #[cfg(unix)]
    use std::fs::OpenOptions;

    const SINK_SOURCE: &str = include_str!("sink.rs");

    /// Scan the code region only: the module prose deliberately *names* the bans it
    /// keeps, so a prose mention must not be mistaken for an implementation.
    fn production_source() -> &'static str {
        let start = SINK_SOURCE.find("\nuse ").unwrap_or(0);
        let end = SINK_SOURCE
            .find("#[cfg(test)]")
            .unwrap_or(SINK_SOURCE.len());
        &SINK_SOURCE[start..end]
    }

    #[cfg(unix)]
    fn wait_with_output_or_kill(
        mut child: std::process::Child,
        timeout: std::time::Duration,
        context: &str,
    ) -> std::process::Output {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match child.try_wait().expect("poll audit child") {
                Some(_) => {
                    return child
                        .wait_with_output()
                        .expect("collect audit child output");
                }
                None if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                None => break,
            }
        }

        let pid = child.id();
        let wchan = std::fs::read_to_string(format!("/proc/{pid}/wchan"))
            .unwrap_or_else(|error| format!("unavailable: {error}"));
        child.kill().expect("kill timed-out audit child");
        let output = child
            .wait_with_output()
            .expect("collect timed-out audit child output");
        panic!(
            "{context} timed out after {timeout:?}: pid={pid} wchan={} status={} stdout={} stderr={}",
            wchan.trim(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn the_sink_never_truncates_seeks_or_rewrites() {
        let source = production_source();
        for banned in [
            "truncate(true)",
            "seek(",
            "set_len(",
            "fs::write(",
            "write(true)",
        ] {
            assert!(
                !source.contains(banned),
                "an append-only sink must never reach for {banned}"
            );
        }
        assert!(source.contains("open_audit_append"));
    }

    #[test]
    fn rotation_consults_only_its_byte_bounds() {
        let source = production_source();
        for banned in [
            "SystemTime",
            "Instant::now",
            "getrandom",
            "rand::",
            "elapsed()",
        ] {
            assert!(
                !source.contains(banned),
                "rotation must be deterministic and must not consult {banned}"
            );
        }
    }

    #[test]
    fn a_zero_bound_is_refused_rather_than_treated_as_unbounded() {
        let temp = tempfile::tempdir().expect("temp dir");
        for (max_bytes, max_files) in [(0, 4), (4_096, 0)] {
            let error = AuditLog::open(AuditSinkConfig {
                path: temp.path().join("audit.jsonl"),
                max_bytes,
                max_files,
                failure_policy: AuditFailurePolicy::FailClosed,
            })
            .expect_err("a zero bound is refused");
            assert!(matches!(error, AuditError::InvalidSink(_)));
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_regular_opened_objects_are_refused() {
        let error = AuditLog::open(AuditSinkConfig {
            path: PathBuf::from("/dev/null"),
            max_bytes: 4_096,
            max_files: 2,
            failure_policy: AuditFailurePolicy::FailClosed,
        })
        .expect_err("a non-regular opened object is refused");
        match error {
            AuditError::InvalidSink(reason) => {
                assert_eq!(reason, "the object is not a regular file");
            }
            other => panic!("non-regular object used the wrong refusal: {other}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn readerless_fifo_is_refused_without_hanging() {
        const CHILD_PATH: &str = "GODOT_MCP_AUDIT_READERLESS_FIFO_CHILD";
        const TEST_NAME: &str =
            "audit::sink::sink_audit::readerless_fifo_is_refused_without_hanging";

        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let error = AuditLog::open(AuditSinkConfig {
                path: PathBuf::from(path),
                max_bytes: 4_096,
                max_files: 2,
                failure_policy: AuditFailurePolicy::FailClosed,
            })
            .expect_err("a reader-less FIFO is refused");
            match error {
                AuditError::InvalidSink(reason) => {
                    assert_eq!(reason, "the diagnostics sink has no reader (ENXIO)");
                }
                other => panic!("reader-less FIFO used the wrong refusal: {other}"),
            }
            return;
        }

        let temp = tempfile::tempdir().expect("temp dir");
        let fifo_path = temp.path().join("audit.fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .expect("run mkfifo");
        assert!(status.success(), "mkfifo failed with {status}");

        // The parent intentionally supplies no FIFO reader. The child process
        // bounds the historical blocking open without risking the test harness.
        let child =
            std::process::Command::new(std::env::current_exe().expect("current test binary"))
                .arg(TEST_NAME)
                .arg("--exact")
                .arg("--nocapture")
                .env(CHILD_PATH, &fifo_path)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("run reader-less FIFO child");
        let output = wait_with_output_or_kill(
            child,
            std::time::Duration::from_secs(3),
            "reader-less FIFO audit sink",
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("running 1 test") && stdout.contains(TEST_NAME),
            "audit child filter did not match exactly one existing test: stdout={stdout}"
        );
        assert!(
            output.status.success(),
            "reader-less FIFO child failed: status={} stdout={} stderr={}",
            output.status,
            stdout,
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[cfg(unix)]
    #[test]
    fn regular_file_aliasing_stdout_is_refused_by_identity() {
        const CHILD_PATH: &str = "GODOT_MCP_AUDIT_STDOUT_IDENTITY_CHILD";

        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let error = AuditLog::open(AuditSinkConfig {
                path: PathBuf::from(path),
                max_bytes: 4_096,
                max_files: 2,
                failure_policy: AuditFailurePolicy::FailClosed,
            })
            .expect_err("the regular file attached to stdout is refused");
            match error {
                AuditError::InvalidSink(reason) => {
                    assert_eq!(reason, "protocol stdout is not a diagnostics sink");
                }
                other => panic!("stdout identity used the wrong refusal: {other}"),
            }
            return;
        }

        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("stdout.jsonl");
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("open regular stdout target");
        let status =
            std::process::Command::new(std::env::current_exe().expect("current test binary"))
                .arg("regular_file_aliasing_stdout_is_refused_by_identity")
                .arg("--nocapture")
                .env(CHILD_PATH, &path)
                .stdout(std::process::Stdio::from(stdout))
                .status()
                .expect("run identity-check child");
        assert!(
            status.success(),
            "identity-check child failed with {status}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn relative_conout_name_is_an_ordinary_audit_sink() {
        const CHILD: &str = "GODOT_MCP_AUDIT_RELATIVE_CONOUT_CHILD";

        if std::env::var_os(CHILD).is_some() {
            AuditLog::open(AuditSinkConfig {
                path: PathBuf::from("conout$"),
                max_bytes: 4_096,
                max_files: 2,
                failure_policy: AuditFailurePolicy::FailClosed,
            })
            .expect("relative conout name opens as an ordinary file");
            return;
        }

        let temp = tempfile::tempdir().expect("temp dir");
        let status =
            std::process::Command::new(std::env::current_exe().expect("current test binary"))
                .arg("relative_conout_name_is_an_ordinary_audit_sink")
                .arg("--nocapture")
                .env(CHILD, "1")
                .current_dir(temp.path())
                .status()
                .expect("run relative-conout child");
        assert!(
            status.success(),
            "relative-conout child failed with {status}"
        );
        assert!(
            temp.path().join("conout$").is_file(),
            "relative conout sink was not created as a regular file"
        );
    }

    #[test]
    fn sequences_are_monotonic_and_start_at_one() {
        let temp = tempfile::tempdir().expect("temp dir");
        let log = AuditLog::open(AuditSinkConfig {
            path: temp.path().join("audit.jsonl"),
            max_bytes: 4_096,
            max_files: 2,
            failure_policy: AuditFailurePolicy::FailOpen,
        })
        .expect("sink opens");
        assert_eq!(log.next_sequence(), 1);
        assert_eq!(log.next_sequence(), 2);
        assert_eq!(log.rotations(), 0);
        assert_eq!(log.retained_files().expect("retained files"), 1);
        assert_eq!(log.retained_bytes().expect("retained bytes"), 0);
        log.probe_writable().expect("a fresh sink is writable");
        log.set_write_failure(true);
        assert!(log.probe_writable().is_err());
    }
}
