//! Control-channel messages between IPC / service-control threads and the
//! scheduler loop, plus the small data types the loop hands back to `main`.

use std::path::PathBuf;

use resticpal_core::schedule::BackupTrigger;
use resticpal_protocol::{
    RepositoryOperationKind, RestoreEntryView, RestoreSnapshotView, RestoreStatusView,
    UpdatePackage,
};

use crate::executor::{BackupOutcome, RepositoryOutcome};
use crate::updater::UpdateInstallOutcome;

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
    UpdateInstallRequested(UpdatePackage),
    UpdateInstallFinished(UpdateInstallOutcome),
    BackupFinished(BackupOutcome),
    RestoreQueryRequested {
        query_id: u64,
        request: RestoreQueryRequest,
    },
    RestoreQueryCancelled {
        query_id: u64,
    },
    RestoreQueryFinished {
        query_id: u64,
        outcome: RestoreQueryOutcome,
    },
    RestoreRequested {
        job_id: u64,
        snapshot_id: String,
        path: String,
        destination: PathBuf,
    },
    RestoreCancelled {
        job_id: u64,
    },
    RestoreFinished {
        job_id: u64,
        status: RestoreStatusView,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreQueryRequest {
    Snapshots,
    Directory { snapshot_id: String, path: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreQueryOutcome {
    Snapshots(Vec<RestoreSnapshotView>),
    Directory(Vec<RestoreEntryView>),
    Failed { code: String },
    Cancelled,
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
