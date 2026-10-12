//! Protocol-safe structured diagnostics and redaction boundaries.
//!
//! This module deliberately has no stdout writer. Stdio stdout belongs solely
//! to MCP framing; diagnostics can target stderr or a bounded rolling file.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe, UnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

const REDACTED: &str = "[REDACTED]";
const MAX_SAFE_FIELD_BYTES: usize = 256;
const DEFAULT_ROLL_BYTES: u64 = 8 * 1024 * 1024;
const DEFAULT_ROLL_FILES: u8 = 5;
const PROTOCOL_STDOUT_SINK: &str = "protocol stdout is not a diagnostics sink";
const NOT_REGULAR_FILE_SINK: &str = "the object is not a regular file";
#[cfg(unix)]
const NO_READER_FILE_SINK: &str = "the diagnostics sink has no reader (ENXIO)";
const MERGED_STDERR_SINK: &str = "stdio serving requires separate stdout and stderr; configure the launcher to keep protocol stdout and process stderr distinct";
static PANIC_HOOK: Once = Once::new();

/// Error returned when a diagnostic field is not safe and bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SafeFieldError {
    /// Empty values are not useful correlation fields.
    #[error("safe diagnostic field must not be empty")]
    Empty,
    /// Fields are bounded before they can reach a log or evidence sink.
    #[error("safe diagnostic field exceeds {MAX_SAFE_FIELD_BYTES} bytes")]
    TooLong,
    /// Controls and separators that could forge records or paths are rejected.
    #[error("safe diagnostic field contains a forbidden character")]
    ForbiddenCharacter,
}

/// A bounded, single-line value approved for diagnostics and evidence metadata.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct SafeField(String);

impl SafeField {
    /// Validate a non-secret field before it reaches an observability sink.
    pub fn new(value: impl Into<String>) -> Result<Self, SafeFieldError> {
        let value = value.into();
        if value.is_empty() {
            return Err(SafeFieldError::Empty);
        }
        if value.len() > MAX_SAFE_FIELD_BYTES {
            return Err(SafeFieldError::TooLong);
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
        {
            return Err(SafeFieldError::ForbiddenCharacter);
        }
        Ok(Self(value))
    }

    /// Return the already-validated safe value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SafeField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("SafeField").field(&self.0).finish()
    }
}

impl fmt::Display for SafeField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

macro_rules! safe_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
        #[serde(transparent)]
        pub struct $name(SafeField);

        impl $name {
            /// Validate and construct the identifier.
            pub fn new(value: impl Into<String>) -> Result<Self, SafeFieldError> {
                SafeField::new(value).map(Self)
            }

            /// Return the validated identifier.
            #[must_use]
            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

safe_id!(
    CorrelationId,
    "A bounded identifier joining work across subsystem events."
);
safe_id!(RequestId, "A bounded identifier for one inbound request.");
safe_id!(
    SessionId,
    "A bounded identifier for one authenticated or local session."
);

macro_rules! redacted_string {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone)]
        pub struct $name(String);

        impl $name {
            /// Wrap sensitive material. Formatting is always redacted.
            #[must_use]
            pub fn new(raw: impl Into<String>) -> Self {
                Self(raw.into())
            }

            /// Explicitly expose material only to the subsystem that consumes it.
            #[must_use]
            pub fn expose(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(REDACTED)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(REDACTED)
            }
        }
    };
}

redacted_string!(
    SecretValue,
    "Secret material with redacted `Debug` and `Display`."
);
redacted_string!(
    TokenValue,
    "Bearer/session token material with redacted formatting."
);
redacted_string!(SourceText, "Project source text with redacted formatting.");

/// Screenshot/media bytes whose formatting can never expose their payload.
#[derive(Clone)]
pub struct ScreenshotBytes(Vec<u8>);

impl ScreenshotBytes {
    /// Wrap screenshot bytes.
    #[must_use]
    pub fn new(raw: Vec<u8>) -> Self {
        Self(raw)
    }

    /// Explicitly expose bytes only to an artifact consumer.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for ScreenshotBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

impl fmt::Display for ScreenshotBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

/// The only supported diagnostic destinations.
#[derive(Debug, Clone)]
pub enum DiagnosticSink {
    /// Structured JSON events to stderr.
    Stderr,
    /// Structured JSON events to a size-bounded rolling file set.
    RollingFile {
        /// Current log file path.
        path: PathBuf,
        /// Rotate before an event would grow the current file past this size.
        max_bytes: u64,
        /// Number of rotated files to retain.
        max_files: u8,
    },
}

/// Configuration for the process-global tracing subscriber.
#[derive(Debug, Clone)]
pub struct TracingConfig {
    /// Fallback filter when `RUST_LOG` is absent or invalid.
    pub level: String,
    /// Stderr or rolling-file destination; stdout is intentionally impossible.
    pub sink: DiagnosticSink,
}

impl Default for TracingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_owned(),
            sink: DiagnosticSink::Stderr,
        }
    }
}

impl TracingConfig {
    /// Translate Todo 2's validated sink configuration without weakening it.
    #[must_use]
    pub fn from_log_sink(sink: &godot_mcp_domain::config::LogSink) -> Self {
        let sink = match sink {
            godot_mcp_domain::config::LogSink::Stderr => DiagnosticSink::Stderr,
            godot_mcp_domain::config::LogSink::File { path } => DiagnosticSink::RollingFile {
                path: PathBuf::from(path),
                max_bytes: DEFAULT_ROLL_BYTES,
                max_files: DEFAULT_ROLL_FILES,
            },
        };
        Self {
            level: "info".to_owned(),
            sink,
        }
    }
}

/// Tracing initialization failure with no secret or source payload fields.
#[derive(Debug, thiserror::Error)]
pub enum TracingInitError {
    /// The rolling path aliases protocol stdout or is otherwise forbidden.
    #[error("invalid diagnostics sink: {0}")]
    InvalidSink(&'static str),
    /// Creating or opening the configured file sink failed.
    #[error("diagnostics file sink is unavailable")]
    FileUnavailable(#[source] io::Error),
    /// Another global subscriber was already installed.
    #[error("a global tracing subscriber is already installed")]
    AlreadyInitialized,
}

/// Install structured tracing and the payload-redacting panic hook.
///
/// There is intentionally no stdout branch in this function. The caller must
/// invoke it before starting worker threads. Standard-library rolling output is
/// synchronous because the workspace has no exact-pinned appender dependency.
pub fn init_tracing(config: &TracingConfig) -> Result<(), TracingInitError> {
    install_redacting_panic_hook();
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&config.level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    match &config.sink {
        DiagnosticSink::Stderr => tracing_subscriber::fmt()
            .json()
            .with_ansi(false)
            .with_env_filter(filter)
            .with_writer(io::stderr)
            .try_init()
            .map_err(|_| TracingInitError::AlreadyInitialized),
        DiagnosticSink::RollingFile {
            path,
            max_bytes,
            max_files,
        } => {
            validate_file_sink(path, *max_bytes, *max_files)?;
            let writer = RollingFileMakeWriter::new(path.clone(), *max_bytes, *max_files)?;
            tracing_subscriber::fmt()
                .json()
                .with_ansi(false)
                .with_env_filter(filter)
                .with_writer(writer)
                .try_init()
                .map_err(|_| TracingInitError::AlreadyInitialized)
        }
    }
}

fn validate_file_sink(path: &Path, max_bytes: u64, max_files: u8) -> Result<(), TracingInitError> {
    if max_bytes == 0 || max_files == 0 {
        return Err(TracingInitError::InvalidSink(
            "rolling bounds must be non-zero",
        ));
    }
    if path.file_name().is_none() {
        return Err(TracingInitError::InvalidSink("file path has no file name"));
    }
    Ok(())
}

#[derive(Clone)]
struct RollingFileMakeWriter {
    state: Arc<Mutex<RollingFileState>>,
}

struct RollingFileState {
    path: PathBuf,
    file: Option<File>,
    max_bytes: u64,
    max_files: u8,
}

impl RollingFileMakeWriter {
    fn new(path: PathBuf, max_bytes: u64, max_files: u8) -> Result<Self, TracingInitError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(TracingInitError::FileUnavailable)?;
        }
        let file = open_validated_append(&path).map_err(tracing_sink_error)?;
        Ok(Self {
            state: Arc::new(Mutex::new(RollingFileState {
                path,
                file: Some(file),
                max_bytes,
                max_files,
            })),
        })
    }
}

impl<'writer> MakeWriter<'writer> for RollingFileMakeWriter {
    type Writer = RollingFileWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        RollingFileWriter {
            state: Arc::clone(&self.state),
        }
    }
}

struct RollingFileWriter {
    state: Arc<Mutex<RollingFileState>>,
}

impl Write for RollingFileWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.file.is_none() {
            state.file =
                Some(open_validated_append(&state.path).map_err(OpenedFileSinkError::into_io)?);
        }
        let current_len = state
            .file
            .as_ref()
            .and_then(|file| file.metadata().ok())
            .map_or(0, |metadata| metadata.len());
        if current_len.saturating_add(bytes.len() as u64) > state.max_bytes {
            state.file.take();
            rotate_files(&state.path, state.max_files)?;
            state.file =
                Some(open_validated_append(&state.path).map_err(OpenedFileSinkError::into_io)?);
        }
        let file = state.file.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "diagnostics file is not open")
        })?;
        file.write_all(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .file
            .as_mut()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "diagnostics file is not open")
            })?
            .flush()
    }
}

fn rotated_path(path: &Path, index: u8) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

fn rotate_files(path: &Path, max_files: u8) -> io::Result<()> {
    let oldest = rotated_path(path, max_files);
    if oldest.exists() {
        fs::remove_file(&oldest)?;
    }
    for index in (1..max_files).rev() {
        let source = rotated_path(path, index);
        if source.exists() {
            fs::rename(source, rotated_path(path, index + 1))?;
        }
    }
    if path.exists() {
        fs::rename(path, rotated_path(path, 1))?;
    }
    Ok(())
}

pub(crate) fn open_validated_append(path: &Path) -> Result<File, OpenedFileSinkError> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        options.custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32);
    }
    let file = options.open(path).map_err(classify_sink_open_error)?;
    validate_opened_file(&file)?;
    #[cfg(unix)]
    clear_nonblocking_after_validation(&file)?;
    Ok(file)
}

fn tracing_sink_error(error: OpenedFileSinkError) -> TracingInitError {
    match error {
        OpenedFileSinkError::Invalid(reason) => TracingInitError::InvalidSink(reason),
        OpenedFileSinkError::Io(error) => TracingInitError::FileUnavailable(error),
    }
}

fn classify_sink_open_error(error: io::Error) -> OpenedFileSinkError {
    #[cfg(unix)]
    if error.raw_os_error() == Some(rustix::io::Errno::NXIO.raw_os_error()) {
        return OpenedFileSinkError::Invalid(NO_READER_FILE_SINK);
    }
    OpenedFileSinkError::Io(error)
}

#[cfg(unix)]
fn clear_nonblocking_after_validation(file: &File) -> Result<(), OpenedFileSinkError> {
    let mut flags = rustix::fs::fcntl_getfl(file).map_err(io::Error::from)?;
    flags.remove(rustix::fs::OFlags::NONBLOCK);
    rustix::fs::fcntl_setfl(file, flags).map_err(io::Error::from)?;
    Ok(())
}

#[derive(Debug)]
pub(crate) enum OpenedFileSinkError {
    Invalid(&'static str),
    Io(io::Error),
}

impl OpenedFileSinkError {
    fn into_io(self) -> io::Error {
        match self {
            Self::Invalid(reason) => io::Error::new(io::ErrorKind::PermissionDenied, reason),
            Self::Io(error) => error,
        }
    }
}

impl From<io::Error> for OpenedFileSinkError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(crate) fn validate_opened_file(file: &File) -> Result<(), OpenedFileSinkError> {
    #[cfg(unix)]
    if !opened_file_is_regular(file)? {
        return Err(OpenedFileSinkError::Invalid(NOT_REGULAR_FILE_SINK));
    }
    if opened_file_aliases_stdout(file)? {
        return Err(OpenedFileSinkError::Invalid(PROTOCOL_STDOUT_SINK));
    }
    Ok(())
}

pub(crate) fn validate_default_stderr() -> Result<(), TracingInitError> {
    #[cfg(unix)]
    let aliases_stdout = opened_file_aliases_stdout(&io::stderr()).map_err(tracing_sink_error)?;
    #[cfg(windows)]
    let aliases_stdout = godot_mcp_domain::sysfs::stderr_aliases_stdout()
        .map_err(|error| TracingInitError::FileUnavailable(io::Error::other(error.to_string())))?;
    #[cfg(not(any(unix, windows)))]
    let aliases_stdout = return Err(TracingInitError::FileUnavailable(io::Error::new(
        io::ErrorKind::Unsupported,
        "standard-stream identity comparison is unavailable on this platform",
    )));

    if aliases_stdout {
        return Err(TracingInitError::InvalidSink(MERGED_STDERR_SINK));
    }
    Ok(())
}

#[cfg(unix)]
fn opened_file_aliases_stdout(file: &impl std::os::fd::AsFd) -> Result<bool, OpenedFileSinkError> {
    let sink = rustix::fs::fstat(file).map_err(io::Error::from)?;
    let stdout = rustix::fs::fstat(io::stdout()).map_err(io::Error::from)?;
    Ok(sink.st_dev == stdout.st_dev && sink.st_ino == stdout.st_ino)
}

#[cfg(unix)]
fn opened_file_is_regular(file: &File) -> Result<bool, OpenedFileSinkError> {
    let sink = rustix::fs::fstat(file).map_err(io::Error::from)?;
    Ok(rustix::fs::FileType::from_raw_mode(sink.st_mode) == rustix::fs::FileType::RegularFile)
}

#[cfg(windows)]
fn opened_file_aliases_stdout(file: &File) -> Result<bool, OpenedFileSinkError> {
    match godot_mcp_domain::sysfs::std_file_aliases_stdout(file) {
        Ok(aliases) => Ok(aliases),
        Err(godot_mcp_domain::sysfs::SysError::NotRegularFile) => {
            Err(OpenedFileSinkError::Invalid(NOT_REGULAR_FILE_SINK))
        }
        Err(godot_mcp_domain::sysfs::SysError::Io(error)) => Err(OpenedFileSinkError::Io(error)),
        Err(error) => Err(OpenedFileSinkError::Io(io::Error::other(error.to_string()))),
    }
}

#[cfg(not(any(unix, windows)))]
fn opened_file_aliases_stdout(_file: &File) -> Result<bool, OpenedFileSinkError> {
    Err(OpenedFileSinkError::Io(io::Error::new(
        io::ErrorKind::Unsupported,
        "opened-file identity comparison is unavailable on this platform",
    )))
}

/// Install a panic hook that never formats the panic payload or source path.
///
/// The hook emits one static JSON object to stderr. `isolate_panic` adds the
/// correlation-aware tracing event at the documented unwind boundary.
pub fn install_redacting_panic_hook() {
    PANIC_HOOK.call_once(|| {
        panic::set_hook(Box::new(|_| {
            let _ = io::stderr().lock().write_all(
                b"{\"level\":\"ERROR\",\"event\":\"panic_hook\",\"panic_payload\":\"[REDACTED]\"}\n",
            );
        }));
    });
}

/// Sanitized failure returned by [`isolate_panic`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("panic isolated at {boundary}; payload [REDACTED]")]
pub struct IsolatedPanic {
    boundary: SafeField,
    correlation_id: CorrelationId,
}

impl IsolatedPanic {
    /// Correlation ID associated with the failed operation.
    #[must_use]
    pub fn correlation_id(&self) -> &CorrelationId {
        &self.correlation_id
    }
}

/// Catch an unwind at an explicitly named boundary and emit only redacted data.
///
/// Callers must opt in with [`AssertUnwindSafe`]; this boundary does not claim
/// that arbitrary captured state is unwind-safe. Release profile panic strategy
/// remains `unwind` so this convention works in shipping builds.
pub fn isolate_panic<F, Output>(
    boundary: &str,
    correlation_id: CorrelationId,
    operation: AssertUnwindSafe<F>,
) -> Result<Output, IsolatedPanic>
where
    F: FnOnce() -> Output + UnwindSafe,
{
    install_redacting_panic_hook();
    let boundary = SafeField::new(boundary).unwrap_or_else(|_| {
        SafeField::new("invalid-boundary").expect("static safe field is valid")
    });
    match panic::catch_unwind(operation) {
        Ok(output) => Ok(output),
        Err(_) => {
            tracing::error!(
                event = "panic_isolated",
                boundary = %boundary,
                correlation_id = %correlation_id,
                panic_payload = REDACTED,
                "panic isolated at unwind boundary"
            );
            Err(IsolatedPanic {
                boundary,
                correlation_id,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn non_regular_opened_sinks_are_rejected() {
        match open_validated_append(Path::new("/dev/null"))
            .expect_err("construction and recovery must reject a device")
        {
            OpenedFileSinkError::Invalid(reason) => {
                assert_eq!(reason, "the object is not a regular file");
            }
            other => panic!("non-regular object used the wrong refusal: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn only_enxio_is_classified_as_a_missing_sink_reader() {
        let enxio = io::Error::from_raw_os_error(rustix::io::Errno::NXIO.raw_os_error());
        match classify_sink_open_error(enxio) {
            OpenedFileSinkError::Invalid(reason) => assert_eq!(reason, NO_READER_FILE_SINK),
            other => panic!("ENXIO used the wrong classification: {other:?}"),
        }

        let denied = io::Error::from_raw_os_error(rustix::io::Errno::ACCESS.raw_os_error());
        match classify_sink_open_error(denied) {
            OpenedFileSinkError::Io(error) => {
                assert_eq!(
                    error.raw_os_error(),
                    Some(rustix::io::Errno::ACCESS.raw_os_error())
                );
            }
            other => panic!("a non-ENXIO error was reclassified: {other:?}"),
        }
    }

    #[test]
    fn a_rotation_failure_does_not_permanently_disconnect_the_sink() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("diagnostics.jsonl");
        let writer = RollingFileMakeWriter::new(path.clone(), 4, 1).expect("sink opens");
        let mut sink = writer.make_writer();

        sink.write_all(b"1234").expect("initial write fills file");
        let rotation_target = rotated_path(&path, 1);
        fs::create_dir(&rotation_target).expect("directory blocks rotation target removal");
        sink.write_all(b"x")
            .expect_err("an undeletable rotation target makes this rotation fail");

        fs::remove_dir(&rotation_target).expect("remove transient rotation obstacle");
        sink.write_all(b"x")
            .expect("a later write recovers without restarting the process");
        assert_eq!(fs::read(path).expect("read live log"), b"x");
    }
}
