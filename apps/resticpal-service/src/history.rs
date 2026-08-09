use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use resticpal_core::status::{BackupRunOutcome, BackupRunRecord};
use rusqlite::{Connection, Transaction, params};
use thiserror::Error;

const HISTORY_SCHEMA_VERSION: i64 = 1;
const MAX_STORED_RUNS: usize = 200;
pub const MAX_HISTORY_RESULTS: usize = 100;
const MAX_IDENTIFIER_BYTES: usize = 128;

#[derive(Debug, Clone)]
pub struct BackupHistoryStore {
    path: PathBuf,
}

impl BackupHistoryStore {
    pub fn next_to_config(config_path: &Path) -> Self {
        Self {
            path: config_path.with_file_name("state.db"),
        }
    }

    pub fn append(&self, run: CompletedBackupRun) -> Result<BackupRunRecord, HistoryError> {
        let mut connection = self.initialized_connection()?;
        let transaction = connection.transaction()?;
        let error_code = run.error_code.as_deref().and_then(sanitize_identifier);
        let snapshot_id = run.snapshot_id.as_deref().and_then(sanitize_identifier);
        transaction.execute(
            "INSERT INTO backup_runs (
                started_at_ms, completed_at_ms, outcome, error_code,
                files_processed, bytes_processed, data_added, snapshot_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                run.started_at.timestamp_millis(),
                run.completed_at.timestamp_millis(),
                outcome_name(run.outcome),
                error_code,
                run.files_processed.map(to_sql_integer),
                run.bytes_processed.map(to_sql_integer),
                run.data_added.map(to_sql_integer),
                snapshot_id,
            ],
        )?;
        let id = u64::try_from(transaction.last_insert_rowid())
            .map_err(|_| HistoryError::InvalidUnsignedValue("id"))?;
        prune(&transaction)?;
        transaction.commit()?;

        Ok(BackupRunRecord {
            id,
            started_at: timestamp(run.started_at.timestamp_millis())?,
            completed_at: timestamp(run.completed_at.timestamp_millis())?,
            outcome: run.outcome,
            error_code,
            files_processed: run.files_processed,
            bytes_processed: run.bytes_processed,
            data_added: run.data_added,
            snapshot_id,
        })
    }

    pub fn recent(&self, limit: usize) -> Result<Vec<BackupRunRecord>, HistoryError> {
        let limit = limit.clamp(1, MAX_HISTORY_RESULTS);
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.initialized_connection()?;
        let mut statement = connection.prepare(
            "SELECT id, started_at_ms, completed_at_ms, outcome,
                    CASE WHEN length(error_code) <= 128 THEN error_code END,
                    files_processed, bytes_processed, data_added,
                    CASE WHEN length(snapshot_id) <= 128 THEN snapshot_id END
             FROM backup_runs
             ORDER BY completed_at_ms DESC, id DESC
             LIMIT ?1",
        )?;
        let rows = statement.query_map(
            [i64::try_from(limit).expect("history limit fits in i64")],
            |row| {
                Ok(RawRunRecord {
                    id: row.get(0)?,
                    started_at_ms: row.get(1)?,
                    completed_at_ms: row.get(2)?,
                    outcome: row.get(3)?,
                    error_code: row.get(4)?,
                    files_processed: row.get(5)?,
                    bytes_processed: row.get(6)?,
                    data_added: row.get(7)?,
                    snapshot_id: row.get(8)?,
                })
            },
        )?;

        rows.map(|row| {
            row.map_err(HistoryError::from)
                .and_then(BackupRunRecord::try_from)
        })
        .collect()
    }

    fn connection(&self) -> Result<Connection, HistoryError> {
        let connection = Connection::open(&self.path)?;
        connection.busy_timeout(Duration::from_secs(2))?;
        Ok(connection)
    }

    fn initialized_connection(&self) -> Result<Connection, HistoryError> {
        let connection = self.connection()?;
        initialize_schema(&connection)?;
        Ok(connection)
    }
}

#[derive(Debug, Clone)]
pub struct CompletedBackupRun {
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub outcome: BackupRunOutcome,
    pub error_code: Option<String>,
    pub files_processed: Option<u64>,
    pub bytes_processed: Option<u64>,
    pub data_added: Option<u64>,
    pub snapshot_id: Option<String>,
}

#[derive(Debug)]
struct RawRunRecord {
    id: i64,
    started_at_ms: i64,
    completed_at_ms: i64,
    outcome: String,
    error_code: Option<String>,
    files_processed: Option<i64>,
    bytes_processed: Option<i64>,
    data_added: Option<i64>,
    snapshot_id: Option<String>,
}

impl TryFrom<RawRunRecord> for BackupRunRecord {
    type Error = HistoryError;

    fn try_from(run: RawRunRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            id: from_sql_integer(run.id, "id")?,
            started_at: timestamp(run.started_at_ms)?,
            completed_at: timestamp(run.completed_at_ms)?,
            outcome: parse_outcome(&run.outcome)?,
            error_code: run.error_code.as_deref().and_then(sanitize_identifier),
            files_processed: run
                .files_processed
                .map(|value| from_sql_integer(value, "files_processed"))
                .transpose()?,
            bytes_processed: run
                .bytes_processed
                .map(|value| from_sql_integer(value, "bytes_processed"))
                .transpose()?,
            data_added: run
                .data_added
                .map(|value| from_sql_integer(value, "data_added"))
                .transpose()?,
            snapshot_id: run.snapshot_id.as_deref().and_then(sanitize_identifier),
        })
    }
}

fn initialize_schema(connection: &Connection) -> Result<(), HistoryError> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > HISTORY_SCHEMA_VERSION {
        return Err(HistoryError::UnsupportedSchema(version));
    }
    connection.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         CREATE TABLE IF NOT EXISTS backup_runs (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             started_at_ms INTEGER NOT NULL,
             completed_at_ms INTEGER NOT NULL,
             outcome TEXT NOT NULL CHECK (
                 outcome IN ('succeeded', 'succeeded_with_warnings', 'failed', 'cancelled')
             ),
             error_code TEXT CHECK (error_code IS NULL OR length(error_code) <= 128),
             files_processed INTEGER,
             bytes_processed INTEGER,
             data_added INTEGER,
             snapshot_id TEXT CHECK (snapshot_id IS NULL OR length(snapshot_id) <= 128)
         );
         CREATE INDEX IF NOT EXISTS backup_runs_completed
             ON backup_runs(completed_at_ms DESC, id DESC);",
    )?;
    connection.pragma_update(None, "user_version", HISTORY_SCHEMA_VERSION)?;
    Ok(())
}

fn prune(transaction: &Transaction<'_>) -> Result<(), rusqlite::Error> {
    transaction.execute(
        "DELETE FROM backup_runs
         WHERE id NOT IN (
             SELECT id FROM backup_runs
             ORDER BY completed_at_ms DESC, id DESC
             LIMIT ?1
         )",
        [i64::try_from(MAX_STORED_RUNS).expect("history capacity fits in i64")],
    )?;
    Ok(())
}

const fn outcome_name(outcome: BackupRunOutcome) -> &'static str {
    match outcome {
        BackupRunOutcome::Succeeded => "succeeded",
        BackupRunOutcome::SucceededWithWarnings => "succeeded_with_warnings",
        BackupRunOutcome::Failed => "failed",
        BackupRunOutcome::Cancelled => "cancelled",
    }
}

fn parse_outcome(value: &str) -> Result<BackupRunOutcome, HistoryError> {
    match value {
        "succeeded" => Ok(BackupRunOutcome::Succeeded),
        "succeeded_with_warnings" => Ok(BackupRunOutcome::SucceededWithWarnings),
        "failed" => Ok(BackupRunOutcome::Failed),
        "cancelled" => Ok(BackupRunOutcome::Cancelled),
        _ => Err(HistoryError::InvalidOutcome(value.to_owned())),
    }
}

fn sanitize_identifier(value: &str) -> Option<String> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }
    Some(value.to_owned())
}

fn timestamp(milliseconds: i64) -> Result<DateTime<Utc>, HistoryError> {
    Utc.timestamp_millis_opt(milliseconds)
        .single()
        .ok_or(HistoryError::InvalidTimestamp(milliseconds))
}

fn to_sql_integer(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn from_sql_integer(value: i64, field: &'static str) -> Result<u64, HistoryError> {
    u64::try_from(value).map_err(|_| HistoryError::InvalidUnsignedValue(field))
}

#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("backup history database failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("unsupported backup-history schema {0}")]
    UnsupportedSchema(i64),
    #[error("backup history contains invalid timestamp {0}")]
    InvalidTimestamp(i64),
    #[error("backup history contains an invalid unsigned value for {0}")]
    InvalidUnsignedValue(&'static str),
    #[error("backup history contains unknown outcome {0}")]
    InvalidOutcome(String),
}

#[cfg(test)]
mod tests {
    use chrono::Duration as ChronoDuration;

    use super::*;

    fn run(completed_at: DateTime<Utc>, outcome: BackupRunOutcome) -> CompletedBackupRun {
        CompletedBackupRun {
            started_at: completed_at - ChronoDuration::minutes(3),
            completed_at,
            outcome,
            error_code: None,
            files_processed: Some(12),
            bytes_processed: Some(1_024),
            data_added: Some(256),
            snapshot_id: Some("abc123".to_owned()),
        }
    }

    #[test]
    fn empty_history_does_not_create_a_database() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = BackupHistoryStore::next_to_config(&directory.path().join("config.toml"));

        assert!(store.recent(50).expect("empty history").is_empty());
        assert!(!directory.path().join("state.db").exists());
    }

    #[test]
    fn completed_runs_round_trip_newest_first() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = BackupHistoryStore::next_to_config(&directory.path().join("config.toml"));
        let now = Utc::now();
        let first = store
            .append(run(now, BackupRunOutcome::Succeeded))
            .expect("first run");
        let second = store
            .append(run(
                now + ChronoDuration::minutes(1),
                BackupRunOutcome::Cancelled,
            ))
            .expect("second run");

        let recent = store.recent(10).expect("recent history");
        assert_eq!(recent, vec![second, first]);
    }

    #[test]
    fn identifiers_are_allowlisted_before_storage() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = BackupHistoryStore::next_to_config(&directory.path().join("config.toml"));
        let mut unsafe_run = run(Utc::now(), BackupRunOutcome::Failed);
        unsafe_run.error_code = Some("contains spaces and a path C:\\Users\\Example".to_owned());
        unsafe_run.snapshot_id = Some("not/a/snapshot".to_owned());

        let stored = store.append(unsafe_run).expect("stored run");
        assert_eq!(stored.error_code, None);
        assert_eq!(stored.snapshot_id, None);

        let connection = store.initialized_connection().expect("connection");
        connection
            .pragma_update(None, "ignore_check_constraints", true)
            .expect("test corruption mode");
        connection
            .execute(
                "UPDATE backup_runs SET error_code = ?1, snapshot_id = ?2 WHERE id = ?3",
                params![
                    r"C:\Users\Example\secret.txt",
                    "not/a/snapshot",
                    to_sql_integer(stored.id)
                ],
            )
            .expect("simulated corrupt values");
        drop(connection);
        let reloaded = store.recent(1).expect("reloaded history");
        assert_eq!(reloaded[0].error_code, None);
        assert_eq!(reloaded[0].snapshot_id, None);
    }

    #[test]
    fn retention_is_bounded() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = BackupHistoryStore::next_to_config(&directory.path().join("config.toml"));
        let now = Utc::now();
        for offset in 0..=MAX_STORED_RUNS {
            store
                .append(run(
                    now + ChronoDuration::seconds(i64::try_from(offset).expect("offset")),
                    BackupRunOutcome::Succeeded,
                ))
                .expect("stored run");
        }

        let connection = store.connection().expect("connection");
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM backup_runs", [], |row| row.get(0))
            .expect("count");
        assert_eq!(
            count,
            i64::try_from(MAX_STORED_RUNS).expect("history capacity")
        );
    }

    #[test]
    fn newer_schema_is_rejected() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("state.db");
        let connection = Connection::open(&path).expect("database");
        connection
            .pragma_update(None, "user_version", HISTORY_SCHEMA_VERSION + 1)
            .expect("newer schema");
        drop(connection);

        let store = BackupHistoryStore::next_to_config(&directory.path().join("config.toml"));
        assert!(matches!(
            store.recent(1),
            Err(HistoryError::UnsupportedSchema(version)) if version == HISTORY_SCHEMA_VERSION + 1
        ));
    }
}
