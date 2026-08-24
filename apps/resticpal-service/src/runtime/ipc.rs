use super::events::RuntimeEvent;
use super::helpers::*;
use super::state::ServiceRuntime;

use std::collections::BTreeMap;

use chrono::{Duration, Utc};
use resticpal_core::config::{RepositoryMode, SecretEnvironmentVariable};
use resticpal_core::policy::PolicyField;
use resticpal_core::restic::{InvocationError, ResticOperation};
use resticpal_core::status::{BackupState, WaitingReason};
use resticpal_protocol::{
    BackupRunFailureDetails, BackupSourcesView, RepositoryOperationKind, RepositoryOperationStatus,
    RepositorySecretUpdate, RepositoryView, Request, RequestCommand, Response, ResponsePayload,
    RetentionView, ScheduleView, UpdatePackage, UpdateSettingsView,
};
use resticpal_windows::credentials::DpapiSecretStore;
use resticpal_windows::named_pipe::ClientIdentity;
use resticpal_windows::user_profiles::discover_backup_sources;

use crate::diagnostics::MAX_DIAGNOSTIC_RESULTS;
use crate::executor::validate_backup_source_paths;
use crate::history::MAX_HISTORY_RESULTS;
use crate::updater;

const MAX_SECRET_UPDATES: usize = 16;
const MIN_UPDATE_HOLD_SECONDS: u32 = 60;
const MAX_UPDATE_HOLD_SECONDS: u32 = updater::UPDATE_INSTALL_TIMEOUT_SECONDS;

impl ServiceRuntime {
    pub fn handle_request(&self, request: Request, identity: ClientIdentity) -> Response {
        let request_id = request.request_id;
        let payload = match request.command {
            RequestCommand::GetStatus => ResponsePayload::Status {
                status: self.status(),
            },
            RequestCommand::GetManagement => {
                if identity.is_elevated_administrator {
                    ResponsePayload::Management {
                        configuration: self.management_view(),
                    }
                } else {
                    administrator_required()
                }
            }
            RequestCommand::Enroll { bootstrap_url } => {
                self.enroll(bootstrap_url.as_bytes(), identity)
            }
            RequestCommand::Unenroll => self.unenroll(identity),
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
            RequestCommand::GetRunFailureDetails { run_id } => {
                if !identity.is_elevated_administrator {
                    administrator_required()
                } else if let Some(store) = &self.history_store {
                    match store.failure_details(run_id) {
                        Ok(Some(details)) => ResponsePayload::RunFailureDetails {
                            details: BackupRunFailureDetails {
                                run_id,
                                items: details.items,
                                omitted: details.omitted,
                            },
                        },
                        Ok(None) => rejected(
                            "history_run_not_found",
                            "The requested backup history entry does not exist.",
                        ),
                        Err(error) => {
                            eprintln!(
                                "could not read sensitive backup failure details for run {run_id}: {error}"
                            );
                            rejected(
                                "history_unavailable",
                                "Backup failure details could not be read from local storage.",
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
            RequestCommand::GetDiagnostics { limit } => {
                let limit = usize::from(limit);
                if !identity.is_elevated_administrator {
                    administrator_required()
                } else if !(1..=MAX_DIAGNOSTIC_RESULTS).contains(&limit) {
                    rejected(
                        "invalid_diagnostics_limit",
                        "Diagnostics requests must contain a limit from 1 through 200.",
                    )
                } else if let Some(log) = &self.diagnostics {
                    match log.recent(limit) {
                        Ok(entries) => ResponsePayload::Diagnostics { entries },
                        Err(_) => rejected(
                            "diagnostics_unavailable",
                            "Operational diagnostics could not be read from local storage.",
                        ),
                    }
                } else {
                    rejected(
                        "diagnostics_unavailable",
                        "Operational diagnostics storage is not available.",
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
            RequestCommand::GetRetention => {
                if identity.is_elevated_administrator {
                    ResponsePayload::Retention {
                        configuration: self.retention_view(),
                    }
                } else {
                    administrator_required()
                }
            }
            RequestCommand::UpdateRetention {
                daily,
                weekly,
                monthly,
                yearly,
                prune_interval_days,
            } => self.update_retention(
                daily,
                weekly,
                monthly,
                yearly,
                prune_interval_days,
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
                    } else if state.update_install_active
                        || state
                            .update_hold_until
                            .is_some_and(|deadline| deadline > Utc::now())
                    {
                        rejected(
                            "update_pending",
                            "A resticpal update is about to start. Try again after it finishes.",
                        )
                    } else {
                        state.manual_requested = true;
                        state.not_before = None;
                        match self.events.send(RuntimeEvent::RunNow) {
                            Ok(()) => ResponsePayload::Accepted {
                                message: "Backup requested. Waiting for the service to start."
                                    .to_owned(),
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
            RequestCommand::PrepareForUpdate { hold_seconds } => {
                self.prepare_for_update(hold_seconds, identity)
            }
            RequestCommand::GetUpdateSettings => ResponsePayload::UpdateSettings {
                configuration: self.update_settings_view(),
            },
            RequestCommand::UpdateUpdateSettings { automatic_install } => {
                self.update_update_settings(automatic_install, identity)
            }
            RequestCommand::InstallUpdate { package } => {
                self.begin_update_install(package, identity)
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
            secrets_locked: self.repository_secrets_locked(),
        }
    }

    fn repository_secrets_locked(&self) -> bool {
        self.field_locked(PolicyField::RepositorySecretRefs) || self.management_view().enrolled
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
        if state.update_install_active
            || state
                .update_hold_until
                .is_some_and(|deadline| deadline > Utc::now())
        {
            return rejected(
                "update_pending",
                "A resticpal update is about to start. Try again after it finishes.",
            );
        }
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

    fn prepare_for_update(&self, hold_seconds: u32, identity: ClientIdentity) -> ResponsePayload {
        if !identity.is_elevated_administrator {
            return administrator_required();
        }
        let _mutation = self
            .configuration_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.config_read().updates.automatic_install {
            return rejected(
                "automatic_updates_enabled",
                "This device's effective policy requires the protected service to install updates silently.",
            );
        }
        if !(MIN_UPDATE_HOLD_SECONDS..=MAX_UPDATE_HOLD_SECONDS).contains(&hold_seconds) {
            return rejected(
                "invalid_update_hold",
                "The update hold must be between one and 30 minutes.",
            );
        }

        let now = Utc::now();
        let mut state = self.state_guard();
        if state.update_install_active {
            return rejected(
                "update_already_running",
                "The protected service is already installing an update.",
            );
        }
        if state
            .update_hold_until
            .is_some_and(|existing_deadline| existing_deadline > now)
        {
            return rejected(
                "update_pending",
                "Another update request is already holding backup work.",
            );
        }
        if matches!(state.status.state, BackupState::Running { .. }) {
            return rejected(
                "backup_running",
                "Wait for the active backup to finish before installing the update.",
            );
        }
        if matches!(
            state.repository_operation,
            RepositoryOperationStatus::Running { .. }
        ) || state.management_operation_active
        {
            return rejected(
                "operation_running",
                "Wait for the current repository or management operation to finish.",
            );
        }

        let deadline = now
            .checked_add_signed(Duration::seconds(i64::from(hold_seconds)))
            .unwrap_or(now);
        if state.update_hold_previous_status.is_none() {
            state.update_hold_previous_status =
                Some((state.status.state.clone(), state.status.next_deadline));
        }
        state.update_hold_until = Some(deadline);
        state.manual_requested = false;
        state.resumed_at = None;
        transition_state(
            &mut state.status,
            BackupState::Waiting {
                reason: WaitingReason::Update,
            },
            now,
        );
        state.status.next_deadline = Some(deadline);

        ResponsePayload::Accepted {
            message: "Backups are held briefly while the update starts.".to_owned(),
        }
    }

    fn update_settings_view(&self) -> UpdateSettingsView {
        let _mutation = self
            .configuration_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        UpdateSettingsView {
            automatic_install: self.config_read().updates.automatic_install,
            automatic_install_locked: self.field_locked(PolicyField::UpdateAutomaticInstall),
        }
    }

    fn update_blocks_configuration_mutation(&self) -> bool {
        let state = self.state_guard();
        state.update_install_active
            || state
                .update_hold_until
                .is_some_and(|deadline| deadline > Utc::now())
    }

    fn update_update_settings(
        &self,
        automatic_install: bool,
        identity: ClientIdentity,
    ) -> ResponsePayload {
        if !identity.is_elevated_administrator {
            return administrator_required();
        }
        let _mutation = self
            .configuration_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.field_locked(PolicyField::UpdateAutomaticInstall) {
            return rejected(
                "managed_field_locked",
                "Automatic update installation is locked by managed policy.",
            );
        }
        {
            let state = self.state_guard();
            if state.update_install_active
                || state
                    .update_hold_until
                    .is_some_and(|deadline| deadline > Utc::now())
            {
                return rejected(
                    "update_pending",
                    "Automatic-update mode cannot change while an update is in progress.",
                );
            }
        }
        let mut candidate = self.local_config_guard().clone();
        candidate.updates.automatic_install = Some(automatic_install);
        if let Some(store) = &self.config_store
            && let Err(error) = store.save(&candidate)
        {
            eprintln!(
                "could not save local configuration to {}: {error}",
                store.path().display()
            );
            return rejected(
                "configuration_save_failed",
                "The automatic-update setting could not be saved.",
            );
        }
        *self.local_config_guard() = candidate;
        self.config_write().updates.automatic_install = automatic_install;
        ResponsePayload::Accepted {
            message: if automatic_install {
                "Signed resticpal updates will install automatically in the background.".to_owned()
            } else {
                "resticpal will ask before installing updates.".to_owned()
            },
        }
    }

    fn begin_update_install(
        &self,
        package: UpdatePackage,
        _identity: ClientIdentity,
    ) -> ResponsePayload {
        let _mutation = self
            .configuration_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let automatic_install = self.config_read().updates.automatic_install;
        if !automatic_install {
            return rejected(
                "automatic_updates_disabled",
                "Automatic update installation is disabled by this device's effective policy.",
            );
        }
        if let Err(error) = updater::validate_package(&package) {
            eprintln!("rejected update metadata for {}: {error}", package.version);
            return rejected(
                "update_metadata_invalid",
                "The signed update metadata is invalid or does not describe a newer resticpal MSI.",
            );
        }

        let now = Utc::now();
        let mut state = self.state_guard();
        if state.update_install_active {
            return rejected(
                "update_already_running",
                "An update is already being installed.",
            );
        }
        if state
            .update_hold_until
            .is_some_and(|existing_deadline| existing_deadline > now)
        {
            return rejected(
                "update_pending",
                "A prompted update is already holding backup work.",
            );
        }
        if matches!(state.status.state, BackupState::Running { .. }) {
            return rejected(
                "backup_running",
                "The update will be retried after the active backup finishes.",
            );
        }
        if matches!(
            state.repository_operation,
            RepositoryOperationStatus::Running { .. }
        ) || state.management_operation_active
        {
            return rejected(
                "operation_running",
                "The update will be retried after the current service operation finishes.",
            );
        }

        let deadline = now
            .checked_add_signed(Duration::seconds(i64::from(MAX_UPDATE_HOLD_SECONDS)))
            .unwrap_or(now);
        if state.update_hold_previous_status.is_none() {
            state.update_hold_previous_status =
                Some((state.status.state.clone(), state.status.next_deadline));
        }
        state.update_install_active = true;
        state.update_hold_until = Some(deadline);
        state.manual_requested = false;
        state.resumed_at = None;
        transition_state(
            &mut state.status,
            BackupState::Waiting {
                reason: WaitingReason::Update,
            },
            now,
        );
        state.status.next_deadline = Some(deadline);

        let version = package.version.clone();
        if self
            .events
            .send(RuntimeEvent::UpdateInstallRequested(package))
            .is_err()
        {
            state.update_install_active = false;
            state.update_hold_until = None;
            if let Some((previous, previous_deadline)) = state.update_hold_previous_status.take() {
                state.status.state = previous;
                state.status.state_since = now;
                state.status.next_deadline = previous_deadline;
            }
            return rejected(
                "service_stopping",
                "The backup service is stopping. Try again shortly.",
            );
        }
        ResponsePayload::Accepted {
            message: format!(
                "resticpal {} will download and install silently after signature verification.",
                version
            ),
        }
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

    fn retention_view(&self) -> RetentionView {
        let (last_retention, last_prune, last_error) = {
            let state = self.state_guard();
            (
                state.service_state.last_retention,
                state.service_state.last_prune,
                state.service_state.last_retention_error.clone(),
            )
        };
        let config = self.config_read();
        RetentionView {
            repository_mode: config.repository.mode,
            daily: config.retention.daily,
            weekly: config.retention.weekly,
            monthly: config.retention.monthly,
            yearly: config.retention.yearly,
            prune_interval_days: config.retention.prune_interval_days,
            daily_locked: self.field_locked(PolicyField::RetentionDaily),
            weekly_locked: self.field_locked(PolicyField::RetentionWeekly),
            monthly_locked: self.field_locked(PolicyField::RetentionMonthly),
            yearly_locked: self.field_locked(PolicyField::RetentionYearly),
            prune_interval_days_locked: self.field_locked(PolicyField::RetentionPruneIntervalDays),
            last_retention,
            last_prune,
            last_error,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn update_retention(
        &self,
        daily: Option<u32>,
        weekly: Option<u32>,
        monthly: Option<u32>,
        yearly: Option<u32>,
        prune_interval_days: Option<u32>,
        identity: ClientIdentity,
    ) -> ResponsePayload {
        if !identity.is_elevated_administrator {
            return administrator_required();
        }
        let _configuration_mutation = self
            .configuration_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.update_blocks_configuration_mutation() {
            return rejected(
                "update_pending",
                "Wait for the resticpal update to finish before changing configuration.",
            );
        }
        if self.config_read().repository.mode == RepositoryMode::AppendOnly {
            return rejected(
                "retention_managed_by_server",
                "Retention for an append-only repository is managed by the server.",
            );
        }
        if daily.is_none()
            && weekly.is_none()
            && monthly.is_none()
            && yearly.is_none()
            && prune_interval_days.is_none()
        {
            return rejected(
                "no_configuration_changes",
                "No retention changes were supplied.",
            );
        }
        if daily.is_some() && self.field_locked(PolicyField::RetentionDaily)
            || weekly.is_some() && self.field_locked(PolicyField::RetentionWeekly)
            || monthly.is_some() && self.field_locked(PolicyField::RetentionMonthly)
            || yearly.is_some() && self.field_locked(PolicyField::RetentionYearly)
            || prune_interval_days.is_some()
                && self.field_locked(PolicyField::RetentionPruneIntervalDays)
        {
            return rejected(
                "managed_field_locked",
                "One or more retention fields are locked by managed policy.",
            );
        }

        let mut candidate = self.local_config_guard().clone();
        let mut effective = self.config();
        if let Some(value) = daily {
            candidate.retention.daily = Some(value);
            effective.retention.daily = value;
        }
        if let Some(value) = weekly {
            candidate.retention.weekly = Some(value);
            effective.retention.weekly = value;
        }
        if let Some(value) = monthly {
            candidate.retention.monthly = Some(value);
            effective.retention.monthly = value;
        }
        if let Some(value) = yearly {
            candidate.retention.yearly = Some(value);
            effective.retention.yearly = value;
        }
        if let Some(value) = prune_interval_days {
            candidate.retention.prune_interval_days = Some(value);
            effective.retention.prune_interval_days = value;
        }
        if let Err(error) = effective.validate() {
            return rejected("invalid_retention", &error.to_string());
        }
        let runtime_state = self.state_guard();
        if matches!(runtime_state.status.state, BackupState::Running { .. }) {
            return rejected(
                "backup_running",
                "Wait for the active backup to finish before changing retention.",
            );
        }
        if matches!(
            runtime_state.repository_operation,
            RepositoryOperationStatus::Running { .. }
        ) {
            return rejected(
                "repository_operation_running",
                "Wait for the repository operation to finish before changing retention.",
            );
        }
        if let Some(store) = &self.config_store
            && store.save(&candidate).is_err()
        {
            return rejected(
                "configuration_save_failed",
                "The local configuration could not be saved.",
            );
        }
        drop(runtime_state);
        *self.local_config_guard() = candidate;
        *self.config_write() = effective;
        let _ = self.events.send(RuntimeEvent::ConfigurationChanged);
        ResponsePayload::Accepted {
            message: "Retention policy updated.".to_owned(),
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
        let _configuration_mutation = self
            .configuration_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.update_blocks_configuration_mutation() {
            return rejected(
                "update_pending",
                "Wait for the resticpal update to finish before changing configuration.",
            );
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
        let _configuration_mutation = self
            .configuration_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.update_blocks_configuration_mutation() {
            return rejected(
                "update_pending",
                "Wait for the resticpal update to finish before changing configuration.",
            );
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
        let data_directory = self
            .config_store
            .as_ref()
            .and_then(|store| store.path().parent());
        if let Err(error) = validate_backup_source_paths(&effective.backup.paths, data_directory) {
            return match error {
                InvocationError::UnsupportedNetworkBackupSource => rejected(
                    "unsupported_network_backup_source",
                    "Network folders cannot be used as backup sources yet. Choose a folder on a local Windows drive.",
                ),
                InvocationError::UnsupportedBackupSourceNamespace => rejected(
                    "unsupported_backup_source_namespace",
                    "Backup sources must use an ordinary local Windows drive path, without junction or device aliases.",
                ),
                InvocationError::ProtectedBackupSource => rejected(
                    "protected_backup_source",
                    "The resticpal service-data folder cannot be selected as a backup source.",
                ),
                _ => rejected("invalid_backup_sources", &error.to_string()),
            };
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
        let _configuration_mutation = self
            .configuration_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.update_blocks_configuration_mutation() {
            return rejected(
                "update_pending",
                "Wait for the resticpal update to finish before changing configuration.",
            );
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
            || !secret_updates.is_empty() && self.repository_secrets_locked()
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
