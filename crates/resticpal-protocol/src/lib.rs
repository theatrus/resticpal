#![forbid(unsafe_code)]

//! Versioned messages and bounded JSON framing for local resticpal IPC.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use resticpal_core::config::{RepositoryMode, SecretEnvironmentVariable};
use resticpal_core::status::{BackupRunRecord, ServiceStatus};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

pub const PROTOCOL_VERSION: u32 = 2;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub protocol_version: u32,
    pub request_id: u64,
    pub command: RequestCommand,
}

impl Request {
    #[must_use]
    pub const fn new(request_id: u64, command: RequestCommand) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            request_id,
            command,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequestCommand {
    GetStatus,
    GetRunHistory {
        limit: u16,
    },
    GetBackupSources,
    DiscoverBackupSources,
    UpdateBackupSources {
        paths: Option<Vec<PathBuf>>,
        exclusions: Option<Vec<String>>,
    },
    GetRepository,
    UpdateRepository {
        display_name: Option<String>,
        url: Option<String>,
        mode: Option<RepositoryMode>,
        options: Option<BTreeMap<String, String>>,
        secret_updates: Vec<RepositorySecretUpdate>,
    },
    ValidateRepository,
    InitializeRepository,
    GetSchedule,
    UpdateSchedule {
        interval_hours: Option<u32>,
        wake_grace_seconds: Option<u64>,
        wake_lock_timeout_seconds: Option<u64>,
        allow_on_battery: Option<bool>,
        allow_metered_network: Option<bool>,
    },
    RunBackupNow,
    CancelBackup,
    DeferBackup {
        minutes: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub protocol_version: u32,
    pub request_id: u64,
    pub payload: ResponsePayload,
}

impl Response {
    #[must_use]
    pub const fn new(request_id: u64, payload: ResponsePayload) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            request_id,
            payload,
        }
    }

    #[must_use]
    pub fn incompatible(request_id: u64, received_version: u32) -> Self {
        Self::new(
            request_id,
            ResponsePayload::Rejected {
                code: "incompatible_protocol".to_owned(),
                message: format!(
                    "client protocol {received_version} is incompatible with service protocol {PROTOCOL_VERSION}"
                ),
            },
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResponsePayload {
    Status {
        status: ServiceStatus,
    },
    RunHistory {
        runs: Vec<BackupRunRecord>,
    },
    BackupSources {
        configuration: BackupSourcesView,
    },
    DiscoveredBackupSources {
        sources: Vec<DiscoveredBackupSource>,
    },
    Repository {
        configuration: RepositoryView,
    },
    Schedule {
        configuration: ScheduleView,
    },
    Accepted {
        message: String,
    },
    Rejected {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupSourcesView {
    pub paths: Vec<PathBuf>,
    pub exclusions: Vec<String>,
    pub paths_locked: bool,
    pub exclusions_locked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveredBackupSource {
    pub profile_name: String,
    pub kind: DiscoveredSourceKind,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveredSourceKind {
    Desktop,
    Documents,
    Pictures,
    Videos,
    Music,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryView {
    pub display_name: Option<String>,
    pub url: Option<String>,
    pub mode: RepositoryMode,
    pub options: BTreeMap<String, String>,
    pub configured_secrets: Vec<SecretEnvironmentVariable>,
    pub operation_status: RepositoryOperationStatus,
    pub display_name_locked: bool,
    pub url_locked: bool,
    pub mode_locked: bool,
    pub options_locked: bool,
    pub secrets_locked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryOperationKind {
    Validate,
    Initialize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RepositoryOperationStatus {
    NotRun,
    ValidationRequired,
    Running {
        operation: RepositoryOperationKind,
    },
    Succeeded {
        operation: RepositoryOperationKind,
        completed_at: DateTime<Utc>,
    },
    Failed {
        operation: RepositoryOperationKind,
        completed_at: DateTime<Utc>,
        code: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleView {
    pub interval_hours: u32,
    pub wake_grace_seconds: u64,
    pub wake_lock_timeout_seconds: u64,
    pub allow_on_battery: bool,
    pub allow_metered_network: bool,
    pub interval_hours_locked: bool,
    pub wake_grace_seconds_locked: bool,
    pub wake_lock_timeout_seconds_locked: bool,
    pub allow_on_battery_locked: bool,
    pub allow_metered_network_locked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum RepositorySecretUpdate {
    Set {
        variable: SecretEnvironmentVariable,
        value: SecretValue,
    },
    Remove {
        variable: SecretEnvironmentVariable,
    },
}

#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(String);

impl SecretValue {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl Serialize for SecretValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SecretValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self)
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

pub fn write_frame<T: Serialize>(mut writer: impl Write, value: &T) -> Result<(), FrameError> {
    let payload = Zeroizing::new(serde_json::to_vec(value)?);
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(payload.len()));
    }

    let length = u32::try_from(payload.len()).expect("maximum frame size fits in u32");
    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<T: DeserializeOwned>(mut reader: impl Read) -> Result<T, FrameError> {
    let mut length = [0_u8; size_of::<u32>()];
    reader.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(FrameError::InvalidLength(length));
    }

    let mut payload = Zeroizing::new(vec![0_u8; length]);
    reader.read_exact(&mut payload)?;
    Ok(serde_json::from_slice(&payload)?)
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("IPC frame I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("IPC frame JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("serialized IPC frame is too large: {0} bytes")]
    TooLarge(usize),
    #[error("invalid IPC frame length: {0} bytes")]
    InvalidLength(usize),
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use chrono::Utc;
    use resticpal_core::RepositoryMode;
    use resticpal_core::status::{BackupProgress, BackupState, ServiceStatus};

    use super::*;

    #[test]
    fn request_round_trips_through_bounded_framing() {
        let request = Request::new(42, RequestCommand::DeferBackup { minutes: 30 });
        let mut bytes = Vec::new();

        write_frame(&mut bytes, &request).expect("request should serialize");
        let decoded: Request = read_frame(Cursor::new(bytes)).expect("request should deserialize");

        assert_eq!(decoded, request);
    }

    #[test]
    fn bounded_history_request_round_trips() {
        let request = Request::new(43, RequestCommand::GetRunHistory { limit: 50 });
        let mut bytes = Vec::new();

        write_frame(&mut bytes, &request).expect("request should serialize");
        let decoded: Request = read_frame(Cursor::new(bytes)).expect("request should deserialize");

        assert_eq!(decoded, request);
    }

    #[test]
    fn status_response_round_trips() {
        let now = Utc::now();
        let response = Response::new(
            7,
            ResponsePayload::Status {
                status: ServiceStatus {
                    state: BackupState::Idle,
                    state_since: now,
                    last_attempt: None,
                    last_success: None,
                    next_deadline: Some(now),
                    repository_display_name: Some("S3 backup".to_owned()),
                    repository_mode: RepositoryMode::AppendOnly,
                    managed_revision: Some("policy-12".to_owned()),
                    progress: Some(BackupProgress {
                        percent_done: Some(42),
                        files_done: 5,
                        total_files: Some(12),
                        bytes_done: 426,
                        total_bytes: Some(1_000),
                        error_count: 0,
                    }),
                },
            },
        );
        let mut bytes = Vec::new();

        write_frame(&mut bytes, &response).expect("response should serialize");
        let decoded: Response =
            read_frame(Cursor::new(bytes)).expect("response should deserialize");

        assert_eq!(decoded, response);
    }

    #[test]
    fn oversized_declared_frame_is_rejected_before_allocation() {
        let declared = u32::try_from(MAX_FRAME_BYTES + 1)
            .expect("test frame size fits in u32")
            .to_le_bytes();

        assert!(matches!(
            read_frame::<Request>(Cursor::new(declared)),
            Err(FrameError::InvalidLength(length)) if length == MAX_FRAME_BYTES + 1
        ));
    }

    #[test]
    fn incompatible_response_preserves_request_id() {
        let response = Response::incompatible(99, PROTOCOL_VERSION + 1);

        assert_eq!(response.request_id, 99);
        assert!(matches!(
            response.payload,
            ResponsePayload::Rejected { ref code, .. } if code == "incompatible_protocol"
        ));
    }

    #[test]
    fn backup_source_update_round_trips_without_untyped_arguments() {
        let request = Request::new(
            100,
            RequestCommand::UpdateBackupSources {
                paths: Some(vec![PathBuf::from(r"C:\Users\Example\Documents")]),
                exclusions: Some(vec!["**/node_modules/**".to_owned()]),
            },
        );
        let mut bytes = Vec::new();

        write_frame(&mut bytes, &request).expect("request should serialize");
        let decoded: Request = read_frame(Cursor::new(bytes)).expect("request should deserialize");

        assert_eq!(decoded, request);
    }

    #[test]
    fn repository_secret_round_trips_but_is_redacted_from_debug_output() {
        let request = Request::new(
            101,
            RequestCommand::UpdateRepository {
                display_name: Some("S3 backup".to_owned()),
                url: Some("s3:https://s3.example.test/bucket".to_owned()),
                mode: Some(RepositoryMode::AppendOnly),
                options: Some(BTreeMap::new()),
                secret_updates: vec![RepositorySecretUpdate::Set {
                    variable: SecretEnvironmentVariable::ResticPassword,
                    value: SecretValue::new("unique-secret"),
                }],
            },
        );
        assert!(!format!("{request:?}").contains("unique-secret"));
        let mut bytes = Vec::new();

        write_frame(&mut bytes, &request).expect("request should serialize");
        let decoded: Request = read_frame(Cursor::new(bytes)).expect("request should deserialize");

        assert_eq!(decoded, request);
    }

    #[test]
    fn repository_operation_status_round_trips() {
        let completed_at = Utc::now();
        let response = Response::new(
            102,
            ResponsePayload::Repository {
                configuration: RepositoryView {
                    display_name: Some("S3 backup".to_owned()),
                    url: Some("s3:https://example.test/bucket".to_owned()),
                    mode: RepositoryMode::AppendOnly,
                    options: BTreeMap::new(),
                    configured_secrets: vec![SecretEnvironmentVariable::ResticPassword],
                    operation_status: RepositoryOperationStatus::Succeeded {
                        operation: RepositoryOperationKind::Validate,
                        completed_at,
                    },
                    display_name_locked: false,
                    url_locked: false,
                    mode_locked: true,
                    options_locked: false,
                    secrets_locked: false,
                },
            },
        );
        let mut bytes = Vec::new();

        write_frame(&mut bytes, &response).expect("response should serialize");
        let decoded: Response =
            read_frame(Cursor::new(bytes)).expect("response should deserialize");

        assert_eq!(decoded, response);
    }

    #[test]
    fn typed_schedule_update_round_trips() {
        let request = Request::new(
            103,
            RequestCommand::UpdateSchedule {
                interval_hours: Some(12),
                wake_grace_seconds: Some(600),
                wake_lock_timeout_seconds: Some(7_200),
                allow_on_battery: Some(false),
                allow_metered_network: Some(true),
            },
        );
        let mut bytes = Vec::new();

        write_frame(&mut bytes, &request).expect("request should serialize");
        let decoded: Request = read_frame(Cursor::new(bytes)).expect("request should deserialize");

        assert_eq!(decoded, request);
    }

    #[test]
    fn exact_protocol_rejects_unknown_fields() {
        let top_level = r#"{
            "protocol_version": 2,
            "request_id": 1,
            "command": { "type": "get_status" },
            "future_field": true
        }"#;
        assert!(serde_json::from_str::<Request>(top_level).is_err());

        let command = r#"{
            "protocol_version": 2,
            "request_id": 1,
            "command": { "type": "defer_backup", "minutes": 30, "future_field": true }
        }"#;
        assert!(serde_json::from_str::<Request>(command).is_err());
    }
}
