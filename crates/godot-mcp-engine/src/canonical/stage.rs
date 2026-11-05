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
use crate::canonical::request::{
    BatchesRequest, CanonicalOperation, CanonicalRequest, TextResourceKind,
};
use crate::canonical::transaction::LinkedProof;
use crate::supervision::boundary::BoundaryProvider;
use crate::supervision::journal::{
    LaunchJournal, OperationFamily, ProcessLaunchProof, TransactionEnvelope, hex_sha256,
};
use crate::validation::diagnostic::{Diagnostic, parse_engine_diagnostics};
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
    /// What each `call_method` of the batch returned, in batch order.
    pub results: Vec<serde_json::Value>,
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

    let results = response
        .call_results()
        .map_err(|reason| CanonicalError::HelperInconclusive {
            reason: format!("the helper's response cannot be read: {reason}"),
        })?;

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
        results,
        response,
        launch_transaction_id: run.transaction_id,
        launch_proof,
        argv: run.argv,
        request_path,
        response_path,
    })
}

/// One file a multi-file batch staged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedTarget {
    /// The project-relative target.
    pub target: String,
    /// Project-relative path of the staged artifact.
    pub artifact_path: String,
    /// The engine-produced canonical bytes. Never synthesised in Rust.
    pub bytes: Vec<u8>,
    /// The target's `uid://…`, carried into the artifact by the engine.
    pub uid: Option<String>,
    /// What each `call_method` of this file's batch returned, in batch order.
    pub results: Vec<serde_json::Value>,
}

/// One helper run's staged files for a multi-file batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedBatches {
    /// The deterministic staging ID of the request.
    pub staging_id: String,
    /// Every staged file, in request order.
    pub targets: Vec<StagedTarget>,
    /// The linked launch transaction the helper ran under.
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

impl StagedBatches {
    /// Every scratch document of the run.
    fn scratch(&self) -> Vec<&String> {
        let mut scratch = vec![&self.request_path, &self.response_path];
        scratch.extend(self.targets.iter().map(|target| &target.artifact_path));
        scratch
    }
}

/// Run the bundled helper once, under `launch_envelope`, applying every batch
/// of `request`, and return each staged file. `kinds` is what
/// [`BatchesRequest::validate`] classified.
///
/// Nothing here touches a target. A refused or uninterpretable run leaves no
/// scratch document behind; a staged run's documents are the caller's to
/// discard with [`discard_batches_scratch`] once published or dropped.
///
/// # Errors
/// The errors of [`stage`]; a refusal names the file it concerns.
pub fn stage_batches(
    context: &EngineContext<'_>,
    root: &SafeRoot,
    request: &BatchesRequest,
    kinds: &[TextResourceKind],
    launch_envelope: &TransactionEnvelope,
) -> Result<StagedBatches, CanonicalError> {
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
    let artifacts = BatchesRequest::artifact_paths(&staging_id, kinds);
    let scratch: Vec<String> = [request_path.clone(), response_path.clone()]
        .into_iter()
        .chain(artifacts.iter().cloned())
        .collect();
    // As in `stage`: a stale document would make a build that ignores
    // `--script` look like a successful run.
    for document in &scratch {
        discard_helper_artifact(&project_root, document)?;
    }
    let staged = stage_batches_run(
        context,
        root,
        request,
        kinds,
        launch_envelope,
        &staging_id,
        &artifacts,
    );
    match staged {
        Ok(staged) => Ok(staged),
        Err(error) => {
            for document in &scratch {
                discard_helper_artifact(&project_root, document)?;
            }
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn stage_batches_run(
    context: &EngineContext<'_>,
    root: &SafeRoot,
    request: &BatchesRequest,
    kinds: &[TextResourceKind],
    launch_envelope: &TransactionEnvelope,
    staging_id: &str,
    artifacts: &[String],
) -> Result<StagedBatches, CanonicalError> {
    let project_root = context.project_root().to_path_buf();
    let request_path = HelperScript::request_path(staging_id);
    let response_path = HelperScript::response_path(staging_id);
    let document = request_document(&request.helper_pairs(staging_id, kinds));
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
    let launch_proof = read_launch_proof(context.launch_journal, &run.transaction_id)?;

    let Some(response_bytes) = read_optional(root, &response_path)? else {
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
        Some(HelperExit::Refused | HelperExit::Failed | HelperExit::Usage) => {
            return Err(batches_refusal(&response));
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
    let answered = staged_targets(&response, request, artifacts)?;
    let mut targets = Vec::with_capacity(answered.len());
    for (target, StagedAnswer { uid, results }) in request.targets.iter().zip(answered) {
        let index = targets.len();
        let artifact_path = artifacts[index].clone();
        let Some(bytes) = read_optional(root, &artifact_path)? else {
            return Err(CanonicalError::HelperInconclusive {
                reason: format!(
                    "the helper reported success but {artifact_path} does not exist, so there are \
                     no canonical bytes to publish for {}",
                    target.target
                ),
            });
        };
        targets.push(StagedTarget {
            target: target.target.clone(),
            artifact_path,
            bytes,
            uid,
            results,
        });
    }
    Ok(StagedBatches {
        staging_id: staging_id.to_owned(),
        targets,
        launch_transaction_id: run.transaction_id,
        launch_proof,
        argv: run.argv,
        request_path,
        response_path,
    })
}

/// What the helper answered for one file of a multi-file run.
#[derive(Debug, Clone, PartialEq)]
pub struct StagedAnswer {
    /// The UID the staged file carries; `None` when the helper named none.
    pub uid: Option<String>,
    /// The call results of the file's batch.
    pub results: Vec<serde_json::Value>,
}

/// The UID and call results the helper answered for each file, checked to be
/// exactly the files requested, in order, each with the artifact it was given.
///
/// # Errors
/// Returns [`CanonicalError::HelperInconclusive`] for an answer that is not
/// that list.
pub fn staged_targets(
    response: &HelperResponse,
    request: &BatchesRequest,
    artifacts: &[String],
) -> Result<Vec<StagedAnswer>, CanonicalError> {
    let inconclusive = |reason: String| CanonicalError::HelperInconclusive { reason };
    let text = response
        .extra
        .get("staged")
        .ok_or_else(|| inconclusive("the helper's answer lists no staged file".to_owned()))?;
    let serde_json::Value::Array(listed) = serde_json::from_str::<serde_json::Value>(text)
        .map_err(|error| inconclusive(format!("the helper's `staged` is not JSON: {error}")))?
    else {
        return Err(inconclusive(
            "the helper's `staged` is not a list".to_owned(),
        ));
    };
    if listed.len() != request.targets.len() {
        return Err(inconclusive(format!(
            "the helper staged {} files of the {} requested",
            listed.len(),
            request.targets.len()
        )));
    }
    let mut answered = Vec::with_capacity(listed.len());
    for (index, entry) in listed.iter().enumerate() {
        let field = |key: &str| entry.get(key).and_then(serde_json::Value::as_str);
        if field("target") != Some(request.targets[index].target.as_str())
            || field("artifact") != artifacts.get(index).map(String::as_str)
        {
            return Err(inconclusive(format!(
                "the helper's staged file {index} is not {} staged at its own artifact",
                request.targets[index].target
            )));
        }
        let results = match entry.get("results") {
            Some(serde_json::Value::Array(results))
                if results.iter().all(serde_json::Value::is_object) =>
            {
                results.clone()
            }
            _ => {
                return Err(inconclusive(format!(
                    "the helper's results for {} are not a list of objects",
                    request.targets[index].target
                )));
            }
        };
        let uid = field("uid")
            .filter(|uid| !uid.is_empty())
            .map(str::to_owned);
        answered.push(StagedAnswer { uid, results });
    }
    Ok(answered)
}

/// A refusal of a multi-file run, naming the file it concerns.
fn batches_refusal(response: &HelperResponse) -> CanonicalError {
    let file = response
        .target
        .as_deref()
        .map(|target| target.strip_prefix("res://").unwrap_or(target).to_owned());
    match (refusal(response), file) {
        (CanonicalError::HelperRefused { status, reason }, Some(file)) => {
            CanonicalError::HelperRefused {
                status,
                reason: format!("{reason} in {file}"),
            }
        }
        (error, _) => error,
    }
}

/// Discard one multi-file run's scratch documents. Never touches anything
/// outside the helper's own versioned area.
///
/// # Errors
/// Returns [`CanonicalError::Io`] when a scratch document cannot be removed.
pub fn discard_batches_scratch(
    project_root: &Path,
    staged: &StagedBatches,
) -> Result<(), CanonicalError> {
    for document in staged.scratch() {
        discard_helper_artifact(project_root, document)?;
    }
    Ok(())
}

/// What one call of a read answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadCall {
    /// The helper's entry for the call: its `index` and `method`, and either the
    /// value's `literal` in Godot's text spelling or an `error_kind` and
    /// `detail` saying why the call was not made.
    pub result: serde_json::Value,
    /// The engine errors and warnings captured after the call's begin marker
    /// and before its end marker.
    pub diagnostics: Vec<Diagnostic>,
    /// Whether both markers were captured, so `diagnostics` holds everything
    /// the call raised. When not, more may have been cut off by the output
    /// cap, or the helper never reached or finished the call.
    pub diagnostics_complete: bool,
}

/// One helper read's answer. Nothing was staged or published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadAnswer {
    /// Every call, in call order.
    pub calls: Vec<ReadCall>,
    /// What the engine reported outside every call: loading the target, or
    /// starting the project's autoloads.
    pub diagnostics: Vec<Diagnostic>,
    /// Whether either captured stream hit its byte cap.
    pub output_truncated: bool,
    /// The launch transaction the helper ran under.
    pub launch_transaction_id: String,
    /// The `process_launch` proof read back independently from the journal.
    pub launch_proof: Option<ProcessLaunchProof>,
    /// The exact argument array issued.
    pub argv: Vec<String>,
}

/// The stderr line the helper prints before call `index` of a read.
fn read_call_begin(index: usize) -> String {
    format!("godot_mcp_read_call_begin {index}")
}

/// The stderr line the helper prints after call `index` of a read.
fn read_call_end(index: usize) -> String {
    format!("godot_mcp_read_call_end {index}")
}

/// Split a read's captured stderr into what each of `calls` calls raised and
/// what was raised outside every call.
///
/// A call is attributed the lines after its own begin marker and before its
/// end marker, and is complete only when both were captured in order. A call
/// whose end marker is missing keeps what was captured after its begin, and
/// every call after it is attributed nothing: the output stopped there.
#[must_use]
pub fn attribute_read_diagnostics(
    stderr: &str,
    calls: usize,
) -> (Vec<(Vec<Diagnostic>, bool)>, Vec<Diagnostic>) {
    let lines: Vec<&str> = stderr
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .collect();
    let mut attributed = Vec::with_capacity(calls);
    let mut inside = vec![false; lines.len()];
    let mut cursor = 0;
    for index in 0..calls {
        let begin = read_call_begin(index);
        let end = read_call_end(index);
        let Some(start) = (cursor..lines.len()).find(|&at| lines[at] == begin) else {
            attributed.push((Vec::new(), false));
            continue;
        };
        let (stop, complete) = match (start + 1..lines.len()).find(|&at| lines[at] == end) {
            Some(stop) => (stop, true),
            None => (lines.len(), false),
        };
        let last = if complete { stop } else { lines.len() - 1 };
        for flag in &mut inside[start..=last] {
            *flag = true;
        }
        attributed.push((
            parse_engine_diagnostics(&lines[start + 1..stop].join("\n")),
            complete,
        ));
        cursor = last + 1;
    }
    let outside: Vec<&str> = lines
        .iter()
        .zip(&inside)
        .filter(|(_, inside)| !**inside)
        .map(|(line, _)| *line)
        .collect();
    (attributed, parse_engine_diagnostics(&outside.join("\n")))
}

/// Run the bundled helper once, under `launch_envelope`, to make the reading
/// calls of a [`CanonicalOperation::ReadCalls`] request, and answer what each
/// returned.
///
/// The helper changes nothing and stages nothing. Its scratch documents are
/// named after the launch transaction, so two identical reads never share
/// them, and are removed again before this returns.
///
/// # Errors
/// Returns [`CanonicalError::Unsupported`] when the build cannot run a script at
/// all, [`CanonicalError::Launch`] when the engine could not be launched or
/// contained, [`CanonicalError::HelperRefused`] when the helper refused the
/// read as a whole, and [`CanonicalError::HelperInconclusive`] when the run
/// produced nothing interpretable.
pub fn read(
    context: &EngineContext<'_>,
    root: &SafeRoot,
    request: &CanonicalRequest,
    kind: TextResourceKind,
    launch_envelope: &TransactionEnvelope,
) -> Result<ReadAnswer, CanonicalError> {
    if request.operation != CanonicalOperation::ReadCalls {
        return Err(CanonicalError::UnusablePath {
            path: request.target.clone(),
            reason: "only a read is answered without staging".to_owned(),
        });
    }
    let calls = request
        .batch
        .as_deref()
        .map(serde_json::from_str::<Vec<serde_json::Value>>)
        .and_then(Result::ok)
        .map(|calls| calls.len())
        .ok_or_else(|| CanonicalError::UnusablePath {
            path: request.target.clone(),
            reason: "a read's calls are a JSON array".to_owned(),
        })?;
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

    let scratch_id: String = hex_sha256(
        format!(
            "{}:{}",
            request.canonical_request_hash(),
            launch_envelope.transaction_id
        )
        .as_bytes(),
    )
    .chars()
    .take(16)
    .collect();
    let request_path = HelperScript::request_path(&scratch_id);
    let response_path = HelperScript::response_path(&scratch_id);
    let artifact_path = HelperScript::artifact_path(&scratch_id, kind.extension());
    let scratch = [&request_path, &response_path, &artifact_path];
    for document in scratch {
        discard_helper_artifact(&project_root, document)?;
    }
    let document = request_document(&request.helper_pairs(&scratch_id, kind));
    root.cas_write(Path::new(&request_path), None, document.as_bytes())?;

    let answered = read_answer(
        context,
        root,
        &project_root,
        launch_envelope,
        &request_path,
        &response_path,
        calls,
    );
    let cleaned = scratch
        .into_iter()
        .try_for_each(|document| discard_helper_artifact(&project_root, document));
    let answer = answered?;
    cleaned?;
    Ok(answer)
}

fn read_answer(
    context: &EngineContext<'_>,
    root: &SafeRoot,
    project_root: &Path,
    launch_envelope: &TransactionEnvelope,
    request_path: &str,
    response_path: &str,
    calls: usize,
) -> Result<ReadAnswer, CanonicalError> {
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
        OperationFamily::DiagnosticProbe,
        helper_arguments(project_root, request_path, response_path),
        context.deadline,
    )?;
    let launch_proof = read_launch_proof(context.launch_journal, &run.transaction_id)?;

    let Some(response_bytes) = read_optional(root, response_path)? else {
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
    let results = response
        .call_results()
        .map_err(|reason| CanonicalError::HelperInconclusive {
            reason: format!("the helper's response cannot be read: {reason}"),
        })?;
    if results.len() != calls
        || results.iter().enumerate().any(|(index, result)| {
            result.get("index").and_then(serde_json::Value::as_u64) != Some(index as u64)
        })
    {
        return Err(CanonicalError::HelperInconclusive {
            reason: format!(
                "the helper answered {} of the read's {calls} calls, or out of order",
                results.len()
            ),
        });
    }
    let (attributed, diagnostics) = attribute_read_diagnostics(&run.stderr, calls);
    Ok(ReadAnswer {
        calls: results
            .into_iter()
            .zip(attributed)
            .map(|(result, (diagnostics, complete))| ReadCall {
                result,
                diagnostics,
                diagnostics_complete: complete,
            })
            .collect(),
        diagnostics,
        output_truncated: run.truncated,
        launch_transaction_id: run.transaction_id,
        launch_proof,
        argv: run.argv,
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
    fn each_read_call_is_attributed_only_the_lines_between_its_markers() {
        let stderr = "WARNING: the autoload starts\n   at: _ready (res://boot.gd:3)\n\
                      godot_mcp_read_call_begin 0\n\
                      godot_mcp_read_call_end 0\n\
                      godot_mcp_read_call_begin 1\n\
                      ERROR: Index p_track = 7 is out of bounds (tracks.size() = 1).\n   \
                      at: track_get_key_count (scene/resources/animation.cpp:2843)\n\
                      godot_mcp_read_call_end 1\n\
                      godot_mcp_read_call_begin 2\n\
                      ERROR: cut off by the cap\n";
        let (calls, outside) = attribute_read_diagnostics(stderr, 4);
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0], (Vec::new(), true), "a clean call raised nothing");
        let (raised, complete) = &calls[1];
        assert!(complete, "both markers of call 1 were captured");
        assert_eq!(raised.len(), 1);
        assert!(raised[0].message.contains("out of bounds"));
        let (cut, complete) = &calls[2];
        assert!(
            !complete,
            "a call whose end marker is missing cannot say it raised nothing more"
        );
        assert_eq!(cut.len(), 1, "what was captured of it is kept");
        assert_eq!(
            calls[3],
            (Vec::new(), false),
            "nothing after a cut-off call was captured"
        );
        assert_eq!(outside.len(), 1, "the autoload warning is nobody's call");
        assert!(outside[0].message.contains("autoload"));
    }

    #[test]
    fn a_read_without_markers_attributes_nothing() {
        let (calls, outside) = attribute_read_diagnostics("ERROR: before anything\n", 2);
        assert_eq!(calls, vec![(Vec::new(), false), (Vec::new(), false)]);
        assert_eq!(outside.len(), 1);
        let (calls, _) = attribute_read_diagnostics(
            "godot_mcp_read_call_end 0\ngodot_mcp_read_call_begin 0\n",
            1,
        );
        assert_eq!(
            calls,
            vec![(Vec::new(), false)],
            "an end before its begin brackets nothing"
        );
        let (calls, outside) = attribute_read_diagnostics("", 1);
        assert_eq!(calls, vec![(Vec::new(), false)]);
        assert!(outside.is_empty());
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
            discard_helper_artifact(&scratch, "godot_mcp_helpers/4/io/absent.request").is_ok(),
            "removing an absent scratch document is not a failure"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    fn two_files() -> (BatchesRequest, Vec<String>) {
        let request = BatchesRequest {
            targets: vec![
                crate::canonical::request::BatchTarget {
                    target: "scenes/a.tscn".to_owned(),
                    batch: "[]".to_owned(),
                    handles: None,
                },
                crate::canonical::request::BatchTarget {
                    target: "themes/b.tres".to_owned(),
                    batch: "[]".to_owned(),
                    handles: None,
                },
            ],
            open_resources: Vec::new(),
            dry_run: false,
        };
        let artifacts = BatchesRequest::artifact_paths(
            "0123456789abcdef",
            &[TextResourceKind::Scene, TextResourceKind::Resource],
        );
        (request, artifacts)
    }

    fn answer(staged: &serde_json::Value) -> HelperResponse {
        HelperResponse::parse(&format!(
            "status=ok\nstaged={}\n",
            serde_json::to_string(staged).expect("JSON")
        ))
    }

    #[test]
    fn a_multi_file_answer_must_list_every_requested_file_in_order() {
        let (request, artifacts) = two_files();
        let good = serde_json::json!([
            {"target": "scenes/a.tscn", "artifact": artifacts[0], "uid": "uid://abc", "results": []},
            {"target": "themes/b.tres", "artifact": artifacts[1], "uid": "",
             "results": [{"index": 0, "method": "set_color"}]},
        ]);
        let answered = staged_targets(&answer(&good), &request, &artifacts).expect("a good answer");
        assert_eq!(answered[0].uid.as_deref(), Some("uid://abc"));
        assert_eq!(answered[1].uid, None, "an empty UID is no UID");
        assert_eq!(answered[1].results.len(), 1);

        let short = serde_json::json!([good[0].clone()]);
        let swapped = serde_json::json!([good[1].clone(), good[0].clone()]);
        let mut elsewhere = good.clone();
        elsewhere[1]["artifact"] = serde_json::json!("godot_mcp_helpers/4/staging/other.tres");
        let mut results = good.clone();
        results[0]["results"] = serde_json::json!("none");
        for wrong in [short, swapped, elsewhere, results] {
            assert!(
                matches!(
                    staged_targets(&answer(&wrong), &request, &artifacts),
                    Err(CanonicalError::HelperInconclusive { .. })
                ),
                "{wrong}"
            );
        }
        assert!(matches!(
            staged_targets(&HelperResponse::parse("status=ok\n"), &request, &artifacts),
            Err(CanonicalError::HelperInconclusive { .. })
        ));
    }

    #[test]
    fn a_multi_file_refusal_names_its_file() {
        let response = HelperResponse::parse(
            "status=refused\nreason=node_not_found\nop_index=2\ndetail=Gun\n\
             target=res://themes/b.tres\ntarget_index=1\n",
        );
        let error = batches_refusal(&response);
        assert_eq!(
            error.to_string(),
            "the bundled canonical-write helper reported refused: node_not_found at operation 2 \
             (Gun) in themes/b.tres"
        );
    }
}
