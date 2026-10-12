//! Running the bundled helper to produce a staged artifact, and reading the
//! linked launch proof back independently.
//!
//! Every Godot invocation here goes through Todo 14's launch protocol by way of
//! Todo 15's [`EngineRunner`]: the boundary is pre-created, the identity receipt
//! is durable before any user code runs, output is byte-capped and the deadline is
//! enforced by the supervisor. There is deliberately no second, bare
//! process-construction path in this module — a process built here would exist
//! outside the containment boundary and outside the journal. A mechanical test in
//! [`crate::canonical`] asserts that against this module's own source text.
//!
//! # The launch transaction ID is minted *before* anything is created
//!
//! Todo 14's lesson was that an identity written after the risky action cannot
//! prove the action did not happen. So the caller mints the launch envelope up
//! front and records its ID in the file transaction's `intent_fsynced` mark
//! **before** [`stage`] is ever called; [`stage`] then launches under exactly that
//! envelope through a one-shot transaction source. A restarted process therefore
//! always knows which launch transaction to look for, even if the crash happened
//! one instruction after the boundary was created.
//!
//! # The proof is read back, not returned
//!
//! [`stage`] does not hand the caller the proof the launch protocol happened to
//! construct in memory. It reads the `process_launch` proof back out of the
//! durable journal, keyed by the launch transaction ID, exactly as a restarted
//! process would. A launch that never reached `committed_fsynced` therefore has no
//! readable proof at all, which is the fail-closed direction.

use std::path::Path;
use std::time::Duration;

use godot_mcp_domain::safe_root::{SafeRoot, SafeRootError};

use crate::canonical::error::CanonicalError;
use crate::canonical::helper::{
    HelperExit, HelperResponse, HelperScript, REQUIRED_CAPABILITIES, helper_arguments,
    request_document,
};
use crate::canonical::request::{CanonicalRequest, TextResourceKind};
use crate::canonical::transaction::LinkedProof;
use crate::supervision::boundary::BoundaryProvider;
use crate::supervision::journal::{
    LaunchJournal, OperationFamily, ProcessLaunchProof, TransactionEnvelope,
};
use crate::validation::runner::{EngineBinding, EngineRunner, TransactionSource};

/// The tool name every canonical-write helper launch is journaled under.
pub const LAUNCH_TOOL_NAME: &str = "canonical.helper_launch";

/// Everything about which engine runs the helper and how it is contained.
pub struct EngineContext<'c> {
    /// Todo 6's durable journal, as the launch protocol and this module see it.
    pub launch_journal: &'c dyn LaunchJournal,
    /// The containment boundary provider.
    pub provider: &'c dyn BoundaryProvider,
    /// The engine binding, assembled from Todo 13's discovery/probe results.
    pub binding: EngineBinding,
    /// Wall-clock budget for the helper.
    pub deadline: Duration,
}

impl EngineContext<'_> {
    /// The held project root the helper runs against.
    #[must_use]
    pub fn project_root(&self) -> &Path {
        &self.binding.project_root
    }

    /// The first required flag this build does not advertise.
    ///
    /// Checked before any launch, because a Godot build silently ignores a flag it
    /// does not support: the run would exit 0 having done nothing.
    #[must_use]
    pub fn missing_capability(&self) -> Option<String> {
        REQUIRED_CAPABILITIES
            .iter()
            .find(|flag| !self.binding.capabilities.contains(**flag))
            .map(|flag| (*flag).to_owned())
    }
}

/// One helper run's staged result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedArtifact {
    /// The deterministic staging ID this request stages under.
    pub staging_id: String,
    /// Project-relative path of the staged artifact, inside the helper's own
    /// versioned staging area.
    pub artifact_path: String,
    /// The engine-produced canonical bytes. Never synthesised in Rust.
    pub bytes: Vec<u8>,
    /// The parsed helper response document.
    pub response: HelperResponse,
    /// The engine-minted `uid://…`, when the operation produced one. Never derived
    /// in Rust.
    pub uid: Option<String>,
    /// The linked launch transaction this helper ran under.
    pub launch_transaction_id: String,
    /// The `process_launch` proof read back independently from the journal.
    pub launch_proof: Option<ProcessLaunchProof>,
    /// The exact argument array issued.
    pub argv: Vec<String>,
    /// Project-relative path of the request document.
    pub request_path: String,
    /// Project-relative path of the response document.
    pub response_path: String,
}

impl StagedArtifact {
    /// The linked proof paired with its transaction ID, ready for the composite
    /// rule. `None` when the launch never became provable.
    #[must_use]
    pub fn linked_proof(&self) -> Option<LinkedProof> {
        self.launch_proof.as_ref().map(|proof| LinkedProof {
            transaction_id: self.launch_transaction_id.clone(),
            proof: proof.clone(),
        })
    }
}

/// A one-shot transaction source that hands the runner exactly the envelope the
/// caller already made durable. It cannot mint a second identity, so the launch
/// can never be journaled under a transaction the file record does not name.
struct PreMinted {
    envelope: TransactionEnvelope,
}

impl TransactionSource for PreMinted {
    fn next(&self, _tool_name: &str, _family: OperationFamily) -> TransactionEnvelope {
        self.envelope.clone()
    }
}

/// Read a leased-root file, treating "not there" as absent rather than as an
/// error.
///
/// `SafeRoot::read` reports a missing object as [`SafeRootError::StalePreimage`]
/// (its `NotFound` mapping). On a read there is no preimage to be stale against,
/// so that spelling means exactly "absent".
///
/// # Errors
/// Returns [`CanonicalError`] for a refused path, a symlink component, an identity
/// change or an I/O failure.
pub fn read_optional(root: &SafeRoot, rel: &str) -> Result<Option<Vec<u8>>, CanonicalError> {
    match root.read(Path::new(rel)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(SafeRootError::StalePreimage { .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Install the pinned helper script into the project's versioned helper directory.
///
/// The bytes are the `include_str!`-embedded, SHA-256-pinned ones. Three outcomes,
/// all fail-closed:
///
/// * absent → published with a compare-and-swap against "absent";
/// * already exactly the pinned bytes → nothing to do;
/// * present but different → [`CanonicalError::ExternallyChanged`]. The install
///   path carries the helper version, so a differing file at this exact path was
///   not written by this server version and is never overwritten.
///
/// # Errors
/// Returns [`CanonicalError`] when the directory cannot be created or the
/// compare-and-swap is refused.
pub fn install_helper(root: &SafeRoot, project_root: &Path) -> Result<String, CanonicalError> {
    let install_path = HelperScript::install_path();
    ensure_helper_directories(project_root)?;
    let pinned = HelperScript::source().as_bytes();
    match read_optional(root, &install_path)? {
        Some(existing) if existing == pinned => Ok(install_path),
        Some(_) => Err(CanonicalError::ExternallyChanged { path: install_path }),
        None => {
            root.cas_write(Path::new(&install_path), None, pinned)?;
            Ok(install_path)
        }
    }
}

/// Create the helper's own scratch directories.
///
/// This is a convenience only: it creates directories, never file contents. Every
/// byte that lands in them is published through [`SafeRoot::cas_write`], which
/// re-walks the path with symlink components forbidden and re-proves the project's
/// identity immediately before publishing — so a symlinked directory created here
/// causes the subsequent write to fail closed rather than to escape the root.
fn ensure_helper_directories(project_root: &Path) -> Result<(), CanonicalError> {
    for relative in [
        format!(
            "{}/{}",
            crate::canonical::helper::HELPER_ROOT,
            crate::canonical::helper::HELPER_VERSION
        ),
        format!(
            "{}/{}/io",
            crate::canonical::helper::HELPER_ROOT,
            crate::canonical::helper::HELPER_VERSION
        ),
        format!(
            "{}/{}/staging",
            crate::canonical::helper::HELPER_ROOT,
            crate::canonical::helper::HELPER_VERSION
        ),
    ] {
        let path = project_root.join(&relative);
        std::fs::create_dir_all(&path).map_err(|error| CanonicalError::Io {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
    }
    Ok(())
}

/// Remove one of the helper's own scratch documents, if present.
///
/// Only ever called on paths inside the helper's versioned area, which is asserted
/// here rather than assumed.
fn discard_helper_artifact(project_root: &Path, relative: &str) -> Result<(), CanonicalError> {
    if !HelperScript::is_helper_owned(relative) {
        return Err(CanonicalError::UnusablePath {
            path: relative.to_owned(),
            reason: "only the helper's own scratch area is discardable".to_owned(),
        });
    }
    let path = project_root.join(relative);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CanonicalError::Io {
            path: path.display().to_string(),
            reason: error.to_string(),
        }),
    }
}

/// Run the bundled helper once, under `launch_envelope`, and return its staged
/// artifact.
///
/// Nothing here touches the caller's target path: the helper is told which staging
/// artifact to write and refuses anything outside its own versioned prefix.
/// Publishing onto the target is [`crate::canonical::publish`]'s job.
///
/// # Errors
/// Returns [`CanonicalError::Unsupported`] when the build cannot run a script at
/// all, [`CanonicalError::Launch`] when the engine could not be launched or
/// contained, [`CanonicalError::HelperRefused`] / [`CanonicalError::UidConflict`]
/// when the helper refused, and [`CanonicalError::HelperInconclusive`] when the
/// run produced nothing interpretable — including the silent-no-op case where the
/// build ignored `--script` and exited 0.
pub fn stage(
    context: &EngineContext<'_>,
    root: &SafeRoot,
    request: &CanonicalRequest,
    kind: TextResourceKind,
    launch_envelope: &TransactionEnvelope,
) -> Result<StagedArtifact, CanonicalError> {
    if let Some(missing) = context.missing_capability() {
        return Err(CanonicalError::Unsupported {
            detection: format!(
                "the configured Godot build does not advertise {missing}; a build that ignores it \
                 exits 0 having run no script at all"
            ),
        });
    }
    let project_root = context.project_root().to_path_buf();
    install_helper(root, &project_root)?;

    let staging_id = request.staging_id();
    let request_path = HelperScript::request_path(&staging_id);
    let response_path = HelperScript::response_path(&staging_id);
    let artifact_path = HelperScript::artifact_path(&staging_id, kind.extension());

    // A stale document from an earlier attempt would let a build that ignores
    // `--script` look like a successful run, so every scratch document is removed
    // before the launch rather than trusted to be overwritten.
    for scratch in [&request_path, &response_path, &artifact_path] {
        discard_helper_artifact(&project_root, scratch)?;
    }
    let document = request_document(&request.helper_pairs(&staging_id, kind));
    root.cas_write(Path::new(&request_path), None, document.as_bytes())?;

    let source = PreMinted {
        envelope: launch_envelope.clone(),
    };
    let runner = EngineRunner::new(
        context.launch_journal,
        context.provider,
        &source,
        context.binding.clone(),
    );
    let run = runner.invoke(
        LAUNCH_TOOL_NAME,
        OperationFamily::ProjectMutation,
        helper_arguments(&project_root, &request_path, &response_path),
        context.deadline,
    )?;

    // The proof is read back out of the durable journal, keyed by the launch
    // transaction ID, exactly as a restarted process would read it. A launch that
    // never reached `committed_fsynced` has no readable proof at all.
    let launch_proof = read_launch_proof(context.launch_journal, &run.transaction_id)?;

    let response_bytes = read_optional(root, &response_path)?;
    let Some(response_bytes) = response_bytes else {
        return Err(CanonicalError::HelperInconclusive {
            reason: format!(
                "the helper wrote no response document (exit status {:?}); a build that ignores \
                 --script exits 0 having done nothing, so this run cannot be interpreted",
                run.observed.exit_code
            ),
        });
    };
    let response = HelperResponse::parse(&String::from_utf8_lossy(&response_bytes));

    match HelperExit::classify(run.observed.exit_code) {
        Some(HelperExit::Ok) if response.reports_ok() => {}
        Some(HelperExit::Refused) | Some(HelperExit::Failed) | Some(HelperExit::Usage) => {
            return Err(refusal(&response));
        }
        Some(HelperExit::Ok) => {
            return Err(CanonicalError::HelperInconclusive {
                reason: format!(
                    "the helper exited 0 but its response document reports status {:?}",
                    response.status
                ),
            });
        }
        None => {
            return Err(CanonicalError::HelperInconclusive {
                reason: format!(
                    "the helper ended with {:?} / signal {:?}, which is none of its four declared \
                     statuses",
                    run.observed.exit_code, run.observed.signal
                ),
            });
        }
    }

    let Some(bytes) = read_optional(root, &artifact_path)? else {
        return Err(CanonicalError::HelperInconclusive {
            reason: format!(
                "the helper reported success but {artifact_path} does not exist, so there are no \
                 canonical bytes to publish"
            ),
        });
    };

    Ok(StagedArtifact {
        staging_id,
        artifact_path,
        bytes,
        uid: response.uid.clone(),
        response,
        launch_transaction_id: run.transaction_id,
        launch_proof,
        argv: run.argv,
        request_path,
        response_path,
    })
}

/// Read one launch transaction's `process_launch` proof out of the durable
/// journal.
///
/// # Errors
/// Returns [`CanonicalError::Journal`] when the durable store cannot be read.
pub fn read_launch_proof(
    journal: &dyn LaunchJournal,
    transaction_id: &str,
) -> Result<Option<ProcessLaunchProof>, CanonicalError> {
    let committed = journal.committed_launches()?;
    Ok(committed
        .into_iter()
        .find(|record| record.envelope.transaction_id == transaction_id)
        .and_then(|record| record.proof))
}

/// Discard one run's scratch documents. Never touches anything outside the
/// helper's own versioned area.
///
/// # Errors
/// Returns [`CanonicalError::Io`] when a scratch document cannot be removed.
pub fn discard_scratch(
    project_root: &Path,
    artifact: &StagedArtifact,
) -> Result<(), CanonicalError> {
    for scratch in [
        &artifact.request_path,
        &artifact.response_path,
        &artifact.artifact_path,
    ] {
        discard_helper_artifact(project_root, scratch)?;
    }
    Ok(())
}

fn refusal(response: &HelperResponse) -> CanonicalError {
    let reason = response
        .reason
        .clone()
        .unwrap_or_else(|| "no reason reported".to_owned());
    if reason == "uid_conflict" {
        return CanonicalError::UidConflict {
            uid: response.uid.clone().unwrap_or_else(|| "unknown".to_owned()),
            existing: response
                .extra
                .get("existing_path")
                .cloned()
                .unwrap_or_else(|| "unknown".to_owned()),
            requested: response
                .extra
                .get("requested_path")
                .cloned()
                .unwrap_or_else(|| "unknown".to_owned()),
        };
    }
    if reason == "binary_resource" {
        return CanonicalError::BinaryResource {
            path: response
                .target
                .clone()
                .unwrap_or_else(|| "unknown".to_owned()),
        };
    }
    // A batch refusal names the operation that could not be applied and why,
    // so the caller can correct exactly that operation.
    let reason = match (response.extra.get("op_index"), response.extra.get("detail")) {
        (Some(index), Some(detail)) if index != "-1" => {
            format!("{reason} at operation {index} ({detail})")
        }
        (_, Some(detail)) if !detail.is_empty() => format!("{reason} ({detail})"),
        _ => reason,
    };
    CanonicalError::HelperRefused {
        status: if response.status.is_empty() {
            "unknown".to_owned()
        } else {
            response.status.clone()
        },
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervision::journal::{
        BoundaryMembership, Disposition, JournalError, LaunchRecord, ProjectIdentityClaim,
        ReleaseStatus, TransportMode,
    };

    struct Journal {
        committed: Vec<LaunchRecord>,
    }

    impl LaunchJournal for Journal {
        fn write_phase(&self, _record: &LaunchRecord) -> Result<(), JournalError> {
            Ok(())
        }

        fn settle(
            &self,
            _record: &LaunchRecord,
            _disposition: Disposition,
            _reason: &str,
            _proof: Option<&ProcessLaunchProof>,
        ) -> Result<(), JournalError> {
            Ok(())
        }

        fn open_launches(&self) -> Result<Vec<LaunchRecord>, JournalError> {
            Ok(Vec::new())
        }

        fn committed_launches(&self) -> Result<Vec<LaunchRecord>, JournalError> {
            Ok(self.committed.clone())
        }

        fn quarantined(&self) -> Result<Vec<String>, JournalError> {
            Ok(Vec::new())
        }
    }

    fn envelope(id: &str) -> TransactionEnvelope {
        TransactionEnvelope {
            transaction_id: id.to_owned(),
            project: ProjectIdentityClaim {
                object_identity: format!("project_{}", "c".repeat(64)),
                root_object: "unix:dev=1,ino=2".to_owned(),
                file_object: "unix:dev=1,ino=3".to_owned(),
                generation: 17,
            },
            transport_mode: TransportMode::Stdio,
            principal: None,
            session: None,
            operation_family: OperationFamily::ProjectMutation,
            tool_name: LAUNCH_TOOL_NAME.to_owned(),
            idempotency_id: "idem".to_owned(),
            canonical_request_hash: "d".repeat(64),
            canonical_request_bytes: 4,
            opened_at_unix_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn the_one_shot_source_can_only_ever_hand_out_the_pre_minted_envelope() {
        let source = PreMinted {
            envelope: envelope("00000000-0000-4000-8000-000000000171"),
        };
        let first = source.next("whatever", OperationFamily::DiagnosticProbe);
        let second = source.next("something else", OperationFamily::RuntimeExecution);
        assert_eq!(first, second);
        assert_eq!(
            first.transaction_id, "00000000-0000-4000-8000-000000000171",
            "the launch is journaled under the ID the file record already made durable"
        );
        assert_eq!(
            first.operation_family,
            OperationFamily::ProjectMutation,
            "the pre-minted family is not overridden by the call site"
        );
    }

    #[test]
    fn a_launch_that_never_committed_has_no_readable_proof() {
        let journal = Journal {
            committed: Vec::new(),
        };
        assert_eq!(
            read_launch_proof(&journal, "00000000-0000-4000-8000-000000000171")
                .expect("readable journal"),
            None,
            "an uncommitted launch is unprovable, which is the fail-closed direction"
        );
    }

    #[test]
    fn a_committed_launchs_proof_is_read_back_by_transaction_id() {
        let proof = ProcessLaunchProof {
            boundary_identity: "container:tx-171".to_owned(),
            pid: Some(11),
            start_time: Some("1".to_owned()),
            boundary_membership: BoundaryMembership::Confirmed,
            release_status: ReleaseStatus::ExitedClean,
            exit_code: Some(0),
            signal: None,
            boundary_empty_after_exit: Some(true),
        };
        let journal = Journal {
            committed: vec![LaunchRecord {
                envelope: envelope("00000000-0000-4000-8000-000000000171"),
                phase: crate::supervision::journal::LaunchPhase::CommittedFsynced,
                payloads: Vec::new(),
                proof: Some(proof.clone()),
            }],
        };
        assert_eq!(
            read_launch_proof(&journal, "00000000-0000-4000-8000-000000000171")
                .expect("readable journal"),
            Some(proof)
        );
        assert_eq!(
            read_launch_proof(&journal, "00000000-0000-4000-8000-0000000001ff")
                .expect("readable journal"),
            None,
            "a different transaction's proof is never substituted"
        );
    }

    #[test]
    fn a_uid_conflict_refusal_keeps_the_engine_minted_uid_and_both_paths() {
        let response = HelperResponse::parse(
            "existing_path=res://a.tres\nreason=uid_conflict\nrequested_path=res://b.tres\n\
             status=refused\nuid=uid://bxyz\n",
        );
        match refusal(&response) {
            CanonicalError::UidConflict {
                uid,
                existing,
                requested,
            } => {
                assert_eq!(uid, "uid://bxyz");
                assert_eq!(existing, "res://a.tres");
                assert_eq!(requested, "res://b.tres");
            }
            other => panic!("a uid_conflict must surface as a UID conflict, got {other}"),
        }
    }

    #[test]
    fn a_binary_refusal_from_the_helper_is_still_a_binary_refusal() {
        let response =
            HelperResponse::parse("reason=binary_resource\nstatus=refused\ntarget=res://a.scn\n");
        assert!(matches!(
            refusal(&response),
            CanonicalError::BinaryResource { .. }
        ));
    }

    #[test]
    fn a_batch_refusal_names_the_refused_operation_and_its_detail() {
        let response = HelperResponse::parse(
            "detail=Missing\nop_index=2\nreason=node_not_found\nstatus=refused\n",
        );
        match refusal(&response) {
            CanonicalError::HelperRefused { status, reason } => {
                assert_eq!(status, "refused");
                assert_eq!(reason, "node_not_found at operation 2 (Missing)");
            }
            other => panic!("a batch refusal must stay a helper refusal, got {other}"),
        }
        // A refusal that is not tied to one operation keeps its detail only.
        let whole = HelperResponse::parse(
            "detail=res://a.tscn\nop_index=-1\nreason=load_failed\nstatus=refused\n",
        );
        assert!(
            matches!(refusal(&whole), CanonicalError::HelperRefused { reason, .. }
                if reason == "load_failed (res://a.tscn)")
        );
    }

    #[test]
    fn only_the_helpers_own_scratch_area_is_discardable() {
        let scratch = std::env::temp_dir().join(format!(
            "godot-mcp-canonical-scratch-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&scratch).expect("scratch dir");
        let error = discard_helper_artifact(&scratch, "scenes/main.tscn")
            .expect_err("a caller path is never discarded");
        assert!(matches!(error, CanonicalError::UnusablePath { .. }));
        assert!(
            discard_helper_artifact(&scratch, "godot_mcp_helpers/2/io/absent.request").is_ok(),
            "removing an absent scratch document is not a failure"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
