//! Atomic, bounded, content-addressed test evidence.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::observability::{SafeField, SafeFieldError};

const MAX_COMMAND_BYTES: usize = 2048;
const MAX_FIXTURES: usize = 32;

/// A replayable workspace-relative command that cannot carry shell expansion,
/// absolute paths, or common sensitive-payload arguments into evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct EvidenceCommand(String);

impl EvidenceCommand {
    /// Validate a command before retaining it as evidence metadata.
    pub fn new(command: &str) -> Result<Self, EvidenceError> {
        if command.is_empty()
            || command.len() > MAX_COMMAND_BYTES
            || command.chars().any(char::is_control)
            || command.contains([';', '|', '&', '>', '<', '`', '$', '\'', '"'])
        {
            return Err(EvidenceError::UnsafeCommand);
        }
        let mut tokens = command.split_ascii_whitespace();
        let Some(executable) = tokens.next() else {
            return Err(EvidenceError::UnsafeCommand);
        };
        if !matches!(executable, "cargo" | "make" | "rustc") {
            return Err(EvidenceError::UnsafeCommand);
        }
        for token in tokens {
            let lower = token.to_ascii_lowercase();
            if Path::new(token).is_absolute()
                || token.split('/').any(|component| component == "..")
                || lower.contains("bearer")
                || lower.contains("token")
                || lower.contains("secret")
                || lower.contains("screenshot")
                || lower.starts_with("source=")
            {
                return Err(EvidenceError::UnsafeCommand);
            }
        }
        Ok(Self(command.to_owned()))
    }
}

/// Test lifecycle phase recorded in evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TestPhase {
    /// Expected failure before implementation.
    Red,
    /// Passing behavior after implementation.
    Green,
}

/// Deterministic evidence metadata. Raw output is represented only by its hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvidenceRecord {
    schema_version: u8,
    test_phase: TestPhase,
    fixture_id: SafeField,
    command: EvidenceCommand,
    exit_code: i32,
    output_sha256: String,
    output_bytes: usize,
    fixture_identifiers: Vec<SafeField>,
}

impl EvidenceRecord {
    /// Build a record while rejecting unbounded or multiline command metadata.
    pub fn new<I, S>(
        test_phase: TestPhase,
        fixture_id: &str,
        command: &str,
        exit_code: i32,
        output: &[u8],
        fixture_identifiers: I,
    ) -> Result<Self, EvidenceError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let fixtures = fixture_identifiers
            .into_iter()
            .map(|value| SafeField::new(value.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        if fixtures.len() > MAX_FIXTURES {
            return Err(EvidenceError::TooManyFixtureIdentifiers);
        }
        Ok(Self {
            schema_version: 1,
            test_phase,
            fixture_id: SafeField::new(fixture_id)?,
            command: EvidenceCommand::new(command)?,
            exit_code,
            output_sha256: hex_sha256(output),
            output_bytes: output.len(),
            fixture_identifiers: fixtures,
        })
    }
}

/// Failure from evidence validation or publication. Payload content is omitted.
#[derive(Debug, thiserror::Error)]
pub enum EvidenceError {
    /// An evidence or fixture identifier was unsafe.
    #[error("unsafe evidence identifier")]
    UnsafeIdentifier(#[from] SafeFieldError),
    /// Command metadata was empty, multiline, controlled, or too large.
    #[error("unsafe evidence command metadata")]
    UnsafeCommand,
    /// Fixture metadata exceeded its fixed cardinality bound.
    #[error("too many evidence fixture identifiers")]
    TooManyFixtureIdentifiers,
    /// Canonical JSON exceeded the configured evidence bound.
    #[error("evidence exceeds configured size bound")]
    TooLarge,
    /// JSON serialization failed without returning source material.
    #[error("evidence serialization failed")]
    Serialization(#[source] serde_json::Error),
    /// Atomic evidence publication failed.
    #[error("evidence publication failed")]
    Io(#[source] io::Error),
}

/// Publisher rooted at one fixed evidence directory.
#[derive(Debug, Clone)]
pub struct EvidenceWriter {
    root: PathBuf,
    max_bytes: usize,
    write_lock: Arc<Mutex<()>>,
}

impl EvidenceWriter {
    /// Create a writer with a non-zero maximum artifact size.
    pub fn new(root: PathBuf, max_bytes: usize) -> Result<Self, EvidenceError> {
        if max_bytes == 0 {
            return Err(EvidenceError::TooLarge);
        }
        fs::create_dir_all(&root).map_err(EvidenceError::Io)?;
        Ok(Self {
            root,
            max_bytes,
            write_lock: Arc::new(Mutex::new(())),
        })
    }

    /// Atomically publish canonical compact JSON under a sanitized file name.
    pub fn write(
        &self,
        artifact_id: &str,
        record: &EvidenceRecord,
    ) -> Result<PathBuf, EvidenceError> {
        let _write_guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let artifact_id = SafeField::new(artifact_id)?;
        let bytes = serde_json::to_vec(record).map_err(EvidenceError::Serialization)?;
        if bytes.len() > self.max_bytes {
            return Err(EvidenceError::TooLarge);
        }
        let final_path = self.root.join(format!("{}.json", artifact_id.as_str()));
        let temp_path = self.root.join(format!(
            ".{}.{}.tmp",
            artifact_id.as_str(),
            std::process::id()
        ));
        write_private_new_or_truncate(&temp_path, &bytes).map_err(EvidenceError::Io)?;
        fs::rename(&temp_path, &final_path).map_err(EvidenceError::Io)?;
        sync_directory(&self.root).map_err(EvidenceError::Io)?;
        Ok(final_path)
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn write_private_new_or_truncate(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}
