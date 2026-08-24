mod atomic_file;
mod conditions;
mod config_store;
mod diagnostics;
mod executor;
mod history;
mod management;
mod power_request;
mod runtime;
mod state;
mod updater;

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;
use std::time::Instant;

use chrono::Utc;
use conditions::{SystemConditions, WinRtApartment};
use config_store::LocalConfigStore;
use executor::{
    BackupOutcome, CancellationToken, DpapiSecretResolver, RepositoryOutcome, ResticExecutor,
    SecretResolver, SystemWakeLockProvider, UnavailableSecretResolver,
};
use resticpal_core::restic::ResticOperation;
use resticpal_core::status::BackupState;
use resticpal_protocol::DiagnosticLevel;
use resticpal_protocol::{RepositoryOperationKind, UpdatePackage};
use resticpal_windows::credentials::DpapiSecretStore;
use resticpal_windows::named_pipe::{DEFAULT_PIPE_NAME, NamedPipeServer};
use runtime::{ManagedPolicyApplyOutcome, RuntimeEvent, ScheduleAction, ServiceRuntime};
use updater::UpdateInstallOutcome;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
};
use windows::core::w;
use windows_service::define_windows_service;
use windows_service::service::{
    PowerEventParam, ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState,
    ServiceStatus as ScmServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::{Result as ServiceResult, service_dispatcher};
use zeroize::Zeroizing;

const SERVICE_NAME: &str = "ResticPal";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
const MANAGEMENT_BUSY_RETRY: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManagementRefreshOutcome {
    Synchronized,
    RuntimeBusy,
    Failed,
}

fn management_refresh_delay(
    refresh_interval_minutes: u32,
    outcome: ManagementRefreshOutcome,
    consecutive_failures: u32,
) -> Duration {
    let configured = Duration::from_secs(u64::from(refresh_interval_minutes) * 60);
    match outcome {
        ManagementRefreshOutcome::Synchronized => configured,
        ManagementRefreshOutcome::RuntimeBusy => MANAGEMENT_BUSY_RETRY,
        ManagementRefreshOutcome::Failed => {
            let exponent = consecutive_failures.saturating_sub(1).min(6);
            Duration::from_secs(60_u64.saturating_mul(1_u64 << exponent)).min(configured)
        }
    }
}

fn main() -> ExitCode {
    let arguments: Vec<OsString> = env::args_os().skip(1).collect();
    if arguments.iter().any(|argument| argument == "--console") {
        return console_smoke_test(&arguments);
    }

    match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let message =
                format!("could not start resticpal through the service dispatcher: {error}");
            eprintln!("{message}");
            record_service_startup_error(&message);
            ExitCode::FAILURE
        }
    }
}

fn console_smoke_test(arguments: &[OsString]) -> ExitCode {
    let (events, _receiver) = mpsc::channel();
    let config_path = config_path(arguments);
    match ServiceRuntime::load(&config_path, events) {
        Ok(runtime) => {
            println!("resticpal service runtime initialized in console smoke-test mode");
            println!("service name: {SERVICE_NAME}");
            println!("configuration: {}", config_path.display());
            println!("state: {:?}", runtime.status().state);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!(
                "could not initialize resticpal from {}: {error}",
                config_path.display()
            );
            ExitCode::FAILURE
        }
    }
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(arguments: Vec<OsString>) {
    if let Err(error) = run_service(&arguments) {
        let message = format!("resticpal service failed: {error}");
        eprintln!("{message}");
        record_service_startup_error(&message);
    }
}

fn run_service(arguments: &[OsString]) -> ServiceResult<()> {
    let _winrt_apartment = match WinRtApartment::initialize() {
        Ok(apartment) => Some(apartment),
        Err(error) => {
            eprintln!("could not initialize Windows Runtime network checks: {error}");
            None
        }
    };
    let (event_tx, event_rx) = mpsc::channel();
    let handler_tx = event_tx.clone();

    let event_handler = move |event| -> ServiceControlHandlerResult {
        let runtime_event = match event {
            ServiceControl::Interrogate => return ServiceControlHandlerResult::NoError,
            ServiceControl::Stop | ServiceControl::Shutdown => RuntimeEvent::Stop,
            ServiceControl::PowerEvent(
                PowerEventParam::ResumeAutomatic
                | PowerEventParam::ResumeSuspend
                | PowerEventParam::ResumeCritical,
            ) => RuntimeEvent::Resume,
            ServiceControl::PowerEvent(PowerEventParam::PowerStatusChange) => {
                RuntimeEvent::PowerStatusChanged
            }
            ServiceControl::TimeChange => RuntimeEvent::TimeChanged,
            _ => return ServiceControlHandlerResult::NotImplemented,
        };

        let _ = handler_tx.send(runtime_event);
        ServiceControlHandlerResult::NoError
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;
    status_handle.set_service_status(ScmServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::StartPending,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 1,
        wait_hint: Duration::from_secs(45),
        process_id: None,
    })?;
    let data_root = program_data_root();
    let _data_root_guard = updater::prepare_service_data_root(&data_root)
        .map_err(|error| windows_service::Error::Winapi(io::Error::other(error)))?;
    let config_path = config_path(arguments);
    let credential_store = credential_store();
    let local_for_management = LocalConfigStore::new(&config_path).load().ok();
    let loaded_policy = local_for_management.as_ref().and_then(|local| {
        match management::load_best_policy(&config_path, local, credential_store.as_ref()) {
            Ok(policy) => policy,
            Err(error) => {
                eprintln!(
                    "managed policy is unavailable; continuing with local configuration: {error}"
                );
                None
            }
        }
    });
    let runtime = match ServiceRuntime::load_with_credentials_and_policy(
        &config_path,
        event_tx.clone(),
        credential_store.clone(),
        loaded_policy.as_ref().map(|loaded| &loaded.policy),
    ) {
        Ok(runtime) => Arc::new(runtime),
        Err(error) => {
            eprintln!(
                "could not load configuration from {}: {error}",
                config_path.display()
            );
            Arc::new(ServiceRuntime::configuration_error(
                &config_path,
                event_tx.clone(),
                credential_store.clone(),
            ))
        }
    };
    runtime.record_diagnostic(
        DiagnosticLevel::Information,
        "service.started",
        "The resticpal service started.",
        None,
    );
    match staged_bootstrap_url() {
        Ok(Some(url)) => match runtime.enroll_bootstrap(&url) {
            Ok(()) => {
                if let Err(error) = clear_staged_bootstrap_url() {
                    eprintln!(
                        "managed enrollment succeeded but its staged URL could not be removed: {error}"
                    );
                }
            }
            Err(error) => eprintln!(
                "staged managed enrollment failed; the URL remains available for a later service start: {error}"
            ),
        },
        Ok(None) => {}
        Err(error) => eprintln!("could not read the installer-staged bootstrap URL: {error}"),
    }
    start_ipc_server(Arc::clone(&runtime));
    start_management_worker(
        Arc::clone(&runtime),
        config_path.clone(),
        credential_store.clone(),
    );
    let executor = ResticExecutor::new(
        restic_path(),
        secret_resolver(credential_store),
        Arc::new(SystemWakeLockProvider),
    );

    status_handle.set_service_status(ScmServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP
            | ServiceControlAccept::SHUTDOWN
            | ServiceControlAccept::POWER_EVENT
            | ServiceControlAccept::TIME_CHANGE,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::ZERO,
        process_id: None,
    })?;

    run_event_loop(
        &event_rx,
        Arc::clone(&runtime),
        executor,
        event_tx,
        &status_handle,
    );

    runtime.record_diagnostic(
        DiagnosticLevel::Information,
        "service.stopped",
        "The resticpal service stopped.",
        None,
    );

    status_handle.set_service_status(ScmServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::ZERO,
        process_id: None,
    })?;

    Ok(())
}

fn start_management_worker(
    runtime: Arc<ServiceRuntime>,
    config_path: PathBuf,
    credential_store: Option<DpapiSecretStore>,
) {
    thread::Builder::new()
        .name("resticpal-management".to_owned())
        .spawn(move || {
            let client = management::ManagementClient::new();
            let config_store = LocalConfigStore::new(&config_path);
            let mut sequence = 0;
            let mut source_identity = None;
            let mut next_refresh = Instant::now();
            let mut refresh_failures = 0_u32;
            let mut next_report = Instant::now();
            let mut report_failures = 0_u32;
            let mut last_observed_state: Option<BackupState> = None;

            loop {
                let local = match config_store.load() {
                    Ok(local) => local,
                    Err(error) => {
                        eprintln!("managed configuration reload failed: {error}");
                        thread::sleep(Duration::from_secs(30));
                        continue;
                    }
                };
                if local.management.mode == resticpal_core::config::ManagementMode::Disabled {
                    source_identity = None;
                    sequence = 0;
                    refresh_failures = 0;
                    last_observed_state = None;
                    thread::sleep(Duration::from_secs(30));
                    continue;
                }
                let current_identity = local.management.clone();
                let now = Instant::now();
                if source_identity.as_ref() != Some(&current_identity) {
                    refresh_failures = 0;
                    let outcome = match management::load_best_policy(
                        &config_path,
                        &local,
                        credential_store.as_ref(),
                    ) {
                        Ok(Some(loaded)) => match runtime
                            .apply_managed_policy_if_current(&loaded.policy, &local.management)
                        {
                            Ok(ManagedPolicyApplyOutcome::Applied) => {
                                sequence = loaded.sequence;
                                ManagementRefreshOutcome::Synchronized
                            }
                            Ok(ManagedPolicyApplyOutcome::RuntimeBusy) => {
                                ManagementRefreshOutcome::RuntimeBusy
                            }
                            Ok(ManagedPolicyApplyOutcome::SourceChanged) => {
                                ManagementRefreshOutcome::Synchronized
                            }
                            Err(error) => {
                                eprintln!("managed source activation failed: {error}");
                                ManagementRefreshOutcome::Failed
                            }
                        },
                        Ok(None) => {
                            sequence = 0;
                            ManagementRefreshOutcome::Synchronized
                        }
                        Err(error) => {
                            eprintln!("managed source activation failed: {error}");
                            ManagementRefreshOutcome::Failed
                        }
                    };
                    refresh_failures = match outcome {
                        ManagementRefreshOutcome::Failed => refresh_failures.saturating_add(1),
                        _ => 0,
                    };
                    source_identity = Some(current_identity);
                    next_refresh = now
                        + management_refresh_delay(
                            local.management.refresh_interval_minutes(),
                            outcome,
                            refresh_failures,
                        );
                    next_report = now;
                    report_failures = 0;
                    last_observed_state = None;
                }

                if now >= next_refresh {
                    let outcome = match management::refresh_policy(
                        &client,
                        &config_path,
                        &local,
                        sequence,
                        credential_store.as_ref(),
                    ) {
                        Ok(loaded) => {
                            if loaded.sequence > sequence {
                                match runtime.apply_managed_policy_if_current(
                                    &loaded.policy,
                                    &local.management,
                                ) {
                                    Ok(ManagedPolicyApplyOutcome::Applied) => {
                                        sequence = loaded.sequence;
                                        ManagementRefreshOutcome::Synchronized
                                    }
                                    Ok(ManagedPolicyApplyOutcome::RuntimeBusy) => {
                                        ManagementRefreshOutcome::RuntimeBusy
                                    }
                                    Ok(ManagedPolicyApplyOutcome::SourceChanged) => {
                                        ManagementRefreshOutcome::Synchronized
                                    }
                                    Err(error) => {
                                        eprintln!("managed policy could not be applied: {error}");
                                        ManagementRefreshOutcome::Failed
                                    }
                                }
                            } else {
                                ManagementRefreshOutcome::Synchronized
                            }
                        }
                        Err(error) => {
                            eprintln!("managed policy refresh failed: {error}");
                            ManagementRefreshOutcome::Failed
                        }
                    };
                    refresh_failures = match outcome {
                        ManagementRefreshOutcome::Failed => refresh_failures.saturating_add(1),
                        _ => 0,
                    };
                    next_refresh = now
                        + management_refresh_delay(
                            local.management.refresh_interval_minutes(),
                            outcome,
                            refresh_failures,
                        );
                }

                if local.management.status_url.is_some() {
                    let status = runtime.status();
                    let state_changed = last_observed_state
                        .as_ref()
                        .is_none_or(|previous| previous != &status.state);
                    if state_changed {
                        next_report = now;
                        last_observed_state = Some(status.state.clone());
                    }
                    if now >= next_report {
                        let result = credential_store.as_ref().map_or_else(
                            || Err("protected credential store is unavailable".to_owned()),
                            |store| {
                                client
                                    .report_status(&local.management, store, status.clone())
                                    .map_err(|error| error.to_string())
                            },
                        );
                        match result {
                            Ok(()) => {
                                report_failures = 0;
                                let cadence = if matches!(status.state, BackupState::Running { .. })
                                {
                                    Duration::from_secs(5 * 60)
                                } else {
                                    Duration::from_secs(6 * 60 * 60)
                                };
                                next_report = now + cadence;
                            }
                            Err(error) => {
                                eprintln!("managed status delivery failed: {error}");
                                report_failures = report_failures.saturating_add(1);
                                let exponent = report_failures.saturating_sub(1).min(6);
                                next_report = now
                                    + Duration::from_secs(60_u64.saturating_mul(1_u64 << exponent));
                            }
                        }
                    }
                }

                thread::sleep(Duration::from_secs(30));
            }
        })
        .expect("the service must be able to create its management thread");
}

fn start_ipc_server(runtime: Arc<ServiceRuntime>) {
    thread::Builder::new()
        .name("resticpal-ipc".to_owned())
        .spawn(move || {
            let server = match NamedPipeServer::new(DEFAULT_PIPE_NAME) {
                Ok(server) => server,
                Err(error) => {
                    eprintln!("could not initialize resticpal IPC: {error}");
                    return;
                }
            };

            loop {
                if let Err(error) =
                    server.serve_one(|request, identity| runtime.handle_request(request, identity))
                {
                    eprintln!("resticpal IPC client failed: {error}");
                    thread::sleep(Duration::from_millis(250));
                }
            }
        })
        .expect("the service must be able to create its IPC thread");
}

fn run_event_loop(
    events: &Receiver<RuntimeEvent>,
    runtime: Arc<ServiceRuntime>,
    executor: ResticExecutor,
    event_sender: mpsc::Sender<RuntimeEvent>,
    status_handle: &ServiceStatusHandle,
) {
    let mut active_backup: Option<CancellationToken> = None;
    let mut active_repository_operation: Option<CancellationToken> = None;
    evaluate_and_maybe_start(&runtime, &executor, &event_sender, &mut active_backup);
    loop {
        let delay = runtime.next_evaluation_delay(Utc::now());
        match events.recv_timeout(delay) {
            Ok(RuntimeEvent::Stop) | Err(RecvTimeoutError::Disconnected) => {
                if let Err(error) = status_handle.set_service_status(ScmServiceStatus {
                    service_type: SERVICE_TYPE,
                    current_state: ServiceState::StopPending,
                    controls_accepted: ServiceControlAccept::empty(),
                    exit_code: ServiceExitCode::Win32(0),
                    checkpoint: 1,
                    wait_hint: SHUTDOWN_DRAIN_TIMEOUT,
                    process_id: None,
                }) {
                    eprintln!("could not report pending service shutdown: {error}");
                }
                if let Some(cancellation) = &active_backup {
                    cancellation.cancel();
                }
                if let Some(cancellation) = &active_repository_operation {
                    cancellation.cancel();
                }
                drain_cancelled_operations(
                    events,
                    &runtime,
                    &mut active_backup,
                    &mut active_repository_operation,
                );
                break;
            }
            Ok(RuntimeEvent::RunNow) => {
                evaluate_and_maybe_start(&runtime, &executor, &event_sender, &mut active_backup);
            }
            Ok(RuntimeEvent::Cancel) => {
                if let Some(cancellation) = &active_backup {
                    cancellation.cancel();
                }
            }
            Ok(RuntimeEvent::BackupFinished(outcome)) => {
                runtime.finish_backup(&outcome);
                active_backup = None;
            }
            Ok(RuntimeEvent::RepositoryOperationRequested(operation)) => {
                if active_backup.is_some() || active_repository_operation.is_some() {
                    runtime.finish_repository_operation(
                        operation,
                        &RepositoryOutcome::failed("repository_operation_conflict"),
                    );
                    continue;
                }
                let cancellation = CancellationToken::default();
                active_repository_operation = Some(cancellation.clone());
                if start_repository_worker(
                    Arc::clone(&runtime),
                    executor.clone(),
                    operation,
                    cancellation,
                    event_sender.clone(),
                )
                .is_err()
                {
                    runtime.finish_repository_operation(
                        operation,
                        &RepositoryOutcome::failed("executor_start_failed"),
                    );
                    active_repository_operation = None;
                }
            }
            Ok(RuntimeEvent::RepositoryOperationFinished { operation, outcome }) => {
                runtime.finish_repository_operation(operation, &outcome);
                active_repository_operation = None;
                evaluate_and_maybe_start(&runtime, &executor, &event_sender, &mut active_backup);
            }
            Ok(RuntimeEvent::UpdateInstallRequested(package)) => {
                if active_backup.is_some() || active_repository_operation.is_some() {
                    runtime.finish_update_install(&UpdateInstallOutcome::Failed {
                        code: "update_operation_conflict",
                    });
                    continue;
                }
                if start_update_worker(Arc::clone(&runtime), package, event_sender.clone()).is_err()
                {
                    runtime.finish_update_install(&UpdateInstallOutcome::Failed {
                        code: "update_worker_start_failed",
                    });
                }
            }
            Ok(RuntimeEvent::UpdateInstallFinished(outcome)) => {
                runtime.finish_update_install(&outcome);
                evaluate_and_maybe_start(&runtime, &executor, &event_sender, &mut active_backup);
            }
            Ok(RuntimeEvent::Resume) => {
                runtime.record_resume(Utc::now());
                evaluate_and_maybe_start(&runtime, &executor, &event_sender, &mut active_backup);
            }
            Ok(
                RuntimeEvent::PowerStatusChanged
                | RuntimeEvent::TimeChanged
                | RuntimeEvent::Deferred
                | RuntimeEvent::ConfigurationChanged,
            )
            | Err(RecvTimeoutError::Timeout) => {
                evaluate_and_maybe_start(&runtime, &executor, &event_sender, &mut active_backup);
            }
        }
    }
}

fn drain_cancelled_operations(
    events: &Receiver<RuntimeEvent>,
    runtime: &ServiceRuntime,
    active_backup: &mut Option<CancellationToken>,
    active_repository_operation: &mut Option<CancellationToken>,
) {
    let deadline = Instant::now() + SHUTDOWN_DRAIN_TIMEOUT;
    while (active_backup.is_some() || active_repository_operation.is_some())
        && Instant::now() < deadline
    {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(RuntimeEvent::BackupFinished(outcome)) => {
                runtime.finish_backup(&outcome);
                *active_backup = None;
            }
            Ok(RuntimeEvent::RepositoryOperationFinished { operation, outcome }) => {
                runtime.finish_repository_operation(operation, &outcome);
                *active_repository_operation = None;
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn evaluate_and_maybe_start(
    runtime: &Arc<ServiceRuntime>,
    executor: &ResticExecutor,
    event_sender: &mpsc::Sender<RuntimeEvent>,
    active_backup: &mut Option<CancellationToken>,
) {
    if active_backup.is_some() {
        return;
    }

    if !matches!(
        runtime.evaluate_schedule(Utc::now(), current_conditions()),
        ScheduleAction::Start { .. }
    ) {
        return;
    }

    let cancellation = CancellationToken::default();
    *active_backup = Some(cancellation.clone());
    if start_backup_worker(
        Arc::clone(runtime),
        executor.clone(),
        cancellation,
        event_sender.clone(),
    )
    .is_err()
    {
        runtime.finish_backup(&BackupOutcome::failed("executor_start_failed"));
        *active_backup = None;
    }
}

fn current_conditions() -> SystemConditions {
    static WARNED: AtomicBool = AtomicBool::new(false);
    match SystemConditions::query() {
        Ok(conditions) => conditions,
        Err(error) => {
            if !WARNED.swap(true, Ordering::Relaxed) {
                eprintln!("could not query power or network conditions: {error}");
            }
            SystemConditions::conservative()
        }
    }
}

fn start_backup_worker(
    runtime: Arc<ServiceRuntime>,
    executor: ResticExecutor,
    cancellation: CancellationToken,
    events: mpsc::Sender<RuntimeEvent>,
) -> std::io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("resticpal-backup".to_owned())
        .spawn(move || {
            let config = runtime.config();
            runtime.record_diagnostic(
                DiagnosticLevel::Information,
                "backup.started",
                "Backup started.",
                None,
            );
            let mut outcome = executor.backup(&config, &cancellation, |progress| {
                runtime.update_progress(progress);
            });
            if matches!(
                &outcome.kind,
                executor::BackupOutcomeKind::Succeeded
                    | executor::BackupOutcomeKind::SucceededWithWarnings
            ) && let Some(plan) = runtime.begin_retention(Utc::now())
            {
                let retention = executor.retention(&config, plan.prune_due, &cancellation);
                outcome = runtime.finish_retention(outcome, &retention, Utc::now());
            }
            let _ = events.send(RuntimeEvent::BackupFinished(outcome));
        })
}

fn start_repository_worker(
    runtime: Arc<ServiceRuntime>,
    executor: ResticExecutor,
    operation: RepositoryOperationKind,
    cancellation: CancellationToken,
    events: mpsc::Sender<RuntimeEvent>,
) -> std::io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("resticpal-repository".to_owned())
        .spawn(move || {
            let config = runtime.config();
            let restic_operation = match operation {
                RepositoryOperationKind::Validate => ResticOperation::Probe,
                RepositoryOperationKind::Initialize => ResticOperation::Initialize,
            };
            let outcome = executor.repository_operation(&config, restic_operation, &cancellation);
            let _ = events.send(RuntimeEvent::RepositoryOperationFinished { operation, outcome });
        })
}

fn start_update_worker(
    runtime: Arc<ServiceRuntime>,
    package: UpdatePackage,
    events: mpsc::Sender<RuntimeEvent>,
) -> std::io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("resticpal-update".to_owned())
        .spawn(move || {
            runtime.record_diagnostic(
                DiagnosticLevel::Information,
                "update.started",
                "A signed resticpal update started downloading.",
                None,
            );
            let outcome = updater::install(&package, &program_data_root());
            let _ = events.send(RuntimeEvent::UpdateInstallFinished(outcome));
        })
}

fn config_path(arguments: &[OsString]) -> PathBuf {
    arguments
        .windows(2)
        .find_map(|pair| (pair[0] == OsStr::new("--config")).then(|| PathBuf::from(&pair[1])))
        .unwrap_or_else(default_config_path)
}

fn staged_bootstrap_url() -> Result<Option<Zeroizing<String>>, io::Error> {
    let mut byte_count = 0_u32;
    // SAFETY: the registry paths are fixed strings and this call requests only
    // the required buffer size.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\resticpal"),
            w!("BootstrapUrl"),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&raw mut byte_count),
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status.0 as i32));
    }
    if !(2..=64 * 1024).contains(&byte_count) || !byte_count.is_multiple_of(2) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "staged bootstrap URL has an invalid registry size",
        ));
    }
    let mut buffer = Zeroizing::new(vec![0_u16; byte_count as usize / 2]);
    // SAFETY: buffer is writable for the byte count returned by the size query.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\resticpal"),
            w!("BootstrapUrl"),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&raw mut byte_count),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status.0 as i32));
    }
    let length = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    let value = String::from_utf16(&buffer[..length])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bootstrap URL is not UTF-16"))?;
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Zeroizing::new(value)))
    }
}

fn clear_staged_bootstrap_url() -> Result<(), io::Error> {
    // SAFETY: the registry paths are fixed null-terminated strings.
    let status = unsafe {
        RegDeleteKeyValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\resticpal"),
            w!("BootstrapUrl"),
        )
    };
    if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status.0 as i32))
    }
}

fn default_config_path() -> PathBuf {
    program_data_root().join("config.toml")
}

fn credential_store_path() -> PathBuf {
    program_data_root().join("Credentials")
}

fn program_data_root() -> PathBuf {
    env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("ResticPal")
}

fn record_service_startup_error(message: &str) {
    let data_root = program_data_root();
    let Ok(_data_root_guard) = updater::prepare_service_data_root(&data_root) else {
        return;
    };
    let path = data_root.join("service-startup-errors.log");
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = writeln!(file, "{} {message}", Utc::now().to_rfc3339());
}

fn credential_store() -> Option<DpapiSecretStore> {
    match DpapiSecretStore::open(credential_store_path()) {
        Ok(store) => Some(store),
        Err(error) => {
            eprintln!("could not initialize the credential store: {error}");
            None
        }
    }
}

fn secret_resolver(store: Option<DpapiSecretStore>) -> Arc<dyn SecretResolver> {
    store.map_or_else(
        || Arc::new(UnavailableSecretResolver) as Arc<dyn SecretResolver>,
        |store| Arc::new(DpapiSecretResolver::new(store)),
    )
}

fn restic_path() -> PathBuf {
    env::current_exe()
        .map(|mut executable| {
            executable.set_file_name("restic.exe");
            executable
        })
        .unwrap_or_else(|_| PathBuf::from("restic.exe"))
}

#[cfg(test)]
mod management_refresh_tests {
    use super::*;

    #[test]
    fn busy_runtime_retries_policy_without_waiting_for_the_full_interval() {
        assert_eq!(
            management_refresh_delay(15, ManagementRefreshOutcome::RuntimeBusy, 0),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn failed_policy_fetch_backs_off_without_exceeding_the_configured_interval() {
        assert_eq!(
            management_refresh_delay(15, ManagementRefreshOutcome::Failed, 1),
            Duration::from_secs(60)
        );
        assert_eq!(
            management_refresh_delay(15, ManagementRefreshOutcome::Failed, 8),
            Duration::from_secs(15 * 60)
        );
        assert_eq!(
            management_refresh_delay(15, ManagementRefreshOutcome::Synchronized, 0),
            Duration::from_secs(15 * 60)
        );
    }
}
