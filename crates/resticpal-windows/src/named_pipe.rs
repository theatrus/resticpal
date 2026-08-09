use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::thread;
use std::time::{Duration, Instant};

use resticpal_protocol::{
    FrameError, PROTOCOL_VERSION, Request, Response, read_frame, write_frame,
};
use thiserror::Error;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING,
    ERROR_PIPE_NOT_CONNECTED, HANDLE, HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    CheckTokenMembership, CreateWellKnownSid, PSECURITY_DESCRIPTOR, PSID, RevertToSelf,
    SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE, WinBuiltinAdministratorsSid,
};
use windows::Win32::Storage::FileSystem::{PIPE_ACCESS_DUPLEX, ReadFile, WriteFile};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, ImpersonateNamedPipeClient, NAMED_PIPE_MODE, PIPE_NOWAIT,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT, SetNamedPipeHandleState,
};
use windows::core::{BOOL, Error as WindowsError, HRESULT, PCWSTR, w};

pub const DEFAULT_PIPE_NAME: &str = r"\\.\pipe\ResticPal.v2";
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
}

impl NamedPipeServer {
    pub fn new(name: &str) -> Result<Self, NamedPipeError> {
        if name.is_empty() || name.encode_utf16().any(|unit| unit == 0) {
            return Err(NamedPipeError::InvalidPipeName);
        }

        Ok(Self {
            name: wide_null(name),
            security: OwnedSecurityDescriptor::new()?,
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
        let attributes = self.security.attributes();
        // SAFETY: the pipe name and security descriptor remain valid for the
        // lifetime of the created handle. Remote clients are explicitly rejected.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(self.name.as_ptr()),
                PIPE_ACCESS_DUPLEX,
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

        let mut connection = PipeConnection {
            handle,
            io_deadline: Instant::now(),
        };
        // SAFETY: connection owns a valid server-side named-pipe handle.
        match unsafe { ConnectNamedPipe(connection.handle, None) } {
            Ok(()) => {}
            Err(error) if error.code() == HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) => {}
            Err(error) => return Err(error.into()),
        }
        let mode = NAMED_PIPE_MODE(PIPE_READMODE_BYTE.0 | PIPE_NOWAIT.0);
        // SAFETY: connection owns a connected named-pipe handle and mode is
        // live for the duration of this synchronous call.
        unsafe { SetNamedPipeHandleState(connection.handle, Some(&raw const mode), None, None) }?;
        connection.io_deadline = Instant::now() + SERVER_IO_TIMEOUT;
        Ok(connection)
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
                    if !matches!(
                        error.kind(),
                        io::ErrorKind::NotFound
                            | io::ErrorKind::PermissionDenied
                            | io::ErrorKind::WouldBlock
                    ) {
                        return Err(error.into());
                    }
                    thread::sleep(CONNECT_RETRY_DELAY);
                }
                Err(error) => return Err(error.into()),
            }
        };
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
        // SAFETY: SDDL is static, and Windows allocates descriptor on success.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)"),
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

    use resticpal_protocol::{RequestCommand, ResponsePayload};

    use super::*;

    static NEXT_PIPE: AtomicU64 = AtomicU64::new(1);

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
