use super::events::RetentionPlan;
use super::state::ServiceRuntime;

use chrono::{DateTime, Duration, Utc};
use resticpal_core::config::RepositoryMode;
use resticpal_core::schedule::completion_deadline;
use resticpal_core::status::{BackupPhase, BackupProgress, BackupRunOutcome, BackupState};
use resticpal_protocol::{DiagnosticLevel, RepositoryOperationKind, RepositoryOperationStatus};

use crate::executor::{
    BackupOutcome, BackupOutcomeKind, RepositoryOutcome, RepositoryOutcomeKind, RetentionOutcome,
    RetentionOutcomeKind,
};
use crate::history::CompletedBackupRun;
use crate::updater::UpdateInstallOutcome;

const INITIAL_FAILURE_RETRY_MINUTES: i64 = 5;
const MAX_FAILURE_BACKOFF_EXPONENT: u32 = 6;

impl ServiceRuntime {
    pub fn finish_update_install(&self, outcome: &UpdateInstallOutcome) {
        let now = Utc::now();
        let mut state = self.state_guard();
        match outcome {
            UpdateInstallOutcome::Completed { .. } => {
                state.update_install_active = false;
                // A successful MSI stops this service as part of the upgrade.
                // Keep the update hold until that happens so no backup starts
                // during Windows Installer finalization.
            }
            UpdateInstallOutcome::Failed { .. } => {
                state.update_install_active = false;
                state.update_hold_until = None;
                if let Some((previous, previous_deadline)) =
                    state.update_hold_previous_status.take()
                {
                    state.status.state = previous;
                    state.status.state_since = now;
                    state.status.next_deadline = previous_deadline;
                }
            }
            UpdateInstallOutcome::Indeterminate { .. } => {
                // The installer could still own files or the Windows Installer
                // transaction. Keep the hold fail-closed until service restart
                // instead of starting a backup into an unknown upgrade state.
                state.update_install_active = true;
            }
        }
        drop(state);

        match outcome {
            UpdateInstallOutcome::Completed { .. } => self.record_diagnostic(
                DiagnosticLevel::Information,
                "update.installer_completed",
                "The signed resticpal update installer completed.",
                None,
            ),
            UpdateInstallOutcome::Failed { code } => self.record_diagnostic(
                DiagnosticLevel::Error,
                "update.failed",
                "The signed resticpal update could not be installed.",
                Some(code),
            ),
            UpdateInstallOutcome::Indeterminate { code } => self.record_diagnostic(
                DiagnosticLevel::Error,
                "update.installer_indeterminate",
                "The update installer could not be confirmed stopped; backups remain paused.",
                Some(code),
            ),
        }
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

    pub fn begin_retention(&self, now: DateTime<Utc>) -> Option<RetentionPlan> {
        let (repository_mode, prune_interval_days) = {
            let config = self.config_read();
            (config.repository.mode, config.retention.prune_interval_days)
        };
        if repository_mode != RepositoryMode::Standard {
            return None;
        }
        let prune_interval = Duration::days(i64::from(prune_interval_days));
        let mut state = self.state_guard();
        state.status.state = BackupState::Running {
            phase: BackupPhase::Retention,
        };
        state.status.state_since = now;
        state.status.progress = None;
        let prune_due = state
            .service_state
            .last_prune
            .is_none_or(|last_prune| now.signed_duration_since(last_prune) >= prune_interval);
        drop(state);
        self.record_diagnostic(
            DiagnosticLevel::Information,
            "retention.started",
            "Snapshot retention started.",
            None,
        );
        Some(RetentionPlan { prune_due })
    }

    pub fn finish_retention(
        &self,
        backup: BackupOutcome,
        retention: &RetentionOutcome,
        now: DateTime<Utc>,
    ) -> BackupOutcome {
        let mut state = self.state_guard();
        let warning = match &retention.kind {
            RetentionOutcomeKind::Succeeded { pruned } => {
                state.service_state.last_retention = Some(now);
                if *pruned {
                    state.service_state.last_prune = Some(now);
                }
                state.service_state.last_retention_error = None;
                None
            }
            RetentionOutcomeKind::Failed { code } => {
                state.service_state.last_retention_error = Some(code.clone());
                Some(code.clone())
            }
            RetentionOutcomeKind::Cancelled => {
                state.service_state.last_retention_error = Some("retention_cancelled".to_owned());
                Some("retention_cancelled".to_owned())
            }
        };
        let service_state = state.service_state.clone();
        drop(state);
        let save_failed = self
            .state_store
            .as_ref()
            .is_some_and(|store| store.save(&service_state).is_err());

        if let Some(code) = warning.as_deref() {
            self.record_diagnostic(
                DiagnosticLevel::Warning,
                "retention.failed",
                "Snapshot retention did not complete.",
                Some(code),
            );
            backup.with_warning(code)
        } else if save_failed {
            self.record_diagnostic(
                DiagnosticLevel::Warning,
                "retention.state_save_failed",
                "Retention completed but its state could not be saved.",
                Some("retention_state_save_failed"),
            );
            backup.with_warning("retention_state_save_failed")
        } else {
            self.record_diagnostic(
                DiagnosticLevel::Information,
                "retention.succeeded",
                "Snapshot retention completed successfully.",
                None,
            );
            backup
        }
    }

    pub fn finish_backup(&self, outcome: &BackupOutcome) {
        self.finish_backup_at(Utc::now(), outcome);
    }

    pub(super) fn finish_backup_at(&self, now: DateTime<Utc>, outcome: &BackupOutcome) {
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
                BackupOutcomeKind::SucceededWithWarnings => (
                    BackupRunOutcome::SucceededWithWarnings,
                    outcome.warning_code.clone(),
                ),
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
                failed_items: outcome.failure_details.items().to_vec(),
                failed_items_omitted: outcome.failure_details.omitted(),
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
        match &outcome.kind {
            BackupOutcomeKind::Succeeded => self.record_diagnostic(
                DiagnosticLevel::Information,
                "backup.succeeded",
                "Backup completed successfully.",
                None,
            ),
            BackupOutcomeKind::SucceededWithWarnings => self.record_diagnostic(
                DiagnosticLevel::Warning,
                "backup.warning",
                "Backup completed with warnings.",
                outcome.warning_code.as_deref(),
            ),
            BackupOutcomeKind::Failed { code } => self.record_diagnostic(
                DiagnosticLevel::Error,
                "backup.failed",
                "Backup failed.",
                Some(code),
            ),
            BackupOutcomeKind::Cancelled => self.record_diagnostic(
                DiagnosticLevel::Information,
                "backup.cancelled",
                "Backup was cancelled.",
                None,
            ),
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
        drop(state);
        match &outcome.kind {
            RepositoryOutcomeKind::Succeeded => self.record_diagnostic(
                DiagnosticLevel::Information,
                "repository.operation_succeeded",
                "Repository operation completed successfully.",
                None,
            ),
            RepositoryOutcomeKind::Failed { code } => self.record_diagnostic(
                DiagnosticLevel::Error,
                "repository.operation_failed",
                "Repository operation failed.",
                Some(code),
            ),
            RepositoryOutcomeKind::Cancelled => self.record_diagnostic(
                DiagnosticLevel::Information,
                "repository.operation_cancelled",
                "Repository operation was cancelled.",
                None,
            ),
        }
    }
}
