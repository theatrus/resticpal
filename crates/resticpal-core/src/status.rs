use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::RepositoryMode;

/// Bounds sensitive per-run source detail retained locally. One explicit
/// detail request therefore remains comfortably below the 1 MiB IPC frame.
pub const MAX_BACKUP_FAILED_ITEMS: usize = 100;
pub const MAX_BACKUP_FAILED_ITEM_BYTES: usize = 4 * 1024;

/// Rejects values that could escape a bounded IPC frame or visually spoof
/// another path when shown in the local UI.
#[must_use]
pub fn is_safe_backup_failed_item(item: &str) -> bool {
    !item.is_empty()
        && item.len() <= MAX_BACKUP_FAILED_ITEM_BYTES
        && !item.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '\u{202a}'
                        | '\u{202b}'
                        | '\u{202c}'
                        | '\u{202d}'
                        | '\u{202e}'
                        | '\u{2066}'
                        | '\u{2067}'
                        | '\u{2068}'
                        | '\u{2069}'
                )
        })
}

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
    RepositoryValidation,
    Update,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupProgress {
    /// Whole-number percentage, clamped to the inclusive range 0..=100.
    pub percent_done: Option<u8>,
    pub files_done: u64,
    pub total_files: Option<u64>,
    pub bytes_done: u64,
    pub total_bytes: Option<u64>,
    pub error_count: u64,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<BackupProgress>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupRunOutcome {
    Succeeded,
    SucceededWithWarnings,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupRunRecord {
    pub id: u64,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub outcome: BackupRunOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_processed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_processed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_added: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    /// Number of source items restic reported it could not read. The paths are
    /// deliberately excluded from this generally readable history summary and
    /// are available only through the administrator-authorized detail request.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub failed_item_count: u64,
}

const fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_failure_items_are_bounded_and_safe_to_display() {
        assert!(is_safe_backup_failed_item(r"C:\Users\Example\document.txt"));
        assert!(!is_safe_backup_failed_item(""));
        assert!(!is_safe_backup_failed_item("line\nbreak"));
        assert!(!is_safe_backup_failed_item("spoof\u{202e}txt.exe"));
        assert!(!is_safe_backup_failed_item(
            &"x".repeat(MAX_BACKUP_FAILED_ITEM_BYTES + 1)
        ));
    }
}
