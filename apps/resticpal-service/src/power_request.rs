//! RAII wrapper for the Windows system-required power request used by backups.

use std::iter;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Power::{
    PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired, PowerSetRequest,
};
use windows::Win32::System::Threading::{
    POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
};
use windows::core::{PWSTR, Result};

pub struct SystemPowerRequest {
    handle: HANDLE,
}

impl SystemPowerRequest {
    #[allow(dead_code)]
    pub fn acquire(reason: &str) -> Result<Self> {
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

impl Drop for SystemPowerRequest {
    fn drop(&mut self) {
        // SAFETY: this type exclusively owns the request handle. Drop runs once.
        let _ = unsafe { PowerClearRequest(self.handle, PowerRequestSystemRequired) };
        // SAFETY: the request has been cleared and the owned handle is no longer used.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}
