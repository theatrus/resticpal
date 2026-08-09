use std::ffi::c_void;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::thread;
use std::time::{Duration, Instant};

use resticpal_protocol::{
    FrameError, PROTOCOL_VERSION, Request, Response, read_frame, write_frame,
};
use thiserror::Error;
use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    CheckTokenMembership, CreateWellKnownSid, PSECURITY_DESCRIPTOR, PSID, RevertToSelf,
    SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE, WinBuiltinAdministratorsSid,
};
use windows::Win32::Storage::FileSystem::{
    FlushFileBuffers, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, ImpersonateNamedPipeClient,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT,
};
use windows::core::{BOOL, Error as WindowsError, HRESULT, PCWSTR, w};

pub const DEFAULT_PIPE_NAME: &str = r"\\.\pipe\ResticPal.v2";
const PIPE_BUFFER_BYTES: u32 = 64 * 1024;
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(20);

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
        // Windows requires the server to read at least one byte from the pipe
        // before it can impersonate and inspect the authenticated client token.
        let identity = connection.client_identity()?;
        let response = if request.protocol_version == PROTOCOL_VERSION {
            handler(request, identity)
        } else {
            Response::incompatible(request.request_id, request.protocol_version)
        };
        write_frame(&mut connection, &response)?;
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

        let connection = PipeConnection { handle };
        // SAFETY: connection owns a valid server-side named-pipe handle.
        match unsafe { ConnectNamedPipe(connection.handle, None) } {
            Ok(()) => Ok(connection),
            Err(error) if error.code() == HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) => {
                Ok(connection)
            }
            Err(error) => Err(error.into()),
        }
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
        let mut connection = loop {
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
        Ok(response)
    }
}

struct PipeConnection {
    handle: HANDLE,
}

impl PipeConnection {
    fn client_identity(&self) -> Result<ClientIdentity, NamedPipeError> {
        // SAFETY: handle is a connected server pipe. The guard always reverts
        // this thread before it handles another client.
        unsafe { ImpersonateNamedPipeClient(self.handle) }?;
        let _guard = ImpersonationGuard;

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
    }
}

impl Read for PipeConnection {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut bytes_read = 0_u32;
        // SAFETY: handle is live and buffer is writable for its full length.
        unsafe { ReadFile(self.handle, Some(buffer), Some(&raw mut bytes_read), None) }
            .map_err(io::Error::other)?;
        Ok(bytes_read as usize)
    }
}

impl Write for PipeConnection {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut bytes_written = 0_u32;
        // SAFETY: handle is live and buffer is readable for its full length.
        unsafe {
            WriteFile(
                self.handle,
                Some(buffer),
                Some(&raw mut bytes_written),
                None,
            )
        }
        .map_err(io::Error::other)?;
        Ok(bytes_written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        // SAFETY: handle is a live writable pipe handle.
        unsafe { FlushFileBuffers(self.handle) }.map_err(io::Error::other)
    }
}

impl Drop for PipeConnection {
    fn drop(&mut self) {
        // SAFETY: this type exclusively owns the server pipe handle.
        let _ = unsafe { DisconnectNamedPipe(self.handle) };
        // SAFETY: the disconnected handle is no longer used after this point.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

struct ImpersonationGuard;

impl Drop for ImpersonationGuard {
    fn drop(&mut self) {
        // SAFETY: constructed only after successful named-pipe impersonation.
        let _ = unsafe { RevertToSelf() };
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
}
