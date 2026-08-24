//! Runtime behavior tests, spanning all runtime submodules.

use std::collections::BTreeMap;

use chrono::{Duration, Utc};
use resticpal_core::config::{EffectiveConfig, LocalConfig, ManagementMode, RepositoryMode};
use resticpal_core::policy::{FieldResolution, ManagedPolicy, PolicyField, ResolvedConfig};
use resticpal_core::schedule::BackupTrigger;
use resticpal_core::status::{
    BackupPhase, BackupProgress, BackupRunOutcome, BackupState, ServiceStatus, WaitingReason,
};
use resticpal_protocol::{
    DiagnosticLevel, ManagementView, RepositoryOperationKind, RepositoryOperationStatus,
    RepositorySecretUpdate, RepositoryView, Request, RequestCommand, ResponsePayload,
    RetentionView, ScheduleView, UpdatePackage, UpdateSettingsView,
};
use resticpal_windows::credentials::DpapiSecretStore;
use resticpal_windows::named_pipe::ClientIdentity;

use crate::conditions::SystemConditions;
use crate::config_store::LocalConfigStore;
use crate::executor::{
    BackupFailureDetails, BackupOutcome, BackupOutcomeKind, RepositoryOutcome,
    RepositoryOutcomeKind, RetentionOutcome, RetentionOutcomeKind,
};
use crate::management::PendingEnrollment;
use crate::state::{ScheduleStateStore, ServiceStateSnapshot};
use crate::updater::UpdateInstallOutcome;

use super::events::RetentionPlan;
use super::scheduler::repository_requires_network;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer, SigningKey};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration as StdDuration;

use resticpal_core::config::{
    BackupConfig, LocalBackupConfig, LocalManagementConfig, LocalRepositoryConfig,
    LocalUpdateConfig, RepositoryConfig, SecretEnvironmentVariable,
};
use resticpal_core::management::{
    EnrollmentMaterial, MANAGEMENT_SCHEMA_VERSION, ManifestPayload, SignedManifestEnvelope,
    VerifiedManifest,
};
use resticpal_core::policy::{Managed, ManagedSchedulePolicy, ManagedUpdatePolicy};
use resticpal_protocol::{PROTOCOL_VERSION, SecretValue};
use resticpal_windows::credentials::CredentialStoreError;
use resticpal_windows::named_pipe::{NamedPipeClient, NamedPipeServer};

use super::*;
use crate::executor::BackupSummary;
use zeroize::Zeroizing;

const USER: ClientIdentity = ClientIdentity {
    is_elevated_administrator: false,
};
const ADMIN: ClientIdentity = ClientIdentity {
    is_elevated_administrator: true,
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

fn available_conditions() -> SystemConditions {
    SystemConditions {
        network_available: true,
        on_battery: false,
        metered_network: false,
    }
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
fn management_state_and_enrollment_require_an_elevated_administrator() {
    let (runtime, _events) = runtime(false);
    assert!(matches!(
        runtime
            .handle_request(Request::new(60, RequestCommand::GetManagement), USER)
            .payload,
        ResponsePayload::Rejected { ref code, .. } if code == "administrator_required"
    ));
    assert!(matches!(
        runtime
            .handle_request(Request::new(61, RequestCommand::GetManagement), ADMIN)
            .payload,
        ResponsePayload::Management {
            configuration: ManagementView {
                mode: ManagementMode::Disabled,
                enrolled: false,
                ..
            }
        }
    ));
    assert!(matches!(
        runtime
            .handle_request(
                Request::new(
                    62,
                    RequestCommand::Enroll {
                        bootstrap_url: SecretValue::new("https://example.invalid/#token=secret"),
                    },
                ),
                USER,
            )
            .payload,
        ResponsePayload::Rejected { ref code, .. } if code == "administrator_required"
    ));
}

#[test]
fn run_now_queues_a_scheduler_evaluation_that_starts_the_executor() {
    let (runtime, events) = runtime(true);
    let response = runtime.handle_request(Request::new(2, RequestCommand::RunBackupNow), USER);

    assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
    assert_eq!(events.recv().expect("runtime event"), RuntimeEvent::RunNow);
    assert_eq!(
        runtime.evaluate_schedule(Utc::now(), available_conditions()),
        ScheduleAction::Start {
            trigger: BackupTrigger::Manual
        }
    );
    assert!(matches!(
        runtime.status().state,
        BackupState::Running {
            phase: BackupPhase::PreparingSnapshot
        }
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
fn deferral_requires_a_configured_and_verified_repository() {
    let (unconfigured, _events) = runtime(false);
    assert!(matches!(
        unconfigured
            .handle_request(
                Request::new(30, RequestCommand::DeferBackup { minutes: 30 }),
                USER,
            )
            .payload,
        ResponsePayload::Rejected { ref code, .. } if code == "not_configured"
    ));

    let directory = tempfile::tempdir().expect("temporary directory");
    let config_path = directory.path().join("config.toml");
    LocalConfigStore::new(&config_path)
        .save(&LocalConfig {
            backup: LocalBackupConfig {
                paths: Some(vec![PathBuf::from(r"C:\Data")]),
                exclusions: Some(Vec::new()),
            },
            repository: LocalRepositoryConfig {
                url: Some("local:C:/backup".to_owned()),
                ..LocalRepositoryConfig::default()
            },
            ..LocalConfig::default()
        })
        .expect("configured local file");
    let (events, _receiver) = mpsc::channel();
    let unverified = ServiceRuntime::load(&config_path, events).expect("runtime");

    assert!(matches!(
        unverified
            .handle_request(
                Request::new(31, RequestCommand::DeferBackup { minutes: 30 }),
                USER,
            )
            .payload,
        ResponsePayload::Rejected { ref code, .. } if code == "repository_not_ready"
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
fn update_preparation_is_admin_only_bounded_and_blocks_new_backup_work() {
    let (runtime, _events) = runtime(true);

    let user_response = runtime.handle_request(
        Request::new(46, RequestCommand::PrepareForUpdate { hold_seconds: 900 }),
        USER,
    );
    assert!(matches!(
        user_response.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "administrator_required"
    ));

    let invalid_response = runtime.handle_request(
        Request::new(47, RequestCommand::PrepareForUpdate { hold_seconds: 59 }),
        ADMIN,
    );
    assert!(matches!(
        invalid_response.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "invalid_update_hold"
    ));

    let accepted = runtime.handle_request(
        Request::new(48, RequestCommand::PrepareForUpdate { hold_seconds: 60 }),
        ADMIN,
    );
    assert!(matches!(accepted.payload, ResponsePayload::Accepted { .. }));
    assert!(matches!(
        runtime.status().state,
        BackupState::Waiting {
            reason: WaitingReason::Update
        }
    ));

    let enable_automatic = runtime.handle_request(
        Request::new(
            481,
            RequestCommand::UpdateUpdateSettings {
                automatic_install: true,
            },
        ),
        ADMIN,
    );
    assert!(matches!(
        enable_automatic.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "update_pending"
    ));
    let automatic_during_prompted_hold = runtime.handle_request(
        Request::new(
            482,
            RequestCommand::InstallUpdate {
                package: UpdatePackage {
                    version: "99.0.0".to_owned(),
                    url: "https://github.com/theatrus/resticpal/releases/download/v99.0.0/resticpal-99.0.0-x64.msi".to_owned(),
                    signature: base64::engine::general_purpose::STANDARD.encode([0_u8; 64]),
                    length: 83_329_024,
                },
            },
        ),
        USER,
    );
    assert!(matches!(
        automatic_during_prompted_hold.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "automatic_updates_disabled"
    ));

    let run_now = runtime.handle_request(Request::new(49, RequestCommand::RunBackupNow), USER);
    assert!(matches!(
        run_now.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "update_pending"
    ));
    assert_eq!(
        runtime.evaluate_schedule(Utc::now(), available_conditions()),
        ScheduleAction::None
    );

    let deadline = runtime.status().next_deadline.expect("update deadline");
    assert!(matches!(
        runtime.evaluate_schedule(deadline + Duration::seconds(1), available_conditions()),
        ScheduleAction::Start { .. }
    ));
}

#[test]
fn update_preparation_rejects_an_active_backup() {
    let (runtime, _events) = runtime(true);
    runtime.state_guard().status.state = BackupState::Running {
        phase: BackupPhase::Uploading,
    };

    let response = runtime.handle_request(
        Request::new(50, RequestCommand::PrepareForUpdate { hold_seconds: 900 }),
        ADMIN,
    );

    assert!(matches!(
        response.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "backup_running"
    ));
}

#[test]
fn expired_update_preparation_restores_an_unconfigured_status() {
    let (runtime, _events) = runtime(false);
    let first_response = runtime.handle_request(
        Request::new(51, RequestCommand::PrepareForUpdate { hold_seconds: 60 }),
        ADMIN,
    );
    assert!(matches!(
        first_response.payload,
        ResponsePayload::Accepted { .. }
    ));

    // A second request can arrive after the deadline but before the scheduler
    // has restored the original state. Extending that stale hold must retain
    // the original Unconfigured snapshot rather than capturing Waiting(Update).
    runtime.state_guard().update_hold_until = Some(Utc::now() - Duration::seconds(1));
    let second_response = runtime.handle_request(
        Request::new(511, RequestCommand::PrepareForUpdate { hold_seconds: 60 }),
        ADMIN,
    );
    assert!(matches!(
        second_response.payload,
        ResponsePayload::Accepted { .. }
    ));

    let deadline = runtime.status().next_deadline.expect("update deadline");
    assert_eq!(
        runtime.evaluate_schedule(deadline + Duration::seconds(1), available_conditions()),
        ScheduleAction::None
    );
    let status = runtime.status();
    assert!(matches!(status.state, BackupState::Unconfigured));
    assert_eq!(status.next_deadline, None);
}

#[test]
fn automatic_update_setting_is_admin_controlled_and_authorizes_the_tray() {
    let (runtime, events) = runtime(false);
    let version = "99.0.0";
    let package = UpdatePackage {
        version: version.to_owned(),
        url: format!(
            "https://github.com/theatrus/resticpal/releases/download/v{version}/resticpal-{version}-x64.msi"
        ),
        signature: base64::engine::general_purpose::STANDARD.encode([0_u8; 64]),
        length: 83_329_024,
    };

    let disabled = runtime.handle_request(
        Request::new(
            52,
            RequestCommand::InstallUpdate {
                package: package.clone(),
            },
        ),
        USER,
    );
    assert!(matches!(
        disabled.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "automatic_updates_disabled"
    ));
    let disabled_for_admin = runtime.handle_request(
        Request::new(
            521,
            RequestCommand::InstallUpdate {
                package: package.clone(),
            },
        ),
        ADMIN,
    );
    assert!(matches!(
        disabled_for_admin.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "automatic_updates_disabled"
    ));

    let user_change = runtime.handle_request(
        Request::new(
            53,
            RequestCommand::UpdateUpdateSettings {
                automatic_install: true,
            },
        ),
        USER,
    );
    assert!(matches!(
        user_change.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "administrator_required"
    ));

    let enabled = runtime.handle_request(
        Request::new(
            54,
            RequestCommand::UpdateUpdateSettings {
                automatic_install: true,
            },
        ),
        ADMIN,
    );
    assert!(matches!(enabled.payload, ResponsePayload::Accepted { .. }));
    assert!(matches!(
        runtime
            .handle_request(Request::new(55, RequestCommand::GetUpdateSettings), USER)
            .payload,
        ResponsePayload::UpdateSettings {
            configuration: UpdateSettingsView {
                automatic_install: true,
                automatic_install_locked: false,
            }
        }
    ));

    let prompted_prepare = runtime.handle_request(
        Request::new(551, RequestCommand::PrepareForUpdate { hold_seconds: 900 }),
        ADMIN,
    );
    assert!(matches!(
        prompted_prepare.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "automatic_updates_enabled"
    ));

    let install = runtime.handle_request(
        Request::new(
            56,
            RequestCommand::InstallUpdate {
                package: package.clone(),
            },
        ),
        USER,
    );
    assert!(matches!(install.payload, ResponsePayload::Accepted { .. }));
    assert_eq!(
        events.recv().expect("update event"),
        RuntimeEvent::UpdateInstallRequested(package)
    );
    assert!(matches!(
        runtime.status().state,
        BackupState::Waiting {
            reason: WaitingReason::Update
        }
    ));

    let disabled_while_installing = runtime.handle_request(
        Request::new(
            561,
            RequestCommand::UpdateUpdateSettings {
                automatic_install: false,
            },
        ),
        ADMIN,
    );
    assert!(matches!(
        disabled_while_installing.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "update_pending"
    ));
    let prompted_during_automatic_install = runtime.handle_request(
        Request::new(562, RequestCommand::PrepareForUpdate { hold_seconds: 900 }),
        ADMIN,
    );
    assert!(matches!(
        prompted_during_automatic_install.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "automatic_updates_enabled"
    ));

    runtime.finish_update_install(&UpdateInstallOutcome::Failed {
        code: "test_update_failed",
    });
    assert!(matches!(runtime.status().state, BackupState::Unconfigured));
}

#[test]
fn indeterminate_installer_keeps_backups_and_new_updates_paused() {
    let (runtime, events) = runtime(true);
    assert!(matches!(
        runtime
            .handle_request(
                Request::new(
                    563,
                    RequestCommand::UpdateUpdateSettings {
                        automatic_install: true,
                    },
                ),
                ADMIN,
            )
            .payload,
        ResponsePayload::Accepted { .. }
    ));
    let package = UpdatePackage {
        version: "99.0.0".to_owned(),
        url: "https://github.com/theatrus/resticpal/releases/download/v99.0.0/resticpal-99.0.0-x64.msi".to_owned(),
        signature: base64::engine::general_purpose::STANDARD.encode([0_u8; 64]),
        length: 83_329_024,
    };
    assert!(matches!(
        runtime
            .handle_request(
                Request::new(
                    564,
                    RequestCommand::InstallUpdate {
                        package: package.clone(),
                    },
                ),
                USER,
            )
            .payload,
        ResponsePayload::Accepted { .. }
    ));
    assert_eq!(
        events.recv().expect("update event"),
        RuntimeEvent::UpdateInstallRequested(package.clone())
    );

    runtime.finish_update_install(&UpdateInstallOutcome::Indeterminate {
        code: "update_installer_indeterminate",
    });
    runtime.state_guard().update_hold_until = Some(Utc::now() - Duration::seconds(1));
    runtime.config_write().repository.secret_refs.insert(
        SecretEnvironmentVariable::ResticPassword,
        "resticpal/repository/restic_password".to_owned(),
    );

    assert!(runtime.state_guard().update_install_active);
    assert_eq!(
        runtime.evaluate_schedule(Utc::now(), available_conditions()),
        ScheduleAction::None
    );
    assert!(matches!(
        runtime.status().state,
        BackupState::Waiting {
            reason: WaitingReason::Update
        }
    ));
    assert!(matches!(
        runtime
            .handle_request(Request::new(565, RequestCommand::RunBackupNow), USER)
            .payload,
        ResponsePayload::Rejected { ref code, .. } if code == "update_pending"
    ));
    assert!(matches!(
        runtime
            .handle_request(Request::new(566, RequestCommand::ValidateRepository), ADMIN)
            .payload,
        ResponsePayload::Rejected { ref code, .. } if code == "update_pending"
    ));
    assert!(matches!(
        runtime
            .handle_request(
                Request::new(
                    567,
                    RequestCommand::Enroll {
                        bootstrap_url: SecretValue::new(
                            "https://example.invalid/#token=not-consumed",
                        ),
                    },
                ),
                ADMIN,
            )
            .payload,
        ResponsePayload::Rejected { ref code, .. } if code == "operation_running"
    ));
    assert!(matches!(
        runtime
            .handle_request(
                Request::new(568, RequestCommand::InstallUpdate { package }),
                USER,
            )
            .payload,
        ResponsePayload::Rejected { ref code, .. } if code == "update_already_running"
    ));
}

#[test]
fn automatic_update_setting_persists_an_explicit_local_override() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config_path = directory.path().join("config.toml");
    LocalConfigStore::new(&config_path)
        .save(&LocalConfig::default())
        .expect("initial config");
    let (events, _receiver) = mpsc::channel();
    let runtime = ServiceRuntime::load(&config_path, events).expect("runtime");

    let response = runtime.handle_request(
        Request::new(
            57,
            RequestCommand::UpdateUpdateSettings {
                automatic_install: true,
            },
        ),
        ADMIN,
    );
    assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
    assert!(runtime.config().updates.automatic_install);

    let persisted = LocalConfigStore::new(&config_path)
        .load()
        .expect("persisted config");
    assert_eq!(persisted.updates.automatic_install, Some(true));

    drop(runtime);
    let (events, _receiver) = mpsc::channel();
    let restarted = ServiceRuntime::load(&config_path, events).expect("restarted runtime");
    assert!(matches!(
        restarted
            .handle_request(Request::new(58, RequestCommand::GetUpdateSettings), USER)
            .payload,
        ResponsePayload::UpdateSettings {
            configuration: UpdateSettingsView {
                automatic_install: true,
                automatic_install_locked: false,
            }
        }
    ));
}

#[test]
fn locked_managed_update_policy_is_reported_enforced_and_authorizes_the_tray() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config_path = directory.path().join("config.toml");
    LocalConfigStore::new(&config_path)
        .save(&LocalConfig::default())
        .expect("initial config");
    let policy = ManagedPolicy {
        revision: "managed-automatic-updates".to_owned(),
        updates: Some(ManagedUpdatePolicy {
            automatic_install: Some(Managed {
                value: true,
                locked: true,
            }),
        }),
        ..ManagedPolicy::default()
    };
    let (events, receiver) = mpsc::channel();
    let runtime =
        ServiceRuntime::load_with_credentials_and_policy(&config_path, events, None, Some(&policy))
            .expect("managed runtime");

    assert!(matches!(
        runtime
            .handle_request(Request::new(59, RequestCommand::GetUpdateSettings), USER)
            .payload,
        ResponsePayload::UpdateSettings {
            configuration: UpdateSettingsView {
                automatic_install: true,
                automatic_install_locked: true,
            }
        }
    ));
    let change = runtime.handle_request(
        Request::new(
            60,
            RequestCommand::UpdateUpdateSettings {
                automatic_install: false,
            },
        ),
        ADMIN,
    );
    assert!(matches!(
        change.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "managed_field_locked"
    ));
    assert_eq!(
        LocalConfigStore::new(&config_path)
            .load()
            .expect("unchanged local config")
            .updates
            .automatic_install,
        None
    );

    let version = "99.0.0";
    let package = UpdatePackage {
        version: version.to_owned(),
        url: format!(
            "https://github.com/theatrus/resticpal/releases/download/v{version}/resticpal-{version}-x64.msi"
        ),
        signature: base64::engine::general_purpose::STANDARD.encode([0_u8; 64]),
        length: 83_329_024,
    };
    let install = runtime.handle_request(
        Request::new(
            61,
            RequestCommand::InstallUpdate {
                package: package.clone(),
            },
        ),
        USER,
    );
    assert!(matches!(install.payload, ResponsePayload::Accepted { .. }));
    assert_eq!(
        receiver.recv().expect("update event"),
        RuntimeEvent::UpdateInstallRequested(package)
    );
}

#[test]
fn locked_managed_update_disablement_overrides_local_opt_in_for_every_caller() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config_path = directory.path().join("config.toml");
    LocalConfigStore::new(&config_path)
        .save(&LocalConfig {
            updates: LocalUpdateConfig {
                automatic_install: Some(true),
            },
            ..LocalConfig::default()
        })
        .expect("initial config");
    let policy = ManagedPolicy {
        revision: "managed-disable-automatic-updates".to_owned(),
        updates: Some(ManagedUpdatePolicy {
            automatic_install: Some(Managed {
                value: false,
                locked: true,
            }),
        }),
        ..ManagedPolicy::default()
    };
    let (events, _receiver) = mpsc::channel();
    let runtime =
        ServiceRuntime::load_with_credentials_and_policy(&config_path, events, None, Some(&policy))
            .expect("managed runtime");
    assert!(matches!(
        runtime
            .handle_request(Request::new(62, RequestCommand::GetUpdateSettings), USER)
            .payload,
        ResponsePayload::UpdateSettings {
            configuration: UpdateSettingsView {
                automatic_install: false,
                automatic_install_locked: true,
            }
        }
    ));

    let version = "99.0.0";
    let package = UpdatePackage {
        version: version.to_owned(),
        url: format!(
            "https://github.com/theatrus/resticpal/releases/download/v{version}/resticpal-{version}-x64.msi"
        ),
        signature: base64::engine::general_purpose::STANDARD.encode([0_u8; 64]),
        length: 83_329_024,
    };
    for identity in [USER, ADMIN] {
        let response = runtime.handle_request(
            Request::new(
                63,
                RequestCommand::InstallUpdate {
                    package: package.clone(),
                },
            ),
            identity,
        );
        assert!(matches!(
            response.payload,
            ResponsePayload::Rejected { ref code, .. } if code == "automatic_updates_disabled"
        ));
    }
    assert_eq!(
        LocalConfigStore::new(&config_path)
            .load()
            .expect("local override remains persisted")
            .updates
            .automatic_install,
        Some(true)
    );
}

#[test]
fn unrelated_configuration_saves_preserve_local_and_managed_update_state() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config_path = directory.path().join("config.toml");
    LocalConfigStore::new(&config_path)
        .save(&LocalConfig {
            updates: LocalUpdateConfig {
                automatic_install: Some(true),
            },
            ..LocalConfig::default()
        })
        .expect("initial config");
    let policy = ManagedPolicy {
        revision: "managed-update-state-preservation".to_owned(),
        updates: Some(ManagedUpdatePolicy {
            automatic_install: Some(Managed {
                value: false,
                locked: true,
            }),
        }),
        ..ManagedPolicy::default()
    };
    let (events, _receiver) = mpsc::channel();
    let runtime =
        ServiceRuntime::load_with_credentials_and_policy(&config_path, events, None, Some(&policy))
            .expect("managed runtime");

    let unrelated_updates = [
        RequestCommand::UpdateBackupSources {
            paths: Some(vec![PathBuf::from(r"C:\Data")]),
            exclusions: None,
        },
        RequestCommand::UpdateSchedule {
            interval_hours: Some(12),
            wake_grace_seconds: None,
            wake_lock_timeout_seconds: None,
            allow_on_battery: None,
            allow_metered_network: None,
        },
        RequestCommand::UpdateRetention {
            daily: Some(14),
            weekly: None,
            monthly: None,
            yearly: None,
            prune_interval_days: None,
        },
        RequestCommand::UpdateRepository {
            display_name: Some("Preservation test".to_owned()),
            url: None,
            mode: None,
            options: None,
            secret_updates: Vec::new(),
        },
    ];

    for (index, command) in unrelated_updates.into_iter().enumerate() {
        let response = runtime.handle_request(Request::new(70 + index as u64, command), ADMIN);
        assert!(
            matches!(response.payload, ResponsePayload::Accepted { .. }),
            "unrelated configuration save {index} was rejected: {:?}",
            response.payload
        );
        assert!(!runtime.config().updates.automatic_install);
        assert_eq!(
            runtime.local_config_guard().updates.automatic_install,
            Some(true)
        );
        assert!(runtime.field_locked(PolicyField::UpdateAutomaticInstall));
    }

    let persisted = LocalConfigStore::new(&config_path)
        .load()
        .expect("persisted config");
    assert_eq!(persisted.updates.automatic_install, Some(true));
}

#[test]
fn configuration_mutations_wait_for_an_active_update() {
    let (runtime, events) = runtime(true);
    assert!(matches!(
        runtime
            .handle_request(
                Request::new(
                    80,
                    RequestCommand::UpdateUpdateSettings {
                        automatic_install: true,
                    },
                ),
                ADMIN,
            )
            .payload,
        ResponsePayload::Accepted { .. }
    ));
    let package = UpdatePackage {
        version: "99.0.0".to_owned(),
        url: "https://github.com/theatrus/resticpal/releases/download/v99.0.0/resticpal-99.0.0-x64.msi".to_owned(),
        signature: base64::engine::general_purpose::STANDARD.encode([0_u8; 64]),
        length: 83_329_024,
    };
    assert!(matches!(
        runtime
            .handle_request(
                Request::new(
                    81,
                    RequestCommand::InstallUpdate {
                        package: package.clone(),
                    },
                ),
                USER,
            )
            .payload,
        ResponsePayload::Accepted { .. }
    ));
    assert_eq!(
        events.recv().expect("update event"),
        RuntimeEvent::UpdateInstallRequested(package)
    );

    let mutations = [
        RequestCommand::UpdateBackupSources {
            paths: Some(vec![PathBuf::from(r"C:\Blocked")]),
            exclusions: None,
        },
        RequestCommand::UpdateSchedule {
            interval_hours: Some(7),
            wake_grace_seconds: None,
            wake_lock_timeout_seconds: None,
            allow_on_battery: None,
            allow_metered_network: None,
        },
        RequestCommand::UpdateRetention {
            daily: Some(13),
            weekly: None,
            monthly: None,
            yearly: None,
            prune_interval_days: None,
        },
        RequestCommand::UpdateRepository {
            display_name: Some("Blocked during update".to_owned()),
            url: None,
            mode: None,
            options: None,
            secret_updates: Vec::new(),
        },
    ];
    for (index, command) in mutations.into_iter().enumerate() {
        let response = runtime.handle_request(Request::new(82 + index as u64, command), ADMIN);
        assert!(
            matches!(
                response.payload,
                ResponsePayload::Rejected { ref code, .. } if code == "update_pending"
            ),
            "configuration mutation {index} was not held: {:?}",
            response.payload
        );
    }
    assert_ne!(
        runtime.config().backup.paths,
        vec![PathBuf::from(r"C:\Blocked")]
    );
    assert_ne!(runtime.config().schedule.interval_hours, 7);
    assert_ne!(runtime.config().retention.daily, 13);
    assert_ne!(
        runtime.config().repository.display_name.as_deref(),
        Some("Blocked during update")
    );
}

#[test]
fn unrelated_configuration_save_waits_for_the_global_mutation_lock() {
    let (runtime, _events) = runtime(true);
    let runtime = Arc::new(runtime);
    let mutation = runtime
        .configuration_mutation
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let worker_runtime = Arc::clone(&runtime);
    let (started_tx, started_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        started_tx.send(()).expect("start signal");
        let response = worker_runtime.handle_request(
            Request::new(
                74,
                RequestCommand::UpdateBackupSources {
                    paths: Some(vec![PathBuf::from(r"C:\Serialized")]),
                    exclusions: None,
                },
            ),
            ADMIN,
        );
        finished_tx.send(response).expect("completion signal");
    });

    started_rx.recv().expect("worker started");
    assert!(
        finished_rx
            .recv_timeout(StdDuration::from_millis(100))
            .is_err(),
        "the unrelated writer bypassed the configuration mutation lock"
    );
    drop(mutation);

    let response = finished_rx
        .recv_timeout(StdDuration::from_secs(2))
        .expect("writer completed after mutation lock was released");
    assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
    worker.join().expect("configuration writer");
}

#[test]
fn progress_and_success_update_the_canonical_status() {
    let (runtime, events) = runtime(true);
    let response = runtime.handle_request(Request::new(7, RequestCommand::RunBackupNow), USER);
    assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
    assert_eq!(events.recv().expect("run event"), RuntimeEvent::RunNow);
    assert!(matches!(
        runtime.evaluate_schedule(Utc::now(), available_conditions()),
        ScheduleAction::Start { .. }
    ));

    runtime.update_progress(BackupProgress {
        percent_done: Some(50),
        files_done: 5,
        total_files: Some(10),
        bytes_done: 500,
        total_bytes: Some(1_000),
        error_count: 0,
    });
    assert!(matches!(
        runtime.status(),
        ServiceStatus {
            state: BackupState::Running {
                phase: BackupPhase::Uploading
            },
            progress: Some(BackupProgress {
                percent_done: Some(50),
                ..
            }),
            ..
        }
    ));

    let before = Utc::now();
    let interval_hours = runtime.config().schedule.interval_hours;
    runtime.finish_backup(&BackupOutcome::succeeded(BackupSummary {
        files_processed: 10,
        bytes_processed: 1_000,
        data_added: 200,
        snapshot_id: Some("snapshot".to_owned()),
    }));
    let status = runtime.status();
    assert_eq!(status.state, BackupState::Succeeded);
    assert!(status.last_success.is_some_and(|value| value >= before));
    assert!(
        status
            .next_deadline
            .is_some_and(|value| { value >= before + Duration::hours(i64::from(interval_hours)) })
    );
    assert_eq!(status.progress, None);
}

#[test]
fn cancellation_request_is_forwarded_only_while_running() {
    let (runtime, events) = runtime(true);
    let idle_cancel = runtime.handle_request(Request::new(8, RequestCommand::CancelBackup), USER);
    assert!(matches!(
        idle_cancel.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "not_running"
    ));

    let _ = runtime.handle_request(Request::new(9, RequestCommand::RunBackupNow), USER);
    assert_eq!(events.recv().expect("run event"), RuntimeEvent::RunNow);
    assert!(matches!(
        runtime.evaluate_schedule(Utc::now(), available_conditions()),
        ScheduleAction::Start { .. }
    ));
    let running_cancel =
        runtime.handle_request(Request::new(10, RequestCommand::CancelBackup), USER);
    assert!(matches!(
        running_cancel.payload,
        ResponsePayload::Accepted { .. }
    ));
    assert_eq!(events.recv().expect("cancel event"), RuntimeEvent::Cancel);
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

#[test]
fn overdue_startup_runs_without_a_manual_request() {
    let (runtime, _events) = runtime(true);

    assert_eq!(
        runtime.evaluate_schedule(Utc::now(), available_conditions()),
        ScheduleAction::Start {
            trigger: BackupTrigger::Scheduled
        }
    );
}

#[test]
fn resume_catch_up_waits_for_the_configured_grace_period() {
    let (runtime, _events) = runtime(true);
    let resumed_at = Utc::now();
    runtime.record_resume(resumed_at);

    assert_eq!(
        runtime.evaluate_schedule(resumed_at, available_conditions()),
        ScheduleAction::None
    );
    assert!(matches!(
        runtime.status().state,
        BackupState::Waiting {
            reason: resticpal_core::status::WaitingReason::WakeGrace
        }
    ));
    assert_eq!(
        runtime.evaluate_schedule(resumed_at + Duration::seconds(300), available_conditions()),
        ScheduleAction::Start {
            trigger: BackupTrigger::ResumeCatchUp
        }
    );
}

#[test]
fn battery_policy_blocks_until_power_conditions_change() {
    let (runtime, _events) = runtime(true);
    runtime.config_write().schedule.allow_on_battery = false;
    let now = Utc::now();

    assert_eq!(
        runtime.evaluate_schedule(
            now,
            SystemConditions {
                on_battery: true,
                ..available_conditions()
            }
        ),
        ScheduleAction::None
    );
    assert!(matches!(
        runtime.status().state,
        BackupState::Waiting {
            reason: resticpal_core::status::WaitingReason::Battery
        }
    ));
    assert!(matches!(
        runtime.evaluate_schedule(now, available_conditions()),
        ScheduleAction::Start { .. }
    ));
}

#[test]
fn manual_backup_runs_on_battery_when_unattended_backups_are_disallowed() {
    let (runtime, events) = runtime(true);
    runtime.config_write().schedule.allow_on_battery = false;
    let now = Utc::now();
    let on_battery = SystemConditions {
        on_battery: true,
        ..available_conditions()
    };

    // The same power state blocks the overdue unattended run.
    assert_eq!(
        runtime.evaluate_schedule(now, on_battery),
        ScheduleAction::None
    );
    assert!(matches!(
        runtime.status().state,
        BackupState::Waiting {
            reason: resticpal_core::status::WaitingReason::Battery
        }
    ));

    let response = runtime.handle_request(Request::new(63, RequestCommand::RunBackupNow), USER);
    assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
    assert_eq!(events.recv().expect("runtime event"), RuntimeEvent::RunNow);
    assert_eq!(
        runtime.evaluate_schedule(now, on_battery),
        ScheduleAction::Start {
            trigger: BackupTrigger::Manual
        }
    );
}

#[test]
fn failed_backups_use_bounded_exponential_retry_delays() {
    let (runtime, _events) = runtime(true);
    let now = Utc::now();
    assert!(matches!(
        runtime.evaluate_schedule(now, available_conditions()),
        ScheduleAction::Start { .. }
    ));
    runtime.finish_backup_at(now, &BackupOutcome::failed("repository_unreachable"));

    assert_eq!(
        runtime.status().next_deadline,
        Some(now + Duration::minutes(5))
    );
    assert_eq!(
        runtime.evaluate_schedule(now, available_conditions()),
        ScheduleAction::None
    );
    assert!(matches!(runtime.status().state, BackupState::Failed { .. }));

    assert!(matches!(
        runtime.evaluate_schedule(now + Duration::minutes(5), available_conditions()),
        ScheduleAction::Start { .. }
    ));
    runtime.finish_backup_at(
        now + Duration::minutes(5),
        &BackupOutcome::failed("repository_unreachable"),
    );
    assert_eq!(
        runtime.status().next_deadline,
        Some(now + Duration::minutes(15))
    );

    for _ in 0..10 {
        runtime.finish_backup_at(now, &BackupOutcome::failed("repository_unreachable"));
    }
    assert_eq!(
        runtime.status().next_deadline,
        Some(now + Duration::minutes(320))
    );
}

#[test]
fn repository_network_detection_distinguishes_local_and_remote_targets() {
    assert!(!repository_requires_network(r"C:\Backups\restic"));
    assert!(!repository_requires_network("local:C:/Backups/restic"));
    assert!(repository_requires_network(r"\\server\share\restic"));
    assert!(repository_requires_network("//server/share/restic"));
    assert!(repository_requires_network("s3:s3.example.test/bucket"));
    assert!(repository_requires_network(
        "sftp:user@example.test:/backup"
    ));
}

#[test]
fn backup_source_configuration_requires_an_elevated_administrator() {
    let (runtime, _events) = runtime(true);

    for command in [
        RequestCommand::GetBackupSources,
        RequestCommand::DiscoverBackupSources,
        RequestCommand::UpdateBackupSources {
            paths: Some(vec![PathBuf::from(r"D:\Data")]),
            exclusions: Some(Vec::new()),
        },
    ] {
        let response = runtime.handle_request(Request::new(20, command), USER);
        assert!(matches!(
            response.payload,
            ResponsePayload::Rejected { ref code, .. } if code == "administrator_required"
        ));
    }
}

#[test]
fn administrator_source_update_is_persisted_and_applied_live() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config_path = directory.path().join("config.toml");
    let store = LocalConfigStore::new(&config_path);
    let local = LocalConfig {
        repository: resticpal_core::config::LocalRepositoryConfig {
            url: Some("local:C:/backup".to_owned()),
            ..Default::default()
        },
        ..LocalConfig::default()
    };
    store.save(&local).expect("initial config");
    let (events, receiver) = mpsc::channel();
    let runtime = ServiceRuntime::load(&config_path, events).expect("runtime should load");

    let response = runtime.handle_request(
        Request::new(
            21,
            RequestCommand::UpdateBackupSources {
                paths: Some(vec![PathBuf::from(r"D:\Data"), PathBuf::from(r"d:\data")]),
                exclusions: Some(vec!["**/cache/**".to_owned(), "**/cache/**".to_owned()]),
            },
        ),
        ADMIN,
    );

    assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
    assert_eq!(
        receiver.recv().expect("configuration event"),
        RuntimeEvent::ConfigurationChanged
    );
    assert_eq!(runtime.config().backup.paths, [PathBuf::from(r"D:\Data")]);
    assert_eq!(runtime.config().backup.exclusions, ["**/cache/**"]);
    assert!(matches!(
        runtime.status().state,
        BackupState::Waiting {
            reason: WaitingReason::RepositoryValidation
        }
    ));
    let persisted = store.load().expect("updated config should load");
    assert_eq!(
        persisted.backup.paths,
        Some(vec![PathBuf::from(r"D:\Data")])
    );
}

#[test]
fn invalid_configuration_can_be_repaired_and_persisted_over_ipc() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config_path = directory.path().join("config.toml");
    std::fs::write(&config_path, "this is not valid TOML = [").expect("invalid config");
    let (events, receiver) = mpsc::channel();
    let runtime = ServiceRuntime::configuration_error(&config_path, events, None);

    let response = runtime.handle_request(
        Request::new(
            32,
            RequestCommand::UpdateBackupSources {
                paths: Some(vec![PathBuf::from(r"C:\Data")]),
                exclusions: Some(Vec::new()),
            },
        ),
        ADMIN,
    );

    assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
    assert_eq!(
        receiver.recv().expect("configuration event"),
        RuntimeEvent::ConfigurationChanged
    );
    let repaired = LocalConfigStore::new(&config_path)
        .load()
        .expect("repaired configuration");
    assert_eq!(repaired.backup.paths, Some(vec![PathBuf::from(r"C:\Data")]));
    assert!(matches!(runtime.status().state, BackupState::Unconfigured));
}

#[test]
fn invalid_source_update_does_not_replace_configuration() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config_path = directory.path().join("config.toml");
    let store = LocalConfigStore::new(&config_path);
    store.save(&LocalConfig::default()).expect("initial config");
    let before = std::fs::read(&config_path).expect("initial bytes");
    let (events, _receiver) = mpsc::channel();
    let runtime = ServiceRuntime::load(&config_path, events).expect("runtime should load");

    let response = runtime.handle_request(
        Request::new(
            22,
            RequestCommand::UpdateBackupSources {
                paths: Some(vec![PathBuf::from("relative")]),
                exclusions: Some(Vec::new()),
            },
        ),
        ADMIN,
    );

    assert!(matches!(
        response.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "invalid_backup_sources"
    ));
    assert_eq!(std::fs::read(&config_path).expect("config remains"), before);
}

#[test]
fn unsafe_source_updates_are_rejected_with_actionable_copy() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config_path = directory.path().join("config.toml");
    let store = LocalConfigStore::new(&config_path);
    store.save(&LocalConfig::default()).expect("initial config");
    let before = std::fs::read(&config_path).expect("initial bytes");
    let (events, _receiver) = mpsc::channel();
    let runtime = ServiceRuntime::load(&config_path, events).expect("runtime should load");

    let response = runtime.handle_request(
        Request::new(
            23,
            RequestCommand::UpdateBackupSources {
                paths: Some(vec![PathBuf::from(r"\\server\share\Data")]),
                exclusions: Some(Vec::new()),
            },
        ),
        ADMIN,
    );

    assert!(matches!(
        response.payload,
        ResponsePayload::Rejected { ref code, ref message }
            if code == "unsupported_network_backup_source"
                && message.contains("local Windows drive")
    ));
    assert_eq!(std::fs::read(&config_path).expect("config remains"), before);

    let protected_response = runtime.handle_request(
        Request::new(
            24,
            RequestCommand::UpdateBackupSources {
                paths: Some(vec![directory.path().to_path_buf()]),
                exclusions: Some(Vec::new()),
            },
        ),
        ADMIN,
    );
    assert!(matches!(
        protected_response.payload,
        ResponsePayload::Rejected { ref code, ref message }
            if code == "protected_backup_source"
                && message.contains("service-data folder")
    ));
    assert_eq!(std::fs::read(&config_path).expect("config remains"), before);
}

#[test]
fn managed_source_locks_are_enforced_by_the_service() {
    let (mut runtime, _events) = runtime(true);
    runtime.field_resolutions.get_mut().unwrap().insert(
        PolicyField::BackupPaths,
        FieldResolution {
            source: resticpal_core::policy::ValueSource::ManagedLocked,
            locked: true,
        },
    );

    let response = runtime.handle_request(
        Request::new(
            23,
            RequestCommand::UpdateBackupSources {
                paths: Some(vec![PathBuf::from(r"D:\Data")]),
                exclusions: None,
            },
        ),
        ADMIN,
    );

    assert!(matches!(
        response.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "managed_field_locked"
    ));
}

#[test]
fn managed_policy_is_applied_live_and_reports_its_lock() {
    let (runtime, events) = runtime(false);
    let policy = ManagedPolicy {
        revision: "managed-8".to_owned(),
        schedule: ManagedSchedulePolicy {
            interval_hours: Some(Managed {
                value: 8,
                locked: true,
            }),
            ..ManagedSchedulePolicy::default()
        },
        ..ManagedPolicy::default()
    };

    assert_eq!(
        runtime
            .apply_managed_policy_if_current(&policy, &LocalManagementConfig::default())
            .expect("valid policy"),
        ManagedPolicyApplyOutcome::Applied
    );
    assert_eq!(runtime.config().schedule.interval_hours, 8);
    assert_eq!(
        runtime.status().managed_revision.as_deref(),
        Some("managed-8")
    );
    assert!(runtime.field_locked(PolicyField::ScheduleIntervalHours));
    assert_eq!(
        events.recv().expect("configuration event"),
        RuntimeEvent::ConfigurationChanged
    );
}

#[test]
fn managed_policy_waits_until_an_active_backup_finishes() {
    let (runtime, events) = runtime(false);
    runtime.state_guard().status.state = BackupState::Running {
        phase: BackupPhase::Uploading,
    };
    let policy = ManagedPolicy {
        revision: "managed-later".to_owned(),
        schedule: ManagedSchedulePolicy {
            interval_hours: Some(Managed {
                value: 4,
                locked: false,
            }),
            ..ManagedSchedulePolicy::default()
        },
        ..ManagedPolicy::default()
    };

    assert_eq!(
        runtime
            .apply_managed_policy_if_current(&policy, &LocalManagementConfig::default())
            .expect("valid policy"),
        ManagedPolicyApplyOutcome::RuntimeBusy
    );
    assert_eq!(runtime.config().schedule.interval_hours, 24);
    assert!(runtime.status().managed_revision.is_none());
    assert!(events.try_recv().is_err());
}

#[test]
fn a_policy_fetched_for_an_old_management_source_cannot_apply_after_unenrollment() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config_path = directory.path().join("config.toml");
    let local = LocalConfig {
        management: LocalManagementConfig {
            mode: ManagementMode::PlainManifest,
            manifest_url: Some("http://127.0.0.1:9/old-policy.json".to_owned()),
            refresh_interval_minutes: Some(15),
            ..LocalManagementConfig::default()
        },
        ..LocalConfig::default()
    };
    LocalConfigStore::new(&config_path)
        .save(&local)
        .expect("managed config");
    let expected_source = local.management.clone();
    let (events, _receiver) = mpsc::channel();
    let runtime = ServiceRuntime::load(&config_path, events).expect("runtime");
    let stale_policy = ManagedPolicy {
        revision: "stale-after-unenroll".to_owned(),
        updates: Some(ManagedUpdatePolicy {
            automatic_install: Some(Managed {
                value: true,
                locked: true,
            }),
        }),
        ..ManagedPolicy::default()
    };

    assert!(matches!(
        runtime.unenroll(ADMIN),
        ResponsePayload::Accepted { .. }
    ));
    assert_eq!(
        runtime
            .apply_managed_policy_if_current(&stale_policy, &expected_source)
            .expect("valid stale policy"),
        ManagedPolicyApplyOutcome::SourceChanged
    );

    assert_eq!(
        runtime.local_config_guard().management.mode,
        ManagementMode::Disabled
    );
    assert!(runtime.status().managed_revision.is_none());
    assert!(!runtime.config().updates.automatic_install);
    assert!(!runtime.field_locked(PolicyField::UpdateAutomaticInstall));
}

#[test]
fn an_unlocked_source_field_can_change_without_overwriting_a_locked_field() {
    let (mut runtime, events) = runtime(true);
    runtime.config_write().backup.exclusions = vec!["managed-pattern".to_owned()];
    runtime.field_resolutions.get_mut().unwrap().insert(
        PolicyField::BackupExclusions,
        FieldResolution {
            source: resticpal_core::policy::ValueSource::ManagedLocked,
            locked: true,
        },
    );

    let response = runtime.handle_request(
        Request::new(
            24,
            RequestCommand::UpdateBackupSources {
                paths: Some(vec![PathBuf::from(r"D:\Data")]),
                exclusions: None,
            },
        ),
        ADMIN,
    );

    assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
    assert_eq!(
        events.recv().expect("configuration event"),
        RuntimeEvent::ConfigurationChanged
    );
    let config = runtime.config();
    assert_eq!(config.backup.paths, [PathBuf::from(r"D:\Data")]);
    assert_eq!(config.backup.exclusions, ["managed-pattern"]);
    assert_eq!(config.repository.url.as_deref(), Some("local:C:/backup"));
}

#[test]
fn repository_configuration_requires_an_elevated_administrator() {
    let (runtime, _events) = runtime(true);
    let update = RequestCommand::UpdateRepository {
        display_name: Some("Backup".to_owned()),
        url: Some("local:C:/backup".to_owned()),
        mode: Some(RepositoryMode::Standard),
        options: Some(BTreeMap::new()),
        secret_updates: Vec::new(),
    };

    for command in [
        RequestCommand::GetRepository,
        update,
        RequestCommand::ValidateRepository,
        RequestCommand::InitializeRepository,
    ] {
        let response = runtime.handle_request(Request::new(30, command), USER);
        assert!(matches!(
            response.payload,
            ResponsePayload::Rejected { ref code, .. } if code == "administrator_required"
        ));
    }
}

#[test]
fn repository_update_rejects_options_that_disable_source_snapshots() {
    let (runtime, _events) = runtime(true);
    let response = runtime.handle_request(
        Request::new(
            31,
            RequestCommand::UpdateRepository {
                display_name: None,
                url: None,
                mode: None,
                options: Some(BTreeMap::from([(
                    "vss.exclude-volumes".to_owned(),
                    "C:".to_owned(),
                )])),
                secret_updates: Vec::new(),
            },
        ),
        ADMIN,
    );

    assert!(matches!(
        response.payload,
        ResponsePayload::Rejected { ref code, ref message }
            if code == "invalid_repository"
                && message.contains("must use Windows filesystem snapshots")
    ));
    assert!(runtime.config().repository.options.is_empty());
}

#[test]
fn repository_credentials_rotate_without_entering_configuration_or_responses() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config_path = directory.path().join("config.toml");
    let config_store = LocalConfigStore::new(&config_path);
    config_store
        .save(&LocalConfig {
            backup: LocalBackupConfig {
                paths: Some(vec![PathBuf::from(r"C:\Data")]),
                exclusions: Some(Vec::new()),
            },
            ..LocalConfig::default()
        })
        .expect("initial config");
    let credentials =
        DpapiSecretStore::open(directory.path().join("Credentials")).expect("credential store");
    let (events, receiver) = mpsc::channel();
    let runtime =
        ServiceRuntime::load_with_credentials(&config_path, events, Some(credentials.clone()))
            .expect("runtime");
    let first_secret = "first-unique-repository-secret";

    let response = runtime.handle_request(
        Request::new(
            31,
            RequestCommand::UpdateRepository {
                display_name: Some("Managed S3".to_owned()),
                url: Some("s3:https://s3.example.test/bucket/device".to_owned()),
                mode: Some(RepositoryMode::AppendOnly),
                options: Some(BTreeMap::from([(
                    "s3.region".to_owned(),
                    "us-west-2".to_owned(),
                )])),
                secret_updates: vec![RepositorySecretUpdate::Set {
                    variable: SecretEnvironmentVariable::ResticPassword,
                    value: SecretValue::new(first_secret),
                }],
            },
        ),
        ADMIN,
    );

    assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
    assert_eq!(
        receiver.recv().expect("configuration event"),
        RuntimeEvent::ConfigurationChanged
    );
    let first_reference =
        runtime.config().repository.secret_refs[&SecretEnvironmentVariable::ResticPassword].clone();
    assert_eq!(
        credentials
            .get(&first_reference)
            .expect("first secret")
            .as_slice(),
        first_secret.as_bytes()
    );
    let config_text = std::fs::read_to_string(&config_path).expect("saved config");
    assert!(!config_text.contains(first_secret));
    let view = runtime.handle_request(Request::new(32, RequestCommand::GetRepository), ADMIN);
    assert!(matches!(
        view.payload,
        ResponsePayload::Repository {
            configuration: RepositoryView {
                mode: RepositoryMode::AppendOnly,
                ref configured_secrets,
                ..
            }
        } if configured_secrets == &[SecretEnvironmentVariable::ResticPassword]
    ));
    assert!(!format!("{view:?}").contains(first_secret));
    assert!(!format!("{view:?}").contains(&first_reference));

    let second_secret = "second-unique-repository-secret";
    let response = runtime.handle_request(
        Request::new(
            33,
            RequestCommand::UpdateRepository {
                display_name: None,
                url: None,
                mode: None,
                options: None,
                secret_updates: vec![RepositorySecretUpdate::Set {
                    variable: SecretEnvironmentVariable::ResticPassword,
                    value: SecretValue::new(second_secret),
                }],
            },
        ),
        ADMIN,
    );
    assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
    assert_eq!(
        receiver.recv().expect("rotation event"),
        RuntimeEvent::ConfigurationChanged
    );
    let second_reference =
        runtime.config().repository.secret_refs[&SecretEnvironmentVariable::ResticPassword].clone();
    assert_ne!(first_reference, second_reference);
    assert!(matches!(
        credentials.get(&first_reference),
        Err(CredentialStoreError::NotFound)
    ));
    assert_eq!(
        credentials
            .get(&second_reference)
            .expect("rotated secret")
            .as_slice(),
        second_secret.as_bytes()
    );

    let response = runtime.handle_request(
        Request::new(
            34,
            RequestCommand::UpdateRepository {
                display_name: None,
                url: None,
                mode: None,
                options: None,
                secret_updates: vec![RepositorySecretUpdate::Remove {
                    variable: SecretEnvironmentVariable::ResticPassword,
                }],
            },
        ),
        ADMIN,
    );
    assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
    assert!(
        !runtime
            .config()
            .repository
            .secret_refs
            .contains_key(&SecretEnvironmentVariable::ResticPassword)
    );
    assert!(matches!(
        credentials.get(&second_reference),
        Err(CredentialStoreError::NotFound)
    ));
}

#[test]
fn repository_policy_locks_are_reported_and_enforced_per_field() {
    let (mut runtime, events) = runtime(true);
    runtime.field_resolutions.get_mut().unwrap().insert(
        PolicyField::RepositoryUrl,
        FieldResolution {
            source: resticpal_core::policy::ValueSource::ManagedLocked,
            locked: true,
        },
    );

    let view = runtime.handle_request(Request::new(35, RequestCommand::GetRepository), ADMIN);
    assert!(matches!(
        view.payload,
        ResponsePayload::Repository {
            configuration: RepositoryView {
                url_locked: true,
                display_name_locked: false,
                ..
            }
        }
    ));

    let rejected_response = runtime.handle_request(
        Request::new(
            36,
            RequestCommand::UpdateRepository {
                display_name: None,
                url: Some("local:D:/replacement".to_owned()),
                mode: None,
                options: None,
                secret_updates: Vec::new(),
            },
        ),
        ADMIN,
    );
    assert!(matches!(
        rejected_response.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "managed_field_locked"
    ));

    let accepted_response = runtime.handle_request(
        Request::new(
            37,
            RequestCommand::UpdateRepository {
                display_name: Some("Friendly name".to_owned()),
                url: None,
                mode: None,
                options: None,
                secret_updates: Vec::new(),
            },
        ),
        ADMIN,
    );
    assert!(matches!(
        accepted_response.payload,
        ResponsePayload::Accepted { .. }
    ));
    assert_eq!(
        events.recv().expect("configuration event"),
        RuntimeEvent::ConfigurationChanged
    );
    let config = runtime.config();
    assert_eq!(config.repository.url.as_deref(), Some("local:C:/backup"));
    assert_eq!(
        config.repository.display_name.as_deref(),
        Some("Friendly name")
    );
}

#[test]
fn repository_validation_is_queued_and_reported_until_completion() {
    let (runtime, events) = runtime(true);
    runtime.config_write().repository.secret_refs.insert(
        SecretEnvironmentVariable::ResticPassword,
        "repository-password".to_owned(),
    );

    let response =
        runtime.handle_request(Request::new(40, RequestCommand::ValidateRepository), ADMIN);
    assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
    assert_eq!(
        events.recv().expect("repository event"),
        RuntimeEvent::RepositoryOperationRequested(RepositoryOperationKind::Validate)
    );
    let duplicate =
        runtime.handle_request(Request::new(41, RequestCommand::ValidateRepository), ADMIN);
    assert!(matches!(
        duplicate.payload,
        ResponsePayload::Rejected { ref code, .. }
            if code == "repository_operation_running"
    ));
    let running = runtime.handle_request(Request::new(42, RequestCommand::GetRepository), ADMIN);
    assert!(matches!(
        running.payload,
        ResponsePayload::Repository {
            configuration: RepositoryView {
                operation_status: RepositoryOperationStatus::Running {
                    operation: RepositoryOperationKind::Validate
                },
                ..
            }
        }
    ));

    runtime.finish_repository_operation(
        RepositoryOperationKind::Validate,
        &RepositoryOutcome {
            kind: RepositoryOutcomeKind::Succeeded,
        },
    );
    let succeeded = runtime.handle_request(Request::new(43, RequestCommand::GetRepository), ADMIN);
    assert!(matches!(
        succeeded.payload,
        ResponsePayload::Repository {
            configuration: RepositoryView {
                operation_status: RepositoryOperationStatus::Succeeded {
                    operation: RepositoryOperationKind::Validate,
                    ..
                },
                ..
            }
        }
    ));
}

#[test]
fn append_only_mode_rejects_repository_initialization() {
    let (runtime, _events) = runtime(true);
    {
        let mut config = runtime.config_write();
        config.repository.mode = RepositoryMode::AppendOnly;
        config.repository.secret_refs.insert(
            SecretEnvironmentVariable::ResticPassword,
            "repository-password".to_owned(),
        );
    }

    let response = runtime.handle_request(
        Request::new(43, RequestCommand::InitializeRepository),
        ADMIN,
    );

    assert!(matches!(
        response.payload,
        ResponsePayload::Rejected { ref code, .. }
            if code == "append_only_initialization_forbidden"
    ));
}

#[test]
fn connection_changes_require_validation_before_backup() {
    let (runtime, events) = runtime(true);
    let response = runtime.handle_request(
        Request::new(
            44,
            RequestCommand::UpdateRepository {
                display_name: None,
                url: Some("local:D:/replacement".to_owned()),
                mode: None,
                options: None,
                secret_updates: Vec::new(),
            },
        ),
        ADMIN,
    );
    assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
    assert_eq!(
        events.recv().expect("configuration event"),
        RuntimeEvent::ConfigurationChanged
    );
    assert_eq!(
        runtime.evaluate_schedule(Utc::now(), available_conditions()),
        ScheduleAction::None
    );
    assert!(matches!(
        runtime.status().state,
        BackupState::Waiting {
            reason: WaitingReason::RepositoryValidation
        }
    ));

    let run = runtime.handle_request(Request::new(45, RequestCommand::RunBackupNow), ADMIN);
    assert!(matches!(
        run.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "repository_not_ready"
    ));
    let view = runtime.handle_request(Request::new(46, RequestCommand::GetRepository), ADMIN);
    assert!(matches!(
        view.payload,
        ResponsePayload::Repository {
            configuration: RepositoryView {
                operation_status: RepositoryOperationStatus::ValidationRequired,
                ..
            }
        }
    ));
}

#[test]
fn repository_validation_gate_survives_service_restart() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config_path = directory.path().join("config.toml");
    LocalConfigStore::new(&config_path)
        .save(&LocalConfig {
            backup: LocalBackupConfig {
                paths: Some(vec![PathBuf::from(r"C:\Data")]),
                exclusions: Some(Vec::new()),
            },
            repository: LocalRepositoryConfig {
                url: Some("local:C:/backup".to_owned()),
                secret_refs: Some(BTreeMap::from([(
                    SecretEnvironmentVariable::ResticPassword,
                    "repository-password".to_owned(),
                )])),
                ..LocalRepositoryConfig::default()
            },
            ..LocalConfig::default()
        })
        .expect("initial config");
    let (events, receiver) = mpsc::channel();
    let runtime = ServiceRuntime::load(&config_path, events).expect("runtime");

    let response = runtime.handle_request(
        Request::new(
            47,
            RequestCommand::UpdateRepository {
                display_name: None,
                url: Some("local:D:/replacement".to_owned()),
                mode: None,
                options: None,
                secret_updates: Vec::new(),
            },
        ),
        ADMIN,
    );
    assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
    assert_eq!(
        receiver.recv().expect("configuration event"),
        RuntimeEvent::ConfigurationChanged
    );
    drop(runtime);

    let (events, receiver) = mpsc::channel();
    let runtime = ServiceRuntime::load(&config_path, events).expect("restarted runtime");
    assert_eq!(
        runtime.evaluate_schedule(Utc::now(), available_conditions()),
        ScheduleAction::None
    );
    let response =
        runtime.handle_request(Request::new(48, RequestCommand::ValidateRepository), ADMIN);
    assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
    assert_eq!(
        receiver.recv().expect("repository event"),
        RuntimeEvent::RepositoryOperationRequested(RepositoryOperationKind::Validate)
    );
    runtime.finish_repository_operation(
        RepositoryOperationKind::Validate,
        &RepositoryOutcome {
            kind: RepositoryOutcomeKind::Succeeded,
        },
    );
    drop(runtime);

    let (events, _receiver) = mpsc::channel();
    let runtime = ServiceRuntime::load(&config_path, events).expect("verified restart");
    let view = runtime.handle_request(Request::new(49, RequestCommand::GetRepository), ADMIN);
    assert!(matches!(
        view.payload,
        ResponsePayload::Repository {
            configuration: RepositoryView {
                operation_status: RepositoryOperationStatus::Succeeded {
                    operation: RepositoryOperationKind::Validate,
                    ..
                },
                ..
            }
        }
    ));
    assert!(matches!(
        runtime.evaluate_schedule(Utc::now(), available_conditions()),
        ScheduleAction::Start { .. }
    ));
}

#[test]
fn schedule_configuration_requires_admin_and_persists_live_updates() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config_path = directory.path().join("config.toml");
    LocalConfigStore::new(&config_path)
        .save(&LocalConfig {
            backup: LocalBackupConfig {
                paths: Some(vec![PathBuf::from(r"C:\Data")]),
                exclusions: Some(Vec::new()),
            },
            repository: LocalRepositoryConfig {
                url: Some("local:C:/backup".to_owned()),
                ..LocalRepositoryConfig::default()
            },
            ..LocalConfig::default()
        })
        .expect("initial config");
    let (events, receiver) = mpsc::channel();
    let runtime = ServiceRuntime::load(&config_path, events).expect("runtime");
    let update = || RequestCommand::UpdateSchedule {
        interval_hours: Some(12),
        wake_grace_seconds: Some(600),
        wake_lock_timeout_seconds: Some(3_600),
        allow_on_battery: Some(false),
        allow_metered_network: Some(false),
    };

    for command in [RequestCommand::GetSchedule, update()] {
        let response = runtime.handle_request(Request::new(50, command), USER);
        assert!(matches!(
            response.payload,
            ResponsePayload::Rejected { ref code, .. } if code == "administrator_required"
        ));
    }

    let response = runtime.handle_request(Request::new(51, update()), ADMIN);
    assert!(matches!(response.payload, ResponsePayload::Accepted { .. }));
    assert_eq!(
        receiver.recv().expect("configuration event"),
        RuntimeEvent::ConfigurationChanged
    );
    let schedule = runtime.config().schedule;
    assert_eq!(schedule.interval_hours, 12);
    assert_eq!(schedule.wake_grace_seconds, 600);
    assert_eq!(schedule.wake_lock_timeout_seconds, 3_600);
    assert!(!schedule.allow_on_battery);
    assert!(!schedule.allow_metered_network);
    let persisted = LocalConfigStore::new(&config_path)
        .load()
        .expect("persisted config");
    assert_eq!(persisted.schedule.interval_hours, Some(12));
    assert_eq!(persisted.schedule.wake_grace_seconds, Some(600));
    assert_eq!(persisted.schedule.wake_lock_timeout_seconds, Some(3_600));
    assert_eq!(persisted.schedule.allow_on_battery, Some(false));
    assert_eq!(persisted.schedule.allow_metered_network, Some(false));
}

#[test]
fn schedule_policy_locks_are_reported_and_enforced_per_field() {
    let (mut runtime, events) = runtime(true);
    runtime.field_resolutions.get_mut().unwrap().insert(
        PolicyField::ScheduleIntervalHours,
        FieldResolution {
            source: resticpal_core::policy::ValueSource::ManagedLocked,
            locked: true,
        },
    );
    let view = runtime.handle_request(Request::new(52, RequestCommand::GetSchedule), ADMIN);
    assert!(matches!(
        view.payload,
        ResponsePayload::Schedule {
            configuration: ScheduleView {
                interval_hours_locked: true,
                allow_on_battery_locked: false,
                ..
            }
        }
    ));

    let rejected_response = runtime.handle_request(
        Request::new(
            53,
            RequestCommand::UpdateSchedule {
                interval_hours: Some(6),
                wake_grace_seconds: None,
                wake_lock_timeout_seconds: None,
                allow_on_battery: None,
                allow_metered_network: None,
            },
        ),
        ADMIN,
    );
    assert!(matches!(
        rejected_response.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "managed_field_locked"
    ));

    let accepted_response = runtime.handle_request(
        Request::new(
            54,
            RequestCommand::UpdateSchedule {
                interval_hours: None,
                wake_grace_seconds: None,
                wake_lock_timeout_seconds: None,
                allow_on_battery: Some(false),
                allow_metered_network: None,
            },
        ),
        ADMIN,
    );
    assert!(matches!(
        accepted_response.payload,
        ResponsePayload::Accepted { .. }
    ));
    assert_eq!(
        events.recv().expect("configuration event"),
        RuntimeEvent::ConfigurationChanged
    );
    assert_eq!(runtime.config().schedule.interval_hours, 24);
    assert!(!runtime.config().schedule.allow_on_battery);
}

#[test]
fn invalid_schedule_update_does_not_replace_configuration() {
    let (runtime, _events) = runtime(true);
    let before = runtime.config().schedule;

    let response = runtime.handle_request(
        Request::new(
            55,
            RequestCommand::UpdateSchedule {
                interval_hours: Some(0),
                wake_grace_seconds: None,
                wake_lock_timeout_seconds: None,
                allow_on_battery: None,
                allow_metered_network: None,
            },
        ),
        ADMIN,
    );

    assert!(matches!(
        response.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "invalid_schedule"
    ));
    assert_eq!(runtime.config().schedule, before);
}

#[test]
fn retention_updates_are_admin_only_bounded_and_live() {
    let (runtime, events) = runtime(true);
    let update = || RequestCommand::UpdateRetention {
        daily: Some(14),
        weekly: Some(8),
        monthly: Some(18),
        yearly: Some(5),
        prune_interval_days: Some(14),
    };
    assert!(matches!(
        runtime.handle_request(Request::new(70, update()), USER).payload,
        ResponsePayload::Rejected { ref code, .. } if code == "administrator_required"
    ));
    assert!(matches!(
        runtime
            .handle_request(Request::new(71, update()), ADMIN)
            .payload,
        ResponsePayload::Accepted { .. }
    ));
    assert_eq!(
        events.recv().expect("configuration event"),
        RuntimeEvent::ConfigurationChanged
    );
    assert_eq!(runtime.config().retention.daily, 14);
    assert_eq!(runtime.config().retention.prune_interval_days, 14);

    let invalid = runtime.handle_request(
        Request::new(
            72,
            RequestCommand::UpdateRetention {
                daily: Some(0),
                weekly: Some(0),
                monthly: Some(0),
                yearly: Some(0),
                prune_interval_days: None,
            },
        ),
        ADMIN,
    );
    assert!(matches!(
        invalid.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "invalid_retention"
    ));
}

#[test]
fn append_only_retention_is_server_managed() {
    let (runtime, _events) = runtime(true);
    runtime.config_write().repository.mode = RepositoryMode::AppendOnly;
    assert_eq!(runtime.begin_retention(Utc::now()), None);
    let view = runtime.handle_request(Request::new(73, RequestCommand::GetRetention), ADMIN);
    assert!(matches!(
        view.payload,
        ResponsePayload::Retention {
            configuration: RetentionView {
                repository_mode: RepositoryMode::AppendOnly,
                ..
            }
        }
    ));
    let update = runtime.handle_request(
        Request::new(
            74,
            RequestCommand::UpdateRetention {
                daily: Some(30),
                weekly: None,
                monthly: None,
                yearly: None,
                prune_interval_days: None,
            },
        ),
        ADMIN,
    );
    assert!(matches!(
        update.payload,
        ResponsePayload::Rejected { ref code, .. } if code == "retention_managed_by_server"
    ));
}

#[test]
fn standard_retention_transitions_phase_and_preserves_backup_success_as_warning() {
    let (runtime, _events) = runtime(true);
    assert!(matches!(
        runtime.evaluate_schedule(Utc::now(), available_conditions()),
        ScheduleAction::Start { .. }
    ));
    let now = Utc::now();
    assert_eq!(
        runtime.begin_retention(now),
        Some(RetentionPlan { prune_due: true })
    );
    assert!(matches!(
        runtime.status().state,
        BackupState::Running {
            phase: BackupPhase::Retention
        }
    ));
    let backup = BackupOutcome::succeeded(BackupSummary {
        files_processed: 1,
        bytes_processed: 2,
        data_added: 3,
        snapshot_id: Some("snapshot".to_owned()),
    });
    let retention = RetentionOutcome {
        kind: RetentionOutcomeKind::Failed {
            code: "retention_prune_failed".to_owned(),
        },
    };
    let outcome = runtime.finish_retention(backup, &retention, now);
    assert_eq!(outcome.kind, BackupOutcomeKind::SucceededWithWarnings);
    assert_eq!(
        outcome.warning_code.as_deref(),
        Some("retention_prune_failed")
    );

    let backup_warning = BackupOutcome::succeeded(BackupSummary {
        files_processed: 1,
        bytes_processed: 2,
        data_added: 3,
        snapshot_id: Some("warning-snapshot".to_owned()),
    })
    .with_warning("restic_vss_fallback");
    let outcome = runtime.finish_retention(backup_warning, &retention, now);
    assert_eq!(outcome.kind, BackupOutcomeKind::SucceededWithWarnings);
    assert_eq!(
        outcome.warning_code.as_deref(),
        Some("restic_vss_fallback"),
        "retention state and diagnostics must not hide the primary backup warning"
    );
}

#[test]
fn diagnostics_are_admin_only_and_never_return_raw_details() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config_path = directory.path().join("config.toml");
    LocalConfigStore::new(&config_path)
        .save(&LocalConfig::default())
        .expect("initial config");
    let (events, _receiver) = mpsc::channel();
    let runtime = ServiceRuntime::load(&config_path, events).expect("runtime");
    runtime.record_diagnostic(
        DiagnosticLevel::Error,
        "backup.failed",
        "Backup failed.",
        Some("Access denied: C:\\Users\\Private"),
    );
    assert!(matches!(
        runtime
            .handle_request(
                Request::new(75, RequestCommand::GetDiagnostics { limit: 10 }),
                USER,
            )
            .payload,
        ResponsePayload::Rejected { ref code, .. } if code == "administrator_required"
    ));
    let response = runtime.handle_request(
        Request::new(76, RequestCommand::GetDiagnostics { limit: 10 }),
        ADMIN,
    );
    let serialized = format!("{response:?}");
    assert!(matches!(
        response.payload,
        ResponsePayload::Diagnostics { .. }
    ));
    assert!(!serialized.contains(r"C:\Users\Private"));
}

#[test]
fn backup_history_summaries_are_user_readable_but_source_failure_paths_are_admin_only() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config_path = directory.path().join("config.toml");
    LocalConfigStore::new(&config_path)
        .save(&LocalConfig {
            backup: LocalBackupConfig {
                paths: Some(vec![PathBuf::from(r"C:\Data")]),
                exclusions: Some(Vec::new()),
            },
            repository: LocalRepositoryConfig {
                url: Some("local:C:/backup".to_owned()),
                ..LocalRepositoryConfig::default()
            },
            ..LocalConfig::default()
        })
        .expect("initial config");
    let mut verified_config = EffectiveConfig::default();
    verified_config.repository.url = Some("local:C:/backup".to_owned());
    let mut service_state = ServiceStateSnapshot::default();
    service_state.mark_repository_verified(&verified_config, Utc::now());
    ScheduleStateStore::next_to_config(&config_path)
        .save(&service_state)
        .expect("verified repository state");
    let (events, receiver) = mpsc::channel();
    let runtime = ServiceRuntime::load(&config_path, events).expect("runtime");
    assert!(matches!(
        runtime
            .handle_request(Request::new(56, RequestCommand::RunBackupNow), USER)
            .payload,
        ResponsePayload::Accepted { .. }
    ));
    assert_eq!(receiver.recv().expect("run event"), RuntimeEvent::RunNow);
    assert!(matches!(
        runtime.evaluate_schedule(Utc::now(), available_conditions()),
        ScheduleAction::Start { .. }
    ));
    let private_path = r"C:\Users\Private\locked.txt";
    runtime.finish_backup(&BackupOutcome::warnings(
        BackupSummary {
            files_processed: 12,
            bytes_processed: 1_024,
            data_added: 256,
            snapshot_id: Some("abc123".to_owned()),
        },
        "restic_partial_source",
        BackupFailureDetails::from_items(vec![private_path.to_owned()], 2),
    ));

    let response = runtime.handle_request(
        Request::new(57, RequestCommand::GetRunHistory { limit: 50 }),
        USER,
    );
    assert!(!format!("{response:?}").contains(private_path));
    let run_id = match &response.payload {
        ResponsePayload::RunHistory { runs } => runs[0].id,
        _ => panic!("expected backup history"),
    };
    assert!(matches!(
        response.payload,
        ResponsePayload::RunHistory { ref runs }
            if matches!(runs.as_slice(), [run]
                if run.outcome == BackupRunOutcome::SucceededWithWarnings
                    && run.files_processed == Some(12)
                    && run.snapshot_id.as_deref() == Some("abc123")
                    && run.failed_item_count == 3)
    ));
    assert!(matches!(
        runtime
            .handle_request(
                Request::new(58, RequestCommand::GetRunFailureDetails { run_id }),
                USER,
            )
            .payload,
        ResponsePayload::Rejected { ref code, .. } if code == "administrator_required"
    ));
    assert!(matches!(
        runtime
            .handle_request(
                Request::new(59, RequestCommand::GetRunFailureDetails { run_id }),
                ADMIN,
            )
            .payload,
        ResponsePayload::RunFailureDetails { details }
            if details.items == [private_path] && details.omitted == 2
    ));
    let diagnostics = runtime.handle_request(
        Request::new(60, RequestCommand::GetDiagnostics { limit: 10 }),
        ADMIN,
    );
    assert!(!format!("{diagnostics:?}").contains(private_path));
    drop(runtime);

    let (events, _receiver) = mpsc::channel();
    let restarted = ServiceRuntime::load(&config_path, events).expect("restarted runtime");
    assert!(matches!(
        restarted
            .handle_request(
                Request::new(61, RequestCommand::GetRunHistory { limit: 1 }),
                USER,
            )
            .payload,
        ResponsePayload::RunHistory { runs } if runs.len() == 1
    ));
    assert!(matches!(
        restarted
            .handle_request(
                Request::new(62, RequestCommand::GetRunFailureDetails { run_id }),
                ADMIN,
            )
            .payload,
        ResponsePayload::RunFailureDetails { details }
            if details.items == [private_path] && details.omitted == 2
    ));
    assert!(matches!(
        restarted
            .handle_request(
                Request::new(63, RequestCommand::GetRunHistory { limit: 0 }),
                USER,
            )
            .payload,
        ResponsePayload::Rejected { ref code, .. } if code == "invalid_history_limit"
    ));
}

#[test]
fn enrollment_commit_protects_credentials_activates_policy_and_unenrolls_cleanly() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config_path = directory.path().join("config.toml");
    let credentials =
        DpapiSecretStore::open(directory.path().join("Credentials")).expect("credentials");
    let (events, _receiver) = mpsc::channel();
    let runtime =
        ServiceRuntime::load_with_credentials(&config_path, events, Some(credentials.clone()))
            .expect("runtime");
    let now = Utc::now();
    let signing_key = SigningKey::from_bytes(&[61_u8; 32]);
    let payload = ManifestPayload {
        schema_version: MANAGEMENT_SCHEMA_VERSION,
        sequence: 4,
        issued_at: now,
        expires_at: Some(now + Duration::days(7)),
        policy: ManagedPolicy {
            revision: "enrollment-4".to_owned(),
            ..ManagedPolicy::default()
        },
    };
    let payload_bytes = serde_json::to_vec(&payload).expect("manifest payload");
    let envelope = SignedManifestEnvelope {
        schema_version: MANAGEMENT_SCHEMA_VERSION,
        algorithm: "ed25519".to_owned(),
        key_id: "server-primary".to_owned(),
        payload: URL_SAFE_NO_PAD.encode(&payload_bytes),
        signature: URL_SAFE_NO_PAD.encode(signing_key.sign(&payload_bytes).to_bytes()),
    };
    let document = serde_json::to_string(&envelope).expect("manifest document");
    runtime
        .commit_enrollment(PendingEnrollment {
            material: EnrollmentMaterial {
                device_id: "test-device".to_owned(),
                manifest_url: "http://127.0.0.1:9/v1/manifest/test-device".to_owned(),
                status_url: "http://127.0.0.1:9/v1/status".to_owned(),
                refresh_interval_minutes: 15,
                initial_manifest_document: document,
                initial_manifest: VerifiedManifest {
                    payload,
                    signed: true,
                },
                status_token: Zeroizing::new("device-token".to_owned()),
                repository_secrets: BTreeMap::from([(
                    SecretEnvironmentVariable::ResticPassword,
                    Zeroizing::new("repository-password".to_owned()),
                )]),
            },
            private_key: Zeroizing::new([67_u8; 32]),
            public_key: URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes()),
        })
        .expect("commit enrollment");

    let local = LocalConfigStore::new(&config_path)
        .load()
        .expect("enrolled config");
    assert_eq!(local.management.mode, ManagementMode::SignedManifest);
    assert_eq!(
        runtime.status().managed_revision.as_deref(),
        Some("enrollment-4")
    );
    let status_ref = local
        .management
        .status_token_ref
        .clone()
        .expect("status reference");
    let key_ref = local
        .management
        .enrollment_key_ref
        .clone()
        .expect("key reference");
    let repository_ref = local.repository.secret_refs.as_ref().expect("secret refs")
        [&SecretEnvironmentVariable::ResticPassword]
        .clone();
    assert_eq!(
        credentials
            .get(&status_ref)
            .expect("status token")
            .as_slice(),
        b"device-token"
    );
    assert_eq!(
        credentials.get(&key_ref).expect("identity key").as_slice(),
        &[67_u8; 32]
    );
    assert_eq!(
        credentials
            .get(&repository_ref)
            .expect("repository secret")
            .as_slice(),
        b"repository-password"
    );
    assert!(matches!(
        runtime
            .handle_request(Request::new(60, RequestCommand::GetRepository), ADMIN)
            .payload,
        ResponsePayload::Repository {
            configuration: RepositoryView {
                ref configured_secrets,
                secrets_locked: true,
                ..
            }
        } if configured_secrets == &[SecretEnvironmentVariable::ResticPassword]
    ));
    assert!(matches!(
        runtime
            .handle_request(
                Request::new(
                    61,
                    RequestCommand::UpdateRepository {
                        display_name: None,
                        url: None,
                        mode: None,
                        options: None,
                        secret_updates: vec![RepositorySecretUpdate::Remove {
                            variable: SecretEnvironmentVariable::ResticPassword,
                        }],
                    },
                ),
                ADMIN,
            )
            .payload,
        ResponsePayload::Rejected { ref code, .. } if code == "managed_field_locked"
    ));

    assert!(matches!(
        runtime.unenroll(ADMIN),
        ResponsePayload::Accepted { .. }
    ));
    let local = LocalConfigStore::new(&config_path)
        .load()
        .expect("unmanaged config");
    assert_eq!(local.management.mode, ManagementMode::Disabled);
    assert!(matches!(
        runtime
            .handle_request(Request::new(62, RequestCommand::GetRepository), ADMIN)
            .payload,
        ResponsePayload::Repository {
            configuration: RepositoryView {
                secrets_locked: false,
                ..
            }
        }
    ));
    assert!(matches!(
        credentials.get(&status_ref),
        Err(CredentialStoreError::NotFound)
    ));
    assert!(matches!(
        credentials.get(&key_ref),
        Err(CredentialStoreError::NotFound)
    ));
    assert_eq!(
        credentials
            .get(&repository_ref)
            .expect("retained repository secret")
            .as_slice(),
        b"repository-password"
    );
}
