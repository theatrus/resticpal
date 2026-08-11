//! Control-channel messages between IPC / service-control threads and the
//! scheduler loop, plus the small data types the loop hands back to `main`.

use resticpal_core::schedule::BackupTrigger;
use resticpal_protocol::RepositoryOperationKind;

use crate::executor::{BackupOutcome, RepositoryOutcome};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPlan {
    pub prune_due: bool,
}
