//! Small free functions shared by more than one runtime submodule.

use chrono::{DateTime, Utc};
use resticpal_core::status::{BackupState, ServiceStatus};
use resticpal_protocol::{RepositoryOperationStatus, ResponsePayload};

pub(super) fn repository_operation_allows_backup(status: &RepositoryOperationStatus) -> bool {
    matches!(status, RepositoryOperationStatus::Succeeded { .. })
}

pub(super) fn transition_state(status: &mut ServiceStatus, next: BackupState, now: DateTime<Utc>) {
    if status.state != next {
        status.state = next;
        status.state_since = now;
    }
}

pub(super) fn rejected(code: &str, message: &str) -> ResponsePayload {
    ResponsePayload::Rejected {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

pub(super) fn administrator_required() -> ResponsePayload {
    rejected(
        "administrator_required",
        "Open resticpal as an administrator to change machine backup settings.",
    )
}
