mod executor;
mod power_request;
mod runtime;

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use executor::{
    BackupOutcome, CancellationToken, ResticExecutor, SystemWakeLockProvider,
    UnavailableSecretResolver,
};
use resticpal_windows::named_pipe::{DEFAULT_PIPE_NAME, NamedPipeServer};
use runtime::{RuntimeEvent, ServiceRuntime};
use windows_service::define_windows_service;
use windows_service::service::{
    PowerEventParam, ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState,
    ServiceStatus as ScmServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{Result as ServiceResult, service_dispatcher};

const SERVICE_NAME: &str = "ResticPal";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

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
    let runtime = match ServiceRuntime::load(&config_path, event_tx.clone()) {
        Ok(runtime) => Arc::new(runtime),
        Err(error) => {
            eprintln!(
                "could not load configuration from {}: {error}",
                config_path.display()
            );
            Arc::new(ServiceRuntime::configuration_error(event_tx.clone()))
        }
    };
    start_ipc_server(Arc::clone(&runtime));
    let executor = ResticExecutor::new(
        restic_path(),
        Arc::new(UnavailableSecretResolver),
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

    run_event_loop(&event_rx, Arc::clone(&runtime), executor, event_tx);

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
) {
    // The scheduler and backup executor will share this event loop. A long
    // timeout is intentional: control, timer, network, and IPC sources wake it.
    let mut active_backup: Option<CancellationToken> = None;
    loop {
        match events.recv_timeout(Duration::from_secs(60)) {
            Ok(RuntimeEvent::Stop) | Err(RecvTimeoutError::Disconnected) => {
                if let Some(cancellation) = &active_backup {
                    cancellation.cancel();
                }
                break;
            }
            Ok(RuntimeEvent::RunNow) => {
                if active_backup.is_some() {
                    continue;
                }
                let cancellation = CancellationToken::default();
                active_backup = Some(cancellation.clone());
                if start_backup_worker(
                    Arc::clone(&runtime),
                    executor.clone(),
                    cancellation,
                    event_sender.clone(),
                )
                .is_err()
                {
                    runtime.finish_backup(&BackupOutcome::failed("executor_start_failed"));
                    active_backup = None;
                }
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
            Ok(
                RuntimeEvent::Resume
                | RuntimeEvent::PowerStatusChanged
                | RuntimeEvent::TimeChanged
                | RuntimeEvent::Deferred,
            )
            | Err(RecvTimeoutError::Timeout) => {
                // Re-evaluate policy and scheduling as executor state is added.
            }
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

fn config_path(arguments: &[OsString]) -> PathBuf {
    arguments
        .windows(2)
        .find_map(|pair| (pair[0] == OsStr::new("--config")).then(|| PathBuf::from(&pair[1])))
        .unwrap_or_else(default_config_path)
}

fn default_config_path() -> PathBuf {
    env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("ResticPal")
        .join("config.toml")
}

fn restic_path() -> PathBuf {
    env::current_exe()
        .map(|mut executable| {
            executable.set_file_name("restic.exe");
            executable
        })
        .unwrap_or_else(|_| PathBuf::from("restic.exe"))
}
