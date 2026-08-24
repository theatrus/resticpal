use super::events::RuntimeEvent;
use super::helpers::*;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use chrono::{DateTime, Utc};
use resticpal_core::config::{EffectiveConfig, LocalConfig};
use resticpal_core::policy::{
    FieldResolution, ManagedPolicy, PolicyError, PolicyField, ResolvedConfig, resolve_config,
};
use resticpal_core::schedule::completion_deadline;
use resticpal_core::status::{BackupState, ServiceStatus, WaitingReason};
use resticpal_protocol::{
    DiagnosticLevel, RepositoryOperationKind, RepositoryOperationStatus, ResponsePayload,
};
use resticpal_windows::credentials::DpapiSecretStore;
use thiserror::Error;

use crate::config_store::{ConfigStoreError, LocalConfigStore};
use crate::diagnostics::DiagnosticLog;
use crate::history::BackupHistoryStore;
use crate::state::{ScheduleStateStore, ServiceStateSnapshot};

pub(super) struct RuntimeState {
    pub(super) status: ServiceStatus,
    pub(super) resumed_at: Option<DateTime<Utc>>,
    pub(super) not_before: Option<DateTime<Utc>>,
    pub(super) update_hold_until: Option<DateTime<Utc>>,
    pub(super) update_hold_previous_status: Option<(BackupState, Option<DateTime<Utc>>)>,
    pub(super) update_install_active: bool,
    pub(super) manual_requested: bool,
    pub(super) consecutive_failures: u32,
    pub(super) repository_operation: RepositoryOperationStatus,
    pub(super) service_state: ServiceStateSnapshot,
    pub(super) management_operation_active: bool,
}

#[derive(Default)]
struct RuntimeStores {
    state: Option<ScheduleStateStore>,
    history: Option<BackupHistoryStore>,
    config: Option<LocalConfigStore>,
    credentials: Option<DpapiSecretStore>,
    diagnostics: Option<DiagnosticLog>,
}

pub struct ServiceRuntime {
    pub(super) config: RwLock<EffectiveConfig>,
    pub(super) local_config: Mutex<LocalConfig>,
    pub(super) field_resolutions: RwLock<BTreeMap<PolicyField, FieldResolution>>,
    /// Serializes every local/effective configuration mutation, including
    /// managed-policy changes. Configuration writers replace whole snapshots,
    /// so they must not derive and commit candidates concurrently.
    pub(super) configuration_mutation: Mutex<()>,
    pub(super) config_store: Option<LocalConfigStore>,
    pub(super) credential_store: Option<DpapiSecretStore>,
    pub(super) state: Mutex<RuntimeState>,
    pub(super) state_store: Option<ScheduleStateStore>,
    pub(super) history_store: Option<BackupHistoryStore>,
    pub(super) diagnostics: Option<DiagnosticLog>,
    pub(super) events: Sender<RuntimeEvent>,
}

impl ServiceRuntime {
    pub fn load(path: &Path, events: Sender<RuntimeEvent>) -> Result<Self, RuntimeInitError> {
        Self::load_with_credentials(path, events, None)
    }

    pub fn load_with_credentials(
        path: &Path,
        events: Sender<RuntimeEvent>,
        credential_store: Option<DpapiSecretStore>,
    ) -> Result<Self, RuntimeInitError> {
        Self::load_with_credentials_and_policy(path, events, credential_store, None)
    }

    pub fn load_with_credentials_and_policy(
        path: &Path,
        events: Sender<RuntimeEvent>,
        credential_store: Option<DpapiSecretStore>,
        managed_policy: Option<&ManagedPolicy>,
    ) -> Result<Self, RuntimeInitError> {
        let config_store = LocalConfigStore::new(path);
        let diagnostics = DiagnosticLog::next_to_config(path);
        let local = config_store.load()?;
        let resolved = resolve_config(&EffectiveConfig::default(), &local, managed_policy)?;
        let state_store = ScheduleStateStore::next_to_config(path);
        let service_state = match state_store.load() {
            Ok(state) => state,
            Err(error) => {
                eprintln!(
                    "could not load service state next to {}: {error}; repository validation will be required",
                    path.display()
                );
                let _ = diagnostics.record(
                    DiagnosticLevel::Warning,
                    "state.load_failed",
                    "Service state could not be loaded; repository validation is required.",
                    Some("state_load_failed"),
                );
                let mut state = ServiceStateSnapshot::default();
                if resolved.effective.repository.url.is_some() {
                    state.require_repository_validation();
                }
                state
            }
        };
        let history_store = Some(BackupHistoryStore::next_to_config(path));
        Ok(Self::from_resolved_with_state(
            resolved,
            local,
            events,
            service_state,
            RuntimeStores {
                state: Some(state_store),
                history: history_store,
                config: Some(config_store),
                credentials: credential_store,
                diagnostics: Some(diagnostics),
            },
        ))
    }

    #[cfg(test)]
    pub fn from_resolved(resolved: ResolvedConfig, events: Sender<RuntimeEvent>) -> Self {
        let mut service_state = ServiceStateSnapshot::default();
        if resolved.effective.repository.url.is_some() {
            service_state.mark_repository_verified(&resolved.effective, Utc::now());
        }
        Self::from_resolved_with_state(
            resolved,
            LocalConfig::default(),
            events,
            service_state,
            RuntimeStores::default(),
        )
    }

    fn from_resolved_with_state(
        resolved: ResolvedConfig,
        local_config: LocalConfig,
        events: Sender<RuntimeEvent>,
        service_state: ServiceStateSnapshot,
        stores: RuntimeStores,
    ) -> Self {
        let RuntimeStores {
            state: state_store,
            history: history_store,
            config: config_store,
            credentials: credential_store,
            diagnostics,
        } = stores;
        let now = Utc::now();
        let configured = resolved.effective.is_configured();
        let last_success = service_state.last_success;
        let repository_operation = if service_state
            .repository_requires_validation(&resolved.effective)
        {
            RepositoryOperationStatus::ValidationRequired
        } else if let Some(completed_at) = service_state.repository_verified_at(&resolved.effective)
        {
            RepositoryOperationStatus::Succeeded {
                operation: RepositoryOperationKind::Validate,
                completed_at,
            }
        } else {
            RepositoryOperationStatus::NotRun
        };
        let repository_ready = repository_operation_allows_backup(&repository_operation);
        let next_deadline = (configured && repository_ready).then(|| {
            completion_deadline(
                last_success,
                now,
                resolved.effective.schedule.interval_hours,
            )
        });
        let status = ServiceStatus {
            state: if configured && !repository_ready {
                BackupState::Waiting {
                    reason: WaitingReason::RepositoryValidation,
                }
            } else if configured {
                BackupState::Idle
            } else {
                BackupState::Unconfigured
            },
            state_since: now,
            last_attempt: None,
            last_success,
            next_deadline,
            repository_display_name: resolved.effective.repository.display_name.clone(),
            repository_mode: resolved.effective.repository.mode,
            managed_revision: resolved.managed_revision,
            progress: None,
        };

        Self {
            config: RwLock::new(resolved.effective),
            local_config: Mutex::new(local_config),
            field_resolutions: RwLock::new(resolved.fields),
            configuration_mutation: Mutex::new(()),
            config_store,
            credential_store,
            state: Mutex::new(RuntimeState {
                status,
                resumed_at: None,
                not_before: None,
                update_hold_until: None,
                update_hold_previous_status: None,
                update_install_active: false,
                manual_requested: false,
                consecutive_failures: 0,
                repository_operation,
                service_state,
                management_operation_active: false,
            }),
            state_store,
            history_store,
            diagnostics,
            events,
        }
    }

    pub fn configuration_error(
        path: &Path,
        events: Sender<RuntimeEvent>,
        credential_store: Option<DpapiSecretStore>,
    ) -> Self {
        let now = Utc::now();
        let diagnostics = DiagnosticLog::next_to_config(path);
        let _ = diagnostics.record(
            DiagnosticLevel::Error,
            "configuration.invalid",
            "The service configuration is invalid.",
            Some("configuration_invalid"),
        );
        Self {
            config: RwLock::new(EffectiveConfig::default()),
            local_config: Mutex::new(LocalConfig::default()),
            field_resolutions: RwLock::new(BTreeMap::new()),
            configuration_mutation: Mutex::new(()),
            config_store: Some(LocalConfigStore::new(path)),
            credential_store,
            state: Mutex::new(RuntimeState {
                status: ServiceStatus {
                    state: BackupState::Failed {
                        code: "configuration_invalid".to_owned(),
                    },
                    state_since: now,
                    last_attempt: None,
                    last_success: None,
                    next_deadline: None,
                    repository_display_name: None,
                    repository_mode: Default::default(),
                    managed_revision: None,
                    progress: None,
                },
                resumed_at: None,
                not_before: None,
                update_hold_until: None,
                update_hold_previous_status: None,
                update_install_active: false,
                manual_requested: false,
                consecutive_failures: 0,
                repository_operation: RepositoryOperationStatus::NotRun,
                service_state: ServiceStateSnapshot::default(),
                management_operation_active: false,
            }),
            state_store: Some(ScheduleStateStore::next_to_config(path)),
            history_store: Some(BackupHistoryStore::next_to_config(path)),
            diagnostics: Some(diagnostics),
            events,
        }
    }

    pub fn record_diagnostic(
        &self,
        level: DiagnosticLevel,
        event_id: &'static str,
        message: &'static str,
        code: Option<&str>,
    ) {
        if let Some(log) = &self.diagnostics {
            let _ = log.record(level, event_id, message, code);
        }
    }

    pub fn status(&self) -> ServiceStatus {
        self.state_guard().status.clone()
    }

    pub fn config(&self) -> EffectiveConfig {
        self.config_read().clone()
    }

    pub(super) fn apply_configuration_status(
        state: &mut RuntimeState,
        config: &EffectiveConfig,
        now: DateTime<Utc>,
    ) {
        state.status.repository_display_name = config.repository.display_name.clone();
        state.status.repository_mode = config.repository.mode;
        if config.is_configured() {
            if !repository_operation_allows_backup(&state.repository_operation) {
                transition_state(
                    &mut state.status,
                    BackupState::Waiting {
                        reason: WaitingReason::RepositoryValidation,
                    },
                    now,
                );
                state.status.next_deadline = None;
                return;
            }
            let scheduled_deadline = completion_deadline(
                state.status.last_success,
                now,
                config.schedule.interval_hours,
            );
            state.status.next_deadline =
                Some(state.not_before.map_or(scheduled_deadline, |not_before| {
                    scheduled_deadline.max(not_before)
                }));
            if matches!(
                state.status.state,
                BackupState::Unconfigured
                    | BackupState::Waiting {
                        reason: WaitingReason::RepositoryValidation
                    }
            ) {
                transition_state(&mut state.status, BackupState::Idle, now);
            }
        } else {
            transition_state(&mut state.status, BackupState::Unconfigured, now);
            state.status.next_deadline = None;
            state.manual_requested = false;
            state.not_before = None;
        }
    }

    pub(super) fn field_locked(&self, field: PolicyField) -> bool {
        self.field_resolutions
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&field)
            .is_some_and(|resolution| resolution.locked)
    }

    pub(super) fn send_event(&self, event: RuntimeEvent, message: &str) -> ResponsePayload {
        match self.events.send(event) {
            Ok(()) => ResponsePayload::Accepted {
                message: message.to_owned(),
            },
            Err(_) => rejected(
                "service_stopping",
                "The backup service is stopping. Try again shortly.",
            ),
        }
    }

    pub(super) fn state_guard(&self) -> MutexGuard<'_, RuntimeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(super) fn config_read(&self) -> RwLockReadGuard<'_, EffectiveConfig> {
        self.config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(super) fn config_write(&self) -> RwLockWriteGuard<'_, EffectiveConfig> {
        self.config
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(super) fn local_config_guard(&self) -> MutexGuard<'_, LocalConfig> {
        self.local_config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Debug, Error)]
pub enum RuntimeInitError {
    #[error(transparent)]
    ConfigStore(#[from] ConfigStoreError),
    #[error(transparent)]
    Policy(#[from] PolicyError),
}
