//! The durable SQLite transaction store.
//!
//! One SQLite database per project state directory holds every transaction
//! record and its staged bytes. Durability is not delegated to defaults:
//!
//! * `journal_mode = WAL` with `synchronous = FULL`, so every committed SQLite
//!   transaction is fsynced to the WAL before the call returns.
//! * After each phase transition the WAL is checkpointed (`wal_checkpoint(FULL)`)
//!   and the containing directory is fsynced, so the record survives process
//!   termination *and* a directory-entry loss.
//! * Staged bytes live in the same database and are therefore covered by the same
//!   fsync, so a `staged_fsynced` record can never reference bytes that were not
//!   durably written. Partial staged bytes are detected by re-hashing.
//!
//! Only legal phase transitions are accepted. An illegal transition is rejected
//! with `conflict` and leaves the stored record byte-identical.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use godot_mcp_bridge::protocol::mutation::{QuarantineHandoff, QuarantineSink};
use godot_mcp_domain::ErrorKind;
use godot_mcp_engine::supervision::journal::{JournalError, LaunchPhase};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use super::record::{
    EffectKind, Phase, PhaseStep, RecordError, SCHEMA_VERSION, TerminalOutcome, TransactionRecord,
    TypedProof, hex_sha256,
};

const SQL_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS journal_meta (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS transactions (
    transaction_id TEXT PRIMARY KEY NOT NULL,
    phase          TEXT NOT NULL,
    sequence       INTEGER NOT NULL,
    record_json    TEXT NOT NULL,
    record_sha256  TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS staged_bytes (
    transaction_id TEXT NOT NULL,
    staging_id     TEXT NOT NULL,
    payload        BLOB NOT NULL,
    PRIMARY KEY (transaction_id, staging_id)
) STRICT;
CREATE TABLE IF NOT EXISTS audit_log (
    sequence       INTEGER PRIMARY KEY AUTOINCREMENT,
    transaction_id TEXT NOT NULL,
    event          TEXT NOT NULL,
    detail         TEXT NOT NULL
) STRICT;
";

/// A durable-store failure.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StoreError {
    /// The store could not be opened or a durability pragma was refused.
    #[error("durable transaction store is unavailable: {0}")]
    Unavailable(String),
    /// A record failed canonical validation on the way in or out.
    #[error(transparent)]
    Record(#[from] RecordError),
    /// The named transaction does not exist.
    #[error("transaction {0} is not journaled")]
    NotFound(String),
    /// A transition the phase machine does not allow.
    #[error("illegal phase transition {from} -> {to} for transaction {transaction_id}")]
    IllegalTransition {
        transaction_id: String,
        from: Phase,
        to: Phase,
    },
    /// A second record with the same transaction ID.
    #[error("transaction {0} is already journaled")]
    DuplicateTransaction(String),
    /// Staged bytes are missing or do not match their recorded hash/length.
    #[error("staged bytes for transaction {transaction_id} are not intact: {reason}")]
    StagedBytesCorrupt {
        transaction_id: String,
        reason: String,
    },
    /// A deliberately injected durability fault. The variant itself is
    /// `test-hooks`-gated, so a default/release build contains neither the fault
    /// hook nor its error case.
    #[cfg(feature = "test-hooks")]
    #[error("injected durability fault at {0}")]
    InjectedFault(String),
    /// SQLite reported a failure.
    #[error("durable transaction store failed: {0}")]
    Sqlite(String),
}

impl StoreError {
    /// Map to the stable domain error taxonomy so a durable-store failure reaches
    /// the caller as a classified outcome rather than an opaque internal error.
    pub(crate) fn error_kind(&self) -> ErrorKind {
        match self {
            #[cfg(feature = "test-hooks")]
            Self::InjectedFault(_) => ErrorKind::Unavailable,
            Self::Unavailable(_) => ErrorKind::Unavailable,
            Self::Record(_) | Self::Sqlite(_) | Self::StagedBytesCorrupt { .. } => {
                ErrorKind::Internal
            }
            Self::NotFound(_) => ErrorKind::NotFound,
            Self::IllegalTransition { .. } | Self::DuplicateTransaction(_) => ErrorKind::Conflict,
        }
    }
}

fn sqlite(error: rusqlite::Error) -> StoreError {
    StoreError::Sqlite(error.to_string())
}

/// Environment variable naming one durable point (`before_fsync:<phase>` or
/// `after_fsync:<phase>`) at which a test-hooks build exits the whole process.
#[cfg(feature = "test-hooks")]
pub(crate) const JOURNAL_KILL_ENV: &str = "GODOT_MCP_TEST_JOURNAL_KILL";

/// The point at which a fault can be injected, so a test can terminate the
/// process before *and* after each phase's durability barrier.
#[cfg(feature = "test-hooks")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultPoint {
    /// Before the phase's SQLite commit + fsync.
    BeforeFsync(PhaseName),
    /// After the phase's SQLite commit + fsync, before the caller observes it.
    AfterFsync(PhaseName),
}

/// A phase name usable in a fault-point selector.
#[cfg(feature = "test-hooks")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseName {
    Intent,
    Staged,
    EffectStarted,
    Committed,
    Compensated,
    Quarantined,
}

#[cfg(feature = "test-hooks")]
impl PhaseName {
    pub(crate) const fn of(phase: Phase) -> Self {
        match phase {
            Phase::IntentFsynced => Self::Intent,
            Phase::StagedFsynced => Self::Staged,
            Phase::EffectStartedFsynced => Self::EffectStarted,
            Phase::CommittedFsynced => Self::Committed,
            Phase::Compensated => Self::Compensated,
            Phase::Quarantined => Self::Quarantined,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Intent => "intent_fsynced",
            Self::Staged => "staged_fsynced",
            Self::EffectStarted => "effect_started_fsynced",
            Self::Committed => "committed_fsynced",
            Self::Compensated => "compensated",
            Self::Quarantined => "quarantined",
        }
    }
}

/// What a read-only inspection of another state directory's journal found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct LegacyStoreSummary {
    /// Every journaled transaction.
    pub(crate) records: u64,
    /// Transactions whose phase is not terminal.
    pub(crate) open: u64,
    /// Transactions settled as write-blocking quarantine.
    pub(crate) quarantined: u64,
}

/// Count the records in a journal this process does not own, without writing
/// to it.
///
/// This is how startup inventories a candidate-derived state directory that a
/// version 0.1 server may have created: the database is opened read-only and
/// only phase counts are read, so inspection can neither migrate nor settle a
/// record that belongs to another state root. An absent store is an empty
/// summary.
pub(crate) fn inspect_legacy_store(directory: &Path) -> Result<LegacyStoreSummary, StoreError> {
    let path = directory.join("transactions.sqlite3");
    if !path.is_file() {
        return Ok(LegacyStoreSummary::default());
    }
    let connection = Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(sqlite)?;
    let count = |predicate: &str| -> Result<u64, StoreError> {
        let total: i64 = connection
            .query_row(
                &format!("SELECT count(*) FROM transactions{predicate}"),
                [],
                |row| row.get(0),
            )
            .map_err(sqlite)?;
        u64::try_from(total).map_err(|_| StoreError::Sqlite("negative record count".to_owned()))
    };
    Ok(LegacyStoreSummary {
        records: count("")?,
        open: count(
            " WHERE phase IN ('intent_fsynced', 'staged_fsynced', 'effect_started_fsynced')",
        )?,
        quarantined: count(" WHERE phase = 'quarantined'")?,
    })
}

/// The durable journal store. Cloning is intentionally impossible: exactly one
/// owner holds the connection, and all access is serialized behind its mutex.
pub(crate) struct TransactionStore {
    connection: Mutex<Connection>,
    directory: PathBuf,
    #[cfg(feature = "test-hooks")]
    fault: Mutex<Option<FaultPoint>>,
}

impl TransactionStore {
    /// Open (creating if absent) the durable store inside `directory`.
    pub(crate) fn open(directory: &Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(directory)
            .map_err(|error| StoreError::Unavailable(error.to_string()))?;
        let path = directory.join("transactions.sqlite3");
        let connection = Connection::open(&path).map_err(sqlite)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(sqlite)?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(sqlite)?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(sqlite)?;
        connection.execute_batch(SQL_SCHEMA).map_err(sqlite)?;
        connection
            .execute(
                "INSERT OR IGNORE INTO journal_meta (key, value) VALUES ('schema_version', ?1)",
                params![SCHEMA_VERSION],
            )
            .map_err(sqlite)?;
        let recorded: String = connection
            .query_row(
                "SELECT value FROM journal_meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .map_err(sqlite)?;
        if recorded != SCHEMA_VERSION {
            return Err(StoreError::Unavailable(format!(
                "durable store schema version {recorded} is not supported"
            )));
        }
        let store = Self {
            connection: Mutex::new(connection),
            directory: directory.to_path_buf(),
            #[cfg(feature = "test-hooks")]
            fault: Mutex::new(None),
        };
        store.durability_barrier()?;
        Ok(store)
    }

    /// Directory holding the durable store files.
    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    /// Arm one durability fault point (test builds only).
    #[cfg(feature = "test-hooks")]
    pub(crate) fn arm_fault(&self, point: Option<FaultPoint>) {
        *self
            .fault
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = point;
    }

    #[cfg(feature = "test-hooks")]
    fn check_fault(&self, point: FaultPoint, kind: EffectKind) -> Result<(), StoreError> {
        let label = match point {
            FaultPoint::BeforeFsync(phase) => format!("before_fsync:{}", phase.as_str()),
            FaultPoint::AfterFsync(phase) => format!("after_fsync:{}", phase.as_str()),
        };
        // A whole-process kill at one durable point, so a crash-recovery test can
        // drive a real server process and restart it against the same journal.
        // `<point>@<effect kind>` narrows the kill to one kind of transaction.
        if std::env::var(JOURNAL_KILL_ENV)
            .is_ok_and(|kill| kill == label || kill == format!("{label}@{}", kind.as_str()))
        {
            std::process::exit(86);
        }
        let armed = *self
            .fault
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if armed == Some(point) {
            return Err(StoreError::InjectedFault(label));
        }
        Ok(())
    }

    #[cfg(not(feature = "test-hooks"))]
    #[expect(
        clippy::unused_self,
        reason = "release builds contain no fault-hook state at all"
    )]
    fn fault_before(&self, _phase: Phase, _kind: EffectKind) -> Result<(), StoreError> {
        Ok(())
    }

    #[cfg(feature = "test-hooks")]
    fn fault_before(&self, phase: Phase, kind: EffectKind) -> Result<(), StoreError> {
        self.check_fault(FaultPoint::BeforeFsync(PhaseName::of(phase)), kind)
    }

    #[cfg(not(feature = "test-hooks"))]
    #[expect(
        clippy::unused_self,
        reason = "release builds contain no fault-hook state at all"
    )]
    fn fault_after(&self, _phase: Phase, _kind: EffectKind) -> Result<(), StoreError> {
        Ok(())
    }

    #[cfg(feature = "test-hooks")]
    fn fault_after(&self, phase: Phase, kind: EffectKind) -> Result<(), StoreError> {
        self.check_fault(FaultPoint::AfterFsync(PhaseName::of(phase)), kind)
    }

    /// Checkpoint the WAL and fsync the containing directory so the last commit
    /// survives both process termination and a directory-entry loss.
    fn durability_barrier(&self) -> Result<(), StoreError> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        connection
            .query_row("PRAGMA wal_checkpoint(FULL)", [], |_row| Ok(()))
            .map_err(sqlite)?;
        drop(connection);
        sync_directory(&self.directory).map_err(|error| StoreError::Unavailable(error.to_string()))
    }

    /// Write the initial `intent_fsynced` record. The identity is durable before
    /// any staging or effect can start, so a crash can never leave an effect
    /// whose owning project/principal/request is unknown.
    pub(crate) fn open_transaction(
        &self,
        mut record: TransactionRecord,
    ) -> Result<TransactionRecord, StoreError> {
        record.schema_version = SCHEMA_VERSION.to_owned();
        record.phase = Phase::IntentFsynced;
        record.phase_history = vec![PhaseStep {
            phase: Phase::IntentFsynced,
            sequence: 1,
        }];
        let bytes = record.to_canonical_bytes()?;
        let json = String::from_utf8(bytes.clone()).map_err(|_| RecordError::Serialization)?;
        self.fault_before(Phase::IntentFsynced, record.effect_kind)?;
        {
            let connection = self
                .connection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let existing: Option<String> = connection
                .query_row(
                    "SELECT transaction_id FROM transactions WHERE transaction_id = ?1",
                    params![record.transaction_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sqlite)?;
            if existing.is_some() {
                return Err(StoreError::DuplicateTransaction(
                    record.transaction_id.clone(),
                ));
            }
            connection
                .execute(
                    "INSERT INTO transactions \
                     (transaction_id, phase, sequence, record_json, record_sha256) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        record.transaction_id,
                        Phase::IntentFsynced.as_str(),
                        1_i64,
                        json,
                        hex_sha256(&bytes)
                    ],
                )
                .map_err(sqlite)?;
            connection
                .execute(
                    "INSERT INTO audit_log (transaction_id, event, detail) VALUES (?1, ?2, ?3)",
                    params![record.transaction_id, "intent_journaled", record.tool_name],
                )
                .map_err(sqlite)?;
        }
        self.durability_barrier()?;
        self.fault_after(Phase::IntentFsynced, record.effect_kind)?;
        Ok(record)
    }

    /// Load one transaction record.
    pub(crate) fn load(&self, transaction_id: &str) -> Result<TransactionRecord, StoreError> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let row: Option<(String, String)> = connection
            .query_row(
                "SELECT record_json, record_sha256 FROM transactions WHERE transaction_id = ?1",
                params![transaction_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sqlite)?;
        let (json, recorded_hash) =
            row.ok_or_else(|| StoreError::NotFound(transaction_id.to_owned()))?;
        if hex_sha256(json.as_bytes()) != recorded_hash {
            return Err(StoreError::Record(RecordError::Malformed));
        }
        TransactionRecord::from_canonical_bytes(json.as_bytes()).map_err(StoreError::Record)
    }

    /// Every transaction whose phase is not terminal, oldest first.
    pub(crate) fn open_transactions(&self) -> Result<Vec<TransactionRecord>, StoreError> {
        let ids: Vec<String> = {
            let connection = self
                .connection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut statement = connection
                .prepare(
                    "SELECT transaction_id FROM transactions \
                     WHERE phase IN ('intent_fsynced', 'staged_fsynced', 'effect_started_fsynced') \
                     ORDER BY sequence ASC, transaction_id ASC",
                )
                .map_err(sqlite)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sqlite)?;
            rows.collect::<Result<_, _>>().map_err(sqlite)?
        };
        ids.iter().map(|id| self.load(id)).collect()
    }

    /// Every durable transaction, oldest first.
    pub(crate) fn all_transactions(&self) -> Result<Vec<TransactionRecord>, StoreError> {
        let ids: Vec<String> = {
            let connection = self
                .connection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut statement = connection
                .prepare(
                    "SELECT transaction_id FROM transactions \
                     ORDER BY sequence ASC, transaction_id ASC",
                )
                .map_err(sqlite)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sqlite)?;
            rows.collect::<Result<_, _>>().map_err(sqlite)?
        };
        ids.iter().map(|id| self.load(id)).collect()
    }

    /// Persist an engine-port record and its complete staged payload set.
    pub(crate) fn persist_canonical_value(
        &self,
        value: &Value,
        staged: &[(String, Vec<u8>)],
    ) -> Result<TransactionRecord, StoreError> {
        let bytes = serde_json::to_vec(value).map_err(|_| RecordError::Serialization)?;
        let record = TransactionRecord::from_canonical_bytes(&bytes)?;
        let target = record.phase;
        if target.is_terminal() != record.terminal_outcome.is_some() {
            return Err(StoreError::Record(RecordError::Malformed));
        }
        let existing = self.load(&record.transaction_id);
        match existing {
            Ok(existing) => self.require_legal(&existing, target)?,
            Err(StoreError::NotFound(_)) if target == Phase::IntentFsynced => {}
            Err(StoreError::NotFound(_)) => {
                return Err(StoreError::IllegalTransition {
                    transaction_id: record.transaction_id.clone(),
                    from: Phase::IntentFsynced,
                    to: target,
                });
            }
            Err(error) => return Err(error),
        }
        for (staging_id, payload) in staged {
            let declared = record
                .staged_write_set
                .iter()
                .find(|entry| entry.staging_id == *staging_id)
                .ok_or_else(|| StoreError::StagedBytesCorrupt {
                    transaction_id: record.transaction_id.clone(),
                    reason: format!("staging id {staging_id} is not in the declared write set"),
                })?;
            if declared.byte_length != payload.len() as u64
                || declared.sha256 != hex_sha256(payload)
            {
                return Err(StoreError::StagedBytesCorrupt {
                    transaction_id: record.transaction_id.clone(),
                    reason: format!("staged bytes for {staging_id} do not match the write set"),
                });
            }
        }
        if staged.len() != record.staged_write_set.len() {
            return Err(StoreError::StagedBytesCorrupt {
                transaction_id: record.transaction_id.clone(),
                reason: "staged byte payloads do not cover the declared write set".to_owned(),
            });
        }

        let json = String::from_utf8(bytes.clone()).map_err(|_| RecordError::Serialization)?;
        self.fault_before(target, record.effect_kind)?;
        {
            let mut connection = self
                .connection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let transaction = connection.transaction().map_err(sqlite)?;
            transaction
                .execute(
                    "INSERT INTO transactions \
                     (transaction_id, phase, sequence, record_json, record_sha256) \
                     VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT(transaction_id) DO UPDATE SET \
                     phase = excluded.phase, sequence = excluded.sequence, \
                     record_json = excluded.record_json, record_sha256 = excluded.record_sha256",
                    params![
                        record.transaction_id,
                        target.as_str(),
                        record.phase_history.len() as i64,
                        json,
                        hex_sha256(&bytes)
                    ],
                )
                .map_err(sqlite)?;
            transaction
                .execute(
                    "DELETE FROM staged_bytes WHERE transaction_id = ?1",
                    params![record.transaction_id],
                )
                .map_err(sqlite)?;
            for (staging_id, payload) in staged {
                transaction
                    .execute(
                        "INSERT INTO staged_bytes \
                         (transaction_id, staging_id, payload) VALUES (?1, ?2, ?3)",
                        params![record.transaction_id, staging_id, payload],
                    )
                    .map_err(sqlite)?;
            }
            transaction
                .execute(
                    "INSERT INTO audit_log (transaction_id, event, detail) VALUES (?1, ?2, ?3)",
                    params![
                        record.transaction_id,
                        target.as_str(),
                        "engine port phase persisted"
                    ],
                )
                .map_err(sqlite)?;
            transaction.commit().map_err(sqlite)?;
        }
        self.durability_barrier()?;
        self.fault_after(target, record.effect_kind)?;
        Ok(record)
    }

    /// Every journaled transaction carrying one idempotency identifier, oldest
    /// first.
    ///
    /// The identifier is already a column of the canonical record, so cross-plane
    /// idempotency reads its answer out of this store and needs no second ledger
    /// and no schema change.
    ///
    /// The lookup key is the identifier *alone*, deliberately. The canonical
    /// request hash is one of the eight identity elements compared afterwards, but
    /// it is not a lookup key: two legitimately distinct requests may be
    /// byte-identical (write the same bytes twice on purpose), and the idempotency
    /// identifier is exactly how a caller says which of those it means. Keying on
    /// the hash as well would refuse the second deliberate write.
    pub(crate) fn transactions_for_idempotency_id(
        &self,
        idempotency_id: &str,
    ) -> Result<Vec<TransactionRecord>, StoreError> {
        let ids: Vec<String> = {
            let connection = self
                .connection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut statement = connection
                .prepare(
                    "SELECT transaction_id FROM transactions \
                     ORDER BY sequence ASC, transaction_id ASC",
                )
                .map_err(sqlite)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sqlite)?;
            rows.collect::<Result<_, _>>().map_err(sqlite)?
        };
        let mut matching = Vec::new();
        for id in &ids {
            let record = self.load(id)?;
            if record.idempotency_id == idempotency_id {
                matching.push(record);
            }
        }
        Ok(matching)
    }

    /// Whether any transaction is quarantined. A quarantined project blocks
    /// every ordinary capability-profile-driven write permit unconditionally.
    pub(crate) fn quarantined_transactions(&self) -> Result<Vec<String>, StoreError> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut statement = connection
            .prepare(
                "SELECT transaction_id FROM transactions WHERE phase = 'quarantined' \
                 ORDER BY transaction_id ASC",
            )
            .map_err(sqlite)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sqlite)?;
        rows.collect::<Result<_, _>>().map_err(sqlite)
    }

    /// Durably record the staged write set and its bytes, then advance to
    /// `staged_fsynced`. The bytes and the record share one fsynced commit.
    pub(crate) fn stage(
        &self,
        transaction_id: &str,
        staged: Vec<(String, Vec<u8>)>,
    ) -> Result<TransactionRecord, StoreError> {
        let mut record = self.load(transaction_id)?;
        let target = Phase::StagedFsynced;
        self.require_legal(&record, target)?;
        for (staging_id, payload) in &staged {
            let declared = record
                .staged_write_set
                .iter()
                .find(|entry| &entry.staging_id == staging_id)
                .ok_or_else(|| StoreError::StagedBytesCorrupt {
                    transaction_id: transaction_id.to_owned(),
                    reason: format!("staging id {staging_id} is not in the declared write set"),
                })?;
            if declared.byte_length != payload.len() as u64
                || declared.sha256 != hex_sha256(payload)
            {
                return Err(StoreError::StagedBytesCorrupt {
                    transaction_id: transaction_id.to_owned(),
                    reason: format!("staged bytes for {staging_id} do not match the write set"),
                });
            }
        }
        if staged.len() != record.staged_write_set.len() {
            return Err(StoreError::StagedBytesCorrupt {
                transaction_id: transaction_id.to_owned(),
                reason: "staged byte payloads do not cover the declared write set".to_owned(),
            });
        }
        record.phase = target;
        record.phase_history.push(PhaseStep {
            phase: target,
            sequence: record.phase_history.len() as u64 + 1,
        });
        let bytes = record.to_canonical_bytes()?;
        let json = String::from_utf8(bytes.clone()).map_err(|_| RecordError::Serialization)?;

        self.fault_before(target, record.effect_kind)?;
        {
            let mut connection = self
                .connection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let transaction = connection.transaction().map_err(sqlite)?;
            for (staging_id, payload) in &staged {
                transaction
                    .execute(
                        "INSERT OR REPLACE INTO staged_bytes \
                         (transaction_id, staging_id, payload) VALUES (?1, ?2, ?3)",
                        params![transaction_id, staging_id, payload],
                    )
                    .map_err(sqlite)?;
            }
            transaction
                .execute(
                    "UPDATE transactions SET phase = ?2, sequence = ?3, record_json = ?4, \
                     record_sha256 = ?5 WHERE transaction_id = ?1",
                    params![
                        transaction_id,
                        target.as_str(),
                        record.phase_history.len() as i64,
                        json,
                        hex_sha256(&bytes)
                    ],
                )
                .map_err(sqlite)?;
            transaction
                .execute(
                    "INSERT INTO audit_log (transaction_id, event, detail) VALUES (?1, ?2, ?3)",
                    params![
                        transaction_id,
                        "staged",
                        format!("{} staged writes", staged.len())
                    ],
                )
                .map_err(sqlite)?;
            transaction.commit().map_err(sqlite)?;
        }
        self.durability_barrier()?;
        self.fault_after(target, record.effect_kind)?;
        Ok(record)
    }

    /// Read back and verify one staged payload. A truncated or altered payload is
    /// reported as corrupt rather than silently replayed.
    pub(crate) fn staged_bytes(
        &self,
        transaction_id: &str,
        staging_id: &str,
    ) -> Result<Vec<u8>, StoreError> {
        let record = self.load(transaction_id)?;
        let declared = record
            .staged_write_set
            .iter()
            .find(|entry| entry.staging_id == staging_id)
            .ok_or_else(|| StoreError::StagedBytesCorrupt {
                transaction_id: transaction_id.to_owned(),
                reason: format!("staging id {staging_id} is not in the declared write set"),
            })?;
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let payload: Option<Vec<u8>> = connection
            .query_row(
                "SELECT payload FROM staged_bytes WHERE transaction_id = ?1 AND staging_id = ?2",
                params![transaction_id, staging_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(sqlite)?;
        let payload = payload.ok_or_else(|| StoreError::StagedBytesCorrupt {
            transaction_id: transaction_id.to_owned(),
            reason: format!("staged bytes for {staging_id} are absent"),
        })?;
        if payload.len() as u64 != declared.byte_length || hex_sha256(&payload) != declared.sha256 {
            return Err(StoreError::StagedBytesCorrupt {
                transaction_id: transaction_id.to_owned(),
                reason: format!("staged bytes for {staging_id} are partial or altered"),
            });
        }
        Ok(payload)
    }

    /// Overwrite one staged payload without touching the record. Test-only: it
    /// exists so a crash-simulation test can inject genuinely partial bytes.
    #[cfg(feature = "test-hooks")]
    pub(crate) fn corrupt_staged_bytes(
        &self,
        transaction_id: &str,
        staging_id: &str,
        payload: &[u8],
    ) -> Result<(), StoreError> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        connection
            .execute(
                "UPDATE staged_bytes SET payload = ?3 \
                 WHERE transaction_id = ?1 AND staging_id = ?2",
                params![transaction_id, staging_id, payload],
            )
            .map_err(sqlite)?;
        Ok(())
    }

    /// Advance to a legal next phase, optionally attaching the observed typed
    /// proof and a terminal reason. Anything illegal is rejected and the stored
    /// record is left byte-identical.
    pub(crate) fn advance(
        &self,
        transaction_id: &str,
        target: Phase,
        proof: Option<TypedProof>,
        terminal_reason: Option<String>,
        terminal_outcome: Option<TerminalOutcome>,
    ) -> Result<TransactionRecord, StoreError> {
        let mut record = self.load(transaction_id)?;
        self.require_legal(&record, target)?;
        if target.is_terminal() != terminal_outcome.is_some() {
            return Err(StoreError::Record(RecordError::Malformed));
        }
        if let Some(proof) = &proof
            && proof.kind() != record.effect_kind
        {
            return Err(StoreError::Record(RecordError::Malformed));
        }
        record.phase = target;
        record.phase_history.push(PhaseStep {
            phase: target,
            sequence: record.phase_history.len() as u64 + 1,
        });
        if proof.is_some() {
            record.proof = proof;
        }
        if terminal_reason.is_some() {
            record.terminal_reason = terminal_reason;
        }
        if terminal_outcome.is_some() {
            record.terminal_outcome = terminal_outcome;
        }
        let bytes = record.to_canonical_bytes()?;
        let json = String::from_utf8(bytes.clone()).map_err(|_| RecordError::Serialization)?;

        self.fault_before(target, record.effect_kind)?;
        {
            let connection = self
                .connection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            connection
                .execute(
                    "UPDATE transactions SET phase = ?2, sequence = ?3, record_json = ?4, \
                     record_sha256 = ?5 WHERE transaction_id = ?1",
                    params![
                        transaction_id,
                        target.as_str(),
                        record.phase_history.len() as i64,
                        json,
                        hex_sha256(&bytes)
                    ],
                )
                .map_err(sqlite)?;
            connection
                .execute(
                    "INSERT INTO audit_log (transaction_id, event, detail) VALUES (?1, ?2, ?3)",
                    params![transaction_id, target.as_str(), "phase advanced"],
                )
                .map_err(sqlite)?;
        }
        self.durability_barrier()?;
        self.fault_after(target, record.effect_kind)?;
        Ok(record)
    }

    /// Write one proof-backed terminal record for an already-`quarantined`
    /// transaction.
    ///
    /// This is the *only* transition out of `quarantined`, it is reachable only
    /// from the operator-recovery authority's `RepairPermit` path, and it accepts
    /// exactly `compensated` or `committed_fsynced` — each of which the caller has
    /// already backed with an independently re-derived typed proof. There is no
    /// force, no overwrite and no quarantine-clear variant.
    pub(crate) fn repair_transition(
        &self,
        transaction_id: &str,
        target: Phase,
        proof: Option<TypedProof>,
        reason: String,
        terminal_outcome: TerminalOutcome,
    ) -> Result<TransactionRecord, StoreError> {
        let mut record = self.load(transaction_id)?;
        if record.phase != Phase::Quarantined {
            return Err(StoreError::IllegalTransition {
                transaction_id: transaction_id.to_owned(),
                from: record.phase,
                to: target,
            });
        }
        if !matches!(target, Phase::Compensated | Phase::CommittedFsynced) {
            return Err(StoreError::IllegalTransition {
                transaction_id: transaction_id.to_owned(),
                from: record.phase,
                to: target,
            });
        }
        let Some(proof) = proof else {
            return Err(StoreError::IllegalTransition {
                transaction_id: transaction_id.to_owned(),
                from: record.phase,
                to: target,
            });
        };
        if proof.kind() != record.effect_kind {
            return Err(StoreError::Record(RecordError::Malformed));
        }
        record.phase = target;
        record.phase_history.push(PhaseStep {
            phase: target,
            sequence: record.phase_history.len() as u64 + 1,
        });
        record.proof = Some(proof);
        record.terminal_reason = Some(reason);
        record.terminal_outcome = Some(terminal_outcome);
        let bytes = record.to_canonical_bytes()?;
        let json = String::from_utf8(bytes.clone()).map_err(|_| RecordError::Serialization)?;

        self.fault_before(target, record.effect_kind)?;
        {
            let connection = self
                .connection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            connection
                .execute(
                    "UPDATE transactions SET phase = ?2, sequence = ?3, record_json = ?4, \
                     record_sha256 = ?5 WHERE transaction_id = ?1",
                    params![
                        transaction_id,
                        target.as_str(),
                        record.phase_history.len() as i64,
                        json,
                        hex_sha256(&bytes)
                    ],
                )
                .map_err(sqlite)?;
            connection
                .execute(
                    "INSERT INTO audit_log (transaction_id, event, detail) VALUES (?1, ?2, ?3)",
                    params![transaction_id, target.as_str(), "repair transition"],
                )
                .map_err(sqlite)?;
        }
        self.durability_barrier()?;
        self.fault_after(target, record.effect_kind)?;
        Ok(record)
    }

    /// Append one audit entry. Used by the operator-recovery authority to record
    /// a repair request *before* any state transition is journaled.
    pub(crate) fn audit(
        &self,
        transaction_id: &str,
        event: &str,
        detail: &str,
    ) -> Result<(), StoreError> {
        {
            let connection = self
                .connection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            connection
                .execute(
                    "INSERT INTO audit_log (transaction_id, event, detail) VALUES (?1, ?2, ?3)",
                    params![transaction_id, event, detail],
                )
                .map_err(sqlite)?;
        }
        self.durability_barrier()
    }

    /// Every audit entry for one transaction, oldest first.
    pub(crate) fn audit_entries(
        &self,
        transaction_id: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut statement = connection
            .prepare(
                "SELECT event, detail FROM audit_log WHERE transaction_id = ?1 \
                 ORDER BY sequence ASC",
            )
            .map_err(sqlite)?;
        let rows = statement
            .query_map(params![transaction_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sqlite)?;
        rows.collect::<Result<_, _>>().map_err(sqlite)
    }

    fn require_legal(&self, record: &TransactionRecord, target: Phase) -> Result<(), StoreError> {
        if record.phase.legal_successors().contains(&target) {
            return Ok(());
        }
        Err(StoreError::IllegalTransition {
            transaction_id: record.transaction_id.clone(),
            from: record.phase,
            to: target,
        })
    }

    fn persist_terminal_quarantine(&self, record: TransactionRecord) -> Result<(), StoreError> {
        if record.phase != Phase::Quarantined {
            return Err(StoreError::Record(RecordError::Malformed));
        }
        let bytes = record.to_canonical_bytes()?;
        let json = String::from_utf8(bytes.clone()).map_err(|_| RecordError::Serialization)?;
        {
            let mut connection = self
                .connection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let transaction = connection.transaction().map_err(sqlite)?;
            let existing: Option<String> = transaction
                .query_row(
                    "SELECT transaction_id FROM transactions WHERE transaction_id = ?1",
                    params![record.transaction_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sqlite)?;
            if existing.is_some() {
                return Err(StoreError::DuplicateTransaction(record.transaction_id));
            }
            transaction
                .execute(
                    "INSERT INTO transactions \
                     (transaction_id, phase, sequence, record_json, record_sha256) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        record.transaction_id,
                        Phase::Quarantined.as_str(),
                        record.phase_history.len() as i64,
                        json,
                        hex_sha256(&bytes)
                    ],
                )
                .map_err(sqlite)?;
            transaction
                .execute(
                    "INSERT INTO audit_log (transaction_id, event, detail) VALUES (?1, ?2, ?3)",
                    params![
                        record.transaction_id,
                        "bridge_outcome_quarantined",
                        record.tool_name
                    ],
                )
                .map_err(sqlite)?;
            transaction.commit().map_err(sqlite)?;
        }
        self.durability_barrier()
    }
}

impl QuarantineSink for TransactionStore {
    fn quarantine(&self, value: &Value, _handoff: &QuarantineHandoff) -> Result<(), JournalError> {
        let bytes =
            serde_json::to_vec(value).map_err(|error| JournalError::Payload(error.to_string()))?;
        let record = TransactionRecord::from_canonical_bytes(&bytes)
            .map_err(|error| JournalError::Payload(error.to_string()))?;
        self.persist_terminal_quarantine(record)
            .map_err(|error| JournalError::Durability {
                phase: LaunchPhase::StagedFsynced,
                reason: format!("bridge quarantine terminal: {error}"),
            })
    }
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        // Windows has no directory-fsync equivalent; `synchronous = FULL` plus the
        // WAL checkpoint above already forced the data to stable storage.
        let _ = path;
        Ok(())
    }
}
