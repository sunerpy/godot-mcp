//! `file`-kind re-derivation.
//!
//! Everything compared here is observed at repair time through Todo 2's
//! descriptor/handle-relative primitives, which re-walk the acquisition spelling,
//! re-check both stable object identities and refuse a symlink at every
//! component. Nothing is read from the proof file and echoed back:
//!
//! * existence and content hash come from [`SafeRoot::read`] on the held root;
//! * the touched object identity of every path is re-derived by the same
//!   symlink-refusing walk the journal record used, and must equal the identity
//!   the record already holds for that path; and
//! * the proof file's own `observed_*` fields are then compared **against** that
//!   derivation, so a claim that disagrees with reality is a mismatch.
//!
//! Which side is proven follows Todo 6's per-operation rules exactly, because the
//! recorded entry already carries the expected `(existence, hash)` pair for its
//! side: `update` requires the recorded hash, `create`'s pre-state and `delete`'s
//! post-state are proven by an existence tombstone with no hash at all, and a
//! present path with no recorded hash can never prove content.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::Path;

use godot_mcp_domain::safe_root::{SafeRoot, SafeRootError};
use godot_mcp_domain::sysfs::{identity_of_file, open_child_regular_file, walk_dir_no_symlinks};
use godot_mcp_server::operator::{
    FileState, ObservedProof, OperatorSession, ProvenDisposition, RecordedExistence, RepairTarget,
};
use sha2::{Digest as _, Sha256};

use super::supplied::{SuppliedFile, SuppliedHeader};
use super::{Proven, RepairError, Verdict};

/// Lowercase hexadecimal SHA-256, matching the journal's own encoding.
#[must_use]
pub fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

/// One path's live observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedPath {
    /// Project-relative path.
    pub path: String,
    /// Whether it exists now.
    pub existence: RecordedExistence,
    /// Its live content hash, or `None` when absent.
    pub sha256: Option<String>,
    /// Its live platform-tagged OS object identity, or `None` when absent.
    pub object_identity: Option<String>,
}

/// Re-derive and compare a `file`-kind repair.
///
/// # Errors
/// Returns [`RepairError::Observation`] when a path cannot be observed at all,
/// which leaves the transaction unchanged rather than assuming absence.
pub fn derive(
    session: &OperatorSession,
    project: &Path,
    target: &RepairTarget,
    header: &SuppliedHeader,
    supplied: &SuppliedFile,
) -> Result<Verdict, RepairError> {
    let root = session.safe_root();
    let mut observed = Vec::with_capacity(target.touched_paths.len());
    for path in &target.touched_paths {
        observed.push(observe(&root, project, path)?);
    }

    if let Some(refusal) = refuse_identities(target, &observed, supplied) {
        return Ok(Verdict::Refused(refusal));
    }

    let pre = compare(&target.pre_hashes, &observed);
    let post = compare(&target.intended_post_hashes, &observed);
    let disposition = match (pre, post) {
        (SideOutcome::Matches, SideOutcome::Matches) => {
            return Ok(Verdict::Refused(
                "the recorded pre-state and intended post-state are indistinguishable on disk, so \
                 neither is proven"
                    .to_owned(),
            ));
        }
        (SideOutcome::Matches, _) => ProvenDisposition::PreState,
        (_, SideOutcome::Matches) => ProvenDisposition::PostState,
        (pre, post) => {
            return Ok(Verdict::Refused(format!(
                "neither recorded state is proven on disk (pre-state {}, post-state {})",
                pre.as_str(),
                post.as_str()
            )));
        }
    };
    if header.claimed_disposition != disposition {
        return Ok(Verdict::Refused(format!(
            "the proof file claims a {} state and the re-derivation proves a {} state",
            header.claimed_disposition.as_str(),
            disposition.as_str()
        )));
    }
    if let Some(refusal) = refuse_claims(&observed, supplied) {
        return Ok(Verdict::Refused(refusal));
    }

    let recorded = match disposition {
        ProvenDisposition::PreState => &target.pre_hashes,
        ProvenDisposition::PostState => &target.intended_post_hashes,
    };
    Ok(Verdict::Proven(Proven {
        disposition,
        observed: ObservedProof::File {
            entries: recorded
                .iter()
                .map(|entry| {
                    let live = observed.iter().find(|live| live.path == entry.path);
                    FileState {
                        path: entry.path.clone(),
                        operation: entry.operation,
                        existence: live.map_or(RecordedExistence::Absent, |live| live.existence),
                        sha256: live.and_then(|live| live.sha256.clone()),
                    }
                })
                .collect(),
        },
        detail: format!(
            "every touched path matches the recorded {} state, and every re-derived object \
             identity matches the record",
            disposition.as_str()
        ),
        live_survivor: None,
    }))
}

fn observe(root: &SafeRoot, project: &Path, relative: &str) -> Result<ObservedPath, RepairError> {
    let bytes = match root.read(Path::new(relative)) {
        Ok(bytes) => Some(bytes),
        // Todo 2 maps an absent in-root target to a stale-preimage outcome; for
        // proof purposes that is exactly the existence tombstone.
        Err(SafeRootError::StalePreimage { .. }) => None,
        Err(error) => {
            return Err(RepairError::Observation(format!(
                "{relative} could not be re-read through the held root: {error}"
            )));
        }
    };
    Ok(ObservedPath {
        path: relative.to_owned(),
        existence: if bytes.is_some() {
            RecordedExistence::Present
        } else {
            RecordedExistence::Absent
        },
        sha256: bytes.as_deref().map(hex_sha256),
        object_identity: if bytes.is_some() {
            object_identity(project, relative)
        } else {
            None
        },
    })
}

/// Re-derive one path's platform-tagged OS object identity, descriptor-relatively.
///
/// The walk refuses to follow a symlink at any component, so the identity is the
/// one the held root designates and never an aliased target's. `None` means the
/// path is absent or is not a regular file, which is a mismatch against a record
/// that holds an identity for it.
fn object_identity(project: &Path, relative: &str) -> Option<String> {
    let full = project.join(relative);
    let parent = full.parent()?;
    let name = full.file_name()?;
    let walked = walk_dir_no_symlinks(parent).ok()?;
    let file = open_child_regular_file(walked.dir(), OsStr::new(name)).ok()?;
    identity_of_file(&file)
        .ok()
        .map(OperatorSession::encode_object_identity)
}

/// Whether the re-derived object identities match the record and the claim.
///
/// The record's own `touched_object_identities` is the authority; the proof file's
/// `observed_touched_object_identities` must equal the re-derived set exactly, so
/// a file replaced by a different object since the transaction was journaled is a
/// mismatch even when its bytes hash the same.
fn refuse_identities(
    target: &RepairTarget,
    observed: &[ObservedPath],
    supplied: &SuppliedFile,
) -> Option<String> {
    for (path, recorded) in &target.touched_object_identities {
        let live = observed
            .iter()
            .find(|live| &live.path == path)
            .and_then(|live| live.object_identity.clone());
        if &live != recorded {
            return Some(format!(
                "{path} is object {} now and the record holds {}",
                live.as_deref().unwrap_or("absent"),
                recorded.as_deref().unwrap_or("absent")
            ));
        }
    }
    let derived: BTreeSet<String> = observed
        .iter()
        .filter_map(|live| live.object_identity.clone())
        .collect();
    let claimed: BTreeSet<String> = supplied
        .observed_touched_object_identities
        .iter()
        .cloned()
        .collect();
    if derived != claimed {
        return Some(format!(
            "the proof file claims touched object identities {claimed:?} and the command \
             re-derived {derived:?}"
        ));
    }
    None
}

/// Whether the proof file's own claims agree with the live derivation.
fn refuse_claims(observed: &[ObservedPath], supplied: &SuppliedFile) -> Option<String> {
    for live in observed {
        match supplied.observed_existence.get(&live.path) {
            None => {
                return Some(format!(
                    "the proof file claims nothing about the touched path {}",
                    live.path
                ));
            }
            Some(claimed) if *claimed != live.existence => {
                return Some(format!(
                    "the proof file claims {} is {} and it is {}",
                    live.path,
                    claimed.as_str(),
                    live.existence.as_str()
                ));
            }
            Some(_) => {}
        }
        let claimed_hash = supplied.observed_hashes.get(&live.path);
        if claimed_hash != live.sha256.as_ref() {
            return Some(format!(
                "the proof file claims {} hashes to {} and it hashes to {}",
                live.path,
                claimed_hash.map_or("nothing", String::as_str),
                live.sha256.as_deref().unwrap_or("nothing")
            ));
        }
    }
    for claimed in supplied.observed_existence.keys() {
        if !observed.iter().any(|live| &live.path == claimed) {
            return Some(format!(
                "the proof file claims a state for {claimed}, which this transaction never touched"
            ));
        }
    }
    None
}

/// Whether one recorded side is proven by the live observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SideOutcome {
    Matches,
    Mismatch,
    NoRecordedState,
}

impl SideOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Matches => "matches",
            Self::Mismatch => "mismatch",
            Self::NoRecordedState => "no recorded state",
        }
    }
}

fn compare(recorded: &[FileState], observed: &[ObservedPath]) -> SideOutcome {
    if recorded.is_empty() {
        return SideOutcome::NoRecordedState;
    }
    for entry in recorded {
        let Some(live) = observed.iter().find(|live| live.path == entry.path) else {
            return SideOutcome::Mismatch;
        };
        if entry.existence != live.existence {
            return SideOutcome::Mismatch;
        }
        match (&entry.sha256, &live.sha256) {
            // An absent path has no hash to compare; existence alone proves it.
            (None, None) => {}
            (Some(expected), Some(actual)) if expected == actual => {}
            _ => return SideOutcome::Mismatch,
        }
        // A present entry with no recorded hash could never prove content.
        if entry.existence == RecordedExistence::Present && entry.sha256.is_none() {
            return SideOutcome::Mismatch;
        }
    }
    SideOutcome::Matches
}

#[cfg(test)]
mod tests {
    use godot_mcp_server::operator::RecordedFileOperation;

    use super::*;

    fn recorded(
        path: &str,
        operation: RecordedFileOperation,
        existence: RecordedExistence,
        sha256: Option<&str>,
    ) -> FileState {
        FileState {
            path: path.to_owned(),
            operation,
            existence,
            sha256: sha256.map(str::to_owned),
        }
    }

    fn live(path: &str, sha256: Option<&str>) -> ObservedPath {
        ObservedPath {
            path: path.to_owned(),
            existence: if sha256.is_some() {
                RecordedExistence::Present
            } else {
                RecordedExistence::Absent
            },
            sha256: sha256.map(str::to_owned),
            object_identity: sha256.map(|_| "unix:dev=1,ino=2".to_owned()),
        }
    }

    /// The hash encoding must be the journal's, or a re-derived hash could never
    /// equal a recorded one.
    #[test]
    fn the_hash_encoding_is_lowercase_hex_sha256() {
        assert_eq!(
            hex_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(hex_sha256(b"a").len(), 64);
    }

    /// `update` is proven only by the recorded hash, and a different hash is a
    /// mismatch rather than a near-match.
    #[test]
    fn an_update_is_proven_only_by_its_recorded_hash() {
        let hash = hex_sha256(b"bytes");
        let entries = vec![recorded(
            "a.tscn",
            RecordedFileOperation::Update,
            RecordedExistence::Present,
            Some(&hash),
        )];
        assert_eq!(
            compare(&entries, &[live("a.tscn", Some(&hash))]),
            SideOutcome::Matches
        );
        assert_eq!(
            compare(&entries, &[live("a.tscn", Some(&hex_sha256(b"other")))]),
            SideOutcome::Mismatch
        );
        assert_eq!(
            compare(&entries, &[live("a.tscn", None)]),
            SideOutcome::Mismatch
        );
        assert_eq!(compare(&entries, &[]), SideOutcome::Mismatch);
    }

    /// A `create` pre-state and a `delete` post-state are existence tombstones:
    /// there is no hash to compare, and demanding one would make them unprovable.
    #[test]
    fn a_tombstone_is_proven_by_existence_alone() {
        for operation in [RecordedFileOperation::Create, RecordedFileOperation::Delete] {
            let entries = vec![recorded(
                "a.tscn",
                operation,
                RecordedExistence::Absent,
                None,
            )];
            assert_eq!(
                compare(&entries, &[live("a.tscn", None)]),
                SideOutcome::Matches
            );
            assert_eq!(
                compare(&entries, &[live("a.tscn", Some(&hex_sha256(b"x")))]),
                SideOutcome::Mismatch
            );
        }
    }

    /// A present entry with no recorded hash proves nothing about content, so it
    /// must never be accepted as a hash-bearing proof.
    #[test]
    fn a_present_entry_without_a_recorded_hash_is_never_proven() {
        let entries = vec![recorded(
            "a.tscn",
            RecordedFileOperation::Update,
            RecordedExistence::Present,
            None,
        )];
        assert_eq!(
            compare(&entries, &[live("a.tscn", Some(&hex_sha256(b"x")))]),
            SideOutcome::Mismatch
        );
    }

    /// An empty recorded side is not a proof of anything; it is the absence of a
    /// recorded state, which must be reported distinctly.
    #[test]
    fn an_empty_recorded_side_is_not_a_match() {
        assert_eq!(
            compare(&[], &[live("a.tscn", None)]),
            SideOutcome::NoRecordedState
        );
    }

    /// A claim that disagrees with the live derivation is a mismatch, in either
    /// direction, and a claim about an untouched path is refused outright.
    #[test]
    fn a_disagreeing_claim_is_refused() {
        let hash = hex_sha256(b"bytes");
        let observed = vec![live("a.tscn", Some(&hash))];
        let mut supplied = SuppliedFile {
            observed_touched_object_identities: vec!["unix:dev=1,ino=2".to_owned()],
            observed_existence: [("a.tscn".to_owned(), RecordedExistence::Present)]
                .into_iter()
                .collect(),
            observed_hashes: [("a.tscn".to_owned(), hash.clone())].into_iter().collect(),
        };
        assert_eq!(refuse_claims(&observed, &supplied), None);

        supplied
            .observed_hashes
            .insert("a.tscn".to_owned(), hex_sha256(b"other"));
        assert!(refuse_claims(&observed, &supplied).is_some());

        supplied.observed_hashes.insert("a.tscn".to_owned(), hash);
        supplied
            .observed_existence
            .insert("b.tscn".to_owned(), RecordedExistence::Absent);
        assert!(
            refuse_claims(&observed, &supplied)
                .is_some_and(|refusal| refusal.contains("never touched"))
        );
    }
}
