#![forbid(unsafe_code)]

//! Versioned messages and bounded JSON framing for local resticpal IPC.

use std::io::{Read, Write};
use std::path::PathBuf;

use resticpal_core::status::ServiceStatus;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestCommand {
    GetStatus,
    GetBackupSources,
    DiscoverBackupSources,
    UpdateBackupSources {
        paths: Option<Vec<PathBuf>>,
        exclusions: Option<Vec<String>>,
    },
    RunBackupNow,
    CancelBackup,
    DeferBackup {
        minutes: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsePayload {
    Status {
        status: ServiceStatus,
    },
    BackupSources {
        configuration: BackupSourcesView,
    },
    DiscoveredBackupSources {
        sources: Vec<DiscoveredBackupSource>,
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
pub struct BackupSourcesView {
    pub paths: Vec<PathBuf>,
    pub exclusions: Vec<String>,
    pub paths_locked: bool,
    pub exclusions_locked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

pub fn write_frame<T: Serialize>(mut writer: impl Write, value: &T) -> Result<(), FrameError> {
    let payload = serde_json::to_vec(value)?;
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

    let mut payload = vec![0_u8; length];
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
}
