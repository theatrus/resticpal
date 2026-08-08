use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::config::ScheduleConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerSnapshot {
    pub now: DateTime<Utc>,
    pub last_success: Option<DateTime<Utc>>,
    pub resumed_at: Option<DateTime<Utc>>,
    pub manual_requested: bool,
    pub backup_running: bool,
    pub network_available: bool,
    pub on_battery: bool,
    pub metered_network: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ScheduleDecision {
    AlreadyRunning,
    Idle {
        next_deadline: DateTime<Utc>,
    },
    Waiting {
        trigger: BackupTrigger,
        blockers: Vec<ScheduleBlocker>,
        retry_at: Option<DateTime<Utc>>,
    },
    Start {
        trigger: BackupTrigger,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupTrigger {
    Manual,
    Scheduled,
    ResumeCatchUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleBlocker {
    WakeGrace,
    NetworkUnavailable,
    BatteryDisallowed,
    MeteredNetworkDisallowed,
}

#[must_use]
pub fn decide(config: &ScheduleConfig, snapshot: &SchedulerSnapshot) -> ScheduleDecision {
    if snapshot.backup_running {
        return ScheduleDecision::AlreadyRunning;
    }

    let next_deadline = snapshot.last_success.map_or(snapshot.now, |last_success| {
        last_success + Duration::hours(i64::from(config.interval_hours))
    });
    let due = snapshot.manual_requested || snapshot.now >= next_deadline;

    if !due {
        return ScheduleDecision::Idle { next_deadline };
    }

    let trigger = if snapshot.manual_requested {
        BackupTrigger::Manual
    } else if snapshot.resumed_at.is_some() {
        BackupTrigger::ResumeCatchUp
    } else {
        BackupTrigger::Scheduled
    };

    let mut blockers = Vec::new();
    let mut retry_at = None;

    if !snapshot.manual_requested
        && let Some(resumed_at) = snapshot.resumed_at
    {
        let grace_end = resumed_at
            + Duration::seconds(i64::try_from(config.wake_grace_seconds).unwrap_or(i64::MAX));
        if snapshot.now < grace_end {
            blockers.push(ScheduleBlocker::WakeGrace);
            retry_at = Some(grace_end);
        }
    }

    if !snapshot.network_available {
        blockers.push(ScheduleBlocker::NetworkUnavailable);
    }
    if snapshot.on_battery && !config.allow_on_battery {
        blockers.push(ScheduleBlocker::BatteryDisallowed);
    }
    if snapshot.metered_network && !config.allow_metered_network {
        blockers.push(ScheduleBlocker::MeteredNetworkDisallowed);
    }

    if blockers.is_empty() {
        ScheduleDecision::Start { trigger }
    } else {
        ScheduleDecision::Waiting {
            trigger,
            blockers,
            retry_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn timestamp(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 8, hour, minute, 0)
            .single()
            .expect("valid timestamp")
    }

    fn snapshot(now: DateTime<Utc>) -> SchedulerSnapshot {
        SchedulerSnapshot {
            now,
            last_success: Some(timestamp(8, 0)),
            resumed_at: None,
            manual_requested: false,
            backup_running: false,
            network_available: true,
            on_battery: false,
            metered_network: false,
        }
    }

    #[test]
    fn waits_until_the_daily_deadline() {
        let state = snapshot(timestamp(12, 0));

        assert_eq!(
            decide(&ScheduleConfig::default(), &state),
            ScheduleDecision::Idle {
                next_deadline: timestamp(8, 0) + Duration::hours(24)
            }
        );
    }

    #[test]
    fn overdue_resume_observes_the_wake_grace_period() {
        let mut state = snapshot(timestamp(9, 3));
        state.last_success = Some(timestamp(8, 0) - Duration::hours(25));
        state.resumed_at = Some(timestamp(9, 0));

        assert_eq!(
            decide(&ScheduleConfig::default(), &state),
            ScheduleDecision::Waiting {
                trigger: BackupTrigger::ResumeCatchUp,
                blockers: vec![ScheduleBlocker::WakeGrace],
                retry_at: Some(timestamp(9, 5)),
            }
        );
    }

    #[test]
    fn manual_request_bypasses_wake_grace_but_not_safety_constraints() {
        let config = ScheduleConfig {
            allow_on_battery: false,
            ..ScheduleConfig::default()
        };
        let mut state = snapshot(timestamp(9, 1));
        state.resumed_at = Some(timestamp(9, 0));
        state.manual_requested = true;
        state.on_battery = true;

        assert_eq!(
            decide(&config, &state),
            ScheduleDecision::Waiting {
                trigger: BackupTrigger::Manual,
                blockers: vec![ScheduleBlocker::BatteryDisallowed],
                retry_at: None,
            }
        );
    }

    #[test]
    fn default_policy_allows_battery_and_metered_networks() {
        let mut state = snapshot(timestamp(9, 0));
        state.last_success = None;
        state.on_battery = true;
        state.metered_network = true;

        assert_eq!(
            decide(&ScheduleConfig::default(), &state),
            ScheduleDecision::Start {
                trigger: BackupTrigger::Scheduled
            }
        );
    }

    #[test]
    fn running_backup_coalesces_new_triggers() {
        let mut state = snapshot(timestamp(9, 0));
        state.backup_running = true;
        state.manual_requested = true;

        assert_eq!(
            decide(&ScheduleConfig::default(), &state),
            ScheduleDecision::AlreadyRunning
        );
    }
}
