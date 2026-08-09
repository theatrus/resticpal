mod atomic_file;
mod conditions;
mod config_store;
mod executor;
mod history;
mod power_request;
mod runtime;
mod state;

use std::env;
use std::ffi::{OsStr, OsString};
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
use executor::{
    BackupOutcome, CancellationToken, DpapiSecretResolver, RepositoryOutcome, ResticExecutor,
    SecretResolver, SystemWakeLockProvider, UnavailableSecretResolver,
};
use resticpal_core::restic::ResticOperation;
use resticpal_protocol::RepositoryOperationKind;
use resticpal_windows::credentials::DpapiSecretStore;
use resticpal_windows::named_pipe::{DEFAULT_PIPE_NAME, NamedPipeServer};
use runtime::{RuntimeEvent, ScheduleAction, ServiceRuntime};
use windows_service::define_windows_service;
use windows_service::service::{
    PowerEventParam, ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState,
    ServiceStatus as ScmServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::{Result as ServiceResult, service_dispatcher};

const SERVICE_NAME: &str = "ResticPal";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

fn main() -> ExitCode {
    let arguments: Vec<OsString> = env::args_os().skip(1).collect();
    if arguments.iter().any(|argument| argument == "--console") {
        return console_smoke_test(&arguments);
    }

    match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("could not start resticpal through the service dispatcher: {error}");
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
    // A real logging sink will replace stderr before service installation is
    // enabled. The SCM does not provide an interactive console.
    if let Err(error) = run_service(&arguments) {
        eprintln!("resticpal service failed: {error}");
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
    let config_path = config_path(arguments);
    let credential_store = credential_store();
    let runtime = match ServiceRuntime::load_with_credentials(
        &config_path,
        event_tx.clone(),
        credential_store.clone(),
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
    start_ipc_server(Arc::clone(&runtime));
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
            let outcome = executor.backup(&config, &cancellation, |progress| {
                runtime.update_progress(progress);
            });
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

fn config_path(arguments: &[OsString]) -> PathBuf {
    arguments
        .windows(2)
        .find_map(|pair| (pair[0] == OsStr::new("--config")).then(|| PathBuf::from(&pair[1])))
        .unwrap_or_else(default_config_path)
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
