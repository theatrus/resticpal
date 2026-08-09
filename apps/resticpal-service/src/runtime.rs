use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use resticpal_core::config::{
    EffectiveConfig, LocalConfig, RepositoryMode, SecretEnvironmentVariable,
};
use resticpal_core::policy::{
    FieldResolution, ManagedPolicy, PolicyError, PolicyField, ResolvedConfig, resolve_config,
};
use resticpal_core::restic::ResticOperation;
use resticpal_core::schedule::{
    BackupTrigger, ScheduleBlocker, ScheduleDecision, SchedulerSnapshot, completion_deadline,
    decide,
};
use resticpal_core::status::{
    BackupPhase, BackupProgress, BackupRunOutcome, BackupState, ServiceStatus, WaitingReason,
};
use resticpal_protocol::{
    BackupSourcesView, RepositoryOperationKind, RepositoryOperationStatus, RepositorySecretUpdate,
    RepositoryView, Request, RequestCommand, Response, ResponsePayload, ScheduleView,
};
use resticpal_windows::credentials::DpapiSecretStore;
use resticpal_windows::named_pipe::ClientIdentity;
use resticpal_windows::user_profiles::discover_backup_sources;
use thiserror::Error;

use crate::conditions::SystemConditions;
use crate::config_store::{ConfigStoreError, LocalConfigStore};
use crate::executor::{BackupOutcome, BackupOutcomeKind, RepositoryOutcome, RepositoryOutcomeKind};
use crate::history::{BackupHistoryStore, CompletedBackupRun, MAX_HISTORY_RESULTS};
use crate::state::{ScheduleStateStore, ServiceStateSnapshot};

const CONDITION_RETRY_SECONDS: u64 = 60;
const INITIAL_FAILURE_RETRY_MINUTES: i64 = 5;
const MAX_FAILURE_BACKOFF_EXPONENT: u32 = 6;
const MAX_SECRET_UPDATES: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    Stop,
    Resume,
    PowerStatusChanged,
    TimeChanged,
    RunNow,
    Cancel,
    Deferred,
    ConfigurationChanged,
    RepositoryOperationRequested(RepositoryOperationKind),
    RepositoryOperationFinished {
        operation: RepositoryOperationKind,
        outcome: RepositoryOutcome,
    },
    BackupFinished(BackupOutcome),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleAction {
    None,
    Start { trigger: BackupTrigger },
}

struct RuntimeState {
    status: ServiceStatus,
    resumed_at: Option<DateTime<Utc>>,
    not_before: Option<DateTime<Utc>>,
    manual_requested: bool,
    consecutive_failures: u32,
    repository_operation: RepositoryOperationStatus,
    service_state: ServiceStateSnapshot,
}

#[derive(Default)]
struct RuntimeStores {
    state: Option<ScheduleStateStore>,
    history: Option<BackupHistoryStore>,
    config: Option<LocalConfigStore>,
    credentials: Option<DpapiSecretStore>,
}

pub struct ServiceRuntime {
    config: RwLock<EffectiveConfig>,
    local_config: Mutex<LocalConfig>,
    field_resolutions: RwLock<BTreeMap<PolicyField, FieldResolution>>,
    config_store: Option<LocalConfigStore>,
    credential_store: Option<DpapiSecretStore>,
    state: Mutex<RuntimeState>,
    state_store: Option<ScheduleStateStore>,
    history_store: Option<BackupHistoryStore>,
    events: Sender<RuntimeEvent>,
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
            config_store,
            credential_store,
            state: Mutex::new(RuntimeState {
                status,
                resumed_at: None,
                not_before: None,
                manual_requested: false,
                consecutive_failures: 0,
                repository_operation,
                service_state,
            }),
            state_store,
            history_store,
            events,
        }
    }

    pub fn configuration_error(
        path: &Path,
        events: Sender<RuntimeEvent>,
        credential_store: Option<DpapiSecretStore>,
    ) -> Self {
        let now = Utc::now();
        Self {
            config: RwLock::new(EffectiveConfig::default()),
            local_config: Mutex::new(LocalConfig::default()),
            field_resolutions: RwLock::new(BTreeMap::new()),
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
                manual_requested: false,
                consecutive_failures: 0,
                repository_operation: RepositoryOperationStatus::NotRun,
                service_state: ServiceStateSnapshot::default(),
            }),
            state_store: Some(ScheduleStateStore::next_to_config(path)),
            history_store: Some(BackupHistoryStore::next_to_config(path)),
            events,
        }
    }

    pub fn status(&self) -> ServiceStatus {
        self.state_guard().status.clone()
    }

    pub fn config(&self) -> EffectiveConfig {
        self.config_read().clone()
    }

    pub fn update_progress(&self, progress: BackupProgress) {
        let mut state = self.state_guard();
        let status = &mut state.status;
        if matches!(status.state, BackupState::Running { .. }) {
            status.state = BackupState::Running {
                phase: BackupPhase::Uploading,
            };
            status.progress = Some(progress);
        }
    }

    pub fn finish_backup(&self, outcome: &BackupOutcome) {
        self.finish_backup_at(Utc::now(), outcome);
    }

    fn finish_backup_at(&self, now: DateTime<Utc>, outcome: &BackupOutcome) {
        let interval_hours = self.config_read().schedule.interval_hours;
        let mut state = self.state_guard();
        let started_at = state.status.last_attempt.unwrap_or(now);
        let succeeded = matches!(
            outcome.kind,
            BackupOutcomeKind::Succeeded | BackupOutcomeKind::SucceededWithWarnings
        );
        state.status.state = match &outcome.kind {
            BackupOutcomeKind::Succeeded => BackupState::Succeeded,
            BackupOutcomeKind::SucceededWithWarnings => BackupState::SucceededWithWarnings,
            BackupOutcomeKind::Failed { code } => BackupState::Failed { code: code.clone() },
            BackupOutcomeKind::Cancelled => BackupState::Cancelled,
        };
        if succeeded {
            state.status.last_success = Some(now);
            state.service_state.last_success = Some(now);
            state.status.next_deadline = Some(completion_deadline(Some(now), now, interval_hours));
            state.not_before = None;
            state.consecutive_failures = 0;
        } else {
            if matches!(outcome.kind, BackupOutcomeKind::Failed { .. }) {
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            }
            let exponent = state
                .consecutive_failures
                .saturating_sub(1)
                .min(MAX_FAILURE_BACKOFF_EXPONENT);
            let retry_minutes = INITIAL_FAILURE_RETRY_MINUTES * i64::from(1_u32 << exponent);
            let retry_at = now + Duration::minutes(retry_minutes);
            state.not_before = Some(retry_at);
            state.status.next_deadline = Some(retry_at);
        }
        state.resumed_at = None;
        state.status.state_since = now;
        state.status.progress = None;
        let service_state = state.service_state.clone();
        drop(state);

        if let Some(store) = &self.history_store {
            let (run_outcome, error_code) = match &outcome.kind {
                BackupOutcomeKind::Succeeded => (BackupRunOutcome::Succeeded, None),
                BackupOutcomeKind::SucceededWithWarnings => {
                    (BackupRunOutcome::SucceededWithWarnings, None)
                }
                BackupOutcomeKind::Failed { code } => {
                    (BackupRunOutcome::Failed, Some(code.clone()))
                }
                BackupOutcomeKind::Cancelled => (BackupRunOutcome::Cancelled, None),
            };
            let summary = outcome.summary.as_ref();
            let run = CompletedBackupRun {
                started_at,
                completed_at: now,
                outcome: run_outcome,
                error_code,
                files_processed: summary.map(|value| value.files_processed),
                bytes_processed: summary.map(|value| value.bytes_processed),
                data_added: summary.map(|value| value.data_added),
                snapshot_id: summary.and_then(|value| value.snapshot_id.clone()),
            };
            if let Err(error) = store.append(run) {
                eprintln!("could not persist backup history: {error}");
            }
        }

        if succeeded
            && let Some(store) = &self.state_store
            && let Err(error) = store.save(&service_state)
        {
            eprintln!("could not persist the last successful backup time: {error}");
        }
    }

    pub fn record_resume(&self, now: DateTime<Utc>) {
        self.state_guard().resumed_at = Some(now);
    }

    pub fn evaluate_schedule(
        &self,
        now: DateTime<Utc>,
        conditions: SystemConditions,
    ) -> ScheduleAction {
        let config = self.config();
        if !config.is_configured() {
            return ScheduleAction::None;
        }

        let mut state = self.state_guard();
        if !repository_operation_allows_backup(&state.repository_operation) {
            return ScheduleAction::None;
        }
        let decision = decide(
            &config.schedule,
            &SchedulerSnapshot {
                now,
                last_success: state.status.last_success,
                resumed_at: state.resumed_at,
                not_before: state.not_before,
                manual_requested: state.manual_requested,
                backup_running: matches!(state.status.state, BackupState::Running { .. }),
                network_required: repository_requires_network(
                    config.repository.url.as_deref().unwrap_or_default(),
                ),
                network_available: conditions.network_available,
                on_battery: conditions.on_battery,
                metered_network: conditions.metered_network,
            },
        );

        match decision {
            ScheduleDecision::AlreadyRunning => ScheduleAction::None,
            ScheduleDecision::Start { trigger } => {
                state.status.state = BackupState::Running {
                    phase: BackupPhase::PreparingSnapshot,
                };
                state.status.state_since = now;
                state.status.last_attempt = Some(now);
                state.status.progress = None;
                state.status.next_deadline = None;
                state.manual_requested = false;
                state.resumed_at = None;
                state.not_before = None;
                ScheduleAction::Start { trigger }
            }
            ScheduleDecision::Idle { next_deadline } => {
                state.status.next_deadline = Some(next_deadline);
                if matches!(state.status.state, BackupState::Waiting { .. }) {
                    transition_state(&mut state.status, BackupState::Idle, now);
                }
                state.resumed_at = None;
                ScheduleAction::None
            }
            ScheduleDecision::Waiting {
                blockers, retry_at, ..
            } => {
                let reason = waiting_reason(blockers[0]);
                transition_state(&mut state.status, BackupState::Waiting { reason }, now);
                state.status.next_deadline = retry_at.or(state.not_before).or(Some(now));
                ScheduleAction::None
            }
        }
    }

    pub fn next_evaluation_delay(&self, now: DateTime<Utc>) -> StdDuration {
        let state = self.state_guard();
        if matches!(
            state.status.state,
            BackupState::Waiting {
                reason: resticpal_core::status::WaitingReason::Network
                    | resticpal_core::status::WaitingReason::Battery
                    | resticpal_core::status::WaitingReason::MeteredNetwork
            }
        ) {
            return StdDuration::from_secs(CONDITION_RETRY_SECONDS);
        }

        state.status.next_deadline.map_or_else(
            || StdDuration::from_secs(60 * 60),
            |deadline| {
                let milliseconds = (deadline - now).num_milliseconds().max(0);
                StdDuration::from_millis(u64::try_from(milliseconds).unwrap_or(u64::MAX))
            },
        )
    }

    pub fn handle_request(&self, request: Request, identity: ClientIdentity) -> Response {
        let request_id = request.request_id;
        let payload = match request.command {
            RequestCommand::GetStatus => ResponsePayload::Status {
                status: self.status(),
            },
            RequestCommand::GetRunHistory { limit } => {
                let limit = usize::from(limit);
                if !(1..=MAX_HISTORY_RESULTS).contains(&limit) {
                    rejected(
                        "invalid_history_limit",
                        "History requests must contain a limit from 1 through 100.",
                    )
                } else if let Some(store) = &self.history_store {
                    match store.recent(limit) {
                        Ok(runs) => ResponsePayload::RunHistory { runs },
                        Err(error) => {
                            eprintln!("could not read backup history: {error}");
                            rejected(
                                "history_unavailable",
                                "Backup history could not be read from local storage.",
                            )
                        }
                    }
                } else {
                    rejected(
                        "history_unavailable",
                        "Backup history storage is not available.",
                    )
                }
            }
            RequestCommand::GetBackupSources => {
                if identity.is_elevated_administrator {
                    ResponsePayload::BackupSources {
                        configuration: self.backup_sources_view(),
                    }
                } else {
                    administrator_required()
                }
            }
            RequestCommand::DiscoverBackupSources => {
                if !identity.is_elevated_administrator {
                    administrator_required()
                } else {
                    match discover_backup_sources() {
                        Ok(sources) => ResponsePayload::DiscoveredBackupSources { sources },
                        Err(error) => {
                            eprintln!("could not discover local user profile folders: {error}");
                            rejected(
                                "source_discovery_failed",
                                "Windows user folders could not be discovered.",
                            )
                        }
                    }
                }
            }
            RequestCommand::UpdateBackupSources { paths, exclusions } => {
                self.update_backup_sources(paths, exclusions, identity)
            }
            RequestCommand::GetRepository => {
                if identity.is_elevated_administrator {
                    ResponsePayload::Repository {
                        configuration: self.repository_view(),
                    }
                } else {
                    administrator_required()
                }
            }
            RequestCommand::UpdateRepository {
                display_name,
                url,
                mode,
                options,
                secret_updates,
            } => self.update_repository(display_name, url, mode, options, secret_updates, identity),
            RequestCommand::ValidateRepository => {
                self.begin_repository_operation(RepositoryOperationKind::Validate, identity)
            }
            RequestCommand::InitializeRepository => {
                self.begin_repository_operation(RepositoryOperationKind::Initialize, identity)
            }
            RequestCommand::GetSchedule => {
                if identity.is_elevated_administrator {
                    ResponsePayload::Schedule {
                        configuration: self.schedule_view(),
                    }
                } else {
                    administrator_required()
                }
            }
            RequestCommand::UpdateSchedule {
                interval_hours,
                wake_grace_seconds,
                wake_lock_timeout_seconds,
                allow_on_battery,
                allow_metered_network,
            } => self.update_schedule(
                interval_hours,
                wake_grace_seconds,
                wake_lock_timeout_seconds,
                allow_on_battery,
                allow_metered_network,
                identity,
            ),
            RequestCommand::RunBackupNow => {
                if !self.config_read().is_configured() {
                    rejected(
                        "not_configured",
                        "Configure backup sources and a repository first.",
                    )
                } else if !repository_operation_allows_backup(
                    &self.state_guard().repository_operation,
                ) {
                    rejected(
                        "repository_not_ready",
                        "Validate or initialize the repository before starting a backup.",
                    )
                } else {
                    let mut state = self.state_guard();
                    if matches!(state.status.state, BackupState::Running { .. }) {
                        rejected("already_running", "A backup is already running.")
                    } else {
                        state.manual_requested = true;
                        state.not_before = None;
                        match self.events.send(RuntimeEvent::RunNow) {
                            Ok(()) => ResponsePayload::Accepted {
                                message: "Backup request accepted.".to_owned(),
                            },
                            Err(_) => {
                                state.manual_requested = false;
                                rejected(
                                    "service_stopping",
                                    "The backup service is stopping. Try again shortly.",
                                )
                            }
                        }
                    }
                }
            }
            RequestCommand::CancelBackup => {
                if matches!(self.state_guard().status.state, BackupState::Running { .. }) {
                    self.send_event(RuntimeEvent::Cancel, "Cancellation requested.")
                } else {
                    rejected("not_running", "There is no running backup to cancel.")
                }
            }
            RequestCommand::DeferBackup { minutes } => {
                if !(1..=24 * 60).contains(&minutes) {
                    rejected(
                        "invalid_deferral",
                        "A deferral must be between one minute and 24 hours.",
                    )
                } else if !self.config_read().is_configured() {
                    rejected(
                        "not_configured",
                        "Configure backup sources and a repository first.",
                    )
                } else {
                    let mut state = self.state_guard();
                    if !repository_operation_allows_backup(&state.repository_operation) {
                        rejected(
                            "repository_not_ready",
                            "Validate or initialize the repository before deferring backups.",
                        )
                    } else if matches!(state.status.state, BackupState::Running { .. }) {
                        rejected("already_running", "A running backup cannot be deferred.")
                    } else {
                        let now = Utc::now();
                        let deadline = now
                            .checked_add_signed(Duration::minutes(i64::from(minutes)))
                            .unwrap_or(now);
                        state.manual_requested = false;
                        state.not_before = Some(deadline);
                        state.status.next_deadline = Some(deadline);
                        drop(state);
                        self.send_event(RuntimeEvent::Deferred, "Backup deferred.")
                    }
                }
            }
        };

        Response::new(request_id, payload)
    }

    fn backup_sources_view(&self) -> BackupSourcesView {
        let config = self.config_read();
        BackupSourcesView {
            paths: config.backup.paths.clone(),
            exclusions: config.backup.exclusions.clone(),
            paths_locked: self.field_locked(PolicyField::BackupPaths),
            exclusions_locked: self.field_locked(PolicyField::BackupExclusions),
        }
    }

    fn repository_view(&self) -> RepositoryView {
        let operation_status = self.state_guard().repository_operation.clone();
        let config = self.config_read();
        RepositoryView {
            display_name: config.repository.display_name.clone(),
            url: config.repository.url.clone(),
            mode: config.repository.mode,
            options: config.repository.options.clone(),
            configured_secrets: config.repository.secret_refs.keys().copied().collect(),
            operation_status,
            display_name_locked: self.field_locked(PolicyField::RepositoryDisplayName),
            url_locked: self.field_locked(PolicyField::RepositoryUrl),
            mode_locked: self.field_locked(PolicyField::RepositoryMode),
            options_locked: self.field_locked(PolicyField::RepositoryOptions),
            secrets_locked: self.field_locked(PolicyField::RepositorySecretRefs),
        }
    }

    fn begin_repository_operation(
        &self,
        operation: RepositoryOperationKind,
        identity: ClientIdentity,
    ) -> ResponsePayload {
        if !identity.is_elevated_administrator {
            return administrator_required();
        }
        let config = self.config();
        if config.repository.url.is_none() {
            return rejected(
                "repository_not_configured",
                "Save a repository URL before testing the connection.",
            );
        }
        if !config
            .repository
            .secret_refs
            .contains_key(&SecretEnvironmentVariable::ResticPassword)
        {
            return rejected(
                "repository_password_required",
                "Store the repository password before testing the connection.",
            );
        }
        if operation == RepositoryOperationKind::Initialize
            && !ResticOperation::Initialize.allowed_in(config.repository.mode)
        {
            return rejected(
                "append_only_initialization_forbidden",
                "Repository creation is disabled in append-only mode.",
            );
        }

        let mut state = self.state_guard();
        if matches!(state.status.state, BackupState::Running { .. }) {
            return rejected(
                "backup_running",
                "Wait for the active backup to finish before testing its repository.",
            );
        }
        if matches!(
            state.repository_operation,
            RepositoryOperationStatus::Running { .. }
        ) {
            return rejected(
                "repository_operation_running",
                "A repository operation is already running.",
            );
        }

        state.repository_operation = RepositoryOperationStatus::Running { operation };
        if config.is_configured() {
            transition_state(
                &mut state.status,
                BackupState::Waiting {
                    reason: WaitingReason::RepositoryValidation,
                },
                Utc::now(),
            );
            state.status.next_deadline = None;
        }
        state.service_state.require_repository_validation();
        if let Some(store) = &self.state_store
            && let Err(error) = store.save(&state.service_state)
        {
            eprintln!("could not persist required repository validation: {error}");
            state.repository_operation = RepositoryOperationStatus::Failed {
                operation,
                completed_at: Utc::now(),
                code: "state_save_failed".to_owned(),
            };
            return rejected(
                "state_save_failed",
                "Repository validation state could not be saved.",
            );
        }
        if self
            .events
            .send(RuntimeEvent::RepositoryOperationRequested(operation))
            .is_err()
        {
            state.repository_operation = RepositoryOperationStatus::Failed {
                operation,
                completed_at: Utc::now(),
                code: "service_stopping".to_owned(),
            };
            return rejected(
                "service_stopping",
                "The backup service is stopping. Try again shortly.",
            );
        }

        ResponsePayload::Accepted {
            message: match operation {
                RepositoryOperationKind::Validate => {
                    "Repository connection test started.".to_owned()
                }
                RepositoryOperationKind::Initialize => "Repository creation started.".to_owned(),
            },
        }
    }

    pub fn finish_repository_operation(
        &self,
        operation: RepositoryOperationKind,
        outcome: &RepositoryOutcome,
    ) {
        let completed_at = Utc::now();
        let config = self.config();
        let mut state = self.state_guard();
        if !matches!(
            state.repository_operation,
            RepositoryOperationStatus::Running {
                operation: running
            } if running == operation
        ) {
            return;
        }
        state.repository_operation = match &outcome.kind {
            RepositoryOutcomeKind::Succeeded => {
                state
                    .service_state
                    .mark_repository_verified(&config, completed_at);
                let save_result = self
                    .state_store
                    .as_ref()
                    .map_or(Ok(()), |store| store.save(&state.service_state));
                if let Err(error) = save_result {
                    eprintln!("could not persist successful repository validation: {error}");
                    RepositoryOperationStatus::Failed {
                        operation,
                        completed_at,
                        code: "state_save_failed".to_owned(),
                    }
                } else {
                    RepositoryOperationStatus::Succeeded {
                        operation,
                        completed_at,
                    }
                }
            }
            RepositoryOutcomeKind::Failed { code } => RepositoryOperationStatus::Failed {
                operation,
                completed_at,
                code: code.clone(),
            },
            RepositoryOutcomeKind::Cancelled => RepositoryOperationStatus::Failed {
                operation,
                completed_at,
                code: "repository_operation_cancelled".to_owned(),
            },
        };
        Self::apply_configuration_status(&mut state, &config, completed_at);
    }

    fn schedule_view(&self) -> ScheduleView {
        let config = self.config_read();
        ScheduleView {
            interval_hours: config.schedule.interval_hours,
            wake_grace_seconds: config.schedule.wake_grace_seconds,
            wake_lock_timeout_seconds: config.schedule.wake_lock_timeout_seconds,
            allow_on_battery: config.schedule.allow_on_battery,
            allow_metered_network: config.schedule.allow_metered_network,
            interval_hours_locked: self.field_locked(PolicyField::ScheduleIntervalHours),
            wake_grace_seconds_locked: self.field_locked(PolicyField::ScheduleWakeGraceSeconds),
            wake_lock_timeout_seconds_locked: self
                .field_locked(PolicyField::ScheduleWakeLockTimeoutSeconds),
            allow_on_battery_locked: self.field_locked(PolicyField::ScheduleAllowOnBattery),
            allow_metered_network_locked: self
                .field_locked(PolicyField::ScheduleAllowMeteredNetwork),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn update_schedule(
        &self,
        interval_hours: Option<u32>,
        wake_grace_seconds: Option<u64>,
        wake_lock_timeout_seconds: Option<u64>,
        allow_on_battery: Option<bool>,
        allow_metered_network: Option<bool>,
        identity: ClientIdentity,
    ) -> ResponsePayload {
        if !identity.is_elevated_administrator {
            return administrator_required();
        }
        if interval_hours.is_none()
            && wake_grace_seconds.is_none()
            && wake_lock_timeout_seconds.is_none()
            && allow_on_battery.is_none()
            && allow_metered_network.is_none()
        {
            return rejected(
                "no_configuration_changes",
                "No schedule changes were supplied.",
            );
        }
        if interval_hours.is_some() && self.field_locked(PolicyField::ScheduleIntervalHours)
            || wake_grace_seconds.is_some()
                && self.field_locked(PolicyField::ScheduleWakeGraceSeconds)
            || wake_lock_timeout_seconds.is_some()
                && self.field_locked(PolicyField::ScheduleWakeLockTimeoutSeconds)
            || allow_on_battery.is_some() && self.field_locked(PolicyField::ScheduleAllowOnBattery)
            || allow_metered_network.is_some()
                && self.field_locked(PolicyField::ScheduleAllowMeteredNetwork)
        {
            return rejected(
                "managed_field_locked",
                "One or more schedule fields are locked by managed policy.",
            );
        }

        let mut candidate = self.local_config_guard().clone();
        let mut effective = self.config();
        if let Some(value) = interval_hours {
            candidate.schedule.interval_hours = Some(value);
            effective.schedule.interval_hours = value;
        }
        if let Some(value) = wake_grace_seconds {
            candidate.schedule.wake_grace_seconds = Some(value);
            effective.schedule.wake_grace_seconds = value;
        }
        if let Some(value) = wake_lock_timeout_seconds {
            candidate.schedule.wake_lock_timeout_seconds = Some(value);
            effective.schedule.wake_lock_timeout_seconds = value;
        }
        if let Some(value) = allow_on_battery {
            candidate.schedule.allow_on_battery = Some(value);
            effective.schedule.allow_on_battery = value;
        }
        if let Some(value) = allow_metered_network {
            candidate.schedule.allow_metered_network = Some(value);
            effective.schedule.allow_metered_network = value;
        }
        if let Err(error) = effective.validate() {
            return rejected("invalid_schedule", &error.to_string());
        }

        let mut runtime_state = self.state_guard();
        if matches!(runtime_state.status.state, BackupState::Running { .. }) {
            return rejected(
                "backup_running",
                "Wait for the active backup to finish before changing its schedule.",
            );
        }
        if matches!(
            runtime_state.repository_operation,
            RepositoryOperationStatus::Running { .. }
        ) {
            return rejected(
                "repository_operation_running",
                "Wait for the repository operation to finish before changing the schedule.",
            );
        }
        if let Some(store) = &self.config_store
            && let Err(error) = store.save(&candidate)
        {
            eprintln!(
                "could not save local configuration to {}: {error}",
                store.path().display()
            );
            return rejected(
                "configuration_save_failed",
                "The local configuration could not be saved.",
            );
        }

        *self.local_config_guard() = candidate;
        *self.config_write() = effective.clone();
        Self::apply_configuration_status(&mut runtime_state, &effective, Utc::now());
        drop(runtime_state);
        let _ = self.events.send(RuntimeEvent::ConfigurationChanged);
        ResponsePayload::Accepted {
            message: "Backup schedule updated.".to_owned(),
        }
    }

    fn update_backup_sources(
        &self,
        paths: Option<Vec<std::path::PathBuf>>,
        exclusions: Option<Vec<String>>,
        identity: ClientIdentity,
    ) -> ResponsePayload {
        if !identity.is_elevated_administrator {
            return administrator_required();
        }
        if paths.is_none() && exclusions.is_none() {
            return rejected(
                "no_configuration_changes",
                "No backup-source changes were supplied.",
            );
        }
        if paths.is_some() && self.field_locked(PolicyField::BackupPaths)
            || exclusions.is_some() && self.field_locked(PolicyField::BackupExclusions)
        {
            return rejected(
                "managed_field_locked",
                "Backup sources are locked by managed policy.",
            );
        }
        let mut candidate = self.local_config_guard().clone();
        let mut effective = self.config();
        if let Some(paths) = paths {
            let paths = deduplicate_paths(paths);
            candidate.backup.paths = Some(paths.clone());
            effective.backup.paths = paths;
        }
        if let Some(exclusions) = exclusions {
            let exclusions = deduplicate_exclusions(exclusions);
            candidate.backup.exclusions = Some(exclusions.clone());
            effective.backup.exclusions = exclusions;
        }
        if let Err(error) = effective.validate() {
            return rejected("invalid_backup_sources", &error.to_string());
        }
        let mut runtime_state = self.state_guard();
        if matches!(runtime_state.status.state, BackupState::Running { .. }) {
            return rejected(
                "backup_running",
                "Wait for the active backup to finish before changing its sources.",
            );
        }
        if let Some(store) = &self.config_store
            && let Err(error) = store.save(&candidate)
        {
            eprintln!(
                "could not save local configuration to {}: {error}",
                store.path().display()
            );
            return rejected(
                "configuration_save_failed",
                "The local configuration could not be saved.",
            );
        }

        *self.local_config_guard() = candidate;
        *self.config_write() = effective.clone();
        Self::apply_configuration_status(&mut runtime_state, &effective, Utc::now());
        drop(runtime_state);
        let _ = self.events.send(RuntimeEvent::ConfigurationChanged);
        ResponsePayload::Accepted {
            message: "Backup sources updated.".to_owned(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn update_repository(
        &self,
        display_name: Option<String>,
        url: Option<String>,
        mode: Option<RepositoryMode>,
        options: Option<BTreeMap<String, String>>,
        secret_updates: Vec<RepositorySecretUpdate>,
        identity: ClientIdentity,
    ) -> ResponsePayload {
        let connection_changed = url.is_some() || options.is_some() || !secret_updates.is_empty();
        if !identity.is_elevated_administrator {
            return administrator_required();
        }
        if display_name.is_none()
            && url.is_none()
            && mode.is_none()
            && options.is_none()
            && secret_updates.is_empty()
        {
            return rejected(
                "no_configuration_changes",
                "No repository changes were supplied.",
            );
        }
        if display_name.is_some() && self.field_locked(PolicyField::RepositoryDisplayName)
            || url.is_some() && self.field_locked(PolicyField::RepositoryUrl)
            || mode.is_some() && self.field_locked(PolicyField::RepositoryMode)
            || options.is_some() && self.field_locked(PolicyField::RepositoryOptions)
            || !secret_updates.is_empty() && self.field_locked(PolicyField::RepositorySecretRefs)
        {
            return rejected(
                "managed_field_locked",
                "One or more repository fields are locked by managed policy.",
            );
        }
        if secret_updates.len() > MAX_SECRET_UPDATES {
            return rejected(
                "too_many_secret_updates",
                "Too many credential changes were supplied in one request.",
            );
        }
        let mut seen_variables = std::collections::BTreeSet::new();
        if secret_updates.iter().any(|update| {
            let variable = match update {
                RepositorySecretUpdate::Set { variable, .. }
                | RepositorySecretUpdate::Remove { variable } => *variable,
            };
            !seen_variables.insert(variable)
        }) {
            return rejected(
                "duplicate_secret_update",
                "Each credential may be changed at most once per request.",
            );
        }
        if !secret_updates.is_empty() && self.credential_store.is_none() {
            return rejected(
                "credential_store_unavailable",
                "The protected credential store is unavailable.",
            );
        }

        let mut candidate = self.local_config_guard().clone();
        let mut effective = self.config();
        if let Some(display_name) = display_name {
            let display_name = nonempty_trimmed(display_name);
            candidate.repository.display_name = display_name.clone();
            effective.repository.display_name = display_name;
        }
        if let Some(url) = url {
            let url = nonempty_trimmed(url);
            candidate.repository.url = url.clone();
            effective.repository.url = url;
        }
        if let Some(mode) = mode {
            candidate.repository.mode = Some(mode);
            effective.repository.mode = mode;
        }
        if let Some(options) = options {
            candidate.repository.options = Some(options.clone());
            effective.repository.options = options;
        }
        if let Err(error) = effective.validate() {
            return rejected("invalid_repository", &error.to_string());
        }

        let mut runtime_state = self.state_guard();
        if matches!(runtime_state.status.state, BackupState::Running { .. }) {
            return rejected(
                "backup_running",
                "Wait for the active backup to finish before changing its repository.",
            );
        }
        if matches!(
            runtime_state.repository_operation,
            RepositoryOperationStatus::Running { .. }
        ) {
            return rejected(
                "repository_operation_running",
                "Wait for the repository operation to finish before changing its settings.",
            );
        }

        let mut created_references = Vec::new();
        let mut retired_references = std::collections::BTreeSet::new();
        if !secret_updates.is_empty() {
            let store = self
                .credential_store
                .as_ref()
                .expect("credential store availability was checked");
            let local_references = candidate
                .repository
                .secret_refs
                .get_or_insert_with(BTreeMap::new);
            for update in secret_updates {
                let result = match update {
                    RepositorySecretUpdate::Set { variable, value } => store
                        .put_new(variable.reference_prefix(), value.as_bytes())
                        .map(|reference| {
                            if let Some(old) = effective
                                .repository
                                .secret_refs
                                .insert(variable, reference.clone())
                            {
                                retired_references.insert(old);
                            }
                            local_references.insert(variable, reference.clone());
                            created_references.push(reference);
                        }),
                    RepositorySecretUpdate::Remove { variable } => {
                        if let Some(old) = effective.repository.secret_refs.remove(&variable) {
                            retired_references.insert(old);
                        }
                        local_references.remove(&variable);
                        Ok(())
                    }
                };
                if let Err(error) = result {
                    eprintln!("could not store a repository credential: {error}");
                    cleanup_credentials(store, &created_references);
                    return rejected(
                        "credential_save_failed",
                        "A repository credential could not be stored securely.",
                    );
                }
            }
            retired_references.retain(|reference| {
                !effective
                    .repository
                    .secret_refs
                    .values()
                    .any(|active| active == reference)
            });
        }
        if let Err(error) = effective.validate() {
            if let Some(store) = &self.credential_store {
                cleanup_credentials(store, &created_references);
            }
            return rejected("invalid_repository", &error.to_string());
        }
        let mut next_service_state = runtime_state.service_state.clone();
        if connection_changed {
            next_service_state.require_repository_validation();
            if let Some(store) = &self.state_store
                && let Err(error) = store.save(&next_service_state)
            {
                eprintln!("could not persist required repository validation: {error}");
                if let Some(credentials) = &self.credential_store {
                    cleanup_credentials(credentials, &created_references);
                }
                return rejected(
                    "state_save_failed",
                    "Repository validation state could not be saved.",
                );
            }
        }
        if let Some(store) = &self.config_store
            && let Err(error) = store.save(&candidate)
        {
            eprintln!(
                "could not save local configuration to {}: {error}",
                store.path().display()
            );
            if let Some(credentials) = &self.credential_store {
                cleanup_credentials(credentials, &created_references);
            }
            return rejected(
                "configuration_save_failed",
                "The local configuration could not be saved.",
            );
        }

        *self.local_config_guard() = candidate;
        *self.config_write() = effective.clone();
        if connection_changed {
            runtime_state.repository_operation = RepositoryOperationStatus::ValidationRequired;
            runtime_state.service_state = next_service_state;
        }
        Self::apply_configuration_status(&mut runtime_state, &effective, Utc::now());
        drop(runtime_state);
        if let Some(store) = &self.credential_store {
            for reference in retired_references {
                if let Err(error) = store.remove(&reference) {
                    eprintln!("could not retire an old repository credential: {error}");
                }
            }
        }
        let _ = self.events.send(RuntimeEvent::ConfigurationChanged);
        ResponsePayload::Accepted {
            message: "Repository settings updated.".to_owned(),
        }
    }

    fn apply_configuration_status(
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

    fn field_locked(&self, field: PolicyField) -> bool {
        self.field_resolutions
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&field)
            .is_some_and(|resolution| resolution.locked)
    }

    pub fn apply_managed_policy(&self, policy: &ManagedPolicy) -> Result<bool, PolicyError> {
        let local = self.local_config_guard().clone();
        let resolved = resolve_config(&EffectiveConfig::default(), &local, Some(policy))?;
        let next_config = resolved.effective;
        let now = Utc::now();
        let mut state = self.state_guard();
        if matches!(state.status.state, BackupState::Running { .. })
            || matches!(
                state.repository_operation,
                RepositoryOperationStatus::Running { .. }
            )
        {
            return Ok(false);
        }

        if state
            .service_state
            .repository_requires_validation(&next_config)
        {
            state.service_state.require_repository_validation();
            if let Some(store) = &self.state_store
                && let Err(error) = store.save(&state.service_state)
            {
                eprintln!("could not persist repository validation requirement: {error}");
            }
            state.repository_operation = RepositoryOperationStatus::ValidationRequired;
        }

        *self.config_write() = next_config.clone();
        *self
            .field_resolutions
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = resolved.fields;
        state.status.managed_revision = resolved.managed_revision;
        Self::apply_configuration_status(&mut state, &next_config, now);
        drop(state);
        let _ = self.events.send(RuntimeEvent::ConfigurationChanged);
        Ok(true)
    }

    fn send_event(&self, event: RuntimeEvent, message: &str) -> ResponsePayload {
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

    fn state_guard(&self) -> MutexGuard<'_, RuntimeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn config_read(&self) -> RwLockReadGuard<'_, EffectiveConfig> {
        self.config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn config_write(&self) -> RwLockWriteGuard<'_, EffectiveConfig> {
        self.config
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn local_config_guard(&self) -> MutexGuard<'_, LocalConfig> {
        self.local_config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn deduplicate_paths(paths: Vec<std::path::PathBuf>) -> Vec<std::path::PathBuf> {
    let mut seen = std::collections::BTreeSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.to_string_lossy().to_lowercase()))
        .collect()
}

fn deduplicate_exclusions(exclusions: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    exclusions
        .into_iter()
        .filter(|exclusion| seen.insert(exclusion.clone()))
        .collect()
}

fn nonempty_trimmed(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn cleanup_credentials(store: &DpapiSecretStore, references: &[String]) {
    for reference in references {
        let _ = store.remove(reference);
    }
}

fn repository_operation_allows_backup(status: &RepositoryOperationStatus) -> bool {
    matches!(status, RepositoryOperationStatus::Succeeded { .. })
}

fn transition_state(status: &mut ServiceStatus, next: BackupState, now: DateTime<Utc>) {
    if status.state != next {
        status.state = next;
        status.state_since = now;
    }
}

fn waiting_reason(blocker: ScheduleBlocker) -> resticpal_core::status::WaitingReason {
    match blocker {
        ScheduleBlocker::WakeGrace => resticpal_core::status::WaitingReason::WakeGrace,
        ScheduleBlocker::NetworkUnavailable => resticpal_core::status::WaitingReason::Network,
        ScheduleBlocker::BatteryDisallowed => resticpal_core::status::WaitingReason::Battery,
        ScheduleBlocker::MeteredNetworkDisallowed => {
            resticpal_core::status::WaitingReason::MeteredNetwork
        }
    }
}

fn repository_requires_network(repository: &str) -> bool {
    let repository = repository.trim();
    let local = repository
        .strip_prefix("local:")
        .or_else(|| repository.strip_prefix("LOCAL:"));
    if let Some(path) = local {
        return path.starts_with(r"\\") || path.starts_with("//");
    }
    if repository.starts_with(r"\\") || repository.starts_with("//") {
        return true;
    }
    if repository.len() >= 3
        && repository.as_bytes()[0].is_ascii_alphabetic()
        && repository.as_bytes()[1] == b':'
        && matches!(repository.as_bytes()[2], b'\\' | b'/')
    {
        return false;
    }
    repository.contains(':')
}

fn rejected(code: &str, message: &str) -> ResponsePayload {
    ResponsePayload::Rejected {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

fn administrator_required() -> ResponsePayload {
    rejected(
        "administrator_required",
        "Open resticpal as an administrator to change machine backup settings.",
    )
}

#[derive(Debug, Error)]
pub enum RuntimeInitError {
    #[error(transparent)]
    ConfigStore(#[from] ConfigStoreError),
    #[error(transparent)]
    Policy(#[from] PolicyError),
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration as StdDuration;

    use resticpal_core::config::{
        BackupConfig, LocalBackupConfig, LocalRepositoryConfig, RepositoryConfig,
        SecretEnvironmentVariable,
    };
    use resticpal_core::policy::{Managed, ManagedSchedulePolicy};
    use resticpal_protocol::{PROTOCOL_VERSION, SecretValue};
    use resticpal_windows::credentials::CredentialStoreError;
    use resticpal_windows::named_pipe::{NamedPipeClient, NamedPipeServer};

    use super::*;
    use crate::executor::BackupSummary;

    const USER: ClientIdentity = ClientIdentity {
        is_elevated_administrator: false,
    };
    const ADMIN: ClientIdentity = ClientIdentity {
        is_elevated_administrator: true,
    };
    static NEXT_PIPE: AtomicU64 = AtomicU64::new(1);

    fn runtime(configured: bool) -> (ServiceRuntime, mpsc::Receiver<RuntimeEvent>) {
        let (events, receiver) = mpsc::channel();
        let mut effective = EffectiveConfig::default();
        if configured {
            effective.backup = BackupConfig {
                paths: vec![PathBuf::from(r"C:\Users\Example\Documents")],
                exclusions: Vec::new(),
            };
            effective.repository = RepositoryConfig {
                url: Some("local:C:/backup".to_owned()),
                ..RepositoryConfig::default()
            };
        }
        let resolved = ResolvedConfig {
            effective,
            managed_revision: None,
            fields: Default::default(),
        };
        (ServiceRuntime::from_resolved(resolved, events), receiver)
    }

    fn available_conditions() -> SystemConditions {
        SystemConditions {
            network_available: true,
            on_battery: false,
            metered_network: false,
        }
    }

    #[test]
    fn status_reports_an_unconfigured_service() {
        let (runtime, _events) = runtime(false);
        let response = runtime.handle_request(Request::new(1, RequestCommand::GetStatus), USER);

        assert!(matches!(
            response.payload,
            ResponsePayload::Status {
                status: ServiceStatus {
                    state: BackupState::Unconfigured,
                    ..
                }
            }
        ));
    }

    #[test]
    fn run_now_queues_a_scheduler_evaluation_that_starts_the_executor() {
        let (runtime, events) = runtime(true);
        let response = runtime.handle_request(Request::new(2, RequestCommand::RunBackupNow), USER);

        assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
        assert_eq!(events.recv().expect("runtime event"), RuntimeEvent::RunNow);
        assert_eq!(
            runtime.evaluate_schedule(Utc::now(), available_conditions()),
            ScheduleAction::Start {
                trigger: BackupTrigger::Manual
            }
        );
        assert!(matches!(
            runtime.status().state,
            BackupState::Running {
                phase: BackupPhase::PreparingSnapshot
            }
        ));
    }

    #[test]
    fn run_now_is_rejected_until_configuration_is_complete() {
        let (runtime, _events) = runtime(false);
        let response = runtime.handle_request(Request::new(3, RequestCommand::RunBackupNow), USER);

        assert!(matches!(
            response.payload,
            ResponsePayload::Rejected { ref code, .. } if code == "not_configured"
        ));
    }

    #[test]
    fn deferral_requires_a_configured_and_verified_repository() {
        let (unconfigured, _events) = runtime(false);
        assert!(matches!(
            unconfigured
                .handle_request(
                    Request::new(30, RequestCommand::DeferBackup { minutes: 30 }),
                    USER,
                )
                .payload,
            ResponsePayload::Rejected { ref code, .. } if code == "not_configured"
        ));

        let directory = tempfile::tempdir().expect("temporary directory");
        let config_path = directory.path().join("config.toml");
        LocalConfigStore::new(&config_path)
            .save(&LocalConfig {
                backup: LocalBackupConfig {
                    paths: Some(vec![PathBuf::from(r"C:\Data")]),
                    exclusions: Some(Vec::new()),
                },
                repository: LocalRepositoryConfig {
                    url: Some("local:C:/backup".to_owned()),
                    ..LocalRepositoryConfig::default()
                },
                ..LocalConfig::default()
            })
            .expect("configured local file");
        let (events, _receiver) = mpsc::channel();
        let unverified = ServiceRuntime::load(&config_path, events).expect("runtime");

        assert!(matches!(
            unverified
                .handle_request(
                    Request::new(31, RequestCommand::DeferBackup { minutes: 30 }),
                    USER,
                )
                .payload,
            ResponsePayload::Rejected { ref code, .. } if code == "repository_not_ready"
        ));
    }

    #[test]
    fn deferral_updates_the_reported_deadline() {
        let (runtime, events) = runtime(true);
        let before = Utc::now();
        let response = runtime.handle_request(
            Request::new(4, RequestCommand::DeferBackup { minutes: 30 }),
            USER,
        );

        assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
        assert_eq!(
            events.recv().expect("runtime event should be sent"),
            RuntimeEvent::Deferred
        );
        let deadline = runtime
            .status()
            .next_deadline
            .expect("deferral sets a deadline");
        assert!(deadline >= before + Duration::minutes(30));
    }

    #[test]
    fn progress_and_success_update_the_canonical_status() {
        let (runtime, events) = runtime(true);
        let response = runtime.handle_request(Request::new(7, RequestCommand::RunBackupNow), USER);
        assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
        assert_eq!(events.recv().expect("run event"), RuntimeEvent::RunNow);
        assert!(matches!(
            runtime.evaluate_schedule(Utc::now(), available_conditions()),
            ScheduleAction::Start { .. }
        ));

        runtime.update_progress(BackupProgress {
            percent_done: Some(50),
            files_done: 5,
            total_files: Some(10),
            bytes_done: 500,
            total_bytes: Some(1_000),
            error_count: 0,
        });
        assert!(matches!(
            runtime.status(),
            ServiceStatus {
                state: BackupState::Running {
                    phase: BackupPhase::Uploading
                },
                progress: Some(BackupProgress {
                    percent_done: Some(50),
                    ..
                }),
                ..
            }
        ));

        let before = Utc::now();
        let interval_hours = runtime.config().schedule.interval_hours;
        runtime.finish_backup(&BackupOutcome::succeeded(BackupSummary {
            files_processed: 10,
            bytes_processed: 1_000,
            data_added: 200,
            snapshot_id: Some("snapshot".to_owned()),
        }));
        let status = runtime.status();
        assert_eq!(status.state, BackupState::Succeeded);
        assert!(status.last_success.is_some_and(|value| value >= before));
        assert!(
            status.next_deadline.is_some_and(|value| {
                value >= before + Duration::hours(i64::from(interval_hours))
            })
        );
        assert_eq!(status.progress, None);
    }

    #[test]
    fn cancellation_request_is_forwarded_only_while_running() {
        let (runtime, events) = runtime(true);
        let idle_cancel =
            runtime.handle_request(Request::new(8, RequestCommand::CancelBackup), USER);
        assert!(matches!(
            idle_cancel.payload,
            ResponsePayload::Rejected { ref code, .. } if code == "not_running"
        ));

        let _ = runtime.handle_request(Request::new(9, RequestCommand::RunBackupNow), USER);
        assert_eq!(events.recv().expect("run event"), RuntimeEvent::RunNow);
        assert!(matches!(
            runtime.evaluate_schedule(Utc::now(), available_conditions()),
            ScheduleAction::Start { .. }
        ));
        let running_cancel =
            runtime.handle_request(Request::new(10, RequestCommand::CancelBackup), USER);
        assert!(matches!(
            running_cancel.payload,
            ResponsePayload::Accepted { .. }
        ));
        assert_eq!(events.recv().expect("cancel event"), RuntimeEvent::Cancel);
    }

    #[test]
    fn request_constructor_uses_current_protocol() {
        assert_eq!(
            Request::new(5, RequestCommand::GetStatus).protocol_version,
            PROTOCOL_VERSION
        );
    }

    #[test]
    fn named_pipe_exposes_the_runtime_status() {
        let (runtime, _events) = runtime(true);
        let runtime = Arc::new(runtime);
        let server_runtime = Arc::clone(&runtime);
        let pipe_name = format!(
            r"\\.\pipe\ResticPal.RuntimeTest.{}.{}",
            std::process::id(),
            NEXT_PIPE.fetch_add(1, Ordering::Relaxed)
        );
        let server_name = pipe_name.clone();
        let server = thread::spawn(move || {
            NamedPipeServer::new(&server_name)
                .expect("server should initialize")
                .serve_one(|request, identity| server_runtime.handle_request(request, identity))
                .expect("service runtime should handle the request");
        });

        let response = NamedPipeClient::request_at(
            &pipe_name,
            &Request::new(6, RequestCommand::GetStatus),
            StdDuration::from_secs(5),
        )
        .expect("client should receive service status");
        server.join().expect("server should stop after one request");

        assert!(matches!(
            response.payload,
            ResponsePayload::Status {
                status: ServiceStatus {
                    state: BackupState::Idle,
                    ..
                }
            }
        ));
    }

    #[test]
    fn overdue_startup_runs_without_a_manual_request() {
        let (runtime, _events) = runtime(true);

        assert_eq!(
            runtime.evaluate_schedule(Utc::now(), available_conditions()),
            ScheduleAction::Start {
                trigger: BackupTrigger::Scheduled
            }
        );
    }

    #[test]
    fn resume_catch_up_waits_for_the_configured_grace_period() {
        let (runtime, _events) = runtime(true);
        let resumed_at = Utc::now();
        runtime.record_resume(resumed_at);

        assert_eq!(
            runtime.evaluate_schedule(resumed_at, available_conditions()),
            ScheduleAction::None
        );
        assert!(matches!(
            runtime.status().state,
            BackupState::Waiting {
                reason: resticpal_core::status::WaitingReason::WakeGrace
            }
        ));
        assert_eq!(
            runtime.evaluate_schedule(resumed_at + Duration::seconds(300), available_conditions()),
            ScheduleAction::Start {
                trigger: BackupTrigger::ResumeCatchUp
            }
        );
    }

    #[test]
    fn battery_policy_blocks_until_power_conditions_change() {
        let (runtime, _events) = runtime(true);
        runtime.config_write().schedule.allow_on_battery = false;
        let now = Utc::now();

        assert_eq!(
            runtime.evaluate_schedule(
                now,
                SystemConditions {
                    on_battery: true,
                    ..available_conditions()
                }
            ),
            ScheduleAction::None
        );
        assert!(matches!(
            runtime.status().state,
            BackupState::Waiting {
                reason: resticpal_core::status::WaitingReason::Battery
            }
        ));
        assert!(matches!(
            runtime.evaluate_schedule(now, available_conditions()),
            ScheduleAction::Start { .. }
        ));
    }

    #[test]
    fn failed_backups_use_bounded_exponential_retry_delays() {
        let (runtime, _events) = runtime(true);
        let now = Utc::now();
        assert!(matches!(
            runtime.evaluate_schedule(now, available_conditions()),
            ScheduleAction::Start { .. }
        ));
        runtime.finish_backup_at(now, &BackupOutcome::failed("repository_unreachable"));

        assert_eq!(
            runtime.status().next_deadline,
            Some(now + Duration::minutes(5))
        );
        assert_eq!(
            runtime.evaluate_schedule(now, available_conditions()),
            ScheduleAction::None
        );
        assert!(matches!(runtime.status().state, BackupState::Failed { .. }));

        assert!(matches!(
            runtime.evaluate_schedule(now + Duration::minutes(5), available_conditions()),
            ScheduleAction::Start { .. }
        ));
        runtime.finish_backup_at(
            now + Duration::minutes(5),
            &BackupOutcome::failed("repository_unreachable"),
        );
        assert_eq!(
            runtime.status().next_deadline,
            Some(now + Duration::minutes(15))
        );

        for _ in 0..10 {
            runtime.finish_backup_at(now, &BackupOutcome::failed("repository_unreachable"));
        }
        assert_eq!(
            runtime.status().next_deadline,
            Some(now + Duration::minutes(320))
        );
    }

    #[test]
    fn repository_network_detection_distinguishes_local_and_remote_targets() {
        assert!(!repository_requires_network(r"C:\Backups\restic"));
        assert!(!repository_requires_network("local:C:/Backups/restic"));
        assert!(repository_requires_network(r"\\server\share\restic"));
        assert!(repository_requires_network("//server/share/restic"));
        assert!(repository_requires_network("s3:s3.example.test/bucket"));
        assert!(repository_requires_network(
            "sftp:user@example.test:/backup"
        ));
    }

    #[test]
    fn backup_source_configuration_requires_an_elevated_administrator() {
        let (runtime, _events) = runtime(true);

        for command in [
            RequestCommand::GetBackupSources,
            RequestCommand::DiscoverBackupSources,
            RequestCommand::UpdateBackupSources {
                paths: Some(vec![PathBuf::from(r"D:\Data")]),
                exclusions: Some(Vec::new()),
            },
        ] {
            let response = runtime.handle_request(Request::new(20, command), USER);
            assert!(matches!(
                response.payload,
                ResponsePayload::Rejected { ref code, .. } if code == "administrator_required"
            ));
        }
    }

    #[test]
    fn administrator_source_update_is_persisted_and_applied_live() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config_path = directory.path().join("config.toml");
        let store = LocalConfigStore::new(&config_path);
        let local = LocalConfig {
            repository: resticpal_core::config::LocalRepositoryConfig {
                url: Some("local:C:/backup".to_owned()),
                ..Default::default()
            },
            ..LocalConfig::default()
        };
        store.save(&local).expect("initial config");
        let (events, receiver) = mpsc::channel();
        let runtime = ServiceRuntime::load(&config_path, events).expect("runtime should load");

        let response = runtime.handle_request(
            Request::new(
                21,
                RequestCommand::UpdateBackupSources {
                    paths: Some(vec![PathBuf::from(r"D:\Data"), PathBuf::from(r"d:\data")]),
                    exclusions: Some(vec!["**/cache/**".to_owned(), "**/cache/**".to_owned()]),
                },
            ),
            ADMIN,
        );

        assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
        assert_eq!(
            receiver.recv().expect("configuration event"),
            RuntimeEvent::ConfigurationChanged
        );
        assert_eq!(runtime.config().backup.paths, [PathBuf::from(r"D:\Data")]);
        assert_eq!(runtime.config().backup.exclusions, ["**/cache/**"]);
        assert!(matches!(
            runtime.status().state,
            BackupState::Waiting {
                reason: WaitingReason::RepositoryValidation
            }
        ));
        let persisted = store.load().expect("updated config should load");
        assert_eq!(
            persisted.backup.paths,
            Some(vec![PathBuf::from(r"D:\Data")])
        );
    }

    #[test]
    fn invalid_configuration_can_be_repaired_and_persisted_over_ipc() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config_path = directory.path().join("config.toml");
        std::fs::write(&config_path, "this is not valid TOML = [").expect("invalid config");
        let (events, receiver) = mpsc::channel();
        let runtime = ServiceRuntime::configuration_error(&config_path, events, None);

        let response = runtime.handle_request(
            Request::new(
                32,
                RequestCommand::UpdateBackupSources {
                    paths: Some(vec![PathBuf::from(r"C:\Data")]),
                    exclusions: Some(Vec::new()),
                },
            ),
            ADMIN,
        );

        assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
        assert_eq!(
            receiver.recv().expect("configuration event"),
            RuntimeEvent::ConfigurationChanged
        );
        let repaired = LocalConfigStore::new(&config_path)
            .load()
            .expect("repaired configuration");
        assert_eq!(repaired.backup.paths, Some(vec![PathBuf::from(r"C:\Data")]));
        assert!(matches!(runtime.status().state, BackupState::Unconfigured));
    }

    #[test]
    fn invalid_source_update_does_not_replace_configuration() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config_path = directory.path().join("config.toml");
        let store = LocalConfigStore::new(&config_path);
        store.save(&LocalConfig::default()).expect("initial config");
        let before = std::fs::read(&config_path).expect("initial bytes");
        let (events, _receiver) = mpsc::channel();
        let runtime = ServiceRuntime::load(&config_path, events).expect("runtime should load");

        let response = runtime.handle_request(
            Request::new(
                22,
                RequestCommand::UpdateBackupSources {
                    paths: Some(vec![PathBuf::from("relative")]),
                    exclusions: Some(Vec::new()),
                },
            ),
            ADMIN,
        );

        assert!(matches!(
            response.payload,
            ResponsePayload::Rejected { ref code, .. } if code == "invalid_backup_sources"
        ));
        assert_eq!(std::fs::read(config_path).expect("config remains"), before);
    }

    #[test]
    fn managed_source_locks_are_enforced_by_the_service() {
        let (mut runtime, _events) = runtime(true);
        runtime.field_resolutions.get_mut().unwrap().insert(
            PolicyField::BackupPaths,
            FieldResolution {
                source: resticpal_core::policy::ValueSource::ManagedLocked,
                locked: true,
            },
        );

        let response = runtime.handle_request(
            Request::new(
                23,
                RequestCommand::UpdateBackupSources {
                    paths: Some(vec![PathBuf::from(r"D:\Data")]),
                    exclusions: None,
                },
            ),
            ADMIN,
        );

        assert!(matches!(
            response.payload,
            ResponsePayload::Rejected { ref code, .. } if code == "managed_field_locked"
        ));
    }

    #[test]
    fn managed_policy_is_applied_live_and_reports_its_lock() {
        let (runtime, events) = runtime(false);
        let policy = ManagedPolicy {
            revision: "managed-8".to_owned(),
            schedule: ManagedSchedulePolicy {
                interval_hours: Some(Managed {
                    value: 8,
                    locked: true,
                }),
                ..ManagedSchedulePolicy::default()
            },
            ..ManagedPolicy::default()
        };

        assert!(runtime.apply_managed_policy(&policy).expect("valid policy"));
        assert_eq!(runtime.config().schedule.interval_hours, 8);
        assert_eq!(
            runtime.status().managed_revision.as_deref(),
            Some("managed-8")
        );
        assert!(runtime.field_locked(PolicyField::ScheduleIntervalHours));
        assert_eq!(
            events.recv().expect("configuration event"),
            RuntimeEvent::ConfigurationChanged
        );
    }

    #[test]
    fn managed_policy_waits_until_an_active_backup_finishes() {
        let (runtime, events) = runtime(false);
        runtime.state_guard().status.state = BackupState::Running {
            phase: BackupPhase::Uploading,
        };
        let policy = ManagedPolicy {
            revision: "managed-later".to_owned(),
            schedule: ManagedSchedulePolicy {
                interval_hours: Some(Managed {
                    value: 4,
                    locked: false,
                }),
                ..ManagedSchedulePolicy::default()
            },
            ..ManagedPolicy::default()
        };

        assert!(!runtime.apply_managed_policy(&policy).expect("valid policy"));
        assert_eq!(runtime.config().schedule.interval_hours, 24);
        assert!(runtime.status().managed_revision.is_none());
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn an_unlocked_source_field_can_change_without_overwriting_a_locked_field() {
        let (mut runtime, events) = runtime(true);
        runtime.config_write().backup.exclusions = vec!["managed-pattern".to_owned()];
        runtime.field_resolutions.get_mut().unwrap().insert(
            PolicyField::BackupExclusions,
            FieldResolution {
                source: resticpal_core::policy::ValueSource::ManagedLocked,
                locked: true,
            },
        );

        let response = runtime.handle_request(
            Request::new(
                24,
                RequestCommand::UpdateBackupSources {
                    paths: Some(vec![PathBuf::from(r"D:\Data")]),
                    exclusions: None,
                },
            ),
            ADMIN,
        );

        assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
        assert_eq!(
            events.recv().expect("configuration event"),
            RuntimeEvent::ConfigurationChanged
        );
        let config = runtime.config();
        assert_eq!(config.backup.paths, [PathBuf::from(r"D:\Data")]);
        assert_eq!(config.backup.exclusions, ["managed-pattern"]);
        assert_eq!(config.repository.url.as_deref(), Some("local:C:/backup"));
    }

    #[test]
    fn repository_configuration_requires_an_elevated_administrator() {
        let (runtime, _events) = runtime(true);
        let update = RequestCommand::UpdateRepository {
            display_name: Some("Backup".to_owned()),
            url: Some("local:C:/backup".to_owned()),
            mode: Some(RepositoryMode::Standard),
            options: Some(BTreeMap::new()),
            secret_updates: Vec::new(),
        };

        for command in [
            RequestCommand::GetRepository,
            update,
            RequestCommand::ValidateRepository,
            RequestCommand::InitializeRepository,
        ] {
            let response = runtime.handle_request(Request::new(30, command), USER);
            assert!(matches!(
                response.payload,
                ResponsePayload::Rejected { ref code, .. } if code == "administrator_required"
            ));
        }
    }

    #[test]
    fn repository_credentials_rotate_without_entering_configuration_or_responses() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config_path = directory.path().join("config.toml");
        let config_store = LocalConfigStore::new(&config_path);
        config_store
            .save(&LocalConfig {
                backup: LocalBackupConfig {
                    paths: Some(vec![PathBuf::from(r"C:\Data")]),
                    exclusions: Some(Vec::new()),
                },
                ..LocalConfig::default()
            })
            .expect("initial config");
        let credentials =
            DpapiSecretStore::open(directory.path().join("Credentials")).expect("credential store");
        let (events, receiver) = mpsc::channel();
        let runtime =
            ServiceRuntime::load_with_credentials(&config_path, events, Some(credentials.clone()))
                .expect("runtime");
        let first_secret = "first-unique-repository-secret";

        let response = runtime.handle_request(
            Request::new(
                31,
                RequestCommand::UpdateRepository {
                    display_name: Some("Managed S3".to_owned()),
                    url: Some("s3:https://s3.example.test/bucket/device".to_owned()),
                    mode: Some(RepositoryMode::AppendOnly),
                    options: Some(BTreeMap::from([(
                        "s3.region".to_owned(),
                        "us-west-2".to_owned(),
                    )])),
                    secret_updates: vec![RepositorySecretUpdate::Set {
                        variable: SecretEnvironmentVariable::ResticPassword,
                        value: SecretValue::new(first_secret),
                    }],
                },
            ),
            ADMIN,
        );

        assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
        assert_eq!(
            receiver.recv().expect("configuration event"),
            RuntimeEvent::ConfigurationChanged
        );
        let first_reference = runtime.config().repository.secret_refs
            [&SecretEnvironmentVariable::ResticPassword]
            .clone();
        assert_eq!(
            credentials
                .get(&first_reference)
                .expect("first secret")
                .as_slice(),
            first_secret.as_bytes()
        );
        let config_text = std::fs::read_to_string(&config_path).expect("saved config");
        assert!(!config_text.contains(first_secret));
        let view = runtime.handle_request(Request::new(32, RequestCommand::GetRepository), ADMIN);
        assert!(matches!(
            view.payload,
            ResponsePayload::Repository {
                configuration: RepositoryView {
                    mode: RepositoryMode::AppendOnly,
                    ref configured_secrets,
                    ..
                }
            } if configured_secrets == &[SecretEnvironmentVariable::ResticPassword]
        ));
        assert!(!format!("{view:?}").contains(first_secret));
        assert!(!format!("{view:?}").contains(&first_reference));

        let second_secret = "second-unique-repository-secret";
        let response = runtime.handle_request(
            Request::new(
                33,
                RequestCommand::UpdateRepository {
                    display_name: None,
                    url: None,
                    mode: None,
                    options: None,
                    secret_updates: vec![RepositorySecretUpdate::Set {
                        variable: SecretEnvironmentVariable::ResticPassword,
                        value: SecretValue::new(second_secret),
                    }],
                },
            ),
            ADMIN,
        );
        assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
        assert_eq!(
            receiver.recv().expect("rotation event"),
            RuntimeEvent::ConfigurationChanged
        );
        let second_reference = runtime.config().repository.secret_refs
            [&SecretEnvironmentVariable::ResticPassword]
            .clone();
        assert_ne!(first_reference, second_reference);
        assert!(matches!(
            credentials.get(&first_reference),
            Err(CredentialStoreError::NotFound)
        ));
        assert_eq!(
            credentials
                .get(&second_reference)
                .expect("rotated secret")
                .as_slice(),
            second_secret.as_bytes()
        );

        let response = runtime.handle_request(
            Request::new(
                34,
                RequestCommand::UpdateRepository {
                    display_name: None,
                    url: None,
                    mode: None,
                    options: None,
                    secret_updates: vec![RepositorySecretUpdate::Remove {
                        variable: SecretEnvironmentVariable::ResticPassword,
                    }],
                },
            ),
            ADMIN,
        );
        assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
        assert!(
            !runtime
                .config()
                .repository
                .secret_refs
                .contains_key(&SecretEnvironmentVariable::ResticPassword)
        );
        assert!(matches!(
            credentials.get(&second_reference),
            Err(CredentialStoreError::NotFound)
        ));
    }

    #[test]
    fn repository_policy_locks_are_reported_and_enforced_per_field() {
        let (mut runtime, events) = runtime(true);
        runtime.field_resolutions.get_mut().unwrap().insert(
            PolicyField::RepositoryUrl,
            FieldResolution {
                source: resticpal_core::policy::ValueSource::ManagedLocked,
                locked: true,
            },
        );

        let view = runtime.handle_request(Request::new(35, RequestCommand::GetRepository), ADMIN);
        assert!(matches!(
            view.payload,
            ResponsePayload::Repository {
                configuration: RepositoryView {
                    url_locked: true,
                    display_name_locked: false,
                    ..
                }
            }
        ));

        let rejected_response = runtime.handle_request(
            Request::new(
                36,
                RequestCommand::UpdateRepository {
                    display_name: None,
                    url: Some("local:D:/replacement".to_owned()),
                    mode: None,
                    options: None,
                    secret_updates: Vec::new(),
                },
            ),
            ADMIN,
        );
        assert!(matches!(
            rejected_response.payload,
            ResponsePayload::Rejected { ref code, .. } if code == "managed_field_locked"
        ));

        let accepted_response = runtime.handle_request(
            Request::new(
                37,
                RequestCommand::UpdateRepository {
                    display_name: Some("Friendly name".to_owned()),
                    url: None,
                    mode: None,
                    options: None,
                    secret_updates: Vec::new(),
                },
            ),
            ADMIN,
        );
        assert!(matches!(
            accepted_response.payload,
            ResponsePayload::Accepted { .. }
        ));
        assert_eq!(
            events.recv().expect("configuration event"),
            RuntimeEvent::ConfigurationChanged
        );
        let config = runtime.config();
        assert_eq!(config.repository.url.as_deref(), Some("local:C:/backup"));
        assert_eq!(
            config.repository.display_name.as_deref(),
            Some("Friendly name")
        );
    }

    #[test]
    fn repository_validation_is_queued_and_reported_until_completion() {
        let (runtime, events) = runtime(true);
        runtime.config_write().repository.secret_refs.insert(
            SecretEnvironmentVariable::ResticPassword,
            "repository-password".to_owned(),
        );

        let response =
            runtime.handle_request(Request::new(40, RequestCommand::ValidateRepository), ADMIN);
        assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
        assert_eq!(
            events.recv().expect("repository event"),
            RuntimeEvent::RepositoryOperationRequested(RepositoryOperationKind::Validate)
        );
        let duplicate =
            runtime.handle_request(Request::new(41, RequestCommand::ValidateRepository), ADMIN);
        assert!(matches!(
            duplicate.payload,
            ResponsePayload::Rejected { ref code, .. }
                if code == "repository_operation_running"
        ));
        let running =
            runtime.handle_request(Request::new(42, RequestCommand::GetRepository), ADMIN);
        assert!(matches!(
            running.payload,
            ResponsePayload::Repository {
                configuration: RepositoryView {
                    operation_status: RepositoryOperationStatus::Running {
                        operation: RepositoryOperationKind::Validate
                    },
                    ..
                }
            }
        ));

        runtime.finish_repository_operation(
            RepositoryOperationKind::Validate,
            &RepositoryOutcome {
                kind: RepositoryOutcomeKind::Succeeded,
            },
        );
        let succeeded =
            runtime.handle_request(Request::new(43, RequestCommand::GetRepository), ADMIN);
        assert!(matches!(
            succeeded.payload,
            ResponsePayload::Repository {
                configuration: RepositoryView {
                    operation_status: RepositoryOperationStatus::Succeeded {
                        operation: RepositoryOperationKind::Validate,
                        ..
                    },
                    ..
                }
            }
        ));
    }

    #[test]
    fn append_only_mode_rejects_repository_initialization() {
        let (runtime, _events) = runtime(true);
        {
            let mut config = runtime.config_write();
            config.repository.mode = RepositoryMode::AppendOnly;
            config.repository.secret_refs.insert(
                SecretEnvironmentVariable::ResticPassword,
                "repository-password".to_owned(),
            );
        }

        let response = runtime.handle_request(
            Request::new(43, RequestCommand::InitializeRepository),
            ADMIN,
        );

        assert!(matches!(
            response.payload,
            ResponsePayload::Rejected { ref code, .. }
                if code == "append_only_initialization_forbidden"
        ));
    }

    #[test]
    fn connection_changes_require_validation_before_backup() {
        let (runtime, events) = runtime(true);
        let response = runtime.handle_request(
            Request::new(
                44,
                RequestCommand::UpdateRepository {
                    display_name: None,
                    url: Some("local:D:/replacement".to_owned()),
                    mode: None,
                    options: None,
                    secret_updates: Vec::new(),
                },
            ),
            ADMIN,
        );
        assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
        assert_eq!(
            events.recv().expect("configuration event"),
            RuntimeEvent::ConfigurationChanged
        );
        assert_eq!(
            runtime.evaluate_schedule(Utc::now(), available_conditions()),
            ScheduleAction::None
        );
        assert!(matches!(
            runtime.status().state,
            BackupState::Waiting {
                reason: WaitingReason::RepositoryValidation
            }
        ));

        let run = runtime.handle_request(Request::new(45, RequestCommand::RunBackupNow), ADMIN);
        assert!(matches!(
            run.payload,
            ResponsePayload::Rejected { ref code, .. } if code == "repository_not_ready"
        ));
        let view = runtime.handle_request(Request::new(46, RequestCommand::GetRepository), ADMIN);
        assert!(matches!(
            view.payload,
            ResponsePayload::Repository {
                configuration: RepositoryView {
                    operation_status: RepositoryOperationStatus::ValidationRequired,
                    ..
                }
            }
        ));
    }

    #[test]
    fn repository_validation_gate_survives_service_restart() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config_path = directory.path().join("config.toml");
        LocalConfigStore::new(&config_path)
            .save(&LocalConfig {
                backup: LocalBackupConfig {
                    paths: Some(vec![PathBuf::from(r"C:\Data")]),
                    exclusions: Some(Vec::new()),
                },
                repository: LocalRepositoryConfig {
                    url: Some("local:C:/backup".to_owned()),
                    secret_refs: Some(BTreeMap::from([(
                        SecretEnvironmentVariable::ResticPassword,
                        "repository-password".to_owned(),
                    )])),
                    ..LocalRepositoryConfig::default()
                },
                ..LocalConfig::default()
            })
            .expect("initial config");
        let (events, receiver) = mpsc::channel();
        let runtime = ServiceRuntime::load(&config_path, events).expect("runtime");

        let response = runtime.handle_request(
            Request::new(
                47,
                RequestCommand::UpdateRepository {
                    display_name: None,
                    url: Some("local:D:/replacement".to_owned()),
                    mode: None,
                    options: None,
                    secret_updates: Vec::new(),
                },
            ),
            ADMIN,
        );
        assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
        assert_eq!(
            receiver.recv().expect("configuration event"),
            RuntimeEvent::ConfigurationChanged
        );
        drop(runtime);

        let (events, receiver) = mpsc::channel();
        let runtime = ServiceRuntime::load(&config_path, events).expect("restarted runtime");
        assert_eq!(
            runtime.evaluate_schedule(Utc::now(), available_conditions()),
            ScheduleAction::None
        );
        let response =
            runtime.handle_request(Request::new(48, RequestCommand::ValidateRepository), ADMIN);
        assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
        assert_eq!(
            receiver.recv().expect("repository event"),
            RuntimeEvent::RepositoryOperationRequested(RepositoryOperationKind::Validate)
        );
        runtime.finish_repository_operation(
            RepositoryOperationKind::Validate,
            &RepositoryOutcome {
                kind: RepositoryOutcomeKind::Succeeded,
            },
        );
        drop(runtime);

        let (events, _receiver) = mpsc::channel();
        let runtime = ServiceRuntime::load(&config_path, events).expect("verified restart");
        let view = runtime.handle_request(Request::new(49, RequestCommand::GetRepository), ADMIN);
        assert!(matches!(
            view.payload,
            ResponsePayload::Repository {
                configuration: RepositoryView {
                    operation_status: RepositoryOperationStatus::Succeeded {
                        operation: RepositoryOperationKind::Validate,
                        ..
                    },
                    ..
                }
            }
        ));
        assert!(matches!(
            runtime.evaluate_schedule(Utc::now(), available_conditions()),
            ScheduleAction::Start { .. }
        ));
    }

    #[test]
    fn schedule_configuration_requires_admin_and_persists_live_updates() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config_path = directory.path().join("config.toml");
        LocalConfigStore::new(&config_path)
            .save(&LocalConfig {
                backup: LocalBackupConfig {
                    paths: Some(vec![PathBuf::from(r"C:\Data")]),
                    exclusions: Some(Vec::new()),
                },
                repository: LocalRepositoryConfig {
                    url: Some("local:C:/backup".to_owned()),
                    ..LocalRepositoryConfig::default()
                },
                ..LocalConfig::default()
            })
            .expect("initial config");
        let (events, receiver) = mpsc::channel();
        let runtime = ServiceRuntime::load(&config_path, events).expect("runtime");
        let update = || RequestCommand::UpdateSchedule {
            interval_hours: Some(12),
            wake_grace_seconds: Some(600),
            wake_lock_timeout_seconds: Some(3_600),
            allow_on_battery: Some(false),
            allow_metered_network: Some(false),
        };

        for command in [RequestCommand::GetSchedule, update()] {
            let response = runtime.handle_request(Request::new(50, command), USER);
            assert!(matches!(
                response.payload,
                ResponsePayload::Rejected { ref code, .. } if code == "administrator_required"
            ));
        }

        let response = runtime.handle_request(Request::new(51, update()), ADMIN);
        assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
        assert_eq!(
            receiver.recv().expect("configuration event"),
            RuntimeEvent::ConfigurationChanged
        );
        let schedule = runtime.config().schedule;
        assert_eq!(schedule.interval_hours, 12);
        assert_eq!(schedule.wake_grace_seconds, 600);
        assert_eq!(schedule.wake_lock_timeout_seconds, 3_600);
        assert!(!schedule.allow_on_battery);
        assert!(!schedule.allow_metered_network);
        let persisted = LocalConfigStore::new(&config_path)
            .load()
            .expect("persisted config");
        assert_eq!(persisted.schedule.interval_hours, Some(12));
        assert_eq!(persisted.schedule.wake_grace_seconds, Some(600));
        assert_eq!(persisted.schedule.wake_lock_timeout_seconds, Some(3_600));
        assert_eq!(persisted.schedule.allow_on_battery, Some(false));
        assert_eq!(persisted.schedule.allow_metered_network, Some(false));
    }

    #[test]
    fn schedule_policy_locks_are_reported_and_enforced_per_field() {
        let (mut runtime, events) = runtime(true);
        runtime.field_resolutions.get_mut().unwrap().insert(
            PolicyField::ScheduleIntervalHours,
            FieldResolution {
                source: resticpal_core::policy::ValueSource::ManagedLocked,
                locked: true,
            },
        );
        let view = runtime.handle_request(Request::new(52, RequestCommand::GetSchedule), ADMIN);
        assert!(matches!(
            view.payload,
            ResponsePayload::Schedule {
                configuration: ScheduleView {
                    interval_hours_locked: true,
                    allow_on_battery_locked: false,
                    ..
                }
            }
        ));

        let rejected_response = runtime.handle_request(
            Request::new(
                53,
                RequestCommand::UpdateSchedule {
                    interval_hours: Some(6),
                    wake_grace_seconds: None,
                    wake_lock_timeout_seconds: None,
                    allow_on_battery: None,
                    allow_metered_network: None,
                },
            ),
            ADMIN,
        );
        assert!(matches!(
            rejected_response.payload,
            ResponsePayload::Rejected { ref code, .. } if code == "managed_field_locked"
        ));

        let accepted_response = runtime.handle_request(
            Request::new(
                54,
                RequestCommand::UpdateSchedule {
                    interval_hours: None,
                    wake_grace_seconds: None,
                    wake_lock_timeout_seconds: None,
                    allow_on_battery: Some(false),
                    allow_metered_network: None,
                },
            ),
            ADMIN,
        );
        assert!(matches!(
            accepted_response.payload,
            ResponsePayload::Accepted { .. }
        ));
        assert_eq!(
            events.recv().expect("configuration event"),
            RuntimeEvent::ConfigurationChanged
        );
        assert_eq!(runtime.config().schedule.interval_hours, 24);
        assert!(!runtime.config().schedule.allow_on_battery);
    }

    #[test]
    fn invalid_schedule_update_does_not_replace_configuration() {
        let (runtime, _events) = runtime(true);
        let before = runtime.config().schedule;

        let response = runtime.handle_request(
            Request::new(
                55,
                RequestCommand::UpdateSchedule {
                    interval_hours: Some(0),
                    wake_grace_seconds: None,
                    wake_lock_timeout_seconds: None,
                    allow_on_battery: None,
                    allow_metered_network: None,
                },
            ),
            ADMIN,
        );

        assert!(matches!(
            response.payload,
            ResponsePayload::Rejected { ref code, .. } if code == "invalid_schedule"
        ));
        assert_eq!(runtime.config().schedule, before);
    }

    #[test]
    fn completed_backup_history_is_sanitized_persisted_and_user_readable() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config_path = directory.path().join("config.toml");
        LocalConfigStore::new(&config_path)
            .save(&LocalConfig {
                backup: LocalBackupConfig {
                    paths: Some(vec![PathBuf::from(r"C:\Data")]),
                    exclusions: Some(Vec::new()),
                },
                repository: LocalRepositoryConfig {
                    url: Some("local:C:/backup".to_owned()),
                    ..LocalRepositoryConfig::default()
                },
                ..LocalConfig::default()
            })
            .expect("initial config");
        let mut verified_config = EffectiveConfig::default();
        verified_config.repository.url = Some("local:C:/backup".to_owned());
        let mut service_state = ServiceStateSnapshot::default();
        service_state.mark_repository_verified(&verified_config, Utc::now());
        ScheduleStateStore::next_to_config(&config_path)
            .save(&service_state)
            .expect("verified repository state");
        let (events, receiver) = mpsc::channel();
        let runtime = ServiceRuntime::load(&config_path, events).expect("runtime");
        assert!(matches!(
            runtime
                .handle_request(Request::new(56, RequestCommand::RunBackupNow), USER)
                .payload,
            ResponsePayload::Accepted { .. }
        ));
        assert_eq!(receiver.recv().expect("run event"), RuntimeEvent::RunNow);
        assert!(matches!(
            runtime.evaluate_schedule(Utc::now(), available_conditions()),
            ScheduleAction::Start { .. }
        ));
        runtime.finish_backup(&BackupOutcome::succeeded(BackupSummary {
            files_processed: 12,
            bytes_processed: 1_024,
            data_added: 256,
            snapshot_id: Some("abc123".to_owned()),
        }));

        let response = runtime.handle_request(
            Request::new(57, RequestCommand::GetRunHistory { limit: 50 }),
            USER,
        );
        assert!(matches!(
            response.payload,
            ResponsePayload::RunHistory { ref runs }
                if matches!(runs.as_slice(), [run]
                    if run.outcome == BackupRunOutcome::Succeeded
                        && run.files_processed == Some(12)
                        && run.snapshot_id.as_deref() == Some("abc123"))
        ));
        drop(runtime);

        let (events, _receiver) = mpsc::channel();
        let restarted = ServiceRuntime::load(&config_path, events).expect("restarted runtime");
        assert!(matches!(
            restarted
                .handle_request(
                    Request::new(58, RequestCommand::GetRunHistory { limit: 1 }),
                    USER,
                )
                .payload,
            ResponsePayload::RunHistory { runs } if runs.len() == 1
        ));
        assert!(matches!(
            restarted
                .handle_request(
                    Request::new(59, RequestCommand::GetRunHistory { limit: 0 }),
                    USER,
                )
                .payload,
            ResponsePayload::Rejected { ref code, .. } if code == "invalid_history_limit"
        ));
    }
}
