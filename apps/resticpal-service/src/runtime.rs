use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use resticpal_core::config::{EffectiveConfig, LocalConfig};
use resticpal_core::policy::{
    FieldResolution, PolicyError, PolicyField, ResolvedConfig, resolve_config,
};
use resticpal_core::schedule::{
    BackupTrigger, ScheduleBlocker, ScheduleDecision, SchedulerSnapshot, decide,
};
use resticpal_core::status::{BackupPhase, BackupProgress, BackupState, ServiceStatus};
use resticpal_protocol::{BackupSourcesView, Request, RequestCommand, Response, ResponsePayload};
use resticpal_windows::named_pipe::ClientIdentity;
use resticpal_windows::user_profiles::discover_backup_sources;
use thiserror::Error;

use crate::conditions::SystemConditions;
use crate::config_store::{ConfigStoreError, LocalConfigStore};
use crate::executor::{BackupOutcome, BackupOutcomeKind};
use crate::state::ScheduleStateStore;

const CONDITION_RETRY_SECONDS: u64 = 60;
const INITIAL_FAILURE_RETRY_MINUTES: i64 = 5;
const MAX_FAILURE_BACKOFF_EXPONENT: u32 = 6;

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
}

pub struct ServiceRuntime {
    config: RwLock<EffectiveConfig>,
    local_config: Mutex<LocalConfig>,
    field_resolutions: BTreeMap<PolicyField, FieldResolution>,
    config_store: Option<LocalConfigStore>,
    state: Mutex<RuntimeState>,
    state_store: Option<ScheduleStateStore>,
    events: Sender<RuntimeEvent>,
}

impl ServiceRuntime {
    pub fn load(path: &Path, events: Sender<RuntimeEvent>) -> Result<Self, RuntimeInitError> {
        let config_store = LocalConfigStore::new(path);
        let local = config_store.load()?;
        let resolved = resolve_config(&EffectiveConfig::default(), &local, None)?;
        let state_store = ScheduleStateStore::next_to_config(path);
        let last_success = match state_store.load_last_success() {
            Ok(last_success) => last_success,
            Err(error) => {
                eprintln!(
                    "could not load schedule state next to {}: {error}; an immediate backup will be eligible",
                    path.display()
                );
                None
            }
        };
        Ok(Self::from_resolved_with_state(
            resolved,
            local,
            events,
            last_success,
            Some(state_store),
            Some(config_store),
        ))
    }

    #[cfg(test)]
    pub fn from_resolved(resolved: ResolvedConfig, events: Sender<RuntimeEvent>) -> Self {
        Self::from_resolved_with_state(resolved, LocalConfig::default(), events, None, None, None)
    }

    fn from_resolved_with_state(
        resolved: ResolvedConfig,
        local_config: LocalConfig,
        events: Sender<RuntimeEvent>,
        last_success: Option<DateTime<Utc>>,
        state_store: Option<ScheduleStateStore>,
        config_store: Option<LocalConfigStore>,
    ) -> Self {
        let now = Utc::now();
        let configured = resolved.effective.is_configured();
        let next_deadline = configured.then(|| {
            last_success.map_or(now, |last_success| {
                last_success
                    + Duration::hours(i64::from(resolved.effective.schedule.interval_hours))
            })
        });
        let status = ServiceStatus {
            state: if configured {
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
            field_resolutions: resolved.fields,
            config_store,
            state: Mutex::new(RuntimeState {
                status,
                resumed_at: None,
                not_before: None,
                manual_requested: false,
                consecutive_failures: 0,
            }),
            state_store,
            events,
        }
    }

    pub fn configuration_error(events: Sender<RuntimeEvent>) -> Self {
        let now = Utc::now();
        Self {
            config: RwLock::new(EffectiveConfig::default()),
            local_config: Mutex::new(LocalConfig::default()),
            field_resolutions: BTreeMap::new(),
            config_store: None,
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
            }),
            state_store: None,
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
            state.status.next_deadline = Some(now + Duration::hours(i64::from(interval_hours)));
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
        drop(state);

        if succeeded
            && let Some(store) = &self.state_store
            && let Err(error) = store.save_last_success(now)
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
            RequestCommand::RunBackupNow => {
                if !self.config_read().is_configured() {
                    rejected(
                        "not_configured",
                        "Configure backup sources and a repository first.",
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
                } else if matches!(self.state_guard().status.state, BackupState::Running { .. }) {
                    rejected("already_running", "A running backup cannot be deferred.")
                } else {
                    let deadline = Utc::now() + Duration::minutes(i64::from(minutes));
                    let mut state = self.state_guard();
                    state.manual_requested = false;
                    state.not_before = Some(deadline);
                    state.status.next_deadline = Some(deadline);
                    drop(state);
                    self.send_event(RuntimeEvent::Deferred, "Backup deferred.")
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

    fn apply_configuration_status(
        state: &mut RuntimeState,
        config: &EffectiveConfig,
        now: DateTime<Utc>,
    ) {
        state.status.repository_display_name = config.repository.display_name.clone();
        state.status.repository_mode = config.repository.mode;
        if config.is_configured() {
            let scheduled_deadline = state.status.last_success.map_or(now, |last_success| {
                last_success + Duration::hours(i64::from(config.schedule.interval_hours))
            });
            state.status.next_deadline =
                Some(state.not_before.map_or(scheduled_deadline, |not_before| {
                    scheduled_deadline.max(not_before)
                }));
            if matches!(state.status.state, BackupState::Unconfigured) {
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
            .get(&field)
            .is_some_and(|resolution| resolution.locked)
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
        return path.starts_with(r"\\");
    }
    if repository.starts_with(r"\\") {
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

    use resticpal_core::config::{BackupConfig, RepositoryConfig};
    use resticpal_protocol::PROTOCOL_VERSION;
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
        assert!(matches!(runtime.status().state, BackupState::Idle));
        let persisted = store.load().expect("updated config should load");
        assert_eq!(
            persisted.backup.paths,
            Some(vec![PathBuf::from(r"D:\Data")])
        );
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
        runtime.field_resolutions.insert(
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
    fn an_unlocked_source_field_can_change_without_overwriting_a_locked_field() {
        let (mut runtime, events) = runtime(true);
        runtime.config_write().backup.exclusions = vec!["managed-pattern".to_owned()];
        runtime.field_resolutions.insert(
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
}
