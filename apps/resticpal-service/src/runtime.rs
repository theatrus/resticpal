use std::fs;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, MutexGuard};

use chrono::{Duration, Utc};
use resticpal_core::config::{EffectiveConfig, LocalConfig, LocalConfigError};
use resticpal_core::policy::{PolicyError, ResolvedConfig, resolve_config};
use resticpal_core::status::{BackupState, ServiceStatus};
use resticpal_protocol::{Request, RequestCommand, Response, ResponsePayload};
use resticpal_windows::named_pipe::ClientIdentity;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeEvent {
    Stop,
    Resume,
    PowerStatusChanged,
    TimeChanged,
    Cancel,
    Deferred,
}

pub struct ServiceRuntime {
    config: EffectiveConfig,
    status: Mutex<ServiceStatus>,
    events: Sender<RuntimeEvent>,
}

impl ServiceRuntime {
    pub fn load(path: &Path, events: Sender<RuntimeEvent>) -> Result<Self, RuntimeInitError> {
        let local = match fs::read_to_string(path) {
            Ok(contents) => LocalConfig::from_toml(&contents)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => LocalConfig::default(),
            Err(error) => return Err(error.into()),
        };
        let resolved = resolve_config(&EffectiveConfig::default(), &local, None)?;
        Ok(Self::from_resolved(resolved, events))
    }

    pub fn from_resolved(resolved: ResolvedConfig, events: Sender<RuntimeEvent>) -> Self {
        let now = Utc::now();
        let configured = resolved.effective.is_configured();
        let status = ServiceStatus {
            state: if configured {
                BackupState::Idle
            } else {
                BackupState::Unconfigured
            },
            state_since: now,
            last_attempt: None,
            last_success: None,
            next_deadline: configured.then_some(now),
            repository_display_name: resolved.effective.repository.display_name.clone(),
            repository_mode: resolved.effective.repository.mode,
            managed_revision: resolved.managed_revision,
        };

        Self {
            config: resolved.effective,
            status: Mutex::new(status),
            events,
        }
    }

    pub fn configuration_error(events: Sender<RuntimeEvent>) -> Self {
        let now = Utc::now();
        Self {
            config: EffectiveConfig::default(),
            status: Mutex::new(ServiceStatus {
                state: BackupState::Failed {
                    code: "configuration_invalid".to_owned(),
                },
                state_since: now,
                last_attempt: None,
                last_success: None,
                next_deadline: None,
                repository_display_name: None,
                repository_mode: Default::default(),
                managed_revision: None,
            }),
            events,
        }
    }

    pub fn status(&self) -> ServiceStatus {
        self.status_guard().clone()
    }

    pub fn handle_request(&self, request: Request, _identity: ClientIdentity) -> Response {
        let request_id = request.request_id;
        let payload = match request.command {
            RequestCommand::GetStatus => ResponsePayload::Status {
                status: self.status(),
            },
            RequestCommand::RunBackupNow => {
                if !self.config.is_configured() {
                    rejected(
                        "not_configured",
                        "Configure backup sources and a repository first.",
                    )
                } else if matches!(self.status_guard().state, BackupState::Running { .. }) {
                    rejected("already_running", "A backup is already running.")
                } else {
                    rejected(
                        "backup_engine_unavailable",
                        "Backup execution is not available in this development build yet.",
                    )
                }
            }
            RequestCommand::CancelBackup => {
                if matches!(self.status_guard().state, BackupState::Running { .. }) {
                    self.send_event(RuntimeEvent::Cancel, "Cancellation requested.")
                } else {
                    rejected("not_running", "There is no running backup to cancel.")
                }
            }
            RequestCommand::DeferBackup { minutes } => {
                if !(1..=24 * 60).contains(&minutes) {
                    rejected(
                        "invalid_deferral",
                        "A deferral must be between one minute and 24 hours.",
                    )
                } else {
                    self.status_guard().next_deadline =
                        Some(Utc::now() + Duration::minutes(i64::from(minutes)));
                    self.send_event(RuntimeEvent::Deferred, "Backup deferred.")
                }
            }
        };

        Response::new(request_id, payload)
    }

    fn send_event(&self, event: RuntimeEvent, message: &str) -> ResponsePayload {
        match self.events.send(event) {
            Ok(()) => ResponsePayload::Accepted {
                message: message.to_owned(),
            },
            Err(_) => rejected(
                "service_stopping",
                "The backup service is stopping. Try again shortly.",
            ),
        }
    }

    fn status_guard(&self) -> MutexGuard<'_, ServiceStatus> {
        self.status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn rejected(code: &str, message: &str) -> ResponsePayload {
    ResponsePayload::Rejected {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

#[derive(Debug, Error)]
pub enum RuntimeInitError {
    #[error("could not read local configuration: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    LocalConfig(#[from] LocalConfigError),
    #[error(transparent)]
    Policy(#[from] PolicyError),
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration as StdDuration;

    use resticpal_core::config::{BackupConfig, RepositoryConfig};
    use resticpal_protocol::PROTOCOL_VERSION;
    use resticpal_windows::named_pipe::{NamedPipeClient, NamedPipeServer};

    use super::*;

    const USER: ClientIdentity = ClientIdentity {
        is_elevated_administrator: false,
    };
    static NEXT_PIPE: AtomicU64 = AtomicU64::new(1);

    fn runtime(configured: bool) -> (ServiceRuntime, mpsc::Receiver<RuntimeEvent>) {
        let (events, receiver) = mpsc::channel();
        let mut effective = EffectiveConfig::default();
        if configured {
            effective.backup = BackupConfig {
                paths: vec![PathBuf::from(r"C:\Users\Example\Documents")],
                exclusions: Vec::new(),
            };
            effective.repository = RepositoryConfig {
                url: Some("local:C:/backup".to_owned()),
                ..RepositoryConfig::default()
            };
        }
        let resolved = ResolvedConfig {
            effective,
            managed_revision: None,
            fields: Default::default(),
        };
        (ServiceRuntime::from_resolved(resolved, events), receiver)
    }

    #[test]
    fn status_reports_an_unconfigured_service() {
        let (runtime, _events) = runtime(false);
        let response = runtime.handle_request(Request::new(1, RequestCommand::GetStatus), USER);

        assert!(matches!(
            response.payload,
            ResponsePayload::Status {
                status: ServiceStatus {
                    state: BackupState::Unconfigured,
                    ..
                }
            }
        ));
    }

    #[test]
    fn run_now_is_rejected_until_the_executor_is_available() {
        let (runtime, _events) = runtime(true);
        let response = runtime.handle_request(Request::new(2, RequestCommand::RunBackupNow), USER);

        assert!(matches!(
            response.payload,
            ResponsePayload::Rejected { ref code, .. } if code == "backup_engine_unavailable"
        ));
    }

    #[test]
    fn run_now_is_rejected_until_configuration_is_complete() {
        let (runtime, _events) = runtime(false);
        let response = runtime.handle_request(Request::new(3, RequestCommand::RunBackupNow), USER);

        assert!(matches!(
            response.payload,
            ResponsePayload::Rejected { ref code, .. } if code == "not_configured"
        ));
    }

    #[test]
    fn deferral_updates_the_reported_deadline() {
        let (runtime, events) = runtime(true);
        let before = Utc::now();
        let response = runtime.handle_request(
            Request::new(4, RequestCommand::DeferBackup { minutes: 30 }),
            USER,
        );

        assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
        assert_eq!(
            events.recv().expect("runtime event should be sent"),
            RuntimeEvent::Deferred
        );
        let deadline = runtime
            .status()
            .next_deadline
            .expect("deferral sets a deadline");
        assert!(deadline >= before + Duration::minutes(30));
    }

    #[test]
    fn request_constructor_uses_current_protocol() {
        assert_eq!(
            Request::new(5, RequestCommand::GetStatus).protocol_version,
            PROTOCOL_VERSION
        );
    }

    #[test]
    fn named_pipe_exposes_the_runtime_status() {
        let (runtime, _events) = runtime(true);
        let runtime = Arc::new(runtime);
        let server_runtime = Arc::clone(&runtime);
        let pipe_name = format!(
            r"\\.\pipe\ResticPal.RuntimeTest.{}.{}",
            std::process::id(),
            NEXT_PIPE.fetch_add(1, Ordering::Relaxed)
        );
        let server_name = pipe_name.clone();
        let server = thread::spawn(move || {
            NamedPipeServer::new(&server_name)
                .expect("server should initialize")
                .serve_one(|request, identity| server_runtime.handle_request(request, identity))
                .expect("service runtime should handle the request");
        });

        let response = NamedPipeClient::request_at(
            &pipe_name,
            &Request::new(6, RequestCommand::GetStatus),
            StdDuration::from_secs(5),
        )
        .expect("client should receive service status");
        server.join().expect("server should stop after one request");

        assert!(matches!(
            response.payload,
            ResponsePayload::Status {
                status: ServiceStatus {
                    state: BackupState::Idle,
                    ..
                }
            }
        ));
    }
}
