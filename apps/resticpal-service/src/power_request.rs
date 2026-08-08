//! RAII wrapper for the Windows system-required power request used by backups.

use std::io;
use std::iter;
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use thiserror::Error;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Power::{
    PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired, PowerSetRequest,
};
use windows::Win32::System::Threading::{
    POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
};
use windows::core::{PWSTR, Result as WindowsResult};

pub struct SystemPowerRequest {
    handle: HANDLE,
}

impl SystemPowerRequest {
    #[allow(dead_code)]
    pub fn acquire(reason: &str) -> WindowsResult<Self> {
        let mut reason: Vec<u16> = reason.encode_utf16().chain(iter::once(0)).collect();
        let context = REASON_CONTEXT {
            Version: 0,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: PWSTR(reason.as_mut_ptr()),
            },
        };

        // SAFETY: `context` and its null-terminated reason string remain valid
        // for the duration of PowerCreateRequest. Windows copies the reason.
        let handle = unsafe { PowerCreateRequest(&raw const context) }?;
        // SAFETY: `handle` was returned by PowerCreateRequest and is owned here.
        if let Err(error) = unsafe { PowerSetRequest(handle, PowerRequestSystemRequired) } {
            // SAFETY: closing our owned handle after PowerSetRequest failed.
            let _ = unsafe { CloseHandle(handle) };
            return Err(error);
        }

        Ok(Self { handle })
    }
}

// SAFETY: Windows power-request handles are process handles with no thread
// affinity. This type owns the handle exclusively and only moves it so the
// watchdog thread can clear and close it.
unsafe impl Send for SystemPowerRequest {}

impl Drop for SystemPowerRequest {
    fn drop(&mut self) {
        // SAFETY: this type exclusively owns the request handle. Drop runs once.
        let _ = unsafe { PowerClearRequest(self.handle, PowerRequestSystemRequired) };
        // SAFETY: the request has been cleared and the owned handle is no longer used.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

/// Owns a system-required request on a watchdog thread. The request is dropped
/// when this lease ends or when the configured safety timeout elapses.
pub struct TimedSystemPowerRequest {
    release: Option<SyncSender<()>>,
    watchdog: Option<JoinHandle<()>>,
}

impl TimedSystemPowerRequest {
    pub fn acquire(
        reason: &str,
        timeout: Duration,
    ) -> std::result::Result<Self, TimedPowerRequestError> {
        let request = SystemPowerRequest::acquire(reason)?;
        let (release, wait_for_release) = mpsc::sync_channel(1);
        let watchdog = thread::Builder::new()
            .name("resticpal-wake-lock".to_owned())
            .spawn(move || {
                let _ = wait_for_release.recv_timeout(timeout);
                drop(request);
            })?;

        Ok(Self {
            release: Some(release),
            watchdog: Some(watchdog),
        })
    }
}

#[derive(Debug, Error)]
pub enum TimedPowerRequestError {
    #[error("could not acquire the Windows power request: {0}")]
    Windows(#[from] windows::core::Error),
    #[error("could not start the power-request watchdog: {0}")]
    Thread(#[from] io::Error),
}

impl Drop for TimedSystemPowerRequest {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
    }
}
