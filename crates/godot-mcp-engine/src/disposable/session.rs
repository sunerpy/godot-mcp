//! One disposable execution: select isolation, copy, run, discard.
//!
//! The order below is normative and is the whole point of the module:
//!
//! | step | why it is where it is |
//! | --- | --- |
//! | select the containment boundary | an untrusted run with no **strong** boundary is refused here, before anything exists on disk |
//! | create the disposable root | only after isolation is settled |
//! | copy the project, filtered | secrets and engine cache never enter |
//! | run the engine against the **copy** | the original tree is never handed to untrusted code |
//! | discard the copy | on request, and on drop if the caller forgot |
//!
//! Isolation is decided first because a decision taken afterwards would be a
//! decision taken too late: by then a copy exists, and the temptation to "just
//! run it anyway under whatever boundary is available" is exactly the silent
//! downgrade this module refuses. [`DisposableSession::prepare`] therefore calls
//! Todo 14's own [`select_provider`], which rejects any non-strong mechanism for
//! [`TrustLevel::Untrusted`] — this module does not implement a second opinion
//! about what counts as isolation.
//!
//! # Not a code sandbox
//!
//! This is mutation containment, **not a code sandbox**. The copy governs which
//! project tree the run may change. It does not restrict what the process may
//! execute, read or reach; every run therefore carries
//! [`posture::NOT_A_SANDBOX`] verbatim. See [`crate::disposable::posture`] for
//! the enumerated list of what a copy does not contain.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use godot_mcp_domain::lease::LeaseManager;
use godot_mcp_domain::safe_root::SafeRoot;

use crate::disposable::copy::{self, CopyManifest};
use crate::disposable::error::DisposableError;
use crate::disposable::exclusion::ExclusionPolicy;
use crate::disposable::posture;
use crate::supervision::boundary::{
    BoundaryKind, BoundaryProvider, BoundaryStrength, TrustLevel, select_provider,
};
use crate::supervision::journal::LaunchJournal;
use crate::validation::import::{ImportReport, ImportRequest, import_project};
use crate::validation::runner::{EngineBinding, EngineRunner, TransactionSource};

/// Distinguishes two sessions created in the same parent directory within one
/// process. Never enters a manifest, so it cannot perturb determinism.
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One disposable execution to prepare.
pub struct DisposableRequest<'p> {
    /// The project the operator actually owns. Never mutated, never handed to
    /// untrusted code.
    pub source_root: PathBuf,
    /// Where the disposable root is created. Created only after isolation is
    /// settled, so a refused request leaves nothing behind.
    pub disposable_parent: PathBuf,
    /// Whether the project's code is trusted. `Untrusted` requires a strong
    /// boundary and is never downgraded.
    pub trust: TrustLevel,
    /// What never enters the copy.
    pub exclusions: ExclusionPolicy,
    /// Candidate containment mechanisms, in the caller's preference order.
    pub providers: Vec<&'p dyn BoundaryProvider>,
}

/// The isolation that was actually selected, recorded so a caller can prove no
/// downgrade happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectedBoundary {
    /// The trust level the selection was made for.
    pub trust: TrustLevel,
    /// The concrete mechanism.
    pub kind: BoundaryKind,
    /// Its strength. For [`TrustLevel::Untrusted`] this is always
    /// [`BoundaryStrength::Strong`], because nothing else is selectable.
    pub strength: BoundaryStrength,
}

impl SelectedBoundary {
    /// `kind/strength`, for the posture line and for diagnostics.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{}/{}", self.kind.as_str(), self.strength.as_str())
    }
}

/// One completed operation inside a disposable copy.
#[derive(Debug, Clone)]
pub struct DisposableRun {
    /// The import report, classified by [`crate::validation::outcome::classify`].
    pub import: ImportReport,
    /// The boundary the run was contained by.
    pub boundary: SelectedBoundary,
    /// The posture statement, carried on every run so no consumer can present
    /// this as a sandbox.
    pub posture: String,
}

/// A prepared disposable copy, bound to one selected boundary provider.
///
/// Dropping the session discards the copy. `discard` exists so a caller can see
/// a cleanup failure instead of losing it in a destructor.
pub struct DisposableSession<'p> {
    provider: &'p dyn BoundaryProvider,
    boundary: SelectedBoundary,
    source_root: PathBuf,
    disposable_root: PathBuf,
    project_root: PathBuf,
    safe_root: Option<SafeRoot>,
    manifest: CopyManifest,
    discarded: bool,
}

impl<'p> DisposableSession<'p> {
    /// Select isolation, then create the filtered copy.
    ///
    /// # Errors
    /// Returns [`DisposableError::IsolationUnavailable`] when no boundary of
    /// sufficient strength is available — before any directory is created, so a
    /// refused request leaves the filesystem untouched. Returns
    /// [`DisposableError::NotAProject`] when the source has no `project.godot`,
    /// [`DisposableError::SymlinkComponent`] for a symlink in the copied region,
    /// and [`DisposableError::Filesystem`] on I/O failure.
    pub fn prepare(request: &DisposableRequest<'p>) -> Result<Self, DisposableError> {
        // 1. Isolation first. Nothing exists on disk yet, so a refusal here is a
        //    refusal with no state to clean up and nothing to be tempted by.
        let provider = select_provider(request.trust, &request.providers).map_err(|error| {
            DisposableError::IsolationUnavailable {
                reason: error.to_string(),
            }
        })?;
        let boundary = SelectedBoundary {
            trust: request.trust,
            kind: provider.kind(),
            strength: provider.strength(),
        };
        debug_assert!(
            request.trust != TrustLevel::Untrusted || boundary.strength == BoundaryStrength::Strong,
            "select_provider must never return a weak boundary for untrusted code"
        );

        // 2. Only now create the disposable root. Whether the parent already
        //    existed is recorded so a refused copy can undo exactly what it
        //    created and no more.
        let parent_existed = request.disposable_parent.exists();
        let serial = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
        let disposable_root = request
            .disposable_parent
            .join(format!("disposable-{}-{serial}", std::process::id()));
        let project_name = request.source_root.file_name().map_or_else(
            || std::ffi::OsString::from("project"),
            std::ffi::OsStr::to_os_string,
        );
        let project_root = disposable_root.join(&project_name);
        std::fs::create_dir_all(&project_root).map_err(|error| DisposableError::Filesystem {
            path: project_root.display().to_string(),
            reason: error.to_string(),
        })?;

        // 3. Copy, filtered. A failure here removes the partial tree, so a
        //    refused copy is indistinguishable from one that never started.
        let manifest =
            match copy::copy_project(&request.source_root, &project_root, &request.exclusions) {
                Ok(manifest) => manifest,
                Err(error) => {
                    let _ = std::fs::remove_dir_all(&disposable_root);
                    if !parent_existed {
                        let _ = std::fs::remove_dir_all(&request.disposable_parent);
                    }
                    return Err(error);
                }
            };
        let safe_root =
            match LeaseManager::new(&disposable_root.join("lease-state")).acquire(&project_root) {
                Ok(lease) => lease.safe_root(),
                Err(error) => {
                    let _ = std::fs::remove_dir_all(&disposable_root);
                    if !parent_existed {
                        let _ = std::fs::remove_dir_all(&request.disposable_parent);
                    }
                    return Err(DisposableError::Filesystem {
                        path: project_root.display().to_string(),
                        reason: format!("could not hold the disposable project root: {error}"),
                    });
                }
            };

        Ok(Self {
            provider,
            boundary,
            source_root: request.source_root.clone(),
            disposable_root,
            project_root,
            safe_root: Some(safe_root),
            manifest,
            discarded: false,
        })
    }

    /// The isolation that was selected.
    #[must_use]
    pub const fn boundary(&self) -> &SelectedBoundary {
        &self.boundary
    }

    /// The disposable root. Everything the run writes at or below this path is
    /// contained; anything it writes elsewhere is not (see
    /// [`crate::disposable::posture`]).
    #[must_use]
    pub fn disposable_root(&self) -> &Path {
        &self.disposable_root
    }

    /// The copied project's root — the only project path an untrusted run is
    /// ever given.
    #[must_use]
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// The original tree, kept for hash comparison only.
    #[must_use]
    pub fn source_root(&self) -> &Path {
        &self.source_root
    }

    /// The deterministic record of what was copied and what was withheld.
    #[must_use]
    pub const fn manifest(&self) -> &CopyManifest {
        &self.manifest
    }

    /// The source tree's digest as observed when the copy was made.
    #[must_use]
    pub fn source_tree_sha256(&self) -> String {
        self.manifest.source_tree_sha256.clone()
    }

    /// The posture statement for this session's boundary.
    #[must_use]
    pub fn posture(&self) -> String {
        posture::statement(&self.boundary.label())
    }

    /// Import the copy's resources with a real engine, inside the copy.
    ///
    /// `binding` is rebound to the copy's project root before it is used, so a
    /// caller cannot accidentally point an untrusted run at the original tree —
    /// whatever `binding.project_root` said on the way in is discarded.
    ///
    /// # Errors
    /// Returns [`DisposableError::Launch`] only when the engine could not be
    /// launched or contained. A failed import, a silently ignored flag and a
    /// deadline all come back as reports.
    pub fn import(
        &self,
        journal: &dyn LaunchJournal,
        transactions: &dyn TransactionSource,
        binding: EngineBinding,
        request: &ImportRequest,
    ) -> Result<DisposableRun, DisposableError> {
        let binding = self.rebind(binding);
        let runner = EngineRunner::new(journal, self.provider, transactions, binding);
        let root = self
            .safe_root
            .as_ref()
            .expect("a live disposable session holds its project root");
        let import = import_project(&runner, root, request).map_err(DisposableError::Launch)?;
        Ok(DisposableRun {
            import,
            boundary: self.boundary,
            posture: self.posture(),
        })
    }

    /// The caller's binding, pointed at the copy and at this session's trust.
    fn rebind(&self, mut binding: EngineBinding) -> EngineBinding {
        binding.project_root = self.project_root.clone();
        binding.trust = self.boundary.trust;
        binding
    }

    /// Remove the copy.
    ///
    /// # Errors
    /// Returns [`DisposableError::Cleanup`] when the copy is still present
    /// afterwards. That is reported rather than swallowed: a copy that outlives
    /// its session is a fact the caller has to know.
    pub fn discard(mut self) -> Result<(), DisposableError> {
        self.discarded = true;
        drop(self.safe_root.take());
        remove_tree(&self.disposable_root)
    }
}

impl std::fmt::Debug for DisposableSession<'_> {
    /// Hand-written because deriving would require a `Debug` bound on Todo 14's
    /// public `BoundaryProvider` trait, which this module must not change.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DisposableSession")
            .field("boundary", &self.boundary.label())
            .field("source_root", &self.source_root)
            .field("disposable_root", &self.disposable_root)
            .field("project_root", &self.project_root)
            .field("copied", &self.manifest.copied.len())
            .field("excluded", &self.manifest.excluded.len())
            .field("manifest_digest", &self.manifest.digest())
            .field("discarded", &self.discarded)
            .finish()
    }
}

impl Drop for DisposableSession<'_> {
    fn drop(&mut self) {
        if self.discarded {
            return;
        }
        // A caller that forgot to discard still gets the copy removed. The
        // failure cannot be reported from here, which is exactly why `discard`
        // exists.
        drop(self.safe_root.take());
        let _ = remove_tree(&self.disposable_root);
    }
}

fn remove_tree(root: &Path) -> Result<(), DisposableError> {
    match std::fs::remove_dir_all(root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(DisposableError::Cleanup {
                path: root.display().to_string(),
                reason: error.to_string(),
            });
        }
    }
    if root.exists() {
        return Err(DisposableError::Cleanup {
            path: root.display().to_string(),
            reason: "the disposable root is still present after removal".to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervision::boundary::{BoundaryError, BoundaryIdentity, ContainmentBoundary};

    struct Fake(BoundaryKind, bool);

    impl BoundaryProvider for Fake {
        fn kind(&self) -> BoundaryKind {
            self.0
        }

        fn available(&self) -> Result<(), BoundaryError> {
            if self.1 {
                Ok(())
            } else {
                Err(BoundaryError::Unavailable(
                    "declined by the test".to_owned(),
                ))
            }
        }

        fn create(
            &self,
            _transaction_id: &str,
        ) -> Result<Box<dyn ContainmentBoundary>, BoundaryError> {
            unreachable!("these tests never launch");
        }

        fn reopen(
            &self,
            _identity: &BoundaryIdentity,
        ) -> Result<Option<Box<dyn ContainmentBoundary>>, BoundaryError> {
            Ok(None)
        }
    }

    #[test]
    fn an_untrusted_request_with_only_a_weak_provider_never_creates_anything() {
        let scratch = std::env::temp_dir().join(format!(
            "godot-mcp-disposable-unit-{}-{}",
            std::process::id(),
            SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let weak = Fake(BoundaryKind::UnixLocator, true);
        let parent = scratch.join("parent");
        let error = DisposableSession::prepare(&DisposableRequest {
            source_root: scratch.join("source"),
            disposable_parent: parent.clone(),
            trust: TrustLevel::Untrusted,
            exclusions: ExclusionPolicy::default(),
            providers: vec![&weak],
        })
        .expect_err("a weak boundary is never enough for untrusted code");
        assert!(matches!(
            error,
            DisposableError::IsolationUnavailable { .. }
        ));
        assert!(
            !parent.exists(),
            "isolation is refused before any directory is created"
        );
    }

    #[test]
    fn the_selected_boundary_label_names_both_mechanism_and_strength() {
        let selected = SelectedBoundary {
            trust: TrustLevel::Untrusted,
            kind: BoundaryKind::CgroupV2,
            strength: BoundaryStrength::Strong,
        };
        assert_eq!(selected.label(), "cgroup_v2/strong");
        assert!(posture::statement(&selected.label()).contains("cgroup_v2/strong"));
    }

    #[test]
    fn removing_an_absent_tree_is_not_a_cleanup_failure() {
        let absent = std::env::temp_dir().join(format!(
            "godot-mcp-disposable-absent-{}-{}",
            std::process::id(),
            SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        assert!(remove_tree(&absent).is_ok());
    }
}
