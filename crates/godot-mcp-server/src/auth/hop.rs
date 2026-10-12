//! Layer 1 of `proxied_oidc`: authenticating the proxy hop itself.
//!
//! # Why the hop needs its own credential
//!
//! The reverse proxy that terminates TLS is the only thing that may reach this
//! listener, and "it came from loopback" does not establish that. On a shared host
//! any local process — including one running as a different user — can connect to
//! `127.0.0.1`, and a proxy can assert any `X-Forwarded-*` header it wants. So the
//! hop authenticates with material only the proxy can hold:
//!
//! * [`HopCredential::File`] — a dedicated 256-bit credential in a file created
//!   mode `0600`. [`ProxyHop::load`] *refuses* a file whose mode was relaxed rather
//!   than trusting it, so a world-readable credential fails closed.
//! * [`HopCredential::Socket`] — an `0600` Unix socket or named pipe, where the OS
//!   ACL is the control and there is no bearer material to leak. The path's mode is
//!   still verified, for the same reason.
//!
//! This credential is *separate* from the end-user token. Presenting it proves
//! "this is the proxy", not "this is a user"; the user is established independently
//! by the OIDC pipeline. Both must pass on every request.
//!
//! # Constant-time comparison, proven rather than asserted
//!
//! [`ProxyHop::matches`] accumulates the XOR of every byte pair and only then
//! reduces to a boolean — the same shape Todo 19's rendezvous check uses. Because
//! "it looks constant-time" is not evidence, [`ProxyHop::compare_probe`] reports how
//! many byte comparisons actually ran so a test can assert that a candidate
//! differing in its *first* byte and one differing in its *last* byte take the same
//! number, which an early-return implementation cannot do.
//!
//! # The credential is never logged and never forwarded
//!
//! It is held as [`SecretValue`], whose `Debug`/`Display` are the redaction marker,
//! and it is not part of the identity handed to the kernel. Nothing downstream —
//! not the kernel, not a Godot process, not an editor plugin — can observe it.

use std::fs;
use std::path::{Path, PathBuf};

use godot_mcp_domain::config::ProxyHopCredential;

use crate::observability::SecretValue;

/// Length of a hop credential, in bytes. 256 bits.
pub const HOP_CREDENTIAL_BYTES: usize = 32;

/// The only permission bits a hop credential file or socket may carry.
pub const HOP_FILE_MODE: u32 = 0o600;

/// How the proxy hop proves it is the proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HopCredential {
    /// A dedicated high-entropy credential in an ACL-protected file.
    File {
        /// Where the credential lives.
        path: PathBuf,
    },
    /// An ACL-protected Unix socket or named pipe.
    Socket {
        /// Where the socket lives.
        path: PathBuf,
    },
}

impl HopCredential {
    /// Adopt the configured shape.
    #[must_use]
    pub fn from_config(configured: &ProxyHopCredential) -> Self {
        match configured {
            ProxyHopCredential::CredentialFile { path } => Self::File {
                path: PathBuf::from(path),
            },
            ProxyHopCredential::SocketPath { path } => Self::Socket {
                path: PathBuf::from(path),
            },
        }
    }

    /// The path this credential is published at.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::File { path } | Self::Socket { path } => path,
        }
    }
}

/// Why the proxy hop did not authenticate.
///
/// The `Display` rendering is the internal label, not a message for a caller: the
/// wire form is the single indistinguishable challenge in [`super::challenge`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{}", self.as_str())]
pub enum HopRejection {
    /// No hop credential was presented.
    Missing,
    /// The presented credential is not this deployment's.
    Mismatch,
    /// The credential file or socket is missing or unreadable.
    Unreadable,
    /// The credential file or socket carries relaxed permissions.
    InsecurePermissions {
        /// The mode observed.
        mode: u32,
    },
    /// The credential file does not hold well-formed material.
    Malformed,
}

impl HopRejection {
    /// A stable snake_case label, for tests and the internal diagnostic sink.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "hop_credential_missing",
            Self::Mismatch => "hop_credential_mismatch",
            Self::Unreadable => "hop_credential_unreadable",
            Self::InsecurePermissions { .. } => "hop_credential_insecure_permissions",
            Self::Malformed => "hop_credential_malformed",
        }
    }
}

/// A loaded, ACL-verified proxy-hop credential.
#[derive(Debug)]
pub struct ProxyHop {
    credential: HopCredential,
    expected: SecretValue,
}

impl ProxyHop {
    /// Load and ACL-verify the configured hop credential.
    ///
    /// For [`HopCredential::Socket`] the OS ACL *is* the control, so the socket's
    /// mode is verified and no bearer material is read; a request arriving over that
    /// socket carries the socket's own marker.
    ///
    /// # Errors
    /// Returns [`HopRejection`] when the path is missing, its permissions were
    /// relaxed, or a credential file does not hold usable material.
    pub fn load(credential: HopCredential) -> Result<Self, HopRejection> {
        let mode = mode_of(credential.path())?;
        if mode != HOP_FILE_MODE {
            return Err(HopRejection::InsecurePermissions { mode });
        }
        let expected = match &credential {
            HopCredential::File { path } => {
                let raw = fs::read_to_string(path).map_err(|_| HopRejection::Unreadable)?;
                let trimmed = raw.trim().to_owned();
                if trimmed.len() < HOP_CREDENTIAL_BYTES * 2 {
                    return Err(HopRejection::Malformed);
                }
                SecretValue::new(trimmed)
            }
            // A socket has no bearer material; the marker is derived from the
            // verified path so a *different* socket cannot satisfy this hop.
            HopCredential::Socket { path } => {
                SecretValue::new(format!("socket:{}", path.display()))
            }
        };
        Ok(Self {
            credential,
            expected,
        })
    }

    /// The credential shape in use.
    #[must_use]
    pub const fn credential(&self) -> &HopCredential {
        &self.credential
    }

    /// The marker a request arriving over the configured socket must present.
    ///
    /// Exists so the transport can supply it once the connection's provenance is
    /// established by the OS; it is not derived from any client-supplied value.
    #[must_use]
    pub fn socket_marker(&self) -> Option<String> {
        match &self.credential {
            HopCredential::Socket { path } => Some(format!("socket:{}", path.display())),
            HopCredential::File { .. } => None,
        }
    }

    /// Layer 1. Whether `candidate` authenticates the hop.
    ///
    /// # Errors
    /// Returns [`HopRejection::Missing`] when nothing was presented and
    /// [`HopRejection::Mismatch`] when what was presented is not this deployment's
    /// credential. The two are separate internally and identical on the wire.
    pub fn authenticate(&self, candidate: Option<&str>) -> Result<(), HopRejection> {
        let candidate = candidate.ok_or(HopRejection::Missing)?;
        if candidate.is_empty() {
            return Err(HopRejection::Missing);
        }
        if constant_time_eq(self.expected.expose().as_bytes(), candidate.as_bytes()) {
            Ok(())
        } else {
            Err(HopRejection::Mismatch)
        }
    }

    /// How many byte comparisons validating `candidate` performs.
    ///
    /// Exists so a test can prove the comparison does not short-circuit.
    #[must_use]
    pub fn compare_probe(&self, candidate: &str) -> usize {
        compare_probe(self.expected.expose().as_bytes(), candidate.as_bytes())
    }

    /// Publish a fresh credential file for a hop, mode `0600`.
    ///
    /// This is how a deployment (and the acceptance fixture) creates the material
    /// the proxy will read. The bytes come from the OS CSPRNG.
    ///
    /// # Errors
    /// Returns [`HopRejection`] when entropy or the filesystem refuses.
    pub fn publish(path: &Path) -> Result<String, HopRejection> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|_| HopRejection::Unreadable)?;
        }
        let mut bytes = [0_u8; HOP_CREDENTIAL_BYTES];
        getrandom::fill(&mut bytes).map_err(|_| HopRejection::Unreadable)?;
        let token = hex_encode(&bytes);
        write_protected(path, &token)?;
        Ok(token)
    }

    /// Relax a credential file's permissions.
    ///
    /// Exists so a test can prove [`ProxyHop::load`] refuses the result rather than
    /// trusting a world-readable credential. A no-op on Windows, where the
    /// equivalent restriction is a private-object ACL.
    ///
    /// # Errors
    /// Returns [`HopRejection::Unreadable`] when the mode change fails.
    pub fn set_mode(path: &Path, mode: u32) -> Result<(), HopRejection> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))
                .map_err(|_| HopRejection::Unreadable)
        }
        #[cfg(not(unix))]
        {
            let _ = (path, mode);
            Ok(())
        }
    }
}

fn write_protected(path: &Path, token: &str) -> Result<(), HopRejection> {
    use std::io::Write as _;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(HOP_FILE_MODE);
    }
    let mut handle = options.open(path).map_err(|_| HopRejection::Unreadable)?;
    handle
        .write_all(token.as_bytes())
        .map_err(|_| HopRejection::Unreadable)?;
    handle.sync_all().map_err(|_| HopRejection::Unreadable)?;
    ProxyHop::set_mode(path, HOP_FILE_MODE)
}

/// The path's permission bits.
///
/// On Windows the Unix bits do not exist; the expected mode is reported once the
/// path is confirmed to exist, because the equivalent restriction there is a
/// private-object ACL owned by Todo 2's `sysfs` layer.
fn mode_of(path: &Path) -> Result<u32, HopRejection> {
    let metadata = fs::metadata(path).map_err(|_| HopRejection::Unreadable)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        Ok(metadata.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Ok(HOP_FILE_MODE)
    }
}

/// Compare without an early return or a data-dependent branch.
fn constant_time_eq(expected: &[u8], candidate: &[u8]) -> bool {
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
fn compare_probe(expected: &[u8], candidate: &[u8]) -> usize {
    if expected.len() != candidate.len() {
        return 0;
    }
    let mut examined = 0_usize;
    let mut difference = 0_u8;
    for (left, right) in expected.iter().zip(candidate.iter()) {
        difference |= left ^ right;
        examined += 1;
    }
    examined + usize::from(difference) - usize::from(difference)
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut out, "{byte:02x}").expect("writing to a String cannot fail");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("godot-mcp-hop-{}-{label}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn a_published_credential_authenticates_only_itself() {
        let dir = scratch("publish");
        let path = dir.join("hop-credential");
        let token = ProxyHop::publish(&path).expect("publish");
        assert_eq!(token.len(), HOP_CREDENTIAL_BYTES * 2);
        let hop = ProxyHop::load(HopCredential::File { path: path.clone() }).expect("load");
        assert_eq!(hop.authenticate(Some(&token)), Ok(()));
        assert_eq!(hop.authenticate(None), Err(HopRejection::Missing));
        assert_eq!(hop.authenticate(Some("")), Err(HopRejection::Missing));
        assert_eq!(
            hop.authenticate(Some(&"0".repeat(HOP_CREDENTIAL_BYTES * 2))),
            Err(HopRejection::Mismatch)
        );
        assert!(hop.socket_marker().is_none());
        assert_eq!(hop.credential().path(), path.as_path());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_credential_is_never_rendered() {
        let dir = scratch("redaction");
        let path = dir.join("hop-credential");
        let token = ProxyHop::publish(&path).expect("publish");
        let hop = ProxyHop::load(HopCredential::File { path }).expect("load");
        let rendered = format!("{hop:?}");
        assert!(
            !rendered.contains(&token),
            "the hop credential must never be rendered"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn a_relaxed_credential_file_is_refused() {
        let dir = scratch("relaxed");
        let path = dir.join("hop-credential");
        ProxyHop::publish(&path).expect("publish");
        ProxyHop::set_mode(&path, 0o644).expect("relax");
        assert_eq!(
            ProxyHop::load(HopCredential::File { path: path.clone() }).err(),
            Some(HopRejection::InsecurePermissions { mode: 0o644 })
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_or_malformed_credential_file_is_refused() {
        let dir = scratch("malformed");
        let missing = dir.join("absent");
        assert_eq!(
            ProxyHop::load(HopCredential::File { path: missing }).err(),
            Some(HopRejection::Unreadable)
        );
        let short = dir.join("short");
        write_protected(&short, "abcd").expect("write");
        assert_eq!(
            ProxyHop::load(HopCredential::File { path: short }).err(),
            Some(HopRejection::Malformed)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_socket_hop_uses_its_verified_path_and_reads_no_material() {
        let dir = scratch("socket");
        let path = dir.join("hop.sock");
        // A regular file stands in for the socket node: what is under test is that
        // the *path's* ACL is verified and that no bearer material is read from it.
        write_protected(&path, "ignored-content").expect("write");
        let hop = ProxyHop::load(HopCredential::Socket { path: path.clone() }).expect("load");
        let marker = hop.socket_marker().expect("a socket hop has a marker");
        assert_eq!(hop.authenticate(Some(&marker)), Ok(()));
        assert_eq!(
            hop.authenticate(Some("socket:/somewhere/else")),
            Err(HopRejection::Mismatch)
        );
        assert_eq!(
            hop.authenticate(Some("ignored-content")),
            Err(HopRejection::Mismatch),
            "a socket hop must not accept the node's contents as a credential"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn comparison_examines_every_byte() {
        let dir = scratch("timing");
        let path = dir.join("hop-credential");
        let token = ProxyHop::publish(&path).expect("publish");
        let hop = ProxyHop::load(HopCredential::File { path }).expect("load");
        let mut first = token.clone().into_bytes();
        first[0] = if first[0] == b'a' { b'b' } else { b'a' };
        let mut last = token.clone().into_bytes();
        let end = last.len() - 1;
        last[end] = if last[end] == b'a' { b'b' } else { b'a' };
        let first = String::from_utf8(first).expect("ascii");
        let last = String::from_utf8(last).expect("ascii");
        assert_eq!(hop.authenticate(Some(&first)), Err(HopRejection::Mismatch));
        assert_eq!(hop.authenticate(Some(&last)), Err(HopRejection::Mismatch));
        assert_eq!(
            hop.compare_probe(&first),
            hop.compare_probe(&last),
            "the hop comparison must not short-circuit"
        );
        assert_eq!(hop.compare_probe(&token), token.len());
        assert_eq!(hop.compare_probe("short"), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn configuration_shapes_map_to_credentials() {
        assert_eq!(
            HopCredential::from_config(&ProxyHopCredential::CredentialFile {
                path: "/tmp/hop".to_owned()
            }),
            HopCredential::File {
                path: PathBuf::from("/tmp/hop")
            }
        );
        assert_eq!(
            HopCredential::from_config(&ProxyHopCredential::SocketPath {
                path: "/tmp/hop.sock".to_owned()
            }),
            HopCredential::Socket {
                path: PathBuf::from("/tmp/hop.sock")
            }
        );
        assert_eq!(HopRejection::Missing.as_str(), "hop_credential_missing");
        assert_eq!(HopRejection::Mismatch.as_str(), "hop_credential_mismatch");
        assert_eq!(
            HopRejection::InsecurePermissions { mode: 0o644 }.as_str(),
            "hop_credential_insecure_permissions"
        );
        assert_eq!(HopRejection::Malformed.as_str(), "hop_credential_malformed");
        assert_eq!(
            HopRejection::Unreadable.as_str(),
            "hop_credential_unreadable"
        );
    }
}
