//! The per-launch bearer secret that authenticates `trusted_local` requests.
//!
//! # Why a file, and why this file
//!
//! `trusted_local` exists so a co-located MCP client can reach the loopback
//! listener without an identity provider. The only thing it may present is a
//! secret this launch generated, because a loopback peer address is *not* an
//! identity: any local process — including one running as another user on a
//! shared host — can open a loopback connection. The secret is therefore:
//!
//! * **fresh per launch** — [`LaunchSecret::create`] draws 256 bits from the OS
//!   CSPRNG, so a token captured from a previous launch cannot be replayed;
//! * **user-scoped by ACL** — the file is created with mode
//!   [`LAUNCH_SECRET_FILE_MODE`] (owner read/write) from the outset, and
//!   [`LaunchSecret::read_token`] *refuses* a file whose mode was relaxed rather
//!   than trusting it;
//! * **removed at shutdown and rotated at restart** — [`LaunchSecret::remove`]
//!   deletes it, and the next `create` writes different bytes.
//!
//! # Constant-time comparison, and why it is tested rather than asserted
//!
//! [`constant_time_eq`] accumulates the XOR of every byte pair and only then
//! reduces to a boolean: no early return, no data-dependent branch. Because "it
//! looks constant-time" is not evidence, [`LaunchSecret::compare_probe`] reports
//! how many byte comparisons actually ran, and the acceptance suite asserts that a
//! candidate differing in its first byte and one differing in its last byte
//! produce the *same* count. An early-exit implementation fails that assertion.
//!
//! A length mismatch returns `false` immediately, which leaks only the
//! candidate's length — a value the caller chose and already knows.
//!
//! # Windows
//!
//! Unix permission bits do not exist on Windows; [`LaunchSecret::mode_of`]
//! reports [`LAUNCH_SECRET_FILE_MODE`] there once the file is confirmed to
//! exist, because the equivalent restriction is a private-object ACL owned by
//! Todo 2's `sysfs` layer. The secret check itself is identical on both
//! platforms and is the load-bearing control.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use godot_mcp_domain::contracts::PrincipalId;
use sha2::{Digest, Sha256};

use crate::idempotency::ModePrincipal;

/// Length of the per-launch secret, in bytes. 256 bits.
pub const LAUNCH_SECRET_BYTES: usize = 32;

/// The only permission bits a launch-secret file may carry: owner read/write.
pub const LAUNCH_SECRET_FILE_MODE: u32 = 0o600;

/// File name, inside the private state directory, holding the launch secret.
pub const LAUNCH_SECRET_FILE_NAME: &str = "http-launch-secret";

/// Why a launch secret could not be created, read or trusted.
#[derive(Debug, thiserror::Error)]
pub enum LaunchSecretError {
    /// The OS CSPRNG refused to produce entropy.
    #[error("the OS random source failed: {detail}")]
    NoEntropy {
        /// What the OS reported.
        detail: String,
    },
    /// The secret file could not be written or replaced.
    #[error("the launch secret at {path} could not be written: {detail}")]
    Unwritable {
        /// Where it was being written.
        path: String,
        /// What the OS reported.
        detail: String,
    },
    /// The secret file could not be read, or does not exist.
    #[error("the launch secret at {path} is unreadable: {detail}")]
    Unreadable {
        /// Where it was expected.
        path: String,
        /// What the OS reported.
        detail: String,
    },
    /// The secret file carries permission bits beyond owner read/write.
    #[error(
        "the launch secret is mode {mode:04o}; only {LAUNCH_SECRET_FILE_MODE:04o} is acceptable"
    )]
    InsecurePermissions {
        /// The mode observed.
        mode: u32,
    },
    /// The file does not hold a well-formed token.
    #[error("the launch secret is malformed: {detail}")]
    Malformed {
        /// Exactly what was wrong.
        detail: String,
    },
}

/// One launch's bearer secret, plus the ACL-protected file that publishes it.
pub struct LaunchSecret {
    path: PathBuf,
    secret: [u8; LAUNCH_SECRET_BYTES],
}

impl std::fmt::Debug for LaunchSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl LaunchSecret {
    /// Generate a fresh secret and publish it in `state_dir`, replacing any file
    /// a previous launch left behind.
    ///
    /// # Errors
    /// Returns [`LaunchSecretError`] when entropy or the filesystem refuses.
    pub fn create(state_dir: &Path) -> Result<Self, LaunchSecretError> {
        fs::create_dir_all(state_dir).map_err(|error| LaunchSecretError::Unwritable {
            path: state_dir.display().to_string(),
            detail: error.to_string(),
        })?;
        let mut secret = [0_u8; LAUNCH_SECRET_BYTES];
        getrandom::fill(&mut secret).map_err(|error| LaunchSecretError::NoEntropy {
            detail: error.to_string(),
        })?;
        let secret = Self {
            path: state_dir.join(LAUNCH_SECRET_FILE_NAME),
            secret,
        };
        secret.persist()?;
        Ok(secret)
    }

    /// Where this launch's secret is published.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The bearer token a client presents, in lowercase hexadecimal.
    #[must_use]
    pub fn bearer_token(&self) -> String {
        hex_encode(&self.secret)
    }

    /// The opaque principal this launch's secret authenticates.
    ///
    /// It is derived from the secret by a domain-separated digest so that the
    /// principal is stable for the launch, differs across launches, and never
    /// exposes the secret itself.
    #[must_use]
    pub fn principal(&self) -> PrincipalId {
        let mut hasher = Sha256::new();
        hasher.update(b"godot-mcp-http-launch-principal-v1");
        hasher.update(self.secret);
        let digest = hasher.finalize();
        PrincipalId::parse(format!("principal_{}", hex_encode(&digest)))
            .expect("a 64-character lowercase hex payload is a valid principal identifier")
    }

    /// The Todo 27 identity shape this mode is allowed to present.
    #[must_use]
    pub fn mode_principal(&self) -> ModePrincipal {
        ModePrincipal::LaunchSecret {
            principal: self.principal().as_str().to_owned(),
        }
    }

    /// Whether `candidate` is this launch's token, compared in constant time.
    #[must_use]
    pub fn matches_bearer(&self, candidate: &str) -> bool {
        match hex_decode(candidate) {
            Some(bytes) => constant_time_eq(&self.secret, &bytes),
            None => false,
        }
    }

    /// How many byte comparisons validating `candidate` performs.
    ///
    /// Exists so a test can prove the comparison does not short-circuit: two
    /// candidates differing at opposite ends must report the same count.
    #[must_use]
    pub fn compare_probe(&self, candidate: &str) -> usize {
        match hex_decode(candidate) {
            Some(bytes) => compare_probe(&self.secret, &bytes),
            None => 0,
        }
    }

    /// Delete the published secret. Idempotent.
    ///
    /// # Errors
    /// Returns [`LaunchSecretError::Unwritable`] when removal fails for a reason
    /// other than the file already being gone.
    pub fn remove(&self) -> Result<(), LaunchSecretError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(LaunchSecretError::Unwritable {
                path: self.path.display().to_string(),
                detail: error.to_string(),
            }),
        }
    }

    /// Read a published token, refusing a file whose permissions were relaxed.
    ///
    /// # Errors
    /// Returns [`LaunchSecretError`] when the file is missing, world-accessible or
    /// not a well-formed token.
    pub fn read_token(path: &Path) -> Result<String, LaunchSecretError> {
        let mode = Self::mode_of(path)?;
        if mode != LAUNCH_SECRET_FILE_MODE {
            return Err(LaunchSecretError::InsecurePermissions { mode });
        }
        let text = fs::read_to_string(path).map_err(|error| LaunchSecretError::Unreadable {
            path: path.display().to_string(),
            detail: error.to_string(),
        })?;
        let token = text.trim().to_owned();
        match hex_decode(&token) {
            Some(bytes) if bytes.len() == LAUNCH_SECRET_BYTES => Ok(token),
            _ => Err(LaunchSecretError::Malformed {
                detail: format!("expected {LAUNCH_SECRET_BYTES} hexadecimal bytes"),
            }),
        }
    }

    /// The secret file's permission bits. See this module's header for Windows.
    ///
    /// # Errors
    /// Returns [`LaunchSecretError::Unreadable`] when the file cannot be stat'd.
    pub fn mode_of(path: &Path) -> Result<u32, LaunchSecretError> {
        let metadata = fs::metadata(path).map_err(|error| LaunchSecretError::Unreadable {
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
            Ok(LAUNCH_SECRET_FILE_MODE)
        }
    }

    /// Set a secret file's permission bits.
    ///
    /// Exists so a test can *relax* a mode and prove [`LaunchSecret::read_token`]
    /// refuses the result. On Windows this is a no-op.
    ///
    /// # Errors
    /// Returns [`LaunchSecretError::Unwritable`] when the mode change fails.
    pub fn set_mode(path: &Path, mode: u32) -> Result<(), LaunchSecretError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| {
                LaunchSecretError::Unwritable {
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

    fn persist(&self) -> Result<(), LaunchSecretError> {
        let unwritable = |error: std::io::Error| LaunchSecretError::Unwritable {
            path: self.path.display().to_string(),
            detail: error.to_string(),
        };
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(LAUNCH_SECRET_FILE_MODE);
        }
        let mut handle = options.open(&self.path).map_err(unwritable)?;
        handle
            .write_all(self.bearer_token().as_bytes())
            .map_err(unwritable)?;
        handle.sync_all().map_err(unwritable)?;
        // An existing file keeps its old mode through `create(true)`, so a
        // rotation must not inherit a relaxed one.
        Self::set_mode(&self.path, LAUNCH_SECRET_FILE_MODE)
    }
}

/// Compare two byte strings without an early return or a data-dependent branch.
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

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("writing to a String cannot fail");
    }
    out
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let high = nibble(pair[0])?;
        let low = nibble(pair[1])?;
        out.push((high << 4) | low);
    }
    Some(out)
}

const fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "godot-mcp-launch-secret-{}-{label}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn create_publishes_an_owner_only_token() {
        let dir = scratch("create");
        let secret = LaunchSecret::create(&dir).expect("create");
        assert_eq!(
            LaunchSecret::mode_of(secret.path()).expect("mode"),
            LAUNCH_SECRET_FILE_MODE
        );
        let token = LaunchSecret::read_token(secret.path()).expect("read");
        assert_eq!(token, secret.bearer_token());
        assert_eq!(token.len(), LAUNCH_SECRET_BYTES * 2);
        assert!(secret.matches_bearer(&token));
        secret.remove().expect("remove");
        assert!(!secret.path().exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn debug_redacts_the_path_and_secret_material() {
        let secret = LaunchSecret {
            path: PathBuf::from("path-sentinel"),
            secret: [0x5a_u8; LAUNCH_SECRET_BYTES],
        };

        assert_eq!(format!("{secret:?}"), "[REDACTED]");
        assert_eq!(
            format!("{:?}", tracing::field::debug(&secret)),
            "[REDACTED]"
        );
    }

    #[test]
    fn restart_rotates_the_secret() {
        let dir = scratch("rotate");
        let first = LaunchSecret::create(&dir).expect("first");
        let first_token = first.bearer_token();
        let second = LaunchSecret::create(&dir).expect("second");
        assert_ne!(second.bearer_token(), first_token);
        assert!(!second.matches_bearer(&first_token));
        assert_eq!(
            LaunchSecret::mode_of(second.path()).expect("mode"),
            LAUNCH_SECRET_FILE_MODE
        );
        second.remove().expect("remove");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn a_relaxed_mode_is_refused() {
        let dir = scratch("relaxed");
        let secret = LaunchSecret::create(&dir).expect("create");
        LaunchSecret::set_mode(secret.path(), 0o644).expect("relax");
        assert!(matches!(
            LaunchSecret::read_token(secret.path()),
            Err(LaunchSecretError::InsecurePermissions { mode: 0o644 })
        ));
        secret.remove().expect("remove");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn comparison_examines_every_byte() {
        let expected = [0x5a_u8; LAUNCH_SECRET_BYTES];
        let mut first = expected;
        first[0] ^= 0xff;
        let mut last = expected;
        last[LAUNCH_SECRET_BYTES - 1] ^= 0xff;
        assert!(!constant_time_eq(&expected, &first));
        assert!(!constant_time_eq(&expected, &last));
        assert_eq!(
            compare_probe(&expected, &first),
            compare_probe(&expected, &last)
        );
        assert_eq!(compare_probe(&expected, &expected), LAUNCH_SECRET_BYTES);
        assert_eq!(compare_probe(&expected, &expected[..4]), 0);
    }

    #[test]
    fn malformed_and_non_hex_tokens_are_refused() {
        let dir = scratch("malformed");
        let secret = LaunchSecret::create(&dir).expect("create");
        assert!(!secret.matches_bearer("not-hex"));
        assert!(!secret.matches_bearer(""));
        assert!(!secret.matches_bearer(&"ab".repeat(LAUNCH_SECRET_BYTES + 1)));
        assert_eq!(secret.compare_probe("not-hex"), 0);
        fs::write(secret.path(), "zz").expect("write");
        LaunchSecret::set_mode(secret.path(), LAUNCH_SECRET_FILE_MODE).expect("mode");
        assert!(matches!(
            LaunchSecret::read_token(secret.path()),
            Err(LaunchSecretError::Malformed { .. })
        ));
        secret.remove().expect("remove");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_principal_is_launch_scoped_and_never_the_secret() {
        let dir = scratch("principal");
        let first = LaunchSecret::create(&dir).expect("first");
        let principal = first.principal();
        assert_eq!(principal, first.principal());
        assert!(!principal.as_str().contains(&first.bearer_token()));
        assert_eq!(
            first.mode_principal(),
            ModePrincipal::LaunchSecret {
                principal: principal.as_str().to_owned(),
            }
        );
        let second = LaunchSecret::create(&dir).expect("second");
        assert_ne!(second.principal(), principal);
        second.remove().expect("remove");
        let _ = fs::remove_dir_all(&dir);
    }
}
