use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::atomic_file::{self, AtomicFileError};

const STATE_SCHEMA_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 16 * 1024;

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
        let mut contents = serde_json::to_vec_pretty(&PersistedScheduleState {
            schema_version: STATE_SCHEMA_VERSION,
            last_success: Some(last_success),
        })?;
        contents.push(b'\n');
        atomic_file::replace(&self.path, &contents, "state")?;
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedScheduleState {
    schema_version: u32,
    last_success: Option<DateTime<Utc>>,
}

#[derive(Debug, Error)]
pub enum StateStoreError {
    #[error("schedule state exceeds its size limit")]
    TooLarge,
    #[error("unsupported schedule-state schema {0}")]
    UnsupportedSchema(u32),
    #[error("schedule-state I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("schedule state is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    AtomicFile(#[from] AtomicFileError),
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
