use super::events::ScheduleAction;
use super::helpers::*;
use super::state::ServiceRuntime;

use std::time::Duration as StdDuration;

use chrono::{DateTime, Utc};
use resticpal_core::schedule::{ScheduleBlocker, ScheduleDecision, SchedulerSnapshot, decide};
use resticpal_core::status::{BackupPhase, BackupState, WaitingReason};

use crate::conditions::SystemConditions;

const CONDITION_RETRY_SECONDS: u64 = 60;

impl ServiceRuntime {
    pub fn record_resume(&self, now: DateTime<Utc>) {
        self.state_guard().resumed_at = Some(now);
    }

    pub fn evaluate_schedule(
        &self,
        now: DateTime<Utc>,
        conditions: SystemConditions,
    ) -> ScheduleAction {
        let config = self.config();
        let mut state = self.state_guard();
        if state
            .update_hold_until
            .is_some_and(|deadline| deadline > now)
        {
            transition_state(
                &mut state.status,
                BackupState::Waiting {
                    reason: WaitingReason::Update,
                },
                now,
            );
            state.status.next_deadline = state.update_hold_until;
            return ScheduleAction::None;
        }
        state.update_hold_until = None;
        if let Some((previous_state, previous_deadline)) = state.update_hold_previous_status.take()
            && matches!(
                state.status.state,
                BackupState::Waiting {
                    reason: WaitingReason::Update
                }
            )
        {
            state.status.state = previous_state;
            state.status.state_since = now;
            state.status.next_deadline = previous_deadline;
        }
        if !config.is_configured() {
            return ScheduleAction::None;
        }
        if state.management_operation_active {
            return ScheduleAction::None;
        }
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
        // While a management operation runs, evaluate_schedule returns early
        // without refreshing next_deadline. If that deadline is already in the
        // past the loop would otherwise wake with a zero delay and busy-spin for
        // the whole (up to 30s network) operation; poll at a bounded cadence.
        if state.management_operation_active {
            return StdDuration::from_secs(CONDITION_RETRY_SECONDS);
        }
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

pub(super) fn repository_requires_network(repository: &str) -> bool {
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
