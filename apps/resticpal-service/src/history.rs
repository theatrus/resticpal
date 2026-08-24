use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use resticpal_core::status::{
    BackupRunOutcome, BackupRunRecord, MAX_BACKUP_FAILED_ITEMS, is_safe_backup_failed_item,
};
use resticpal_windows::credentials::{CredentialStoreError, protect_service_data_path};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use thiserror::Error;
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

const HISTORY_SCHEMA_VERSION: i64 = 2;
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
        let failure_details = normalize_failure_details(run.failed_items, run.failed_items_omitted);
        let files_processed = run.files_processed.map(normalize_sql_unsigned);
        let bytes_processed = run.bytes_processed.map(normalize_sql_unsigned);
        let data_added = run.data_added.map(normalize_sql_unsigned);
        transaction.execute(
            "INSERT INTO backup_runs (
                started_at_ms, completed_at_ms, outcome, error_code,
                files_processed, bytes_processed, data_added, snapshot_id,
                failed_items_omitted
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                run.started_at.timestamp_millis(),
                run.completed_at.timestamp_millis(),
                outcome_name(run.outcome),
                error_code,
                files_processed.map(to_sql_integer),
                bytes_processed.map(to_sql_integer),
                data_added.map(to_sql_integer),
                snapshot_id,
                to_sql_integer(failure_details.omitted),
            ],
        )?;
        let id = u64::try_from(transaction.last_insert_rowid())
            .map_err(|_| HistoryError::InvalidUnsignedValue("id"))?;
        for (ordinal, item) in failure_details.items.iter().enumerate() {
            transaction.execute(
                "INSERT INTO backup_run_failed_items (run_id, ordinal, item)
                 VALUES (?1, ?2, ?3)",
                params![
                    to_sql_integer(id),
                    i64::try_from(ordinal).expect("failure-item ordinal fits in i64"),
                    item,
                ],
            )?;
        }
        prune(&transaction)?;
        transaction.commit()?;

        let failed_item_count = u64::try_from(failure_details.items.len())
            .unwrap_or(u64::MAX)
            .saturating_add(failure_details.omitted);

        Ok(BackupRunRecord {
            id,
            started_at: timestamp(run.started_at.timestamp_millis())?,
            completed_at: timestamp(run.completed_at.timestamp_millis())?,
            outcome: run.outcome,
            error_code,
            files_processed,
            bytes_processed,
            data_added,
            snapshot_id,
            failed_item_count,
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
                    CASE WHEN length(snapshot_id) <= 128 THEN snapshot_id END,
                    failed_items_omitted + (
                        SELECT COUNT(*) FROM backup_run_failed_items AS failed
                        WHERE failed.run_id = backup_runs.id
                    )
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
                    failed_item_count: row.get(9)?,
                })
            },
        )?;

        rows.map(|row| {
            row.map_err(HistoryError::from)
                .and_then(BackupRunRecord::try_from)
        })
        .collect()
    }

    pub fn failure_details(
        &self,
        run_id: u64,
    ) -> Result<Option<StoredRunFailureDetails>, HistoryError> {
        if !self.path.exists() {
            return Ok(None);
        }
        let Ok(run_id) = i64::try_from(run_id) else {
            return Ok(None);
        };
        let connection = self.initialized_connection()?;
        let counts: Option<(i64, i64)> = connection
            .query_row(
                "SELECT failed_items_omitted, (
                     SELECT COUNT(*) FROM backup_run_failed_items AS failed
                     WHERE failed.run_id = backup_runs.id
                 )
                 FROM backup_runs WHERE id = ?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((omitted, stored_count)) = counts else {
            return Ok(None);
        };
        let omitted = from_sql_integer(omitted, "failed_items_omitted")?;
        let stored_count = from_sql_integer(stored_count, "failed_item_count")?;
        let mut statement = connection.prepare(
            "SELECT item FROM backup_run_failed_items
             WHERE run_id = ?1
             ORDER BY ordinal ASC
             LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![
                run_id,
                i64::try_from(MAX_BACKUP_FAILED_ITEMS).expect("failure detail limit fits in i64")
            ],
            |row| row.get::<_, String>(0),
        )?;
        let mut items = Vec::new();
        for row in rows {
            let item = row?;
            if is_safe_backup_failed_item(&item) && !items.contains(&item) {
                items.push(item);
            }
        }
        let retained = u64::try_from(items.len()).unwrap_or(u64::MAX);
        let omitted = omitted.saturating_add(stored_count.saturating_sub(retained));
        Ok(Some(StoredRunFailureDetails { items, omitted }))
    }

    fn connection(&self) -> Result<Connection, HistoryError> {
        if let Some(parent) = self.path.parent() {
            validate_storage_entry(parent, true)?;
            protect_service_data_path(parent)?;
        }
        for path in [
            self.path.clone(),
            sqlite_sidecar_path(&self.path, "-wal"),
            sqlite_sidecar_path(&self.path, "-shm"),
        ] {
            validate_storage_entry(&path, false)?;
        }
        let connection = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(Duration::from_secs(2))?;
        Ok(connection)
    }

    fn initialized_connection(&self) -> Result<Connection, HistoryError> {
        let connection = self.connection()?;
        initialize_schema(&connection)?;
        self.protect_database_files()?;
        Ok(connection)
    }

    fn protect_database_files(&self) -> Result<(), HistoryError> {
        for path in [
            self.path.clone(),
            sqlite_sidecar_path(&self.path, "-wal"),
            sqlite_sidecar_path(&self.path, "-shm"),
        ] {
            if path.exists() {
                protect_service_data_path(&path)?;
            }
        }
        Ok(())
    }
}

fn sqlite_sidecar_path(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn validate_storage_entry(path: &Path, directory: bool) -> Result<(), HistoryError> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound && !directory => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let is_reparse = metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;
    if is_reparse || (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err(HistoryError::UnsafeStorageEntry);
    }
    Ok(())
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
    pub failed_items: Vec<String>,
    pub failed_items_omitted: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRunFailureDetails {
    pub items: Vec<String>,
    pub omitted: u64,
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
    failed_item_count: i64,
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
            failed_item_count: from_sql_integer(run.failed_item_count, "failed_item_count")?,
        })
    }
}

fn initialize_schema(connection: &Connection) -> Result<(), HistoryError> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > HISTORY_SCHEMA_VERSION {
        return Err(HistoryError::UnsupportedSchema(version));
    }
    connection.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
    )?;
    match version {
        0 => {
            if table_exists(connection, "backup_runs")? {
                migrate_legacy_schema(
                    connection,
                    !column_exists(connection, "backup_runs", "failed_items_omitted")?,
                )?;
            } else {
                let transaction = connection.unchecked_transaction()?;
                transaction.execute_batch(
                    "CREATE TABLE backup_runs (
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
                     snapshot_id TEXT CHECK (snapshot_id IS NULL OR length(snapshot_id) <= 128),
                     failed_items_omitted INTEGER NOT NULL DEFAULT 0
                         CHECK (failed_items_omitted >= 0)
                 );
                 CREATE INDEX backup_runs_completed
                     ON backup_runs(completed_at_ms DESC, id DESC);
                 CREATE TABLE backup_run_failed_items (
                     run_id INTEGER NOT NULL REFERENCES backup_runs(id) ON DELETE CASCADE,
                     ordinal INTEGER NOT NULL CHECK (ordinal >= 0 AND ordinal < 100),
                     item TEXT NOT NULL CHECK (
                         length(CAST(item AS BLOB)) BETWEEN 1 AND 4096
                         AND instr(item, char(0)) = 0
                         AND instr(item, char(10)) = 0
                         AND instr(item, char(13)) = 0
                     ),
                     PRIMARY KEY (run_id, ordinal)
                 );",
                )?;
                transaction.pragma_update(None, "user_version", HISTORY_SCHEMA_VERSION)?;
                transaction.commit()?;
            }
        }
        1 => migrate_legacy_schema(connection, true)?,
        HISTORY_SCHEMA_VERSION => {}
        _ => return Err(HistoryError::UnsupportedSchema(version)),
    }
    Ok(())
}

fn migrate_legacy_schema(
    connection: &Connection,
    add_omitted_column: bool,
) -> Result<(), HistoryError> {
    let transaction = connection.unchecked_transaction()?;
    if add_omitted_column {
        transaction.execute_batch(
            "ALTER TABLE backup_runs ADD COLUMN failed_items_omitted INTEGER NOT NULL DEFAULT 0
                 CHECK (failed_items_omitted >= 0);",
        )?;
    }
    transaction.execute_batch(
        "CREATE INDEX IF NOT EXISTS backup_runs_completed
             ON backup_runs(completed_at_ms DESC, id DESC);
         CREATE TABLE IF NOT EXISTS backup_run_failed_items (
                     run_id INTEGER NOT NULL REFERENCES backup_runs(id) ON DELETE CASCADE,
                     ordinal INTEGER NOT NULL CHECK (ordinal >= 0 AND ordinal < 100),
                     item TEXT NOT NULL CHECK (
                         length(CAST(item AS BLOB)) BETWEEN 1 AND 4096
                         AND instr(item, char(0)) = 0
                         AND instr(item, char(10)) = 0
                         AND instr(item, char(13)) = 0
                     ),
                     PRIMARY KEY (run_id, ordinal)
                 );",
    )?;
    transaction.pragma_update(None, "user_version", HISTORY_SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn table_exists(connection: &Connection, name: &str) -> Result<bool, HistoryError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            [name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn column_exists(connection: &Connection, table: &str, column: &str) -> Result<bool, HistoryError> {
    let mut statement = connection.prepare("SELECT name FROM pragma_table_info(?1)")?;
    let mut rows = statement.query([table])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(0)? == column {
            return Ok(true);
        }
    }
    Ok(false)
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

fn normalize_failure_details(items: Vec<String>, omitted: u64) -> StoredRunFailureDetails {
    let mut normalized = Vec::new();
    let mut omitted = omitted;
    for item in items {
        if normalized.contains(&item) {
            continue;
        }
        if normalized.len() >= MAX_BACKUP_FAILED_ITEMS || !is_safe_backup_failed_item(&item) {
            omitted = omitted.saturating_add(1);
        } else {
            normalized.push(item);
        }
    }
    let retained = u64::try_from(normalized.len()).unwrap_or(u64::MAX);
    let omitted = omitted.min((i64::MAX as u64).saturating_sub(retained));
    StoredRunFailureDetails {
        items: normalized,
        omitted,
    }
}

fn timestamp(milliseconds: i64) -> Result<DateTime<Utc>, HistoryError> {
    Utc.timestamp_millis_opt(milliseconds)
        .single()
        .ok_or(HistoryError::InvalidTimestamp(milliseconds))
}

fn to_sql_integer(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn normalize_sql_unsigned(value: u64) -> u64 {
    value.min(i64::MAX as u64)
}

fn from_sql_integer(value: i64, field: &'static str) -> Result<u64, HistoryError> {
    u64::try_from(value).map_err(|_| HistoryError::InvalidUnsignedValue(field))
}

#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("backup history database failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("backup history local access protection failed: {0}")]
    Protection(#[from] CredentialStoreError),
    #[error("backup history storage contains an unexpected file or reparse point")]
    UnsafeStorageEntry,
    #[error("backup history filesystem access failed: {0}")]
    FileSystem(#[from] io::Error),
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
            failed_items: Vec::new(),
            failed_items_omitted: 0,
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
    fn sensitive_failure_items_are_bounded_and_loaded_only_on_explicit_request() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = BackupHistoryStore::next_to_config(&directory.path().join("config.toml"));
        let mut partial = run(Utc::now(), BackupRunOutcome::SucceededWithWarnings);
        partial.error_code = Some("restic_partial_source".to_owned());
        partial.failed_items = vec![
            r"C:\Users\Example\private.txt".to_owned(),
            r"C:\Users\Example\private.txt".to_owned(),
            "C:\\Users\\Example\\line\nbreak.txt".to_owned(),
        ];
        partial.failed_items.extend(
            (0..MAX_BACKUP_FAILED_ITEMS).map(|index| format!(r"C:\Data\failure-{index}.txt")),
        );
        partial.failed_items_omitted = 2;

        let stored = store.append(partial).expect("stored warning run");

        assert_eq!(stored.failed_item_count, 104);
        let summaries = store.recent(1).expect("history summaries");
        assert_eq!(summaries[0].failed_item_count, 104);
        let details = store
            .failure_details(stored.id)
            .expect("failure detail query")
            .expect("run exists");
        assert_eq!(details.items.len(), MAX_BACKUP_FAILED_ITEMS);
        assert_eq!(details.items[0], r"C:\Users\Example\private.txt");
        assert_eq!(details.omitted, 4);
        assert!(!details.items.iter().any(|item| item.contains('\n')));
        let connection = store.initialized_connection().expect("connection");
        connection
            .pragma_update(None, "ignore_check_constraints", true)
            .expect("test corruption mode");
        connection
            .execute(
                "UPDATE backup_run_failed_items SET item = ?1
                 WHERE run_id = ?2 AND ordinal = 0",
                params![
                    "C:\\Users\\Example\\spoof\u{202e}txt.exe",
                    to_sql_integer(stored.id)
                ],
            )
            .expect("simulated corrupt path");
        drop(connection);
        let reread = store
            .failure_details(stored.id)
            .expect("corrupt detail query")
            .expect("run still exists");
        assert_eq!(reread.items.len(), MAX_BACKUP_FAILED_ITEMS - 1);
        assert_eq!(reread.omitted, 5);
        assert!(!reread.items.iter().any(|item| item.contains('\u{202e}')));
        assert!(
            store
                .failure_details(u64::MAX)
                .expect("missing query")
                .is_none()
        );
    }

    #[test]
    fn counters_are_normalized_consistently_with_sqlite_storage() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = BackupHistoryStore::next_to_config(&directory.path().join("config.toml"));
        let mut oversized = run(Utc::now(), BackupRunOutcome::Succeeded);
        oversized.files_processed = Some(u64::MAX);

        let stored = store.append(oversized).expect("stored run");
        let reloaded = store.recent(1).expect("reloaded run");

        assert_eq!(stored.files_processed, Some(i64::MAX as u64));
        assert_eq!(reloaded[0].files_processed, stored.files_processed);
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
        let failed_item_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM backup_run_failed_items", [], |row| {
                row.get(0)
            })
            .expect("failed item count");
        assert_eq!(failed_item_count, 0);
    }

    #[test]
    fn version_one_history_is_migrated_without_losing_runs() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("state.db");
        let connection = Connection::open(&path).expect("database");
        connection
            .execute_batch(
                "CREATE TABLE backup_runs (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     started_at_ms INTEGER NOT NULL,
                     completed_at_ms INTEGER NOT NULL,
                     outcome TEXT NOT NULL,
                     error_code TEXT,
                     files_processed INTEGER,
                     bytes_processed INTEGER,
                     data_added INTEGER,
                     snapshot_id TEXT
                 );
                 CREATE INDEX backup_runs_completed
                     ON backup_runs(completed_at_ms DESC, id DESC);
                 INSERT INTO backup_runs (
                     started_at_ms, completed_at_ms, outcome, error_code,
                     files_processed, bytes_processed, data_added, snapshot_id
                 ) VALUES (1, 2, 'succeeded', NULL, 3, 4, 5, 'abc123');
                 PRAGMA user_version = 1;",
            )
            .expect("version one schema");
        drop(connection);

        let store = BackupHistoryStore { path: path.clone() };
        let runs = store.recent(1).expect("migrated history");

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].failed_item_count, 0);
        assert_eq!(
            Connection::open(path)
                .expect("reopened database")
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .expect("schema version"),
            HISTORY_SCHEMA_VERSION
        );
    }

    #[test]
    fn interrupted_legacy_schema_creation_with_version_zero_is_recovered() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("state.db");
        let connection = Connection::open(&path).expect("database");
        connection
            .execute_batch(
                "CREATE TABLE backup_runs (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     started_at_ms INTEGER NOT NULL,
                     completed_at_ms INTEGER NOT NULL,
                     outcome TEXT NOT NULL,
                     error_code TEXT,
                     files_processed INTEGER,
                     bytes_processed INTEGER,
                     data_added INTEGER,
                     snapshot_id TEXT
                 );
                 CREATE INDEX backup_runs_completed
                     ON backup_runs(completed_at_ms DESC, id DESC);",
            )
            .expect("interrupted legacy schema");
        drop(connection);

        let store = BackupHistoryStore { path };
        let stored = store
            .append(run(Utc::now(), BackupRunOutcome::Succeeded))
            .expect("recovered schema accepts a run");

        assert_eq!(stored.failed_item_count, 0);
        assert_eq!(store.recent(1).expect("recovered history"), [stored]);
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
