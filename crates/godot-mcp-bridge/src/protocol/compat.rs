//! The compatibility matrix: what pairing of server, plugin and project is
//! admitted, and every way one is not.
//!
//! # Compatibility is decided by MAJOR, and only by MAJOR
//!
//! [`PROTOCOL_VERSION`] is a semantic version. The rule is deliberately blunt:
//!
//! * **equal MAJOR** → compatible. A `MINOR`/`PATCH` difference in either
//!   direction is fine, because within one MAJOR this protocol only ever *adds*
//!   optional payload members and *adds* method names. A newer plugin talking to
//!   an older server simply gets `unsupported` for methods the server never
//!   advertised in `capabilities`; nothing silently misbehaves.
//! * **different MAJOR, either way** → [`CompatibilityDecision::MajorVersionUnsupported`].
//!   Not just "newer is refused": an *older* MAJOR is refused too, because a
//!   removed or reinterpreted field is exactly as dangerous as an unknown one, and
//!   this bridge dispatches editor mutations.
//! * **unparsable** → [`CompatibilityDecision::MalformedVersion`]. A version this
//!   code cannot compare is never optimistically treated as compatible.
//!
//! # Project identity is compared by OS object, not by path
//!
//! Three separate members must all match: the public
//! [`ProjectClaim::object_identity`], the root directory's OS object identity, and
//! `project.godot`'s OS object identity — plus the generation. Comparing paths
//! would accept a symlinked, bind-mounted, renamed or replaced project as "the
//! same one". Comparing object identities does not, and comparing the generation
//! additionally rejects a project that is *the same inode* but has been mutated
//! since this server last revalidated it.
//!
//! The plan forbids accepting a different project root. That is enforced here as
//! three distinct, separately reported refusals rather than one lumped mismatch,
//! so a diagnostic says which fact disagreed.
//!
//! # A refusal is classified, never silent
//!
//! Every non-compatible decision maps to one of Todo 3's ten fixed error kinds via
//! [`CompatibilityDecision::error_kind`], so the refusal reaches the caller as a
//! classified error rather than a dropped connection.

use godot_mcp_domain::ErrorKind;

use super::envelope::{PluginIdentity, ProjectClaim};

/// The bridge protocol version this build speaks.
pub const PROTOCOL_VERSION: &str = "1.0.0";

/// The MAJOR component of [`PROTOCOL_VERSION`]. The only compatibility axis.
pub const PROTOCOL_MAJOR: u32 = 1;

/// A parsed semantic version, to the precision this protocol compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolVersion {
    /// The compatibility axis.
    pub major: u32,
    /// Additive changes within a MAJOR.
    pub minor: u32,
    /// Fixes within a MINOR.
    pub patch: u32,
}

impl ProtocolVersion {
    /// Parse `MAJOR.MINOR.PATCH`. Anything else is `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }

    /// This build's own version.
    ///
    /// # Panics
    /// Never in practice: [`PROTOCOL_VERSION`] is a literal this module's tests
    /// prove parsable.
    #[must_use]
    pub fn current() -> Self {
        Self::parse(PROTOCOL_VERSION).unwrap_or(Self {
            major: PROTOCOL_MAJOR,
            minor: 0,
            patch: 0,
        })
    }
}

/// The matrix's verdict on one pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompatibilityDecision {
    /// Admitted: same MAJOR, same project object identities, same generation.
    Compatible,

    /// The peer's protocol version was not `MAJOR.MINOR.PATCH`.
    MalformedVersion,

    /// The peer's MAJOR differs from this build's, in either direction.
    MajorVersionUnsupported {
        /// This build's MAJOR.
        expected: u32,
        /// The peer's MAJOR.
        observed: u32,
    },

    /// The peer named a different public project identity.
    ProjectIdentityMismatch,

    /// The peer named a different root-directory OS object.
    ProjectRootMismatch,

    /// The peer named a different `project.godot` OS object.
    ProjectFileMismatch,

    /// Same project, but the peer's generation is not the one this server
    /// revalidated.
    ProjectGenerationStale {
        /// The generation this server holds.
        expected: u64,
        /// The generation the peer claimed.
        observed: u64,
    },
}

impl CompatibilityDecision {
    /// Whether the pairing is admitted.
    #[must_use]
    pub const fn is_compatible(&self) -> bool {
        matches!(self, Self::Compatible)
    }

    /// The classified error kind for a refusal, or `None` when compatible.
    ///
    /// A version refusal is `unsupported`; every project disagreement is
    /// `conflict`, because the project *is* there but is not the one agreed on.
    #[must_use]
    pub const fn error_kind(&self) -> Option<ErrorKind> {
        match self {
            Self::Compatible => None,
            Self::MalformedVersion => Some(ErrorKind::InvalidInput),
            Self::MajorVersionUnsupported { .. } => Some(ErrorKind::Unsupported),
            Self::ProjectIdentityMismatch
            | Self::ProjectRootMismatch
            | Self::ProjectFileMismatch
            | Self::ProjectGenerationStale { .. } => Some(ErrorKind::Conflict),
        }
    }

    /// A stable, payload-free spelling for diagnostics and audit records.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Compatible => "compatible",
            Self::MalformedVersion => "malformed_version",
            Self::MajorVersionUnsupported { .. } => "major_version_unsupported",
            Self::ProjectIdentityMismatch => "project_identity_mismatch",
            Self::ProjectRootMismatch => "project_root_mismatch",
            Self::ProjectFileMismatch => "project_file_mismatch",
            Self::ProjectGenerationStale { .. } => "project_generation_stale",
        }
    }
}

/// What this server will admit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompatibilityMatrix {
    version: String,
    project: ProjectClaim,
}

impl CompatibilityMatrix {
    /// A matrix pinned to `version` and the project this server revalidated.
    #[must_use]
    pub fn new(version: &str, project: ProjectClaim) -> Self {
        Self {
            version: version.to_owned(),
            project,
        }
    }

    /// The project this matrix admits, and no other.
    #[must_use]
    pub const fn project(&self) -> &ProjectClaim {
        &self.project
    }

    /// The protocol version this matrix speaks.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Decide one pairing.
    ///
    /// Version first, then project identity, root, file, generation — in that
    /// order, so the reported reason is the most fundamental disagreement rather
    /// than whichever check happened to run first.
    #[must_use]
    pub fn decide(&self, plugin: &PluginIdentity, claimed: &ProjectClaim) -> CompatibilityDecision {
        let Some(peer) = ProtocolVersion::parse(&plugin.protocol_version) else {
            return CompatibilityDecision::MalformedVersion;
        };
        let Some(ours) = ProtocolVersion::parse(&self.version) else {
            return CompatibilityDecision::MalformedVersion;
        };
        if peer.major != ours.major {
            return CompatibilityDecision::MajorVersionUnsupported {
                expected: ours.major,
                observed: peer.major,
            };
        }
        if claimed.object_identity != self.project.object_identity {
            return CompatibilityDecision::ProjectIdentityMismatch;
        }
        if claimed.root_object != self.project.root_object {
            return CompatibilityDecision::ProjectRootMismatch;
        }
        if claimed.file_object != self.project.file_object {
            return CompatibilityDecision::ProjectFileMismatch;
        }
        if claimed.generation != self.project.generation {
            return CompatibilityDecision::ProjectGenerationStale {
                expected: self.project.generation,
                observed: claimed.generation,
            };
        }
        CompatibilityDecision::Compatible
    }
}

#[cfg(test)]
mod tests {
    use godot_mcp_domain::ErrorKind;

    use super::super::envelope::{PluginIdentity, ProjectClaim};
    use super::{
        CompatibilityDecision, CompatibilityMatrix, PROTOCOL_MAJOR, PROTOCOL_VERSION,
        ProtocolVersion,
    };

    fn project() -> ProjectClaim {
        ProjectClaim {
            object_identity: format!("project_{}", "a".repeat(64)),
            root_object: "unix:dev=1,ino=2".to_owned(),
            file_object: "unix:dev=1,ino=3".to_owned(),
            generation: 3,
        }
    }

    fn plugin(version: &str) -> PluginIdentity {
        PluginIdentity {
            plugin_version: "0.1.0".to_owned(),
            protocol_version: version.to_owned(),
            godot_version: "4.4.1.stable".to_owned(),
        }
    }

    fn matrix() -> CompatibilityMatrix {
        CompatibilityMatrix::new(PROTOCOL_VERSION, project())
    }

    #[test]
    fn this_builds_own_version_parses_and_its_major_is_pinned() {
        let parsed = ProtocolVersion::parse(PROTOCOL_VERSION).expect("parses");
        assert_eq!(parsed.major, PROTOCOL_MAJOR);
        assert_eq!(ProtocolVersion::current(), parsed);
    }

    #[test]
    fn the_same_major_is_compatible_in_both_directions() {
        for version in ["1.0.0", "1.0.9", "1.4.0", "1.9999.9999"] {
            assert_eq!(
                matrix().decide(&plugin(version), &project()),
                CompatibilityDecision::Compatible,
                "version {version}"
            );
        }
    }

    #[test]
    fn a_different_major_is_refused_in_both_directions() {
        assert_eq!(
            matrix().decide(&plugin("2.0.0"), &project()),
            CompatibilityDecision::MajorVersionUnsupported {
                expected: 1,
                observed: 2
            }
        );
        assert_eq!(
            matrix().decide(&plugin("0.9.0"), &project()),
            CompatibilityDecision::MajorVersionUnsupported {
                expected: 1,
                observed: 0
            }
        );
        assert_eq!(
            matrix().decide(&plugin("2.0.0"), &project()).error_kind(),
            Some(ErrorKind::Unsupported)
        );
    }

    #[test]
    fn an_unparsable_version_is_never_optimistically_admitted() {
        for version in [
            "",
            "1",
            "1.0",
            "1.0.0.0",
            "1.0.x",
            "v1.0.0",
            "not-a-version",
        ] {
            assert_eq!(
                matrix().decide(&plugin(version), &project()),
                CompatibilityDecision::MalformedVersion,
                "version {version:?}"
            );
        }
        assert_eq!(
            CompatibilityDecision::MalformedVersion.error_kind(),
            Some(ErrorKind::InvalidInput)
        );
    }

    #[test]
    fn each_project_fact_is_refused_separately() {
        let mut identity = project();
        identity.object_identity = format!("project_{}", "b".repeat(64));
        assert_eq!(
            matrix().decide(&plugin("1.0.0"), &identity),
            CompatibilityDecision::ProjectIdentityMismatch
        );

        let mut root = project();
        root.root_object = "unix:dev=1,ino=999".to_owned();
        assert_eq!(
            matrix().decide(&plugin("1.0.0"), &root),
            CompatibilityDecision::ProjectRootMismatch
        );

        let mut file = project();
        file.file_object = "unix:dev=1,ino=999".to_owned();
        assert_eq!(
            matrix().decide(&plugin("1.0.0"), &file),
            CompatibilityDecision::ProjectFileMismatch
        );

        let mut generation = project();
        generation.generation = 4;
        assert_eq!(
            matrix().decide(&plugin("1.0.0"), &generation),
            CompatibilityDecision::ProjectGenerationStale {
                expected: 3,
                observed: 4
            }
        );

        // A rolled-back generation is stale too, not silently accepted.
        let mut older = project();
        older.generation = 2;
        assert_eq!(
            matrix().decide(&plugin("1.0.0"), &older),
            CompatibilityDecision::ProjectGenerationStale {
                expected: 3,
                observed: 2
            }
        );
    }

    #[test]
    fn every_project_disagreement_is_a_conflict() {
        for decision in [
            CompatibilityDecision::ProjectIdentityMismatch,
            CompatibilityDecision::ProjectRootMismatch,
            CompatibilityDecision::ProjectFileMismatch,
            CompatibilityDecision::ProjectGenerationStale {
                expected: 1,
                observed: 2,
            },
        ] {
            assert_eq!(decision.error_kind(), Some(ErrorKind::Conflict));
            assert!(!decision.is_compatible());
        }
        assert_eq!(CompatibilityDecision::Compatible.error_kind(), None);
        assert!(CompatibilityDecision::Compatible.is_compatible());
    }

    #[test]
    fn the_version_check_precedes_every_project_check() {
        // Both wrong: the version is reported, because a peer that cannot be
        // parsed cannot be trusted to have meant its project claim either.
        let mut wrong = project();
        wrong.root_object = "unix:dev=9,ino=9".to_owned();
        assert_eq!(
            matrix().decide(&plugin("2.0.0"), &wrong),
            CompatibilityDecision::MajorVersionUnsupported {
                expected: 1,
                observed: 2
            }
        );
    }

    #[test]
    fn every_decision_has_a_stable_spelling() {
        let spellings = [
            CompatibilityDecision::Compatible.as_str(),
            CompatibilityDecision::MalformedVersion.as_str(),
            CompatibilityDecision::MajorVersionUnsupported {
                expected: 1,
                observed: 2,
            }
            .as_str(),
            CompatibilityDecision::ProjectIdentityMismatch.as_str(),
            CompatibilityDecision::ProjectRootMismatch.as_str(),
            CompatibilityDecision::ProjectFileMismatch.as_str(),
            CompatibilityDecision::ProjectGenerationStale {
                expected: 1,
                observed: 2,
            }
            .as_str(),
        ];
        let mut sorted = spellings.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), spellings.len(), "every spelling is distinct");
    }

    #[test]
    fn a_matrix_with_an_unparsable_own_version_admits_nothing() {
        let broken = CompatibilityMatrix::new("garbage", project());
        assert_eq!(
            broken.decide(&plugin("1.0.0"), &project()),
            CompatibilityDecision::MalformedVersion
        );
    }
}
