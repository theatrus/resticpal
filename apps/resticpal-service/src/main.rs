mod power_request;

use std::env;
use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use windows_service::define_windows_service;
use windows_service::service::{
    PowerEventParam, ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState,
    ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{Result as ServiceResult, service_dispatcher};

const SERVICE_NAME: &str = "ResticPal";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeEvent {
    Stop,
    Resume,
    PowerStatusChanged,
    TimeChanged,
}

fn main() -> ExitCode {
    if env::args_os().any(|argument| argument == "--console") {
        return console_smoke_test();
    }

    match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("could not start resticpal through the service dispatcher: {error}");
            ExitCode::FAILURE
        }
    }
}

fn console_smoke_test() -> ExitCode {
    println!("resticpal service runtime initialized in console smoke-test mode");
    println!("service name: {SERVICE_NAME}");
    ExitCode::SUCCESS
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    // A real logging sink will replace stderr before service installation is
    // enabled. The SCM does not provide an interactive console.
    if let Err(error) = run_service() {
        eprintln!("resticpal service failed: {error}");
    }
}

fn run_service() -> ServiceResult<()> {
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
    status_handle.set_service_status(ServiceStatus {
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

    run_event_loop(&event_rx);

    status_handle.set_service_status(ServiceStatus {
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

fn run_event_loop(events: &Receiver<RuntimeEvent>) {
    // The scheduler and IPC server will share this event loop. A long timeout is
    // intentional: wake, power, network, timer, and IPC sources should wake it.
    loop {
        match events.recv_timeout(Duration::from_secs(60)) {
            Ok(RuntimeEvent::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(
                RuntimeEvent::Resume | RuntimeEvent::PowerStatusChanged | RuntimeEvent::TimeChanged,
            )
            | Err(RecvTimeoutError::Timeout) => {
                // Re-evaluate scheduling once the runtime state store exists.
            }
        }
    }
}
