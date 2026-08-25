//! Administrator-only snapshot browsing and service-owned file recovery.
//!
//! Repository paths are deliberately confined to this module's private state
//! and elevated IPC responses. They must never enter canonical service status,
//! remote reports, ordinary-user history, or diagnostic messages.

use super::events::{RestoreQueryOutcome, RestoreQueryRequest, RuntimeEvent};
use super::helpers::{administrator_required, rejected, repository_operation_allows_backup};
use super::state::{RuntimeState, ServiceRuntime};

use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

use chrono::Utc;
use resticpal_core::config::{MAX_PATH_CHARACTERS, ManagementMode};
use resticpal_core::policy::PolicyField;
use resticpal_core::restic::{validate_restore_snapshot_id, validate_restore_snapshot_path};
use resticpal_core::status::BackupState;
use resticpal_protocol::{
    DiagnosticLevel, MAX_FRAME_BYTES, MAX_RESTORE_QUERY_PAGE_SIZE, RepositoryOperationStatus,
    ResponsePayload, RestoreJobState, RestoreNodeType, RestoreQueryKind, RestoreQueryState,
    RestoreQueryView, RestoreSettingsView, RestoreStatusView,
};
use resticpal_windows::named_pipe::ClientIdentity;

use crate::executor::RestoreProgress;

const MAX_RETAINED_RESTORE_QUERIES: usize = 8;
const MAX_AUTHORIZED_RESTORE_NODES: usize = 65_536;
const MAX_RESTORE_RESPONSE_BYTES: usize = MAX_FRAME_BYTES - 16 * 1024;

impl ServiceRuntime {
    pub(super) fn restore_settings(&self, identity: ClientIdentity) -> ResponsePayload {
        if !identity.is_elevated_administrator {
            return administrator_required();
        }

        let _mutation = self
            .configuration_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ResponsePayload::RestoreSettings {
            configuration: RestoreSettingsView {
                enabled: self.config_read().restore.enabled,
                enabled_locked: self.field_locked(PolicyField::RestoreEnabled),
                managed: self.local_config_guard().management.mode != ManagementMode::Disabled,
            },
        }
    }

    pub(super) fn update_restore_settings(
        &self,
        enabled: bool,
        identity: ClientIdentity,
    ) -> ResponsePayload {
        if !identity.is_elevated_administrator {
            return administrator_required();
        }

        let _mutation = self
            .configuration_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.field_locked(PolicyField::RestoreEnabled) {
            return rejected(
                "managed_field_locked",
                "File restoration is locked by managed policy.",
            );
        }

        let mut state = self.state_guard();
        if state.restore_operation_active {
            return rejected(
                "restore_running",
                "Wait for the current recovery operation to finish before changing its setting.",
            );
        }
        if state.update_install_active
            || state
                .update_hold_until
                .is_some_and(|deadline| deadline > Utc::now())
        {
            return rejected(
                "update_pending",
                "Wait for the resticpal update to finish before changing recovery settings.",
            );
        }

        let mut candidate = self.local_config_guard().clone();
        candidate.restore.enabled = Some(enabled);
        if let Some(store) = &self.config_store
            && let Err(error) = store.save(&candidate)
        {
            eprintln!(
                "could not save local configuration to {}: {error}",
                store.path().display()
            );
            return rejected(
                "configuration_save_failed",
                "The file-restoration setting could not be saved.",
            );
        }

        *self.local_config_guard() = candidate;
        self.config_write().restore.enabled = enabled;
        if !enabled {
            clear_sensitive_restore_state(&mut state);
        }
        drop(state);
        let _ = self.events.send(RuntimeEvent::ConfigurationChanged);
        ResponsePayload::Accepted {
            message: if enabled {
                "Administrators can browse backups and restore files on this PC.".to_owned()
            } else {
                "File restoration is disabled on this PC.".to_owned()
            },
        }
    }

    pub(super) fn begin_restore_query(
        &self,
        request: RestoreQueryRequest,
        identity: ClientIdentity,
    ) -> ResponsePayload {
        if !identity.is_elevated_administrator {
            return administrator_required();
        }
        if let RestoreQueryRequest::Directory { snapshot_id, path } = &request
            && let Some(response) = validate_snapshot_selection(snapshot_id, path)
        {
            return response;
        }

        let _mutation = self
            .configuration_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(response) = self.require_restore_enabled() {
            return response;
        }

        let mut state = self.state_guard();
        if let Some(response) = ensure_repository_is_available(&state) {
            return response;
        }

        if let RestoreQueryRequest::Directory { snapshot_id, path } = &request
            && let Some(response) =
                authorize_snapshot_node(&state, snapshot_id, path, Some(RestoreNodeType::Directory))
        {
            return response;
        }

        if matches!(request, RestoreQueryRequest::Snapshots) {
            // A fresh repository inventory invalidates previous selections;
            // clients may use only exact IDs returned by the local-host query.
            state.authorized_restore_snapshots.clear();
            state.restore_query_snapshot_ids.clear();
            state.restore_queries.clear();
        }

        let query_id = next_restore_identifier(&mut state);
        while state.restore_queries.len() >= MAX_RETAINED_RESTORE_QUERIES {
            let Some(oldest) = state.restore_queries.keys().next().copied() else {
                break;
            };
            state.restore_queries.remove(&oldest);
            state.restore_query_snapshot_ids.remove(&oldest);
        }

        let kind = match &request {
            RestoreQueryRequest::Snapshots => RestoreQueryKind::Snapshots,
            RestoreQueryRequest::Directory { snapshot_id, .. } => {
                state
                    .restore_query_snapshot_ids
                    .insert(query_id, snapshot_id.clone());
                RestoreQueryKind::Directory
            }
        };
        state.restore_queries.insert(
            query_id,
            RestoreQueryView {
                query_id,
                kind,
                state: RestoreQueryState::Running,
                snapshots: Vec::new(),
                entries: Vec::new(),
                total: 0,
                message: None,
            },
        );
        state.active_restore_query = Some(query_id);
        state.restore_operation_active = true;

        if self
            .events
            .send(RuntimeEvent::RestoreQueryRequested { query_id, request })
            .is_err()
        {
            state.restore_queries.remove(&query_id);
            state.restore_query_snapshot_ids.remove(&query_id);
            state.active_restore_query = None;
            state.restore_operation_active = false;
            return service_stopping();
        }

        ResponsePayload::RestoreQueryStarted { query_id }
    }

    pub(super) fn restore_query(
        &self,
        query_id: u64,
        offset: u32,
        limit: u16,
        identity: ClientIdentity,
    ) -> ResponsePayload {
        if let Some(response) = self.authorize_restore(identity) {
            return response;
        }
        if !(1..=MAX_RESTORE_QUERY_PAGE_SIZE).contains(&limit) {
            return rejected(
                "invalid_restore_query_limit",
                "Recovery listings must request between one and 100 results.",
            );
        }

        let state = self.state_guard();
        let Some(query) = state.restore_queries.get(&query_id) else {
            return rejected(
                "restore_query_not_found",
                "The requested recovery listing has expired or does not exist.",
            );
        };

        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
        let available = match query.kind {
            RestoreQueryKind::Snapshots => query.snapshots.len(),
            RestoreQueryKind::Directory => query.entries.len(),
        };
        if offset > available {
            return rejected(
                "invalid_restore_query_offset",
                "The recovery listing position is outside its available results.",
            );
        }

        let end = offset.saturating_add(usize::from(limit)).min(available);
        let mut result = RestoreQueryView {
            query_id,
            kind: query.kind,
            state: query.state,
            snapshots: if query.kind == RestoreQueryKind::Snapshots {
                query.snapshots[offset..end].to_vec()
            } else {
                Vec::new()
            },
            entries: if query.kind == RestoreQueryKind::Directory {
                query.entries[offset..end].to_vec()
            } else {
                Vec::new()
            },
            total: query.total,
            message: query.message.clone(),
        };

        loop {
            let Ok(encoded) = serde_json::to_vec(&result) else {
                return rejected(
                    "restore_query_encoding_failed",
                    "The recovery listing could not be encoded safely.",
                );
            };
            if encoded.len() <= MAX_RESTORE_RESPONSE_BYTES {
                return ResponsePayload::RestoreQuery { result };
            }
            let removed = match result.kind {
                RestoreQueryKind::Snapshots => result.snapshots.pop().is_some(),
                RestoreQueryKind::Directory => result.entries.pop().is_some(),
            };
            if !removed {
                return rejected(
                    "restore_query_item_too_large",
                    "A recovery listing entry exceeds the safe response size.",
                );
            }
        }
    }

    pub(super) fn cancel_restore_query(
        &self,
        query_id: u64,
        identity: ClientIdentity,
    ) -> ResponsePayload {
        if let Some(response) = self.authorize_restore(identity) {
            return response;
        }

        let state = self.state_guard();
        if state.active_restore_query != Some(query_id) {
            return rejected(
                "restore_query_not_running",
                "The requested recovery listing is no longer running.",
            );
        }
        self.send_event(
            RuntimeEvent::RestoreQueryCancelled { query_id },
            "Recovery listing cancellation requested.",
        )
    }

    pub fn finish_restore_query(&self, query_id: u64, outcome: RestoreQueryOutcome) {
        let restore_enabled = self.config_read().restore.enabled;
        let mut state = self.state_guard();
        if state.active_restore_query != Some(query_id) {
            return;
        }

        if !state.restore_queries.contains_key(&query_id) {
            state.active_restore_query = None;
            state.restore_operation_active = false;
            return;
        }
        match outcome {
            RestoreQueryOutcome::Snapshots(snapshots) => {
                let mut authorized_snapshots = std::collections::BTreeMap::new();
                let mut authorized_count = 0_usize;
                let mut authorization_limit_exceeded = false;
                for snapshot in &snapshots {
                    let mut nodes = std::collections::BTreeMap::new();
                    nodes.insert("/".to_owned(), RestoreNodeType::Directory);
                    for source in &snapshot.paths {
                        if let Some(path) = snapshot_source_path(source) {
                            nodes.insert(path, RestoreNodeType::Directory);
                        }
                    }
                    authorized_count = authorized_count.saturating_add(nodes.len());
                    if authorized_count > MAX_AUTHORIZED_RESTORE_NODES {
                        authorization_limit_exceeded = true;
                        break;
                    }
                    authorized_snapshots.insert(snapshot.id.clone(), nodes);
                }
                if authorization_limit_exceeded {
                    state.authorized_restore_snapshots.clear();
                } else {
                    state.authorized_restore_snapshots = authorized_snapshots;
                }
                let query = state
                    .restore_queries
                    .get_mut(&query_id)
                    .expect("the query was found above");
                if authorization_limit_exceeded {
                    query.state = RestoreQueryState::Failed;
                    query.message = Some(
                        restore_failure_message("restore_authorization_limit_exceeded").to_owned(),
                    );
                } else {
                    query.total = u64::try_from(snapshots.len()).unwrap_or(u64::MAX);
                    query.snapshots = snapshots;
                    query.state = RestoreQueryState::Succeeded;
                }
            }
            RestoreQueryOutcome::Directory(entries) => {
                let snapshot_id = state.restore_query_snapshot_ids.get(&query_id).cloned();
                let authorized_count = state
                    .authorized_restore_snapshots
                    .values()
                    .map(std::collections::BTreeMap::len)
                    .sum::<usize>();
                let additional_count = snapshot_id
                    .as_ref()
                    .and_then(|snapshot_id| state.authorized_restore_snapshots.get(snapshot_id))
                    .map_or(0, |nodes| {
                        entries
                            .iter()
                            .filter(|entry| !nodes.contains_key(&entry.path))
                            .count()
                    });
                if authorized_count.saturating_add(additional_count) > MAX_AUTHORIZED_RESTORE_NODES
                {
                    let query = state
                        .restore_queries
                        .get_mut(&query_id)
                        .expect("the query was found above");
                    query.state = RestoreQueryState::Failed;
                    query.message = Some(
                        restore_failure_message("restore_authorization_limit_exceeded").to_owned(),
                    );
                } else {
                    if let Some(snapshot_id) = snapshot_id
                        && let Some(nodes) =
                            state.authorized_restore_snapshots.get_mut(&snapshot_id)
                    {
                        for entry in &entries {
                            nodes.insert(entry.path.clone(), entry.node_type);
                        }
                    }
                    let query = state
                        .restore_queries
                        .get_mut(&query_id)
                        .expect("the query was found above");
                    query.total = u64::try_from(entries.len()).unwrap_or(u64::MAX);
                    query.entries = entries;
                    query.state = RestoreQueryState::Succeeded;
                }
            }
            RestoreQueryOutcome::Failed { code } => {
                let query = state
                    .restore_queries
                    .get_mut(&query_id)
                    .expect("the query was found above");
                query.state = RestoreQueryState::Failed;
                query.message = Some(restore_failure_message(&code).to_owned());
            }
            RestoreQueryOutcome::Cancelled => {
                let query = state
                    .restore_queries
                    .get_mut(&query_id)
                    .expect("the query was found above");
                query.state = RestoreQueryState::Cancelled;
                query.message = Some("The recovery listing was cancelled.".to_owned());
            }
        }
        state.active_restore_query = None;
        state.restore_operation_active = false;
        if !restore_enabled {
            clear_sensitive_restore_state(&mut state);
        }
    }

    pub(super) fn begin_restore(
        &self,
        snapshot_id: String,
        path: String,
        destination: PathBuf,
        identity: ClientIdentity,
    ) -> ResponsePayload {
        if !identity.is_elevated_administrator {
            return administrator_required();
        }
        if let Some(response) = validate_snapshot_selection(&snapshot_id, &path) {
            return response;
        }
        if path == "/" {
            return rejected(
                "invalid_restore_path",
                "Choose one file or folder rather than the entire repository root.",
            );
        }
        if !destination.is_absolute()
            || destination.as_os_str().encode_wide().count() > MAX_PATH_CHARACTERS
        {
            return rejected(
                "invalid_restore_destination",
                "Choose an existing folder on a local Windows drive.",
            );
        }

        let _mutation = self
            .configuration_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(response) = self.require_restore_enabled() {
            return response;
        }
        let mut state = self.state_guard();
        if let Some(response) = ensure_repository_is_available(&state) {
            return response;
        }
        if let Some(response) = authorize_snapshot_node(&state, &snapshot_id, &path, None) {
            return response;
        }

        let job_id = next_restore_identifier(&mut state);
        state.restore_status = RestoreStatusView {
            job_id: Some(job_id),
            state: RestoreJobState::Running,
            files_restored: Some(0),
            bytes_restored: Some(0),
            total_files: None,
            total_bytes: None,
            destination: None,
            message: Some("Preparing a verified file restore.".to_owned()),
        };
        state.restore_operation_active = true;
        if self
            .events
            .send(RuntimeEvent::RestoreRequested {
                job_id,
                snapshot_id,
                path,
                destination,
            })
            .is_err()
        {
            state.restore_status = RestoreStatusView::default();
            state.restore_operation_active = false;
            return service_stopping();
        }
        drop(state);
        self.record_diagnostic(
            DiagnosticLevel::Information,
            "restore.started",
            "An administrator started verified file recovery.",
            None,
        );

        ResponsePayload::RestoreStarted { job_id }
    }

    pub(super) fn restore_status(&self, identity: ClientIdentity) -> ResponsePayload {
        if let Some(response) = self.authorize_active_restore(identity) {
            return response;
        }
        ResponsePayload::RestoreStatus {
            status: self.state_guard().restore_status.clone(),
        }
    }

    pub(super) fn cancel_restore(&self, identity: ClientIdentity) -> ResponsePayload {
        if let Some(response) = self.authorize_active_restore(identity) {
            return response;
        }
        let state = self.state_guard();
        if state.restore_status.state != RestoreJobState::Running {
            return rejected(
                "restore_not_running",
                "No file restore is currently running.",
            );
        }
        let Some(job_id) = state.restore_status.job_id else {
            return rejected(
                "restore_not_running",
                "No file restore is currently running.",
            );
        };
        self.send_event(
            RuntimeEvent::RestoreCancelled { job_id },
            "File restore cancellation requested.",
        )
    }

    pub fn update_restore_progress(&self, job_id: u64, progress: RestoreProgress) {
        let mut state = self.state_guard();
        if state.restore_status.job_id != Some(job_id)
            || state.restore_status.state != RestoreJobState::Running
        {
            return;
        }
        state.restore_status.files_restored = progress.files_restored;
        state.restore_status.bytes_restored = progress.bytes_restored;
        state.restore_status.total_files = progress.total_files;
        state.restore_status.total_bytes = progress.total_bytes;
        if let Some(destination) = progress.destination {
            state.restore_status.destination = Some(destination.display().to_string());
        }
        state.restore_status.message =
            Some("Restoring and verifying the selected files.".to_owned());
    }

    pub fn finish_restore(&self, job_id: u64, status: RestoreStatusView) {
        let restore_enabled = self.config_read().restore.enabled;
        let mut state = self.state_guard();
        if state.restore_status.job_id != Some(job_id) {
            return;
        }
        state.restore_status = status.clone();
        state.restore_operation_active = false;
        if !restore_enabled {
            clear_sensitive_restore_state(&mut state);
        }
        drop(state);

        let (level, event_id, message) = match status.state {
            RestoreJobState::Succeeded => (
                DiagnosticLevel::Information,
                "restore.succeeded",
                "Verified file recovery completed successfully.",
            ),
            RestoreJobState::Cancelled => (
                DiagnosticLevel::Information,
                "restore.cancelled",
                "An administrator cancelled file recovery.",
            ),
            _ => (
                DiagnosticLevel::Error,
                "restore.failed",
                "Verified file recovery could not be completed.",
            ),
        };
        self.record_diagnostic(level, event_id, message, None);
    }

    fn authorize_restore(&self, identity: ClientIdentity) -> Option<ResponsePayload> {
        if !identity.is_elevated_administrator {
            return Some(administrator_required());
        }
        self.require_restore_enabled()
    }

    fn authorize_active_restore(&self, identity: ClientIdentity) -> Option<ResponsePayload> {
        if !identity.is_elevated_administrator {
            return Some(administrator_required());
        }
        if self.config_read().restore.enabled
            || self.state_guard().restore_status.state == RestoreJobState::Running
        {
            None
        } else {
            self.require_restore_enabled()
        }
    }

    fn require_restore_enabled(&self) -> Option<ResponsePayload> {
        if self.config_read().restore.enabled {
            None
        } else {
            Some(rejected(
                "restore_disabled",
                "File restoration is disabled by this PC's current settings or managed policy.",
            ))
        }
    }
}

pub(super) fn clear_sensitive_restore_state(state: &mut RuntimeState) {
    if state.restore_operation_active {
        return;
    }
    state.restore_queries.clear();
    state.authorized_restore_snapshots.clear();
    state.restore_query_snapshot_ids.clear();
    state.active_restore_query = None;
    state.restore_status = RestoreStatusView::default();
}

fn validate_snapshot_selection(snapshot_id: &str, path: &str) -> Option<ResponsePayload> {
    if validate_restore_snapshot_id(snapshot_id).is_err() {
        return Some(rejected(
            "invalid_restore_snapshot",
            "Choose an exact backup snapshot from this repository.",
        ));
    }
    if validate_restore_snapshot_path(path).is_err() {
        return Some(rejected(
            "invalid_restore_path",
            "Choose one existing file or folder inside the selected backup.",
        ));
    }
    None
}

fn ensure_repository_is_available(state: &RuntimeState) -> Option<ResponsePayload> {
    if !repository_operation_allows_backup(&state.repository_operation) {
        return Some(rejected(
            "repository_not_ready",
            "Validate the backup repository before browsing or restoring files.",
        ));
    }
    if matches!(state.status.state, BackupState::Running { .. }) {
        return Some(rejected(
            "backup_running",
            "Wait for the active backup to finish before browsing or restoring files.",
        ));
    }
    if state.restore_operation_active {
        return Some(rejected(
            "restore_running",
            "Another backup browsing or file-recovery operation is already running.",
        ));
    }
    if state.update_install_active
        || state
            .update_hold_until
            .is_some_and(|deadline| deadline > Utc::now())
    {
        return Some(rejected(
            "update_pending",
            "Wait for the resticpal update to finish before recovering files.",
        ));
    }
    if state.management_operation_active
        || matches!(
            state.repository_operation,
            RepositoryOperationStatus::Running { .. }
        )
    {
        return Some(rejected(
            "operation_running",
            "Wait for the current repository or management operation to finish.",
        ));
    }
    None
}

fn authorize_snapshot_node(
    state: &RuntimeState,
    snapshot_id: &str,
    path: &str,
    expected_type: Option<RestoreNodeType>,
) -> Option<ResponsePayload> {
    let Some(nodes) = state.authorized_restore_snapshots.get(snapshot_id) else {
        return Some(rejected(
            "restore_snapshot_not_authorized",
            "Choose a backup belonging to this PC from the current snapshot listing.",
        ));
    };
    let Some(node_type) = nodes.get(path) else {
        return Some(rejected(
            "restore_path_not_authorized",
            "Browse to the selected file or folder before recovering it.",
        ));
    };
    if expected_type.is_some_and(|expected| *node_type != expected) {
        return Some(rejected(
            "invalid_restore_path",
            "Only backup folders can be opened in the recovery browser.",
        ));
    }
    None
}

fn snapshot_source_path(source: &str) -> Option<String> {
    let bytes = source.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || !matches!(bytes[2], b'\\' | b'/')
    {
        return None;
    }
    // restic preserves the original Windows drive-letter case in both the
    // snapshot metadata and its virtual filesystem, so changing the case
    // would authorize a path that cannot actually be browsed or restored.
    let drive = bytes[0] as char;
    let suffix = source[2..].replace('\\', "/");
    let suffix = suffix.trim_end_matches('/');
    let normalized = if suffix.is_empty() {
        format!("/{drive}")
    } else {
        format!("/{drive}{suffix}")
    };
    validate_restore_snapshot_path(&normalized)
        .is_ok()
        .then_some(normalized)
}

fn next_restore_identifier(state: &mut RuntimeState) -> u64 {
    let identifier = state.next_restore_identifier;
    state.next_restore_identifier = identifier.checked_add(1).unwrap_or(1);
    identifier
}

fn service_stopping() -> ResponsePayload {
    rejected(
        "service_stopping",
        "The backup service is stopping. Try again shortly.",
    )
}

pub fn restore_failure_message(code: &str) -> &'static str {
    match code {
        "restore_cancelled" => "File recovery was cancelled.",
        "invalid_restore_snapshot" | "restore_snapshot_invalid" => {
            "The selected backup snapshot is no longer available."
        }
        "invalid_restore_path" | "restore_path_invalid" => {
            "The selected backup item is not available for recovery."
        }
        "invalid_restore_destination"
        | "restore_destination_invalid"
        | "restore_destination_network_unsupported"
        | "restore_destination_unavailable"
        | "restore_destination_not_directory"
        | "restore_destination_alias_unsupported"
        | "restore_destination_protected" => {
            "Choose an existing, unprotected folder on a local Windows drive."
        }
        "restore_destination_create_failed" | "restore_destination_creation_failed" => {
            "A new recovery folder could not be created in the selected destination."
        }
        "restore_destination_security_failed" => {
            "Windows could not safely protect or grant access to the recovered files."
        }
        "restore_verification_failed" => "The recovered files could not be verified.",
        "restore_unsupported_node" => {
            "The selected folder contains a symbolic link, device, or another unsupported item."
        }
        "restore_subtree_limit_exceeded" | "restore_directory_limit_exceeded" => {
            "The selected recovery folder exceeds the safe repository-listing limit."
        }
        "restore_authorization_limit_exceeded" => {
            "The recovery browser reached its safe memory limit. Refresh the backup listing and try again."
        }
        "restic_termination_failed" => {
            "Windows could not safely stop the recovery process; its files remain protected."
        }
        "restore_timed_out" => "File recovery exceeded its safe execution limit.",
        "executor_start_failed" => "The recovery worker could not be started.",
        _ => "The backup repository could not complete the requested recovery operation.",
    }
}
