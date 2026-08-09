use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use windows::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::core::PCWSTR;

const STATE_SCHEMA_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 16 * 1024;
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct ScheduleStateStore {
    path: PathBuf,
}

impl ScheduleStateStore {
    pub fn next_to_config(config_path: &Path) -> Self {
        Self {
            path: config_path.with_file_name("state.json"),
        }
    }

    pub fn load_last_success(&self) -> Result<Option<DateTime<Utc>>, StateStoreError> {
        let mut file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut contents = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_STATE_BYTES + 1)
            .read_to_end(&mut contents)?;
        if contents.len() as u64 > MAX_STATE_BYTES {
            return Err(StateStoreError::TooLarge);
        }
        let state: PersistedScheduleState = serde_json::from_slice(&contents)?;
        if state.schema_version != STATE_SCHEMA_VERSION {
            return Err(StateStoreError::UnsupportedSchema(state.schema_version));
        }
        Ok(state.last_success)
    }

    pub fn save_last_success(&self, last_success: DateTime<Utc>) -> Result<(), StateStoreError> {
        let parent = self.path.parent().ok_or(StateStoreError::MissingParent)?;
        fs::create_dir_all(parent)?;
        let contents = serde_json::to_vec_pretty(&PersistedScheduleState {
            schema_version: STATE_SCHEMA_VERSION,
            last_success: Some(last_success),
        })?;
        let temporary = parent.join(format!(
            ".state-{}-{}.tmp",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = TemporaryFile(temporary.clone());
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(&contents)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        replace_file(&temporary, &self.path)?;
        cleanup.disarm();
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedScheduleState {
    schema_version: u32,
    last_success: Option<DateTime<Utc>>,
}

fn replace_file(source: &Path, target: &Path) -> Result<(), windows::core::Error> {
    let source = wide_null(source.as_os_str());
    let target = wide_null(target.as_os_str());
    // SAFETY: both buffers are live null-terminated paths, and the source is a
    // newly created file in the same directory as the target.
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(target.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

struct TemporaryFile(PathBuf);

impl TemporaryFile {
    fn disarm(mut self) {
        self.0 = PathBuf::new();
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.0);
        }
    }
}

#[derive(Debug, Error)]
pub enum StateStoreError {
    #[error("schedule state has no parent directory")]
    MissingParent,
    #[error("schedule state exceeds its size limit")]
    TooLarge,
    #[error("unsupported schedule-state schema {0}")]
    UnsupportedSchema(u32),
    #[error("schedule-state I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("schedule state is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("could not atomically replace schedule state: {0}")]
    Windows(#[from] windows::core::Error),
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn missing_state_means_no_previous_success() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = ScheduleStateStore::next_to_config(&directory.path().join("config.toml"));

        assert_eq!(store.load_last_success().expect("missing is valid"), None);
    }

    #[test]
    fn last_success_round_trips_and_is_atomically_replaced() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = ScheduleStateStore::next_to_config(&directory.path().join("config.toml"));
        let first = Utc
            .with_ymd_and_hms(2026, 8, 8, 10, 0, 0)
            .single()
            .expect("timestamp");
        let second = first + chrono::Duration::hours(1);

        store.save_last_success(first).expect("first save");
        store.save_last_success(second).expect("replacement save");
        assert_eq!(store.load_last_success().expect("load"), Some(second));
    }
}
