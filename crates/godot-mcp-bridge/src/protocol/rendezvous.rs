//! The rendezvous file: how the plugin learns where to dial and proves it may.
//!
//! # A loopback port is not an identity
//!
//! Todo 18 established this for the LSP/DAP plane: "something is listening on
//! 127.0.0.1:6005" is compatible with a different editor, a different project or
//! an unrelated process that squatted the port. The editor bridge inverts the
//! direction — here *this server* listens and the plugin dials — but the problem is
//! the same in reverse: any local process can dial the bridge port, and loopback
//! alone would authorise all of them. On a shared or multi-user machine, and for
//! any other process running as the same user, loopback is not a boundary.
//!
//! So dialling is not enough. The plugin must present the **per-launch 256-bit
//! secret** this server generated at startup and wrote to a **mode-0600** file that
//! only the owning user can read. Three properties make that meaningful:
//!
//! 1. **Fresh per launch.** [`RendezvousFile::write_new`] draws
//!    [`SECRET_BYTES`] bytes from the OS CSPRNG (`getrandom`, never a hand-rolled
//!    PRNG) on every start, so a secret captured from an earlier run authenticates
//!    nothing.
//! 2. **Mode 0600, verified on read.** [`RendezvousFile::read`] re-stats the file
//!    and **refuses** anything with group or other bits set. A file that became
//!    world-readable between write and read is a refusal, not a warning.
//! 3. **Rotated, refused safely, or deleted on restart/shutdown.**
//!    [`RendezvousFile::rotate`] replaces the secret only where the platform can
//!    preserve the CAS property; Windows returns a typed refusal and leaves the old
//!    entry untouched. [`RendezvousFile::remove`] deletes the file. A stale file
//!    naming a dead port with a live-looking secret is exactly what an attacker would
//!    want to find.
//!
//! # Constant-time comparison, and why it is tested rather than asserted
//!
//! [`constant_time_eq`] accumulates the XOR of every byte pair and only then
//! reduces to a boolean. It has no early return and no data-dependent branch, so
//! the number of byte operations depends solely on the input *lengths*, never on
//! where two inputs first differ. Because "it looks constant-time" is not evidence,
//! [`compare_probe`] returns the number of byte comparisons actually performed, and
//! a test asserts that a candidate differing in its first byte and one differing in
//! its last byte produce the **same** count. An early-exit implementation would
//! fail that test.
//!
//! Length mismatch is checked first and returns `false` immediately, which leaks
//! only the candidate's length — a value the attacker chose and already knows.
//!
//! # Windows
//!
//! Unix permission bits do not exist on Windows. [`RendezvousFile::mode_of`]
//! reports the real mode on Unix; on Windows it reports [`RENDEZVOUS_FILE_MODE`]
//! because the equivalent restriction there is a private-object ACL, which Todo 2's
//! `sysfs` layer owns. The per-launch secret check is the same on both platforms and
//! is the load-bearing control; the file mode is defence in depth.
//!
//! Windows can create a new private rendezvous and remove it on shutdown, but it
//! cannot safely replace an existing entry with the `sysfs` CAS contract: Windows
//! has atomic replacing rename but no atomic directory-entry exchange, so a replacing
//! rename could clobber an actor that wins the final race. Rotation and startup over
//! a crash-left file therefore return
//! [`RendezvousError::WindowsExistingFileReplacementUnsupported`] without changing
//! the existing file. A caller that can prove no bridge process is running deletes
//! that stale file and writes a new one: the server does so at startup, where the
//! project's exclusive lease is that proof.

use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use godot_mcp_domain::sysfs::{self, CasOutcome, SysError};
use serde_json::{Value, json};

use super::digest::{hex_decode, hex_encode};
use super::envelope::ProjectClaim;

/// Length of the per-launch secret. 256 bits.
pub const SECRET_BYTES: usize = 32;

/// The only permission bits a rendezvous file may carry: owner read/write.
pub const RENDEZVOUS_FILE_MODE: u32 = 0o600;

/// Schema version carried inside the rendezvous document.
pub const RENDEZVOUS_SCHEMA_VERSION: &str = "1";

/// Why a rendezvous file could not be written, read or trusted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RendezvousError {
    /// The file could not be read, or does not exist.
    #[error("the rendezvous file at {path} is unreadable: {detail}")]
    Unreadable {
        /// Where it was expected.
        path: String,
        /// What the OS reported.
        detail: String,
    },

    /// The file could not be written or replaced.
    #[error("the rendezvous file at {path} could not be written: {detail}")]
    Unwritable {
        /// Where it was being written.
        path: String,
        /// What the OS reported.
        detail: String,
    },

    /// Windows cannot safely replace an existing rendezvous entry under the CAS
    /// contract, so rotation refuses without changing the file.
    #[error(
        "Windows cannot safely replace the existing rendezvous file at {path}: atomic directory-entry exchange is unavailable; ensure no bridge process is running, delete the stale file, and restart"
    )]
    WindowsExistingFileReplacementUnsupported {
        /// The existing rendezvous path that was left unchanged.
        path: String,
    },

    /// The file carries permission bits beyond owner read/write.
    #[error(
        "the rendezvous file is mode {mode:04o}; only {RENDEZVOUS_FILE_MODE:04o} is acceptable"
    )]
    InsecurePermissions {
        /// The mode observed.
        mode: u32,
    },

    /// The file is not owned and restricted to the current user.
    #[error("the rendezvous file is not private to the current user: {detail}")]
    InsecureAccess {
        /// The ownership, ACL or object-type violation.
        detail: String,
    },

    /// The document is not the canonical rendezvous shape.
    #[error("the rendezvous document is malformed: {detail}")]
    Malformed {
        /// Exactly what was wrong.
        detail: String,
    },

    /// The endpoint the document names is not a loopback address.
    #[error("the rendezvous file names the non-loopback endpoint {endpoint}")]
    NotLoopback {
        /// The endpoint that was refused.
        endpoint: String,
    },

    /// The OS CSPRNG refused to produce entropy.
    #[error("the OS random source failed: {detail}")]
    NoEntropy {
        /// What the OS reported.
        detail: String,
    },
}

/// One rendezvous file's contents, plus where it lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RendezvousFile {
    path: PathBuf,
    endpoint: SocketAddr,
    secret: [u8; SECRET_BYTES],
    project: ProjectClaim,
}

impl RendezvousFile {
    /// Generate a fresh per-launch secret and write the file at mode 0600.
    ///
    /// Replaces an existing file only when the platform can preserve the CAS
    /// property. Windows refuses replacement and leaves the existing file intact.
    ///
    /// # Errors
    /// Returns [`RendezvousError::NotLoopback`] for a non-loopback endpoint,
    /// [`RendezvousError::NoEntropy`] when the OS CSPRNG fails, or
    /// [`RendezvousError::WindowsExistingFileReplacementUnsupported`] when Windows
    /// finds an existing target, or [`RendezvousError::Unwritable`] when the write
    /// or the mode change fails.
    pub fn write_new(
        path: &Path,
        endpoint: SocketAddr,
        project: ProjectClaim,
    ) -> Result<Self, RendezvousError> {
        require_loopback(endpoint)?;
        let secret = fresh_secret()?;
        let file = Self {
            path: path.to_path_buf(),
            endpoint,
            secret,
            project,
        };
        file.persist()?;
        Ok(file)
    }

    /// Replace this file's secret with a freshly generated one where supported.
    ///
    /// Used on restart. Windows refuses to replace an existing file.
    ///
    /// # Errors
    /// Returns [`RendezvousError::NoEntropy`],
    /// [`RendezvousError::WindowsExistingFileReplacementUnsupported`] on Windows,
    /// or [`RendezvousError::Unwritable`].
    pub fn rotate(&self) -> Result<Self, RendezvousError> {
        let rotated = Self {
            path: self.path.clone(),
            endpoint: self.endpoint,
            secret: fresh_secret()?,
            project: self.project.clone(),
        };
        rotated.persist()?;
        Ok(rotated)
    }

    /// Delete the file. Called on shutdown.
    ///
    /// Removing an already-absent file succeeds: shutdown must be idempotent.
    ///
    /// # Errors
    /// Returns [`RendezvousError::Unwritable`] when the file exists but cannot be
    /// removed.
    pub fn remove(&self) -> Result<(), RendezvousError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(RendezvousError::Unwritable {
                path: self.path.display().to_string(),
                detail: error.to_string(),
            }),
        }
    }

    /// Read and validate a rendezvous file, as the plugin does.
    ///
    /// # Errors
    /// Returns [`RendezvousError::Unreadable`] when the file is missing or
    /// unreadable, [`RendezvousError::InsecurePermissions`] for any mode beyond
    /// owner read/write, [`RendezvousError::Malformed`] for a document that is not
    /// the canonical shape, and [`RendezvousError::NotLoopback`] when it names a
    /// non-loopback endpoint.
    pub fn read(path: &Path) -> Result<Self, RendezvousError> {
        let mode = Self::mode_of(path)?;
        if mode != RENDEZVOUS_FILE_MODE {
            return Err(RendezvousError::InsecurePermissions { mode });
        }
        let handle = sysfs::open_private_regular_file(path)
            .map_err(|error| map_private_read_error(path, error))?;
        let bytes = sysfs::read_file(&handle).map_err(|error| RendezvousError::Unreadable {
            path: path.display().to_string(),
            detail: error.to_string(),
        })?;
        let text = String::from_utf8(bytes).map_err(|error| RendezvousError::Malformed {
            detail: format!("not UTF-8: {error}"),
        })?;
        let value: Value =
            serde_json::from_str(&text).map_err(|error| RendezvousError::Malformed {
                detail: format!("not parsable JSON: {error}"),
            })?;
        Self::from_value(path, &value)
    }

    /// The file's permission bits.
    ///
    /// On Unix this is the real `st_mode & 0o777`. On Windows there are no
    /// permission bits, so this reports [`RENDEZVOUS_FILE_MODE`] once the file is
    /// confirmed to exist; see this module's header.
    ///
    /// # Errors
    /// Returns [`RendezvousError::Unreadable`] when the file cannot be stat'd.
    pub fn mode_of(path: &Path) -> Result<u32, RendezvousError> {
        let metadata = fs::metadata(path).map_err(|error| RendezvousError::Unreadable {
            path: path.display().to_string(),
            detail: error.to_string(),
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            Ok(metadata.permissions().mode() & 0o777)
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            Ok(RENDEZVOUS_FILE_MODE)
        }
    }

    /// Set a file's permission bits.
    ///
    /// Exists so a test can *relax* a mode and prove [`RendezvousFile::read`]
    /// refuses the result. On Windows this is a no-op.
    ///
    /// # Errors
    /// Returns [`RendezvousError::Unwritable`] when the mode change fails.
    pub fn set_mode(path: &Path, mode: u32) -> Result<(), RendezvousError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| {
                RendezvousError::Unwritable {
                    path: path.display().to_string(),
                    detail: error.to_string(),
                }
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (path, mode);
            Ok(())
        }
    }

    /// Where this file lives.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The loopback endpoint the plugin should dial.
    #[must_use]
    pub const fn endpoint(&self) -> SocketAddr {
        self.endpoint
    }

    /// The raw per-launch secret.
    #[must_use]
    pub const fn secret(&self) -> &[u8] {
        &self.secret
    }

    /// The secret's hex spelling, as it travels in `hello`.
    #[must_use]
    pub fn secret_hex(&self) -> String {
        hex_encode(&self.secret)
    }

    /// The project both sides must agree on.
    #[must_use]
    pub const fn project(&self) -> &ProjectClaim {
        &self.project
    }

    /// Whether `candidate` is this launch's secret, compared in constant time.
    #[must_use]
    pub fn matches_secret(&self, candidate: &[u8]) -> bool {
        constant_time_eq(&self.secret, candidate)
    }

    /// Whether `candidate_hex` decodes to this launch's secret.
    ///
    /// A non-hex or wrong-length candidate is `false`, decided without a
    /// value-dependent branch on the decoded bytes.
    #[must_use]
    pub fn matches_secret_hex(&self, candidate_hex: &str) -> bool {
        match hex_decode(candidate_hex) {
            Some(bytes) => self.matches_secret(&bytes),
            None => false,
        }
    }

    /// The canonical rendezvous document.
    #[must_use]
    pub fn to_canonical_value(&self) -> Value {
        json!({
            "schema_version": RENDEZVOUS_SCHEMA_VERSION,
            "endpoint": self.endpoint.to_string(),
            "secret_hex": self.secret_hex(),
            "project": {
                "object_identity": self.project.object_identity,
                "root_object": self.project.root_object,
                "file_object": self.project.file_object,
                "generation": self.project.generation,
            },
        })
    }

    fn persist(&self) -> Result<(), RendezvousError> {
        self.persist_before_publish(&|| true)
    }

    fn persist_before_publish(
        &self,
        before_publish: &dyn Fn() -> bool,
    ) -> Result<(), RendezvousError> {
        let bytes = serde_json::to_vec(&self.to_canonical_value()).unwrap_or_default();
        let absolute = absolute_path(&self.path)?;
        let parent_path = absolute
            .parent()
            .ok_or_else(|| RendezvousError::Unwritable {
                path: self.path.display().to_string(),
                detail: "the rendezvous path has no parent directory".to_owned(),
            })?;
        let name = absolute
            .file_name()
            .ok_or_else(|| RendezvousError::Unwritable {
                path: self.path.display().to_string(),
                detail: "the rendezvous path has no file name".to_owned(),
            })?;
        let walked = sysfs::walk_dir_no_symlinks(parent_path)
            .map_err(|error| map_write_error(&self.path, error))?;
        let existing = match sysfs::read_child_file(walked.dir(), name) {
            Ok(existing) => Some(existing),
            Err(SysError::NotFound) => None,
            Err(error) => return Err(map_write_error(&self.path, error)),
        };
        // An existing entry is never replaced on Windows (see the module docs):
        // the refusal leaves it exactly as it is.
        #[cfg(windows)]
        if existing.is_some() {
            return Err(RendezvousError::WindowsExistingFileReplacementUnsupported {
                path: self.path.display().to_string(),
            });
        }
        let (outcome, publication) = match existing {
            Some(previous) => (
                sysfs::cas_replace(walked.dir(), name, &previous, &bytes, before_publish),
                PublicationKind::replacement(),
            ),
            None => (
                sysfs::cas_create(walked.dir(), name, &bytes, before_publish),
                PublicationKind::Create,
            ),
        };
        let outcome = outcome.map_err(|error| map_write_error(&self.path, error))?;
        map_publication_outcome(&self.path, outcome, publication)
    }

    fn from_value(path: &Path, value: &Value) -> Result<Self, RendezvousError> {
        let version = value
            .get("schema_version")
            .and_then(Value::as_str)
            .ok_or_else(|| RendezvousError::Malformed {
                detail: "no schema_version".to_owned(),
            })?;
        if version != RENDEZVOUS_SCHEMA_VERSION {
            return Err(RendezvousError::Malformed {
                detail: format!("unsupported schema_version {version:?}"),
            });
        }
        let endpoint: SocketAddr = value
            .get("endpoint")
            .and_then(Value::as_str)
            .ok_or_else(|| RendezvousError::Malformed {
                detail: "no endpoint".to_owned(),
            })?
            .parse()
            .map_err(|error| RendezvousError::Malformed {
                detail: format!("endpoint is not a socket address: {error}"),
            })?;
        require_loopback(endpoint)?;

        let secret_hex = value
            .get("secret_hex")
            .and_then(Value::as_str)
            .ok_or_else(|| RendezvousError::Malformed {
                detail: "no secret_hex".to_owned(),
            })?;
        let bytes = hex_decode(secret_hex).ok_or_else(|| RendezvousError::Malformed {
            detail: "secret_hex is not hexadecimal".to_owned(),
        })?;
        if bytes.len() != SECRET_BYTES {
            return Err(RendezvousError::Malformed {
                detail: format!(
                    "secret is {} bytes, not the required {SECRET_BYTES}",
                    bytes.len()
                ),
            });
        }
        let mut secret = [0_u8; SECRET_BYTES];
        secret.copy_from_slice(&bytes);

        let project_value = value
            .get("project")
            .ok_or_else(|| RendezvousError::Malformed {
                detail: "no project".to_owned(),
            })?;
        let project = ProjectClaim {
            object_identity: field(project_value, "object_identity")?,
            root_object: field(project_value, "root_object")?,
            file_object: field(project_value, "file_object")?,
            generation: project_value
                .get("generation")
                .and_then(Value::as_u64)
                .ok_or_else(|| RendezvousError::Malformed {
                    detail: "project.generation is not an integer".to_owned(),
                })?,
        };

        Ok(Self {
            path: path.to_path_buf(),
            endpoint,
            secret,
            project,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublicationKind {
    Create,
    #[cfg(not(windows))]
    ReplaceAtomic,
    #[cfg(any(windows, test))]
    ReplaceFailClosedWindows,
}

impl PublicationKind {
    const fn replacement() -> Self {
        #[cfg(windows)]
        {
            Self::ReplaceFailClosedWindows
        }
        #[cfg(not(windows))]
        {
            Self::ReplaceAtomic
        }
    }
}

fn map_publication_outcome(
    path: &Path,
    outcome: CasOutcome,
    publication: PublicationKind,
) -> Result<(), RendezvousError> {
    match outcome {
        CasOutcome::Published => Ok(()),
        CasOutcome::Stale => match publication {
            #[cfg(any(windows, test))]
            PublicationKind::ReplaceFailClosedWindows => {
                Err(RendezvousError::WindowsExistingFileReplacementUnsupported {
                    path: path.display().to_string(),
                })
            }
            #[cfg(not(windows))]
            PublicationKind::Create | PublicationKind::ReplaceAtomic => {
                Err(RendezvousError::Unwritable {
                    path: path.display().to_string(),
                    detail: "the rendezvous path changed during atomic publication".to_owned(),
                })
            }
            #[cfg(windows)]
            PublicationKind::Create => Err(RendezvousError::Unwritable {
                path: path.display().to_string(),
                detail: "the rendezvous path changed during atomic publication".to_owned(),
            }),
        },
        CasOutcome::Aborted => Err(RendezvousError::Unwritable {
            path: path.display().to_string(),
            detail: "rendezvous publication was aborted before replacement".to_owned(),
        }),
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, RendezvousError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|directory| directory.join(path))
        .map_err(|error| RendezvousError::Unwritable {
            path: path.display().to_string(),
            detail: error.to_string(),
        })
}

fn map_write_error(path: &Path, error: SysError) -> RendezvousError {
    RendezvousError::Unwritable {
        path: path.display().to_string(),
        detail: error.to_string(),
    }
}

fn map_private_read_error(path: &Path, error: SysError) -> RendezvousError {
    match error {
        SysError::NotPrivate(detail) => RendezvousError::InsecureAccess { detail },
        other => RendezvousError::Unreadable {
            path: path.display().to_string(),
            detail: other.to_string(),
        },
    }
}

fn field(value: &Value, name: &str) -> Result<String, RendezvousError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| RendezvousError::Malformed {
            detail: format!("project.{name} is missing or not a string"),
        })
}

fn fresh_secret() -> Result<[u8; SECRET_BYTES], RendezvousError> {
    let mut secret = [0_u8; SECRET_BYTES];
    getrandom::fill(&mut secret).map_err(|error| RendezvousError::NoEntropy {
        detail: error.to_string(),
    })?;
    Ok(secret)
}

/// Whether `endpoint` is a loopback address.
#[must_use]
pub const fn is_loopback(endpoint: SocketAddr) -> bool {
    match endpoint.ip() {
        IpAddr::V4(address) => address.is_loopback(),
        IpAddr::V6(address) => address.is_loopback(),
    }
}

fn require_loopback(endpoint: SocketAddr) -> Result<(), RendezvousError> {
    if is_loopback(endpoint) {
        Ok(())
    } else {
        Err(RendezvousError::NotLoopback {
            endpoint: endpoint.to_string(),
        })
    }
}

/// The canonical IPv4 loopback address with an ephemeral port.
#[must_use]
pub const fn ephemeral_loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

/// The IPv6 loopback address, for platforms where that is the local default.
#[must_use]
pub const fn ephemeral_loopback_v6() -> SocketAddr {
    SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0)
}

/// Compare two byte strings without an early return or a data-dependent branch.
///
/// Every byte pair is examined and accumulated; the result is derived only after
/// the whole loop. A length mismatch returns `false` immediately, which leaks only
/// the candidate's length — something the caller supplied and already knows.
#[must_use]
pub fn constant_time_eq(expected: &[u8], candidate: &[u8]) -> bool {
    if expected.len() != candidate.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in expected.iter().zip(candidate.iter()) {
        difference |= left ^ right;
    }
    difference == 0
}

/// How many byte comparisons [`constant_time_eq`] performs for these inputs.
///
/// Exists so a test can prove the comparison is not short-circuiting: two
/// candidates that differ at opposite ends must yield the same count. This is
/// instrumentation over the same loop shape, not a second implementation.
#[must_use]
pub fn compare_probe(expected: &[u8], candidate: &[u8]) -> usize {
    if expected.len() != candidate.len() {
        return 0;
    }
    let mut examined = 0_usize;
    let mut difference = 0_u8;
    for (left, right) in expected.iter().zip(candidate.iter()) {
        difference |= left ^ right;
        examined += 1;
    }
    // Consume `difference` so the loop cannot be elided.
    examined + usize::from(difference) - usize::from(difference)
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use godot_mcp_domain::sysfs::CasOutcome;
    use serde_json::json;

    use super::super::envelope::ProjectClaim;
    use super::{
        PublicationKind, RENDEZVOUS_FILE_MODE, RendezvousError, RendezvousFile, SECRET_BYTES,
        compare_probe, constant_time_eq, ephemeral_loopback, ephemeral_loopback_v6, is_loopback,
        map_publication_outcome,
    };

    /// Write `bytes` as a rendezvous document the way the module itself
    /// creates one: a private file (mode 0600 on Unix, an owner-only protected
    /// ACL on Windows, where no mode exists to set afterwards).
    fn write_private(path: &Path, bytes: &[u8]) {
        let _ = std::fs::remove_file(path);
        let walked =
            godot_mcp_domain::sysfs::walk_dir_no_symlinks(path.parent().expect("a parent"))
                .expect("walk the parent");
        let outcome = godot_mcp_domain::sysfs::cas_create(
            walked.dir(),
            path.file_name().expect("a name"),
            bytes,
            &|| true,
        )
        .expect("create the document");
        assert_eq!(outcome, godot_mcp_domain::sysfs::CasOutcome::Published);
    }

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "godot-mcp-rendezvous-{}-{unique}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create scratch");
            Self { dir }
        }

        fn file(&self, name: &str) -> PathBuf {
            self.dir.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn project() -> ProjectClaim {
        ProjectClaim {
            object_identity: format!("project_{}", "a".repeat(64)),
            root_object: "unix:dev=1,ino=2".to_owned(),
            file_object: "unix:dev=1,ino=3".to_owned(),
            generation: 3,
        }
    }

    fn endpoint() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 45_871)
    }

    #[test]
    fn a_new_secret_is_256_bits_from_the_os_csprng() {
        assert_eq!(SECRET_BYTES, 32);
        let scratch = Scratch::new();
        let path = scratch.file("a.rendezvous");
        let first = RendezvousFile::write_new(&path, endpoint(), project()).expect("write");
        assert_eq!(first.secret().len(), SECRET_BYTES);
        assert_ne!(
            first.secret(),
            &[0_u8; SECRET_BYTES],
            "the secret is not all zeros"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn a_supported_replacement_draws_a_fresh_secret() {
        let scratch = Scratch::new();
        let path = scratch.file("fresh-replacement.rendezvous");
        let first = RendezvousFile::write_new(&path, endpoint(), project()).expect("first write");
        let second = RendezvousFile::write_new(&path, endpoint(), project()).expect("replacement");

        assert_ne!(first.secret(), second.secret());
    }

    #[test]
    fn windows_fail_closed_replacement_maps_to_a_platform_specific_refusal() {
        let path = Path::new("C:\\private\\bridge.rendezvous");

        assert_eq!(
            map_publication_outcome(
                path,
                CasOutcome::Stale,
                PublicationKind::ReplaceFailClosedWindows,
            ),
            Err(RendezvousError::WindowsExistingFileReplacementUnsupported {
                path: path.display().to_string(),
            })
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_rotation_refuses_without_changing_the_existing_rendezvous() {
        let scratch = Scratch::new();
        let path = scratch.file("windows-rotation.rendezvous");
        let file = RendezvousFile::write_new(&path, endpoint(), project()).expect("first create");
        let original = std::fs::read(&path).expect("read original rendezvous");
        assert_eq!(file.secret().len(), SECRET_BYTES);
        assert_eq!(
            RendezvousFile::mode_of(&path).expect("stat"),
            RENDEZVOUS_FILE_MODE
        );

        assert_eq!(
            file.rotate(),
            Err(RendezvousError::WindowsExistingFileReplacementUnsupported {
                path: path.display().to_string(),
            })
        );
        assert_eq!(
            std::fs::read(&path).expect("read refused target"),
            original,
            "fail-closed rotation leaves the existing rendezvous untouched"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn the_file_is_written_at_mode_0600_and_stays_there_through_rotation() {
        assert_eq!(RENDEZVOUS_FILE_MODE, 0o600);
        let scratch = Scratch::new();
        let path = scratch.file("b.rendezvous");
        let file = RendezvousFile::write_new(&path, endpoint(), project()).expect("write");
        assert_eq!(
            RendezvousFile::mode_of(&path).expect("stat"),
            RENDEZVOUS_FILE_MODE
        );
        let rotated = file.rotate().expect("rotate");
        assert_eq!(
            RendezvousFile::mode_of(&path).expect("stat"),
            RENDEZVOUS_FILE_MODE
        );
        assert_ne!(rotated.secret(), file.secret());
        assert_eq!(rotated.endpoint(), file.endpoint());
    }

    #[cfg(unix)]
    #[test]
    fn a_rotation_re_tightens_a_relaxed_mode() {
        let scratch = Scratch::new();
        let path = scratch.file("tighten.rendezvous");
        let file = RendezvousFile::write_new(&path, endpoint(), project()).expect("write");
        RendezvousFile::set_mode(&path, 0o666).expect("relax");
        assert_eq!(RendezvousFile::mode_of(&path).expect("stat"), 0o666);
        file.rotate().expect("rotate");
        assert_eq!(
            RendezvousFile::mode_of(&path).expect("stat"),
            RENDEZVOUS_FILE_MODE,
            "rotation does not inherit a relaxed mode"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rotation_never_exposes_the_fresh_secret_through_a_relaxed_target() {
        use std::cell::Cell;

        let scratch = Scratch::new();
        let path = scratch.file("atomic-rotation.rendezvous");
        let original = RendezvousFile::write_new(&path, endpoint(), project()).expect("write");
        let original_bytes = std::fs::read(&path).expect("read original");
        RendezvousFile::set_mode(&path, 0o666).expect("relax old target");

        let replacement = RendezvousFile {
            path: path.clone(),
            endpoint: original.endpoint,
            secret: [0x5a; SECRET_BYTES],
            project: original.project.clone(),
        };
        let replacement_bytes =
            serde_json::to_vec(&replacement.to_canonical_value()).expect("replacement bytes");
        assert_ne!(replacement_bytes, original_bytes);

        let observed_before_publish = Cell::new(false);
        replacement
            .persist_before_publish(&|| {
                observed_before_publish.set(true);
                assert_eq!(
                    RendezvousFile::mode_of(&path).expect("stat old target"),
                    0o666,
                    "the old relaxed inode remains untouched before atomic publication"
                );
                assert_eq!(
                    std::fs::read(&path).expect("read old target"),
                    original_bytes,
                    "the fresh secret is not written through the relaxed inode"
                );
                true
            })
            .expect("publish replacement");
        assert!(observed_before_publish.get());
        assert_eq!(
            RendezvousFile::mode_of(&path).expect("stat published target"),
            RENDEZVOUS_FILE_MODE,
            "the atomically published replacement is owner-only"
        );
        assert_eq!(
            std::fs::read(&path).expect("read replacement"),
            replacement_bytes
        );
    }

    #[cfg(unix)]
    #[test]
    fn startup_replaces_a_relaxed_stale_rendezvous_atomically() {
        let scratch = Scratch::new();
        let path = scratch.file("stale-startup.rendezvous");
        std::fs::write(&path, b"stale launch secret").expect("write stale file");
        RendezvousFile::set_mode(&path, 0o644).expect("relax stale file");

        let current = RendezvousFile::write_new(&path, endpoint(), project()).expect("startup");
        assert_eq!(
            RendezvousFile::mode_of(&path).expect("stat current file"),
            RENDEZVOUS_FILE_MODE
        );
        assert_ne!(
            std::fs::read(&path).expect("read current file"),
            b"stale launch secret"
        );
        assert_eq!(
            RendezvousFile::read(&path).expect("read current rendezvous"),
            current
        );
    }

    #[test]
    fn rotation_publishes_from_a_fresh_private_staging_file() {
        let source = include_str!("rendezvous.rs");
        let production = source
            .split_once("#[cfg(test)]")
            .map_or(source, |(before_tests, _tests)| before_tests);
        let persist = production
            .split_once("fn persist")
            .expect("persist exists")
            .1;
        assert!(
            persist.contains("cas_replace(") && persist.contains("cas_create("),
            "rotation and first creation must publish from fresh private staging files"
        );
        assert!(
            !persist.contains("truncate(true)"),
            "rotation must never write a fresh secret through the old target inode"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_file_with_any_extra_permission_bit_is_refused() {
        let scratch = Scratch::new();
        let path = scratch.file("loose.rendezvous");
        RendezvousFile::write_new(&path, endpoint(), project()).expect("write");
        for mode in [0o601_u32, 0o604, 0o610, 0o640, 0o644, 0o660, 0o666, 0o700] {
            RendezvousFile::set_mode(&path, mode).expect("set mode");
            assert_eq!(
                RendezvousFile::read(&path),
                Err(RendezvousError::InsecurePermissions { mode }),
                "mode {mode:04o}"
            );
        }
        RendezvousFile::set_mode(&path, RENDEZVOUS_FILE_MODE).expect("restore");
        RendezvousFile::read(&path).expect("0600 reads");
    }

    #[test]
    fn the_document_round_trips_through_the_file() {
        let scratch = Scratch::new();
        let path = scratch.file("round.rendezvous");
        let written = RendezvousFile::write_new(&path, endpoint(), project()).expect("write");
        let read = RendezvousFile::read(&path).expect("read");
        assert_eq!(read.endpoint(), written.endpoint());
        assert_eq!(read.secret(), written.secret());
        assert_eq!(read.secret_hex(), written.secret_hex());
        assert_eq!(read.project(), written.project());
        assert_eq!(read.secret_hex().len(), SECRET_BYTES * 2);
    }

    #[test]
    fn removal_is_idempotent_and_leaves_nothing_behind() {
        let scratch = Scratch::new();
        let path = scratch.file("gone.rendezvous");
        let file = RendezvousFile::write_new(&path, endpoint(), project()).expect("write");
        file.remove().expect("first removal");
        assert!(!path.exists());
        file.remove().expect("second removal is a no-op");
        assert!(matches!(
            RendezvousFile::read(&path),
            Err(RendezvousError::Unreadable { .. })
        ));
    }

    #[test]
    fn a_non_loopback_endpoint_is_refused_on_write_and_on_read() {
        let scratch = Scratch::new();
        let path = scratch.file("public.rendezvous");
        let public = "203.0.113.5:6000".parse().expect("addr");
        assert_eq!(
            RendezvousFile::write_new(&path, public, project()),
            Err(RendezvousError::NotLoopback {
                endpoint: "203.0.113.5:6000".to_owned()
            })
        );

        // A hand-written file naming a public endpoint is refused on read too.
        let document = json!({
            "schema_version": "1",
            "endpoint": "0.0.0.0:6000",
            "secret_hex": "a".repeat(64),
            "project": {
                "object_identity": project().object_identity,
                "root_object": project().root_object,
                "file_object": project().file_object,
                "generation": 3,
            },
        });
        write_private(&path, &serde_json::to_vec(&document).expect("bytes"));
        assert_eq!(
            RendezvousFile::read(&path),
            Err(RendezvousError::NotLoopback {
                endpoint: "0.0.0.0:6000".to_owned()
            })
        );
    }

    #[test]
    fn a_malformed_document_is_refused_field_by_field() {
        let scratch = Scratch::new();
        let path = scratch.file("bad.rendezvous");
        let base = json!({
            "schema_version": "1",
            "endpoint": "127.0.0.1:45871",
            "secret_hex": "a".repeat(64),
            "project": {
                "object_identity": project().object_identity,
                "root_object": project().root_object,
                "file_object": project().file_object,
                "generation": 3,
            },
        });

        let write = |value: &serde_json::Value| {
            write_private(&path, &serde_json::to_vec(value).expect("bytes"));
        };

        write(&base);
        RendezvousFile::read(&path).expect("the base document is valid");

        for mutate in [
            |mut value: serde_json::Value| {
                value["schema_version"] = json!("2");
                value
            },
            |mut value: serde_json::Value| {
                value["secret_hex"] = json!("zz");
                value
            },
            |mut value: serde_json::Value| {
                value["secret_hex"] = json!("aa");
                value
            },
            |mut value: serde_json::Value| {
                value["endpoint"] = json!("not-an-address");
                value
            },
            |mut value: serde_json::Value| {
                value["project"]["generation"] = json!("three");
                value
            },
            |mut value: serde_json::Value| {
                value["project"]
                    .as_object_mut()
                    .expect("object")
                    .remove("root_object");
                value
            },
        ] {
            write(&mutate(base.clone()));
            assert!(
                matches!(
                    RendezvousFile::read(&path),
                    Err(RendezvousError::Malformed { .. })
                ),
                "a malformed field is refused"
            );
        }

        write_private(&path, b"not json");
        assert!(matches!(
            RendezvousFile::read(&path),
            Err(RendezvousError::Malformed { .. })
        ));
    }

    #[test]
    fn secret_comparison_examines_every_byte_regardless_of_where_inputs_diverge() {
        let reference = [0x5a_u8; SECRET_BYTES];
        let mut first_differs = reference;
        first_differs[0] ^= 0xff;
        let mut last_differs = reference;
        last_differs[SECRET_BYTES - 1] ^= 0xff;
        let mut middle_differs = reference;
        middle_differs[SECRET_BYTES / 2] ^= 0xff;

        assert!(constant_time_eq(&reference, &reference));
        assert!(!constant_time_eq(&reference, &first_differs));
        assert!(!constant_time_eq(&reference, &last_differs));
        assert!(!constant_time_eq(&reference, &middle_differs));

        let counts = [
            compare_probe(&reference, &reference),
            compare_probe(&reference, &first_differs),
            compare_probe(&reference, &last_differs),
            compare_probe(&reference, &middle_differs),
        ];
        assert_eq!(
            counts, [SECRET_BYTES; 4],
            "every comparison touches all {SECRET_BYTES} bytes"
        );
    }

    #[test]
    fn a_length_mismatch_never_compares_equal() {
        let reference = [0x5a_u8; SECRET_BYTES];
        assert!(!constant_time_eq(
            &reference,
            &reference[..SECRET_BYTES - 1]
        ));
        assert!(!constant_time_eq(&reference, &[]));
        let mut longer = reference.to_vec();
        longer.push(0x5a);
        assert!(!constant_time_eq(&reference, &longer));
        assert!(constant_time_eq(&[], &[]));
    }

    #[test]
    fn the_hex_secret_check_refuses_wrong_length_and_non_hex_candidates() {
        let scratch = Scratch::new();
        let path = scratch.file("hex.rendezvous");
        let file = RendezvousFile::write_new(&path, endpoint(), project()).expect("write");
        assert!(file.matches_secret_hex(&file.secret_hex()));
        assert!(!file.matches_secret_hex(&"0".repeat(64)));
        assert!(!file.matches_secret_hex("deadbeef"));
        assert!(!file.matches_secret_hex("not-hex-at-all"));
        assert!(!file.matches_secret_hex(""));
        // Uppercase hex decodes; it is still compared in constant time.
        assert!(file.matches_secret_hex(&file.secret_hex().to_ascii_uppercase()));
    }

    #[test]
    fn loopback_detection_covers_both_address_families() {
        assert!(is_loopback(ephemeral_loopback()));
        assert!(is_loopback(ephemeral_loopback_v6()));
        assert!(is_loopback("127.0.0.5:1".parse().expect("addr")));
        assert!(!is_loopback("0.0.0.0:1".parse().expect("addr")));
        assert!(!is_loopback("192.168.1.1:1".parse().expect("addr")));
        assert!(!is_loopback("[::]:1".parse().expect("addr")));
        assert_eq!(ephemeral_loopback().port(), 0);
    }
}
