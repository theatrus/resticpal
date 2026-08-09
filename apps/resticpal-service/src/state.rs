use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use resticpal_core::config::{EffectiveConfig, SecretEnvironmentVariable};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::atomic_file::{self, AtomicFileError};

const STATE_SCHEMA_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 2 * 1024 * 1024;

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

    pub fn load(&self) -> Result<ServiceStateSnapshot, StateStoreError> {
        let mut file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ServiceStateSnapshot::default());
            }
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
        Ok(ServiceStateSnapshot {
            last_success: state.last_success,
            repository_validation_required: state.repository_validation_required,
            verified_repository: state.verified_repository,
            repository_verified_at: state.repository_verified_at,
        })
    }

    pub fn save(&self, state: &ServiceStateSnapshot) -> Result<(), StateStoreError> {
        let mut contents = serde_json::to_vec_pretty(&PersistedScheduleState {
            schema_version: STATE_SCHEMA_VERSION,
            last_success: state.last_success,
            repository_validation_required: state.repository_validation_required,
            verified_repository: state.verified_repository.clone(),
            repository_verified_at: state.repository_verified_at,
        })?;
        contents.push(b'\n');
        atomic_file::replace(&self.path, &contents, "state")?;
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceStateSnapshot {
    pub last_success: Option<DateTime<Utc>>,
    repository_validation_required: bool,
    verified_repository: Option<RepositoryIdentity>,
    repository_verified_at: Option<DateTime<Utc>>,
}

impl ServiceStateSnapshot {
    pub fn repository_requires_validation(&self, config: &EffectiveConfig) -> bool {
        self.repository_validation_required
            || self
                .verified_repository
                .as_ref()
                .is_some_and(|verified| verified != &RepositoryIdentity::from_config(config))
    }

    pub fn require_repository_validation(&mut self) {
        self.repository_validation_required = true;
        self.verified_repository = None;
        self.repository_verified_at = None;
    }

    pub fn mark_repository_verified(
        &mut self,
        config: &EffectiveConfig,
        completed_at: DateTime<Utc>,
    ) {
        self.repository_validation_required = false;
        self.verified_repository = Some(RepositoryIdentity::from_config(config));
        self.repository_verified_at = Some(completed_at);
    }

    pub fn repository_verified_at(&self, config: &EffectiveConfig) -> Option<DateTime<Utc>> {
        (!self.repository_requires_validation(config))
            .then_some(self.repository_verified_at)
            .flatten()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryIdentity {
    url: Option<String>,
    options: BTreeMap<String, String>,
    secret_refs: BTreeMap<SecretEnvironmentVariable, String>,
}

impl RepositoryIdentity {
    fn from_config(config: &EffectiveConfig) -> Self {
        Self {
            url: config.repository.url.clone(),
            options: config.repository.options.clone(),
            secret_refs: config.repository.secret_refs.clone(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedScheduleState {
    schema_version: u32,
    last_success: Option<DateTime<Utc>>,
    #[serde(default)]
    repository_validation_required: bool,
    #[serde(default)]
    verified_repository: Option<RepositoryIdentity>,
    #[serde(default)]
    repository_verified_at: Option<DateTime<Utc>>,
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

        assert_eq!(
            store.load().expect("missing is valid"),
            ServiceStateSnapshot::default()
        );
    }

    #[test]
    fn version_one_schedule_only_state_remains_compatible() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config_path = directory.path().join("config.toml");
        let store = ScheduleStateStore::next_to_config(&config_path);
        fs::write(
            config_path.with_file_name("state.json"),
            br#"{"schema_version":1,"last_success":null}"#,
        )
        .expect("legacy state");

        assert_eq!(
            store.load().expect("legacy state loads"),
            ServiceStateSnapshot::default()
        );
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

        let mut state = ServiceStateSnapshot {
            last_success: Some(first),
            ..ServiceStateSnapshot::default()
        };
        store.save(&state).expect("first save");
        state.last_success = Some(second);
        store.save(&state).expect("replacement save");
        assert_eq!(store.load().expect("load"), state);
    }

    #[test]
    fn repository_verification_is_bound_to_connection_fields() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = ScheduleStateStore::next_to_config(&directory.path().join("config.toml"));
        let mut config = EffectiveConfig::default();
        config.repository.url = Some("local:C:/backup".to_owned());
        let mut state = ServiceStateSnapshot::default();
        state.require_repository_validation();
        assert!(state.repository_requires_validation(&config));

        let verified_at = Utc::now();
        state.mark_repository_verified(&config, verified_at);
        store.save(&state).expect("verified state");
        let loaded = store.load().expect("load verified state");
        assert!(!loaded.repository_requires_validation(&config));
        assert_eq!(loaded.repository_verified_at(&config), Some(verified_at));

        config.repository.url = Some("local:D:/replacement".to_owned());
        assert!(loaded.repository_requires_validation(&config));
    }
}
