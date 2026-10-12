//! The normative order of one canonical write, and the composite commit gate.
//!
//! The sequence below is the whole point of this module. Each step exists where it
//! is because a step taken later would be a step taken too late:
//!
//! | step | why here |
//! | --- | --- |
//! | refuse quarantined state | a blocked project never starts a new mutation |
//! | validate the request | `.scn`, a non-text target, an escaping path and an open resource are all decidable before anything exists |
//! | mint the **launch** transaction ID | Todo 14's lesson: an identity written after the risky action cannot prove the action did not happen |
//! | `intent_fsynced` | records pre-hashes, touched identities and `required_subproofs.process_launch_transaction_id` **before** any process exists |
//! | run the helper | supervised, contained, staging-only; the target path is never handed to it |
//! | `staged_fsynced` | records the engine-produced artifact's hash and the intended post-state |
//! | publish (CAS) | the only step that touches the target, and only against the recorded preimage |
//! | `effect_started_fsynced` | the publish returned; the outcome is not yet classified |
//! | classify + settle | commit **only** on file evidence ∧ clean-terminal linked proof |
//!
//! # The composite gate has no bypass
//!
//! [`CanonicalWriter::execute`] reaches `committed_fsynced` through exactly one
//! expression: [`transaction::classify_startup`], which requires the conjunction
//! of the transaction's own post-hash evidence **and**
//! [`ProcessLaunchProof::is_clean_terminal_process`] on the linked launch
//! transaction's proof. There is no second commit path, no force flag, and no
//! branch where matching bytes alone suffice.
//!
//! # Dry run cannot write
//!
//! [`CanonicalWriter::preview`] is a separate entry point that returns a
//! [`CanonicalDiff`] and **never calls the publisher**. The property is
//! structural rather than a checked flag: `preview` computes the diff and returns
//! it; the only code that calls `SafeRoot::cas_write` on a target lives in
//! `execute`. A mechanical test in [`crate::canonical`] asserts that `preview`'s
//! own source text contains no publish call.

use std::path::Path;

use godot_mcp_domain::safe_root::SafeRoot;

use crate::canonical::error::CanonicalError;
use crate::canonical::request::{CanonicalOperation, CanonicalRequest};
use crate::canonical::stage::{self, EngineContext, StagedArtifact, read_optional};
use crate::canonical::transaction::{
    self, Existence, FileEntry, FileJournal, FileOperation, FileProof, FileRecord,
    StartupClassification,
};
use crate::supervision::journal::{Disposition, LaunchPhase, TransactionEnvelope, hex_sha256};

/// The tool name a canonical write is journaled under.
pub const TOOL_NAME: &str = "canonical.write_resource";

/// What a dry run would change. No bytes are written to produce this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalDiff {
    /// The project-relative target.
    pub target: String,
    /// What the write does to it.
    pub operation: FileOperation,
    /// The target's current state.
    pub before: FileEntry,
    /// The state the write would produce.
    pub after: FileEntry,
    /// The engine-produced canonical bytes, exactly as they would be published.
    pub staged_bytes: Vec<u8>,
    /// The engine-minted UID the operation produced, when there is one.
    pub uid: Option<String>,
    /// The linked launch transaction the helper ran under, so a dry run is as
    /// auditable as a publishing one.
    pub launch_transaction_id: String,
}

impl CanonicalDiff {
    /// Whether the write would actually change the target's bytes.
    #[must_use]
    pub fn changes_bytes(&self) -> bool {
        self.before.sha256 != self.after.sha256 || self.before.existence != self.after.existence
    }
}

/// One completed canonical write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalWrite {
    /// The `file` transaction this write was journaled under.
    pub transaction_id: String,
    /// The linked launch transaction whose `process_launch` proof the commit
    /// additionally required.
    pub launch_transaction_id: String,
    /// The project-relative target.
    pub target: String,
    /// The published canonical bytes, produced by the engine.
    pub bytes: Vec<u8>,
    /// The engine-minted UID, when the operation produced one.
    pub uid: Option<String>,
    /// The terminal classification. `ExactPostState` is the only success.
    pub classification: StartupClassification,
    /// The typed `file` proof written at settle time.
    pub proof: FileProof,
    /// The exact argument array issued to the engine.
    pub argv: Vec<String>,
}

impl CanonicalWrite {
    /// Whether the write provably completed.
    #[must_use]
    pub fn is_committed(&self) -> bool {
        self.classification == StartupClassification::ExactPostState
    }
}

/// Drives one canonical write against one durable journal pair and one leased
/// root.
pub struct CanonicalWriter<'w> {
    file_journal: &'w dyn FileJournal,
    root: &'w SafeRoot,
    context: EngineContext<'w>,
}

impl<'w> CanonicalWriter<'w> {
    /// Bind a writer to the canonical-write journal, the leased root and the
    /// engine context.
    #[must_use]
    pub const fn new(
        file_journal: &'w dyn FileJournal,
        root: &'w SafeRoot,
        context: EngineContext<'w>,
    ) -> Self {
        Self {
            file_journal,
            root,
            context,
        }
    }

    /// The engine context this writer drives.
    #[must_use]
    pub const fn context(&self) -> &EngineContext<'w> {
        &self.context
    }

    /// Refuse to start a new mutation while any transaction is quarantined.
    ///
    /// # Errors
    /// Returns [`CanonicalError::MutationBlocked`] while a quarantined transaction
    /// awaits operator repair.
    pub fn ensure_mutation_permitted(&self) -> Result<(), CanonicalError> {
        let quarantined = self.file_journal.quarantined()?;
        if quarantined.is_empty() {
            return Ok(());
        }
        Err(CanonicalError::MutationBlocked {
            count: quarantined.len(),
            transactions: quarantined.join(", "),
        })
    }

    /// Compute what a write would change, without publishing anything.
    ///
    /// The helper still runs — that is the only way to obtain canonical bytes and
    /// an engine-minted UID — but its output stays inside the helper's own staging
    /// area and this function has no publisher to hand it to.
    ///
    /// # Errors
    /// Returns the same refusals as [`CanonicalWriter::execute`], minus every
    /// publish-related one, because nothing is published.
    pub fn preview(
        &self,
        request: &CanonicalRequest,
        file_envelope: &TransactionEnvelope,
        launch_envelope: &TransactionEnvelope,
    ) -> Result<CanonicalDiff, CanonicalError> {
        self.ensure_mutation_permitted()?;
        let kind = request.validate()?;
        let before = self.observe(&request.target, request.operation)?;
        self.refuse_missing_target(request, &before)?;

        let record = self.intent_record(request, file_envelope, launch_envelope, &before)?;
        self.file_journal.write_phase(&record)?;

        let staged = stage::stage(&self.context, self.root, request, kind, launch_envelope)?;
        let after = FileEntry::present(&request.target, request.operation.into(), &staged.bytes);

        // No publish call reaches this point, and there is no branch below that
        // could reach one: the diff is assembled and returned.
        let diff = CanonicalDiff {
            target: request.target.clone(),
            operation: request.operation.into(),
            before,
            after,
            staged_bytes: staged.bytes.clone(),
            uid: staged.uid.clone(),
            launch_transaction_id: staged.launch_transaction_id.clone(),
        };
        let mut settled = record;
        settled.staged = vec![transaction::StagedOutput::of(
            &staged.artifact_path,
            &staged.staging_id,
            &staged.bytes,
        )];
        settled.phase = LaunchPhase::StagedFsynced;
        self.file_journal.write_phase(&settled)?;
        self.file_journal.settle(
            &settled,
            Disposition::Compensated,
            "canonical_write_dry_run_published_nothing",
            None,
        )?;
        stage::discard_scratch(self.context.project_root(), &staged)?;
        Ok(diff)
    }

    /// Run one canonical write end to end.
    ///
    /// # Errors
    /// Returns [`CanonicalError::MutationBlocked`] while a quarantine is
    /// outstanding, the request's own refusals for `.scn`/non-text/escaping/open
    /// targets, [`CanonicalError::ExternallyChanged`] when the target moved under
    /// the transaction, [`CanonicalError::UidConflict`] on a conflicting UID, and
    /// [`CanonicalError::Quarantined`] when the composed proof rule cannot
    /// establish either exact state — including the case where the file bytes match
    /// but the linked `process_launch` proof does not show a clean terminal
    /// process.
    pub fn execute(
        &self,
        request: &CanonicalRequest,
        file_envelope: &TransactionEnvelope,
        launch_envelope: &TransactionEnvelope,
    ) -> Result<CanonicalWrite, CanonicalError> {
        self.ensure_mutation_permitted()?;
        let kind = request.validate()?;
        if request.dry_run {
            return Err(CanonicalError::UnusablePath {
                path: request.target.clone(),
                reason: "a dry-run request never reaches the publishing path; call preview()"
                    .to_owned(),
            });
        }

        // ---- Step 1: the pre-state, recorded before anything exists. ----------
        //
        // The preimage bytes are captured **here**, once, and carried to the
        // publish. Re-reading the target at publish time would compare it against
        // itself: an external actor that changed the file during the helper run
        // would then be silently overwritten instead of detected.
        let before_bytes = read_optional(self.root, &request.target)?;
        let before = Self::entry_of(
            &request.target,
            request.operation.into(),
            before_bytes.as_deref(),
        );
        self.refuse_missing_target(request, &before)?;
        if request.operation == CanonicalOperation::CreateResource
            && before.existence == Existence::Present
        {
            return Err(CanonicalError::ExternallyChanged {
                path: request.target.clone(),
            });
        }

        // ---- Step 2: `intent_fsynced`, binding the linked launch transaction
        //      ID before any process exists. ---------------------------------
        let mut record = self.intent_record(request, file_envelope, launch_envelope, &before)?;
        self.file_journal.write_phase(&record)?;

        // ---- Step 3: the helper produces canonical bytes into staging. --------
        let staged = stage::stage(&self.context, self.root, request, kind, launch_envelope)?;

        // ---- Step 4: `staged_fsynced`, recording the artifact and the intended
        //      post-state. ----------------------------------------------------
        let intended = FileEntry::present(&request.target, request.operation.into(), &staged.bytes);
        record.staged = vec![transaction::StagedOutput::of(
            &staged.artifact_path,
            &staged.staging_id,
            &staged.bytes,
        )];
        record.intended_post_hashes = vec![intended.clone()];
        record.phase = LaunchPhase::StagedFsynced;
        self.file_journal.write_phase(&record)?;

        // ---- Step 5: publish, atomically, against the recorded preimage. ------
        let publish = self.publish(&request.target, before_bytes.as_deref(), &staged);

        // ---- Step 6: `effect_started_fsynced` — the publish returned. ----------
        record.phase = LaunchPhase::EffectStartedFsynced;
        self.file_journal.write_phase(&record)?;

        // ---- Step 7: observe, classify, settle. -------------------------------
        let observed = vec![self.observe(&request.target, request.operation)?];
        let proof = FileProof {
            entries: observed.clone(),
        };
        let linked = staged.linked_proof();
        let classification = transaction::classify_startup(&record, &observed, linked.as_ref());
        let disposition = classification.disposition();
        self.file_journal.settle(
            &record,
            disposition,
            &classification.terminal_reason(),
            Some(&proof),
        )?;
        stage::discard_scratch(self.context.project_root(), &staged)?;

        // A publish failure is only reportable after the transaction is settled:
        // the durable classification is what a restarted process reads, so it must
        // exist before this call returns either way.
        publish?;

        match classification {
            StartupClassification::ExactPostState => Ok(CanonicalWrite {
                transaction_id: record.envelope.transaction_id.clone(),
                launch_transaction_id: staged.launch_transaction_id.clone(),
                target: request.target.clone(),
                bytes: staged.bytes.clone(),
                uid: staged.uid.clone(),
                classification,
                proof,
                argv: staged.argv.clone(),
            }),
            StartupClassification::ExactPreState => Err(CanonicalError::ExternallyChanged {
                path: request.target.clone(),
            }),
            StartupClassification::Quarantine { ref reason } => {
                if observed.iter().any(|entry| intended.matches(entry)) {
                    Err(CanonicalError::UnprovenComposite {
                        reason: reason.clone(),
                    })
                } else {
                    Err(CanonicalError::Quarantined {
                        transaction_id: record.envelope.transaction_id.clone(),
                        reason: reason.clone(),
                    })
                }
            }
        }
    }

    /// Classify one already-journaled transaction after a restart, reading the
    /// linked proof back out of the launch journal.
    ///
    /// # Errors
    /// Returns [`CanonicalError`] when the leased root or a journal cannot be read.
    pub fn reconcile(&self, record: &FileRecord) -> Result<StartupClassification, CanonicalError> {
        let mut observed = Vec::new();
        for entry in record
            .intended_post_hashes
            .iter()
            .chain(record.pre_hashes.iter())
        {
            if observed
                .iter()
                .any(|seen: &FileEntry| seen.path == entry.path)
            {
                continue;
            }
            observed.push(self.observe_path(&entry.path, entry.operation)?);
        }
        let linked = match record.required_launch_transaction_id.as_deref() {
            Some(id) => stage::read_launch_proof(self.context.launch_journal, id)?.map(|proof| {
                transaction::LinkedProof {
                    transaction_id: id.to_owned(),
                    proof,
                }
            }),
            None => None,
        };
        Ok(transaction::classify_startup(
            record,
            &observed,
            linked.as_ref(),
        ))
    }

    /// Settle one reconciled transaction with its classification.
    ///
    /// # Errors
    /// Returns [`CanonicalError::Journal`] when the durable write fails.
    pub fn settle_reconciled(
        &self,
        record: &FileRecord,
        classification: &StartupClassification,
    ) -> Result<Disposition, CanonicalError> {
        let mut entries = Vec::new();
        for entry in record
            .intended_post_hashes
            .iter()
            .chain(record.pre_hashes.iter())
        {
            if entries
                .iter()
                .any(|seen: &FileEntry| seen.path == entry.path)
            {
                continue;
            }
            entries.push(self.observe_path(&entry.path, entry.operation)?);
        }
        let proof = FileProof { entries };
        let disposition = classification.disposition();
        self.file_journal.settle(
            record,
            disposition,
            &classification.terminal_reason(),
            Some(&proof),
        )?;
        Ok(disposition)
    }

    /// Publish the engine-produced bytes onto the target, atomically, against the
    /// **recorded** preimage rather than a fresh read.
    ///
    /// `expected` is `None` for a create, which makes the publish a
    /// create-if-absent; anything already at the path fails closed. For an update
    /// it is the exact bytes observed before the helper ran, so a file changed
    /// during the run is refused rather than overwritten.
    fn publish(
        &self,
        target: &str,
        expected: Option<&[u8]>,
        staged: &StagedArtifact,
    ) -> Result<(), CanonicalError> {
        self.root
            .cas_write(Path::new(target), expected, &staged.bytes)
            .map_err(CanonicalError::from)
    }

    fn entry_of(target: &str, operation: FileOperation, bytes: Option<&[u8]>) -> FileEntry {
        match bytes {
            Some(bytes) => FileEntry {
                path: target.to_owned(),
                operation,
                existence: Existence::Present,
                sha256: Some(hex_sha256(bytes)),
            },
            None => FileEntry::absent(target, operation),
        }
    }

    fn observe(
        &self,
        target: &str,
        operation: CanonicalOperation,
    ) -> Result<FileEntry, CanonicalError> {
        self.observe_path(target, operation.into())
    }

    fn observe_path(
        &self,
        target: &str,
        operation: FileOperation,
    ) -> Result<FileEntry, CanonicalError> {
        let bytes = read_optional(self.root, target)?;
        Ok(Self::entry_of(target, operation, bytes.as_deref()))
    }

    fn refuse_missing_target(
        &self,
        request: &CanonicalRequest,
        before: &FileEntry,
    ) -> Result<(), CanonicalError> {
        if request.operation.requires_existing_target() && before.existence == Existence::Absent {
            return Err(CanonicalError::ExternallyChanged {
                path: request.target.clone(),
            });
        }
        Ok(())
    }

    fn intent_record(
        &self,
        request: &CanonicalRequest,
        file_envelope: &TransactionEnvelope,
        launch_envelope: &TransactionEnvelope,
        before: &FileEntry,
    ) -> Result<FileRecord, CanonicalError> {
        Ok(FileRecord {
            envelope: file_envelope.clone(),
            phase: LaunchPhase::IntentFsynced,
            touched: vec![
                (request.target.clone(), None),
                (
                    "project.godot".to_owned(),
                    Some(file_envelope.project.file_object.clone()),
                ),
                (
                    "project_root".to_owned(),
                    Some(file_envelope.project.root_object.clone()),
                ),
            ],
            pre_hashes: vec![before.clone()],
            staged: Vec::new(),
            intended_post_hashes: Vec::new(),
            proof: None,
            // Bound before any process exists, so a restarted process always knows
            // which launch transaction's proof the commit requires.
            required_launch_transaction_id: Some(launch_envelope.transaction_id.clone()),
        })
    }
}

impl From<CanonicalOperation> for FileOperation {
    fn from(operation: CanonicalOperation) -> Self {
        match operation {
            CanonicalOperation::CreateResource => Self::Create,
            CanonicalOperation::UpdateResource
            | CanonicalOperation::RegisterUid
            | CanonicalOperation::ApplyBatch => Self::Update,
            // A UID query publishes nothing; the entry exists only so the
            // transaction records which path was inspected.
            CanonicalOperation::QueryUid => Self::Update,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_create_is_a_create_and_a_register_is_an_update() {
        assert_eq!(
            FileOperation::from(CanonicalOperation::CreateResource),
            FileOperation::Create
        );
        assert_eq!(
            FileOperation::from(CanonicalOperation::UpdateResource),
            FileOperation::Update
        );
        assert_eq!(
            FileOperation::from(CanonicalOperation::RegisterUid),
            FileOperation::Update,
            "registering a UID re-saves an existing resource"
        );
    }

    #[test]
    fn a_diff_reports_whether_the_bytes_would_actually_change() {
        let unchanged = CanonicalDiff {
            target: "scenes/main.tscn".to_owned(),
            operation: FileOperation::Update,
            before: FileEntry::present("scenes/main.tscn", FileOperation::Update, b"same"),
            after: FileEntry::present("scenes/main.tscn", FileOperation::Update, b"same"),
            staged_bytes: b"same".to_vec(),
            uid: None,
            launch_transaction_id: "00000000-0000-4000-8000-000000000171".to_owned(),
        };
        assert!(!unchanged.changes_bytes());

        let created = CanonicalDiff {
            before: FileEntry::absent("scenes/main.tscn", FileOperation::Create),
            after: FileEntry::present("scenes/main.tscn", FileOperation::Create, b"new"),
            operation: FileOperation::Create,
            staged_bytes: b"new".to_vec(),
            ..unchanged
        };
        assert!(created.changes_bytes());
    }
}
