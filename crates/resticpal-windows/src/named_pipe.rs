use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use resticpal_protocol::{
    FrameError, PROTOCOL_VERSION, Request, Response, read_frame, write_frame,
};
use thiserror::Error;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
    ERROR_PIPE_LISTENING, ERROR_PIPE_NOT_CONNECTED, HANDLE, HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    CheckTokenMembership, CreateWellKnownSid, PSECURITY_DESCRIPTOR, PSID, RevertToSelf,
    SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE, WinBuiltinAdministratorsSid,
};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeServerProcessId, ImpersonateNamedPipeClient,
    NAMED_PIPE_MODE, PIPE_NOWAIT, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, SetNamedPipeHandleState,
};
use windows::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, SC_HANDLE,
    SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO, SERVICE_QUERY_STATUS, SERVICE_RUNNING,
    SERVICE_STATUS_PROCESS,
};
use windows::core::{BOOL, Error as WindowsError, HRESULT, PCWSTR, w};

pub const DEFAULT_PIPE_NAME: &str = r"\\.\pipe\ResticPal.v5";
const PIPE_BUFFER_BYTES: u32 = 64 * 1024;
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(20);
const SERVER_IO_TIMEOUT: Duration = Duration::from_secs(5);
const IO_RETRY_DELAY: Duration = Duration::from_millis(10);

/// Identity properties derived by Windows from the connected pipe client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientIdentity {
    pub is_elevated_administrator: bool,
}

pub struct NamedPipeServer {
    name: Vec<u16>,
    security: OwnedSecurityDescriptor,
    first_instance: AtomicBool,
    pending_instance: Mutex<Option<PipeConnection>>,
}

impl NamedPipeServer {
    pub fn new(name: &str) -> Result<Self, NamedPipeError> {
        if name.is_empty() || name.encode_utf16().any(|unit| unit == 0) {
            return Err(NamedPipeError::InvalidPipeName);
        }

        Ok(Self {
            name: wide_null(name),
            security: OwnedSecurityDescriptor::new()?,
            first_instance: AtomicBool::new(true),
            pending_instance: Mutex::new(None),
        })
    }

    /// Accepts and handles one request on a new pipe instance.
    ///
    /// Keeping framing to one request/response per connection bounds resources
    /// and makes malformed-client recovery trivial. A later status subscription
    /// endpoint can use a dedicated long-lived connection.
    pub fn serve_one(
        &self,
        handler: impl FnOnce(Request, ClientIdentity) -> Response,
    ) -> Result<(), NamedPipeError> {
        let mut connection = self.accept()?;
        let request: Request = read_frame(&mut connection)?;
        connection.reset_deadline();
        // Windows requires the server to read at least one byte from the pipe
        // before it can impersonate and inspect the authenticated client token.
        let identity = connection.client_identity()?;
        let response = if request.protocol_version == PROTOCOL_VERSION {
            handler(request, identity)
        } else {
            Response::incompatible(request.request_id, request.protocol_version)
        };
        connection.reset_deadline();
        write_frame(&mut connection, &response)?;
        connection.wait_for_response_consumption()?;
        Ok(())
    }

    fn accept(&self) -> Result<PipeConnection, NamedPipeError> {
        let mut pending = self
            .pending_instance
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut connection = match pending.take() {
            Some(connection) => connection,
            None => {
                // Recovering from an unexpected poisoned/empty state must
                // never attach a later instance to an attacker's new pipe.
                self.first_instance.store(true, Ordering::Release);
                self.create_instance()?
            }
        };

        // SAFETY: connection owns a valid server-side named-pipe handle. A
        // client may already have connected to the precreated successor.
        match unsafe { ConnectNamedPipe(connection.handle, None) } {
            Ok(()) => {}
            Err(error) if error.code() == HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) => {}
            Err(error) => {
                // Broken clients must not create a gap in secure ownership.
                // Replace their unusable instance while its handle remains
                // live; if replacement fails, retain that handle and retry.
                *pending = Some(match self.create_instance() {
                    Ok(successor) => successor,
                    Err(_) => connection,
                });
                return Err(error.into());
            }
        }

        // Reserve the next service-owned name BEFORE servicing or releasing
        // the current connection, including malformed/disconnected clients.
        match self.create_instance() {
            Ok(successor) => *pending = Some(successor),
            Err(error) => {
                // Fail closed without ever releasing the last trusted handle.
                *pending = Some(connection);
                return Err(error);
            }
        }
        drop(pending);

        let mode = NAMED_PIPE_MODE(PIPE_READMODE_BYTE.0 | PIPE_NOWAIT.0);
        // SAFETY: connection owns a connected named-pipe handle and mode is
        // live for the duration of this synchronous call.
        unsafe { SetNamedPipeHandleState(connection.handle, Some(&raw const mode), None, None) }?;
        connection.io_deadline = Instant::now() + SERVER_IO_TIMEOUT;
        Ok(connection)
    }

    fn create_instance(&self) -> Result<PipeConnection, NamedPipeError> {
        let attributes = self.security.attributes();
        // FIRST remains armed until CreateNamedPipeW actually succeeds, so a
        // failed squat check cannot make the next retry join the attacker.
        let first_instance = self.first_instance.load(Ordering::Acquire);
        let open_mode = if first_instance {
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            PIPE_ACCESS_DUPLEX
        };
        // SAFETY: the pipe name and security descriptor remain valid for the
        // lifetime of the created handle. Remote clients are explicitly rejected.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(self.name.as_ptr()),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                PIPE_BUFFER_BYTES,
                PIPE_BUFFER_BYTES,
                0,
                Some(&raw const attributes),
            )
        };
        if handle.is_invalid() {
            return Err(WindowsError::from_thread().into());
        }
        if first_instance {
            self.first_instance.store(false, Ordering::Release);
        }
        Ok(PipeConnection {
            handle,
            io_deadline: Instant::now(),
        })
    }
}

pub struct NamedPipeClient;

impl NamedPipeClient {
    pub fn request(request: &Request) -> Result<Response, NamedPipeError> {
        Self::request_at(DEFAULT_PIPE_NAME, request, DEFAULT_CONNECT_TIMEOUT)
    }

    pub fn request_at(
        name: &str,
        request: &Request,
        timeout: Duration,
    ) -> Result<Response, NamedPipeError> {
        let deadline = Instant::now() + timeout;
        let connection = loop {
            match OpenOptions::new().read(true).write(true).open(name) {
                Ok(connection) => break connection,
                Err(error) if Instant::now() < deadline => {
                    if !pipe_connect_error_is_retryable(&error) {
                        return Err(error.into());
                    }
                    thread::sleep(CONNECT_RETRY_DELAY);
                }
                Err(error) => return Err(error.into()),
            }
        };
        if name == DEFAULT_PIPE_NAME {
            authenticate_service_pipe(HANDLE(connection.as_raw_handle()))?;
        }
        let mode = NAMED_PIPE_MODE(PIPE_READMODE_BYTE.0 | PIPE_NOWAIT.0);
        // SAFETY: the file owns a connected named-pipe handle and mode is live
        // for the duration of this synchronous call.
        unsafe {
            SetNamedPipeHandleState(
                HANDLE(connection.as_raw_handle()),
                Some(&raw const mode),
                None,
                None,
            )
        }?;
        let mut connection = TimedClientConnection {
            file: connection,
            io_deadline: deadline,
        };

        write_frame(&mut connection, request)?;
        let response: Response = read_frame(&mut connection)?;
        if response.protocol_version != PROTOCOL_VERSION {
            return Err(NamedPipeError::IncompatibleResponseProtocol {
                expected: PROTOCOL_VERSION,
                actual: response.protocol_version,
            });
        }
        if response.request_id != request.request_id {
            return Err(NamedPipeError::MismatchedResponse {
                expected: request.request_id,
                actual: response.request_id,
            });
        }
        // Acknowledging after the complete frame lets the server close without
        // FlushFileBuffers, whose semantics otherwise permit a client to block
        // the service thread indefinitely.
        let _ = connection.write_all(&[0]);
        Ok(response)
    }
}

fn authenticate_service_pipe(connection: HANDLE) -> Result<(), NamedPipeError> {
    let mut server_pid = 0_u32;
    // SAFETY: connection is a live client pipe and the PID output is writable.
    unsafe { GetNamedPipeServerProcessId(connection, &raw mut server_pid) }
        .map_err(NamedPipeError::ServiceIdentityUnavailable)?;

    // Interactive users need only the SCM connection and SERVICE_QUERY_STATUS
    // rights; no service-control or elevation capability is requested.
    let manager = OwnedServiceHandle(
        unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) }
            .map_err(NamedPipeError::ServiceIdentityUnavailable)?,
    );
    let service = OwnedServiceHandle(
        unsafe { OpenServiceW(manager.0, w!("ResticPal"), SERVICE_QUERY_STATUS) }
            .map_err(NamedPipeError::ServiceIdentityUnavailable)?,
    );
    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut bytes_needed = 0_u32;
    // SAFETY: status is correctly aligned, initialized, and writable for its
    // full SERVICE_STATUS_PROCESS size throughout the synchronous query.
    let buffer = unsafe {
        std::slice::from_raw_parts_mut(
            (&raw mut status).cast::<u8>(),
            size_of::<SERVICE_STATUS_PROCESS>(),
        )
    };
    unsafe {
        QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            Some(buffer),
            &raw mut bytes_needed,
        )
    }
    .map_err(NamedPipeError::ServiceIdentityUnavailable)?;
    if !service_pipe_identity_matches(server_pid, &status) {
        return Err(NamedPipeError::UntrustedServicePipe);
    }
    Ok(())
}

fn service_pipe_identity_matches(server_pid: u32, status: &SERVICE_STATUS_PROCESS) -> bool {
    server_pid != 0
        && status.dwCurrentState == SERVICE_RUNNING
        && status.dwProcessId != 0
        && server_pid == status.dwProcessId
}

struct OwnedServiceHandle(SC_HANDLE);

impl Drop for OwnedServiceHandle {
    fn drop(&mut self) {
        // SAFETY: the SCM/service handle is exclusively owned by this guard.
        let _ = unsafe { CloseServiceHandle(self.0) };
    }
}

fn pipe_connect_error_is_retryable(error: &io::Error) -> bool {
    // CreateFile returns ERROR_PIPE_BUSY while the service consumes the
    // previous response acknowledgement and creates its next pipe instance.
    // Rust does not classify that Win32 code as WouldBlock.
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied | io::ErrorKind::WouldBlock
    ) || error.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32)
}

struct TimedClientConnection {
    file: File,
    io_deadline: Instant,
}

impl TimedClientConnection {
    fn wait_until_deadline(&self) -> io::Result<()> {
        if Instant::now() >= self.io_deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "named-pipe service did not complete its response in time",
            ));
        }
        thread::sleep(IO_RETRY_DELAY);
        Ok(())
    }

    fn wait_for_io(&self, error: &io::Error) -> io::Result<()> {
        if !matches!(
            error.raw_os_error(),
            Some(code) if code == ERROR_NO_DATA.0 as i32 || code == ERROR_PIPE_LISTENING.0 as i32
        ) {
            return Err(io::Error::new(error.kind(), error.to_string()));
        }
        self.wait_until_deadline()
    }
}

impl Read for TimedClientConnection {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.file.read(buffer) {
                Ok(0) => self.wait_until_deadline()?,
                Ok(read) => return Ok(read),
                Err(error) => self.wait_for_io(&error)?,
            }
        }
    }
}

impl Write for TimedClientConnection {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        loop {
            match self.file.write(buffer) {
                Ok(0) => self.wait_until_deadline()?,
                Ok(written) => return Ok(written),
                Err(error) => self.wait_for_io(&error)?,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct PipeConnection {
    handle: HANDLE,
    io_deadline: Instant,
}

impl PipeConnection {
    fn reset_deadline(&mut self) {
        self.io_deadline = Instant::now() + SERVER_IO_TIMEOUT;
    }

    fn wait_until_deadline(&self) -> io::Result<()> {
        if Instant::now() >= self.io_deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "named-pipe client did not complete its I/O in time",
            ));
        }
        thread::sleep(IO_RETRY_DELAY);
        Ok(())
    }

    fn client_identity(&self) -> Result<ClientIdentity, NamedPipeError> {
        // SAFETY: handle is a connected server pipe. The guard always reverts
        // this thread before it handles another client.
        unsafe { ImpersonateNamedPipeClient(self.handle) }?;
        let guard = ImpersonationGuard { active: true };
        let identity = (|| {
            let mut sid_buffer = [0_u8; SECURITY_MAX_SID_SIZE as usize];
            let mut sid_size = SECURITY_MAX_SID_SIZE;
            let administrator_sid = PSID(sid_buffer.as_mut_ptr().cast::<c_void>());
            // SAFETY: sid_buffer is SECURITY_MAX_SID_SIZE bytes and remains live.
            unsafe {
                CreateWellKnownSid(
                    WinBuiltinAdministratorsSid,
                    None,
                    Some(administrator_sid),
                    &raw mut sid_size,
                )
            }?;

            let mut is_member = BOOL::default();
            // SAFETY: None checks the current impersonation token. SID and output
            // pointers remain valid through the call.
            unsafe { CheckTokenMembership(None, administrator_sid, &raw mut is_member) }?;
            Ok(ClientIdentity {
                is_elevated_administrator: is_member.as_bool(),
            })
        })();
        guard.revert()?;
        identity
    }

    fn wait_for_io(&self, error: &WindowsError) -> io::Result<()> {
        let code = error.code();
        if code != HRESULT::from_win32(ERROR_NO_DATA.0)
            && code != HRESULT::from_win32(ERROR_PIPE_LISTENING.0)
        {
            return Err(io::Error::other(error.clone()));
        }
        self.wait_until_deadline()
    }

    fn wait_for_response_consumption(&mut self) -> io::Result<()> {
        let mut acknowledgement = [0_u8; 1];
        loop {
            let mut bytes_read = 0_u32;
            // SAFETY: handle is live and acknowledgement is writable.
            match unsafe {
                ReadFile(
                    self.handle,
                    Some(&mut acknowledgement),
                    Some(&raw mut bytes_read),
                    None,
                )
            } {
                Ok(()) if bytes_read > 0 => return Ok(()),
                Ok(()) => self.wait_until_deadline()?,
                Err(error)
                    if error.code() == HRESULT::from_win32(ERROR_BROKEN_PIPE.0)
                        || error.code() == HRESULT::from_win32(ERROR_PIPE_NOT_CONNECTED.0) =>
                {
                    return Ok(());
                }
                Err(error) => self.wait_for_io(&error)?,
            }
        }
    }
}

impl Read for PipeConnection {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut bytes_read = 0_u32;
        loop {
            // SAFETY: handle is live and buffer is writable for its full length.
            match unsafe { ReadFile(self.handle, Some(buffer), Some(&raw mut bytes_read), None) } {
                Ok(()) if bytes_read == 0 => self.wait_until_deadline()?,
                Ok(()) => return Ok(bytes_read as usize),
                Err(error) => self.wait_for_io(&error)?,
            }
        }
    }
}

impl Write for PipeConnection {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut bytes_written = 0_u32;
        loop {
            // SAFETY: handle is live and buffer is readable for its full length.
            match unsafe {
                WriteFile(
                    self.handle,
                    Some(buffer),
                    Some(&raw mut bytes_written),
                    None,
                )
            } {
                Ok(()) if bytes_written == 0 => self.wait_until_deadline()?,
                Ok(()) => return Ok(bytes_written as usize),
                Err(error) => self.wait_for_io(&error)?,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        // FlushFileBuffers waits indefinitely for the client to consume every
        // response byte. Writes are already synchronous, and closing the server
        // handle preserves buffered bytes for the client without allowing a
        // connected client to stall the single request loop.
        Ok(())
    }
}

impl Drop for PipeConnection {
    fn drop(&mut self) {
        // SAFETY: this type exclusively owns the server pipe handle. Closing
        // leaves already-written response bytes readable by the client.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

struct ImpersonationGuard {
    active: bool,
}

impl ImpersonationGuard {
    fn revert(mut self) -> Result<(), WindowsError> {
        // SAFETY: the guard is constructed only after successful impersonation.
        unsafe { RevertToSelf() }?;
        self.active = false;
        Ok(())
    }
}

impl Drop for ImpersonationGuard {
    fn drop(&mut self) {
        if self.active {
            // SAFETY: an active guard follows successful named-pipe impersonation.
            let _ = unsafe { RevertToSelf() };
        }
    }
}

struct OwnedSecurityDescriptor {
    descriptor: PSECURITY_DESCRIPTOR,
}

impl OwnedSecurityDescriptor {
    fn new() -> Result<Self, WindowsError> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // Protected DACL: LocalSystem and elevated administrators have full
        // access; interactive users can read and write requests. The service
        // performs operation-level authorization after connection.
        //
        // Interactive users are granted 0x12019B rather than GENERIC_READ |
        // GENERIC_WRITE. The pipe generic-write mapping folds in bit 0x4
        // (FILE_CREATE_PIPE_INSTANCE), so `GW` would let any local user stand up
        // a rogue instance of this pipe and intercept the elevated UI's
        // secret-bearing requests. The explicit mask keeps read/write data,
        // read/write attributes, read control, and synchronize while withholding
        // create-instance, which stays limited to SYSTEM, administrators, and
        // the pipe's actual owner. Owner Rights receives only the exact 0x4
        // create-instance bit so a medium-integrity development service can
        // reserve successors even when this crate is built as a dependency. In
        // production the owner is LocalSystem, which already has full access;
        // ordinary interactive callers never gain create-instance rights.
        let security_descriptor = w!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x4;;;OW)(A;;0x12019b;;;IU)");
        // SAFETY: SDDL is static, and Windows allocates descriptor on success.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                security_descriptor,
                SDDL_REVISION_1,
                &raw mut descriptor,
                None,
            )
        }?;
        Ok(Self { descriptor })
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
                .expect("SECURITY_ATTRIBUTES size fits in u32"),
            lpSecurityDescriptor: self.descriptor.0,
            bInheritHandle: false.into(),
        }
    }
}

impl Drop for OwnedSecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: descriptor was allocated by LocalAlloc inside the conversion API.
        let _ = unsafe { LocalFree(Some(HLOCAL(self.descriptor.0))) };
    }
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[derive(Debug, Error)]
pub enum NamedPipeError {
    #[error("invalid named-pipe name")]
    InvalidPipeName,
    #[error("Windows named-pipe operation failed: {0}")]
    Windows(#[from] WindowsError),
    #[error("named-pipe I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("could not authenticate the running ResticPal Windows service: {0}")]
    ServiceIdentityUnavailable(#[source] WindowsError),
    #[error("named-pipe server is not the running ResticPal Windows service")]
    UntrustedServicePipe,
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("service response protocol {actual} does not match expected protocol {expected}")]
    IncompatibleResponseProtocol { expected: u32, actual: u32 },
    #[error("service response ID {actual} does not match request ID {expected}")]
    MismatchedResponse { expected: u64, actual: u64 },
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;

    use resticpal_protocol::{RequestCommand, ResponsePayload};

    use super::*;

    static NEXT_PIPE: AtomicU64 = AtomicU64::new(1);

    fn test_pipe_name() -> String {
        format!(
            r"\\.\pipe\ResticPal.Test.{}.{}",
            std::process::id(),
            NEXT_PIPE.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn first_pipe_instance(name: &str) -> Result<PipeConnection, WindowsError> {
        let name = wide_null(name);
        // SAFETY: the test owns the name buffer throughout synchronous
        // creation and transfers the returned handle into PipeConnection.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                PIPE_BUFFER_BYTES,
                PIPE_BUFFER_BYTES,
                0,
                None,
            )
        };
        if handle.is_invalid() {
            return Err(WindowsError::from_thread());
        }
        Ok(PipeConnection {
            handle,
            io_deadline: Instant::now(),
        })
    }

    #[test]
    fn running_service_identity_requires_the_exact_nonzero_pipe_server_pid() {
        let mut status = SERVICE_STATUS_PROCESS {
            dwCurrentState: SERVICE_RUNNING,
            dwProcessId: 4_242,
            ..SERVICE_STATUS_PROCESS::default()
        };
        assert!(service_pipe_identity_matches(4_242, &status));
        assert!(!service_pipe_identity_matches(0, &status));
        assert!(!service_pipe_identity_matches(4_243, &status));
        status.dwProcessId = 0;
        assert!(!service_pipe_identity_matches(4_242, &status));
        status.dwProcessId = 4_242;
        status.dwCurrentState = windows::Win32::System::Services::SERVICE_STOPPED;
        assert!(!service_pipe_identity_matches(4_242, &status));
    }

    #[test]
    fn repeated_initial_squat_failures_never_join_an_attackers_pipe() {
        let pipe_name = test_pipe_name();
        let attacker = first_pipe_instance(&pipe_name).expect("attacker pre-squatted pipe");
        let server = NamedPipeServer::new(&pipe_name).expect("secure server initialization");
        for _ in 0..2 {
            let refused = server.serve_one(|_, _| panic!("a squatted request must never run"));
            assert!(matches!(refused, Err(NamedPipeError::Windows(_))));
            assert!(
                server.first_instance.load(Ordering::Acquire),
                "FIRST_PIPE_INSTANCE must remain armed after every failed attempt"
            );
        }
        drop(attacker);

        let client_name = pipe_name.clone();
        let client = thread::spawn(move || {
            NamedPipeClient::request_at(
                &client_name,
                &Request::new(7_101, RequestCommand::GetStatus),
                Duration::from_secs(5),
            )
        });
        server
            .serve_one(|request, _| {
                Response::new(
                    request.request_id,
                    ResponsePayload::Accepted {
                        message: "trusted owner".to_owned(),
                    },
                )
            })
            .expect("secure ownership should recover after the attacker exits");
        assert_eq!(
            client
                .join()
                .expect("client thread")
                .expect("trusted client response")
                .request_id,
            7_101
        );
    }

    #[test]
    fn secure_pipe_name_remains_reserved_between_completed_requests() {
        let pipe_name = test_pipe_name();
        let server_name = pipe_name.clone();
        let (completed_tx, completed_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let server = NamedPipeServer::new(&server_name).expect("secure server");
            server
                .serve_one(|request, _| {
                    Response::new(
                        request.request_id,
                        ResponsePayload::Accepted {
                            message: "first".to_owned(),
                        },
                    )
                })
                .expect("first secure response");
            completed_tx.send(()).expect("first-request signal");
            continue_rx.recv().expect("resume secure server");
            server
                .serve_one(|request, _| {
                    Response::new(
                        request.request_id,
                        ResponsePayload::Accepted {
                            message: "second".to_owned(),
                        },
                    )
                })
                .expect("second secure response");
        });

        NamedPipeClient::request_at(
            &pipe_name,
            &Request::new(7_102, RequestCommand::GetStatus),
            Duration::from_secs(5),
        )
        .expect("first request");
        completed_rx.recv().expect("completed request");
        assert!(
            first_pipe_instance(&pipe_name).is_err(),
            "a successor must reserve the name even while the service loop is idle"
        );
        continue_tx.send(()).expect("allow second request");
        NamedPipeClient::request_at(
            &pipe_name,
            &Request::new(7_103, RequestCommand::GetStatus),
            Duration::from_secs(5),
        )
        .expect("second request");
        server.join().expect("secure server thread");
    }

    #[test]
    fn malformed_clients_cannot_open_a_gap_for_first_instance_squatting() {
        let pipe_name = test_pipe_name();
        let server = NamedPipeServer::new(&pipe_name).expect("secure server");
        let malformed_name = pipe_name.clone();
        let malformed = thread::spawn(move || {
            let mut client = loop {
                match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&malformed_name)
                {
                    Ok(client) => break client,
                    Err(error) if pipe_connect_error_is_retryable(&error) => {
                        thread::sleep(CONNECT_RETRY_DELAY);
                    }
                    Err(error) => panic!("malformed fixture should connect: {error}"),
                }
            };
            client
                .write_all(&u32::MAX.to_le_bytes())
                .expect("oversized malicious frame length");
        });
        assert!(
            server
                .serve_one(|_, _| panic!("malformed request must never reach its handler"))
                .is_err()
        );
        malformed.join().expect("malformed client thread");
        assert!(
            first_pipe_instance(&pipe_name).is_err(),
            "rejecting a malformed client must preserve a secure reserved successor"
        );

        let valid_name = pipe_name.clone();
        let valid = thread::spawn(move || {
            NamedPipeClient::request_at(
                &valid_name,
                &Request::new(7_104, RequestCommand::GetStatus),
                Duration::from_secs(5),
            )
        });
        server
            .serve_one(|request, _| {
                Response::new(
                    request.request_id,
                    ResponsePayload::Accepted {
                        message: "recovered".to_owned(),
                    },
                )
            })
            .expect("secure server must recover after malformed input");
        valid
            .join()
            .expect("valid client thread")
            .expect("valid client response");
    }

    #[test]
    fn acl_protected_pipe_round_trips_a_request() {
        let pipe_name = format!(
            r"\\.\pipe\ResticPal.Test.{}.{}",
            std::process::id(),
            NEXT_PIPE.fetch_add(1, Ordering::Relaxed)
        );
        let server_name = pipe_name.clone();
        let server = thread::spawn(move || {
            NamedPipeServer::new(&server_name)
                .expect("server should initialize")
                .serve_one(|request, _identity| {
                    Response::new(
                        request.request_id,
                        ResponsePayload::Accepted {
                            message: "request received".to_owned(),
                        },
                    )
                })
                .expect("server should handle one request");
        });
        let request = Request::new(123, RequestCommand::RunBackupNow);

        let response = NamedPipeClient::request_at(&pipe_name, &request, Duration::from_secs(5))
            .expect("client should complete request");
        server.join().expect("server thread should finish");

        assert_eq!(response.request_id, request.request_id);
        assert_eq!(
            response.payload,
            ResponsePayload::Accepted {
                message: "request received".to_owned()
            }
        );
    }

    #[test]
    fn client_response_wait_observes_the_requested_timeout() {
        let pipe_name = format!(
            r"\\.\pipe\ResticPal.Test.{}.{}",
            std::process::id(),
            NEXT_PIPE.fetch_add(1, Ordering::Relaxed)
        );
        let server_name = pipe_name.clone();
        let server = thread::spawn(move || {
            let _ = NamedPipeServer::new(&server_name)
                .expect("server should initialize")
                .serve_one(|request, _identity| {
                    thread::sleep(Duration::from_millis(300));
                    Response::new(
                        request.request_id,
                        ResponsePayload::Accepted {
                            message: "late response".to_owned(),
                        },
                    )
                });
        });
        let request = Request::new(124, RequestCommand::GetStatus);
        let started = Instant::now();

        let result = NamedPipeClient::request_at(&pipe_name, &request, Duration::from_millis(100));

        assert!(matches!(
            result,
            Err(NamedPipeError::Frame(FrameError::Io(ref error)))
                if error.kind() == io::ErrorKind::TimedOut
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
        server.join().expect("server thread should finish");
    }

    #[test]
    fn client_waits_for_the_already_reserved_secure_successor() {
        let pipe_name = format!(
            r"\\.\pipe\ResticPal.Test.{}.{}",
            std::process::id(),
            NEXT_PIPE.fetch_add(1, Ordering::Relaxed)
        );
        let server_name = pipe_name.clone();
        let server = thread::spawn(move || {
            let server = NamedPipeServer::new(&server_name).expect("server should initialize");
            for message in ["first request", "second request"] {
                server
                    .serve_one(|request, _identity| {
                        Response::new(
                            request.request_id,
                            ResponsePayload::Accepted {
                                message: message.to_owned(),
                            },
                        )
                    })
                    .expect("server should handle both requests");
            }
        });

        let first_request = Request::new(125, RequestCommand::GetStatus);
        let mut first_connection = loop {
            match OpenOptions::new().read(true).write(true).open(&pipe_name) {
                Ok(connection) => break connection,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    thread::sleep(CONNECT_RETRY_DELAY);
                }
                Err(error) => panic!("first client should connect: {error}"),
            }
        };
        write_frame(&mut first_connection, &first_request).expect("first request frame");
        let first_response: Response =
            read_frame(&mut first_connection).expect("first response frame");
        assert_eq!(first_response.request_id, first_request.request_id);

        let busy_error = io::Error::from_raw_os_error(ERROR_PIPE_BUSY.0 as i32);
        assert!(pipe_connect_error_is_retryable(&busy_error));

        let release_first = thread::spawn(move || {
            thread::sleep(Duration::from_millis(500));
            first_connection
                .write_all(&[0])
                .expect("first response acknowledgement");
        });
        let second_request = Request::new(126, RequestCommand::GetStatus);
        let started = Instant::now();
        let second_response =
            NamedPipeClient::request_at(&pipe_name, &second_request, Duration::from_secs(5))
                .expect("second client should connect to the reserved secure successor");

        assert!(started.elapsed() >= Duration::from_millis(100));
        assert_eq!(second_response.request_id, second_request.request_id);
        release_first.join().expect("first client should finish");
        server.join().expect("server thread should finish");
    }

    #[test]
    fn server_accepts_a_request_split_across_multiple_writes() {
        let pipe_name = format!(
            r"\\.\pipe\ResticPal.Test.{}.{}",
            std::process::id(),
            NEXT_PIPE.fetch_add(1, Ordering::Relaxed)
        );
        let server_name = pipe_name.clone();
        let server = thread::spawn(move || {
            NamedPipeServer::new(&server_name)
                .expect("server should initialize")
                .serve_one(|request, _identity| {
                    Response::new(
                        request.request_id,
                        ResponsePayload::Accepted {
                            message: "split request received".to_owned(),
                        },
                    )
                })
                .expect("split request should complete");
        });
        let request = Request::new(125, RequestCommand::GetStatus);
        let mut frame = Vec::new();
        write_frame(&mut frame, &request).expect("request frame");
        let mut client = loop {
            match OpenOptions::new().read(true).write(true).open(&pipe_name) {
                Ok(client) => break client,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    thread::sleep(CONNECT_RETRY_DELAY);
                }
                Err(error) => panic!("client should connect: {error}"),
            }
        };

        client.write_all(&frame[..4]).expect("frame header");
        thread::sleep(Duration::from_millis(100));
        client.write_all(&frame[4..]).expect("frame payload");
        let response: Response = read_frame(&mut client).expect("response");
        client.write_all(&[0]).expect("response acknowledgement");
        server.join().expect("server thread should finish");

        assert_eq!(response.request_id, request.request_id);
    }
}
