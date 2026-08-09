use std::fs;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use resticpal_core::config::{EffectiveConfig, LocalConfig, LocalConfigError};
use resticpal_core::policy::{PolicyError, ResolvedConfig, resolve_config};
use resticpal_core::schedule::{
    BackupTrigger, ScheduleBlocker, ScheduleDecision, SchedulerSnapshot, decide,
};
use resticpal_core::status::{BackupPhase, BackupProgress, BackupState, ServiceStatus};
use resticpal_protocol::{Request, RequestCommand, Response, ResponsePayload};
use resticpal_windows::named_pipe::ClientIdentity;
use thiserror::Error;

use crate::conditions::SystemConditions;
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
    config: EffectiveConfig,
    state: Mutex<RuntimeState>,
    state_store: Option<ScheduleStateStore>,
    events: Sender<RuntimeEvent>,
}

impl ServiceRuntime {
    pub fn load(path: &Path, events: Sender<RuntimeEvent>) -> Result<Self, RuntimeInitError> {
        let local = match fs::read_to_string(path) {
            Ok(contents) => LocalConfig::from_toml(&contents)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => LocalConfig::default(),
            Err(error) => return Err(error.into()),
        };
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
            events,
            last_success,
            Some(state_store),
        ))
    }

    #[cfg(test)]
    pub fn from_resolved(resolved: ResolvedConfig, events: Sender<RuntimeEvent>) -> Self {
        Self::from_resolved_with_state(resolved, events, None, None)
    }

    fn from_resolved_with_state(
        resolved: ResolvedConfig,
        events: Sender<RuntimeEvent>,
        last_success: Option<DateTime<Utc>>,
        state_store: Option<ScheduleStateStore>,
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
            config: resolved.effective,
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
            config: EffectiveConfig::default(),
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
        self.config.clone()
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
            state.status.next_deadline =
                Some(now + Duration::hours(i64::from(self.config.schedule.interval_hours)));
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
        if !self.config.is_configured() {
            return ScheduleAction::None;
        }

        let mut state = self.state_guard();
        let decision = decide(
            &self.config.schedule,
            &SchedulerSnapshot {
                now,
                last_success: state.status.last_success,
                resumed_at: state.resumed_at,
                not_before: state.not_before,
                manual_requested: state.manual_requested,
                backup_running: matches!(state.status.state, BackupState::Running { .. }),
                network_required: repository_requires_network(
                    self.config.repository.url.as_deref().unwrap_or_default(),
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

    pub fn handle_request(&self, request: Request, _identity: ClientIdentity) -> Response {
        let request_id = request.request_id;
        let payload = match request.command {
            RequestCommand::GetStatus => ResponsePayload::Status {
                status: self.status(),
            },
            RequestCommand::RunBackupNow => {
                if !self.config.is_configured() {
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

#[derive(Debug, Error)]
pub enum RuntimeInitError {
    #[error("could not read local configuration: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    LocalConfig(#[from] LocalConfigError),
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
        let (mut runtime, _events) = runtime(true);
        runtime.config.schedule.allow_on_battery = false;
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
}
