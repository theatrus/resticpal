use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::RepositoryMode;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BackupState {
    Unconfigured,
    Idle,
    Waiting { reason: WaitingReason },
    Running { phase: BackupPhase },
    Succeeded,
    SucceededWithWarnings,
    Failed { code: String },
    Cancelled,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitingReason {
    WakeGrace,
    Network,
    PolicyBackoff,
    Battery,
    MeteredNetwork,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupPhase {
    PreparingSnapshot,
    Scanning,
    Uploading,
    Finalizing,
    Retention,
    Checking,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub state: BackupState,
    pub state_since: DateTime<Utc>,
    pub last_attempt: Option<DateTime<Utc>>,
    pub last_success: Option<DateTime<Utc>>,
    pub next_deadline: Option<DateTime<Utc>>,
    pub repository_display_name: Option<String>,
    pub repository_mode: RepositoryMode,
    pub managed_revision: Option<String>,
}
