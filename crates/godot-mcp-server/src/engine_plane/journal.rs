use std::collections::BTreeMap;
use std::sync::Arc;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::safe_root::SafeRoot;
use godot_mcp_engine::canonical::transaction::{
    FileJournal, FileProof, FileRecord, canonical_terminal_value as canonical_file_terminal_value,
};
use godot_mcp_engine::supervision::journal::{
    Disposition, JournalError, LaunchJournal, LaunchPhase, LaunchRecord, ProcessLaunchProof,
    canonical_terminal_value as canonical_launch_terminal_value,
};
use serde_json::Value;

use crate::journal::{EffectKind, Phase, TerminalOutcome, TransactionRecord, TransactionStore};

pub(super) struct EngineJournal {
    store: Arc<TransactionStore>,
    root: Option<SafeRoot>,
}

impl EngineJournal {
    pub(super) const fn new(store: Arc<TransactionStore>) -> Self {
        Self { store, root: None }
    }

    pub(super) const fn with_root(store: Arc<TransactionStore>, root: SafeRoot) -> Self {
        Self {
            store,
            root: Some(root),
        }
    }

    pub(super) fn root(&self) -> Result<&SafeRoot, JournalError> {
        self.root.as_ref().ok_or_else(|| {
            JournalError::Payload("canonical file journal has no held safe root".to_owned())
        })
    }

    fn launch_records(
        &self,
        records: Vec<TransactionRecord>,
    ) -> Result<Vec<LaunchRecord>, JournalError> {
        records
            .into_iter()
            .filter(|record| {
                record.effect_kind == EffectKind::ProcessLaunch
                    && record
                        .staged_write_set
                        .iter()
                        .any(|entry| entry.path == LaunchRecord::INTENT_PAYLOAD)
            })
            .map(|record| self.launch_record(&record))
            .collect()
    }

    fn launch_record(&self, record: &TransactionRecord) -> Result<LaunchRecord, JournalError> {
        let value = serde_json::to_value(record)
            .map_err(|error| JournalError::Payload(error.to_string()))?;
        let mut staged = BTreeMap::new();
        for entry in &record.staged_write_set {
            let bytes = self
                .store
                .staged_bytes(&record.transaction_id, &entry.staging_id)
                .map_err(|error| JournalError::Payload(error.to_string()))?;
            staged.insert(entry.staging_id.clone(), bytes);
        }
        LaunchRecord::from_canonical_value(&value, &|id| staged.get(id).cloned())
    }

    fn persist_launch_value(
        &self,
        record: &LaunchRecord,
        value: &Value,
    ) -> Result<(), JournalError> {
        let staged = record
            .payloads
            .iter()
            .map(|payload| (payload.staging_id(), payload.bytes.clone()))
            .collect::<Vec<_>>();
        self.store
            .persist_canonical_value(value, &staged)
            .map(|_| ())
            .map_err(|error| JournalError::Durability {
                phase: record.phase,
                reason: error.to_string(),
            })
    }

    fn persist_file_value(&self, record: &FileRecord, value: &Value) -> Result<(), JournalError> {
        let root = self.root()?;
        let staged = record
            .staged
            .iter()
            .map(|entry| {
                root.read(std::path::Path::new(&entry.path))
                    .map(|bytes| (entry.staging_id.clone(), bytes))
                    .map_err(|error| JournalError::Payload(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.store
            .persist_canonical_value(value, &staged)
            .map(|_| ())
            .map_err(|error| JournalError::Durability {
                phase: record.phase,
                reason: error.to_string(),
            })
    }
}

impl LaunchJournal for EngineJournal {
    fn write_phase(&self, record: &LaunchRecord) -> Result<(), JournalError> {
        let mut value = record.to_canonical_value()?;
        if record.phase == LaunchPhase::CommittedFsynced {
            insert_terminal_outcome(&mut value, Disposition::Committed)?;
        }
        self.persist_launch_value(record, &value)
    }

    fn settle(
        &self,
        record: &LaunchRecord,
        disposition: Disposition,
        reason: &str,
        proof: Option<&ProcessLaunchProof>,
    ) -> Result<(), JournalError> {
        let mut value = canonical_launch_terminal_value(record, disposition, reason, proof)?;
        insert_terminal_outcome(&mut value, disposition)?;
        self.persist_launch_value(record, &value)
    }

    fn open_launches(&self) -> Result<Vec<LaunchRecord>, JournalError> {
        let records = self
            .store
            .open_transactions()
            .map_err(|error| JournalError::Payload(error.to_string()))?;
        self.launch_records(records)
    }

    fn committed_launches(&self) -> Result<Vec<LaunchRecord>, JournalError> {
        let records = self
            .store
            .all_transactions()
            .map_err(|error| JournalError::Payload(error.to_string()))?
            .into_iter()
            .filter(|record| record.phase == Phase::CommittedFsynced)
            .collect();
        self.launch_records(records)
    }

    fn quarantined(&self) -> Result<Vec<String>, JournalError> {
        self.store
            .quarantined_transactions()
            .map_err(|error| JournalError::Payload(error.to_string()))
    }
}

impl FileJournal for EngineJournal {
    fn write_phase(&self, record: &FileRecord) -> Result<(), JournalError> {
        self.persist_file_value(record, &record.to_canonical_value()?)
    }

    fn settle(
        &self,
        record: &FileRecord,
        disposition: Disposition,
        reason: &str,
        proof: Option<&FileProof>,
    ) -> Result<(), JournalError> {
        let mut value = canonical_file_terminal_value(record, disposition, reason, proof)?;
        insert_terminal_outcome(&mut value, disposition)?;
        self.persist_file_value(record, &value)
    }

    fn open_transactions(&self) -> Result<Vec<FileRecord>, JournalError> {
        let records = self
            .store
            .open_transactions()
            .map_err(|error| JournalError::Payload(error.to_string()))?;
        records
            .into_iter()
            .filter(|record| record.effect_kind == EffectKind::File)
            .map(|record| {
                let value = serde_json::to_value(record)
                    .map_err(|error| JournalError::Payload(error.to_string()))?;
                FileRecord::from_canonical_value(&value)
            })
            .collect()
    }

    fn quarantined(&self) -> Result<Vec<String>, JournalError> {
        LaunchJournal::quarantined(self)
    }
}

fn insert_terminal_outcome(
    value: &mut Value,
    disposition: Disposition,
) -> Result<(), JournalError> {
    let outcome = match disposition {
        Disposition::Committed => TerminalOutcome::Succeeded,
        Disposition::Compensated => TerminalOutcome::failed(ErrorKind::Unavailable),
        Disposition::Quarantined => TerminalOutcome::failed(ErrorKind::Uncertain),
    };
    value
        .as_object_mut()
        .ok_or_else(|| JournalError::Payload("canonical record is not an object".to_owned()))?
        .insert(
            "terminal_outcome".to_owned(),
            serde_json::to_value(outcome)
                .map_err(|error| JournalError::Payload(error.to_string()))?,
        );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use godot_mcp_engine::supervision::journal::{
        BoundaryMembership, LaunchJournal, LaunchPayload, LaunchPhase, LaunchRecord,
        OperationFamily, ProcessLaunchProof, ProjectIdentityClaim, ReleaseStatus,
        TransactionEnvelope, TransportMode,
    };
    use serde_json::json;

    use crate::journal::TransactionStore;

    use super::EngineJournal;

    fn envelope() -> TransactionEnvelope {
        TransactionEnvelope {
            transaction_id: "00000000-0000-4000-8000-000000001401".to_owned(),
            project: ProjectIdentityClaim {
                object_identity: format!("project_{}", "a".repeat(64)),
                root_object: "unix:dev=1,ino=2".to_owned(),
                file_object: "unix:dev=1,ino=3".to_owned(),
                generation: 7,
            },
            transport_mode: TransportMode::Stdio,
            principal: None,
            session: Some("stdio".to_owned()),
            operation_family: OperationFamily::RuntimeExecution,
            tool_name: "project.run".to_owned(),
            idempotency_id: "idem-engine-journal".to_owned(),
            canonical_request_hash: "b".repeat(64),
            canonical_request_bytes: 42,
            opened_at_unix_ms: 1_700_000_000_000,
        }
    }

    fn payload(name: &str, identity: &str) -> LaunchPayload {
        let mut facts = BTreeMap::new();
        facts.insert("boundary_kind".to_owned(), json!("container_adapter"));
        facts.insert("boundary_identity".to_owned(), json!(identity));
        facts.insert("boundary_strength".to_owned(), json!("strong"));
        LaunchPayload::new(name, &facts, Vec::new())
    }

    #[test]
    fn launch_phases_round_trip_through_the_single_transaction_store() {
        // Given: the production store adapter and two cumulative launch payloads.
        let temp = tempfile::tempdir().expect("temporary journal");
        let store = Arc::new(TransactionStore::open(temp.path()).expect("open journal"));
        let journal = EngineJournal::new(store);
        let intent = payload(LaunchRecord::INTENT_PAYLOAD, "boundary:1401");
        let receipt = payload(LaunchRecord::STAGED_PAYLOAD, "boundary:1401");

        // When: the launch protocol advances from intent to staged.
        journal
            .write_phase(&LaunchRecord {
                envelope: envelope(),
                phase: LaunchPhase::IntentFsynced,
                payloads: vec![intent.clone()],
                proof: None,
            })
            .expect("write intent");
        journal
            .write_phase(&LaunchRecord {
                envelope: envelope(),
                phase: LaunchPhase::StagedFsynced,
                payloads: vec![intent, receipt],
                proof: None,
            })
            .expect("write staged");

        // Then: the same store reconstructs the staged launch and both payloads.
        let open = journal.open_launches().expect("read open launches");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].phase, LaunchPhase::StagedFsynced);
        assert_eq!(open[0].payloads.len(), 2);
    }

    #[test]
    fn committed_launch_phase_carries_the_required_terminal_outcome() {
        let temp = tempfile::tempdir().expect("temporary journal");
        let store = Arc::new(TransactionStore::open(temp.path()).expect("open journal"));
        let journal = EngineJournal::new(Arc::clone(&store));
        let intent = payload(LaunchRecord::INTENT_PAYLOAD, "boundary:1401");
        let staged = payload(LaunchRecord::STAGED_PAYLOAD, "boundary:1401");
        let receipt = payload(LaunchRecord::RECEIPT_PAYLOAD, "boundary:1401");
        let proof = ProcessLaunchProof {
            boundary_identity: "boundary:1401".to_owned(),
            pid: Some(1401),
            start_time: Some("17".to_owned()),
            boundary_membership: BoundaryMembership::Confirmed,
            release_status: ReleaseStatus::ExitedClean,
            exit_code: Some(0),
            signal: None,
            boundary_empty_after_exit: Some(true),
        };

        for (phase, payloads, proof) in [
            (LaunchPhase::IntentFsynced, vec![intent.clone()], None),
            (
                LaunchPhase::StagedFsynced,
                vec![intent.clone(), staged.clone()],
                None,
            ),
            (
                LaunchPhase::EffectStartedFsynced,
                vec![intent.clone(), staged.clone(), receipt.clone()],
                Some(proof.clone()),
            ),
            (
                LaunchPhase::CommittedFsynced,
                vec![intent, staged, receipt],
                Some(proof),
            ),
        ] {
            journal
                .write_phase(&LaunchRecord {
                    envelope: envelope(),
                    phase,
                    payloads,
                    proof,
                })
                .expect("write launch phase");
        }

        let records = store.all_transactions().expect("read transaction");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].phase, crate::journal::Phase::CommittedFsynced);
        assert_eq!(
            records[0].terminal_outcome,
            Some(crate::journal::TerminalOutcome::Succeeded)
        );
    }
}
