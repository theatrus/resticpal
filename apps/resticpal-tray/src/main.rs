#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::mem::size_of;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use resticpal_core::status::BackupState;
use resticpal_protocol::{Request, RequestCommand, Response, ResponsePayload};
use resticpal_windows::named_pipe::{NamedPipeClient, NamedPipeError};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CW_USEDEFAULT, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, IDC_ARROW, IDI_APPLICATION,
    LoadCursorW, LoadIconW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MENU_ITEM_FLAGS, MSG,
    MessageBoxW, PostQuitMessage, RegisterClassW, SetForegroundWindow, TPM_NONOTIFY, TPM_RETURNCMD,
    TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_DESTROY,
    WM_LBUTTONDBLCLK, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::{Error, Result, w};

const WINDOW_CLASS: windows::core::PCWSTR = w!("ResticPalTrayWindow");
const TRAY_CALLBACK: u32 = WM_APP + 1;
const TRAY_ICON_ID: u32 = 1;
const MENU_OPEN: usize = 1;
const MENU_RUN_BACKUP: usize = 2;
const MENU_EXIT: usize = 3;
const MF_STRING: MENU_ITEM_FLAGS = MENU_ITEM_FLAGS(0);
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn main() {
    if let Err(error) = run() {
        let message = format!("resticpal tray could not start:\n\n{error}");
        let wide_message = wide_null(&message);
        // SAFETY: both strings are valid and null terminated for the duration
        // of the synchronous MessageBoxW call.
        unsafe {
            MessageBoxW(
                None,
                windows::core::PCWSTR(wide_message.as_ptr()),
                w!("resticpal"),
                MB_OK | MB_ICONERROR,
            );
        }
    }
}

fn run() -> Result<()> {
    // SAFETY: None asks Windows for the module containing this executable.
    let module = unsafe { GetModuleHandleW(None) }?;
    let instance = HINSTANCE(module.0);
    // SAFETY: loading predefined Windows resources does not transfer ownership.
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }?;

    let window_class = WNDCLASSW {
        hCursor: cursor,
        hInstance: instance,
        lpszClassName: WINDOW_CLASS,
        lpfnWndProc: Some(window_proc),
        ..WNDCLASSW::default()
    };

    // SAFETY: `window_class` contains valid handles and pointers for this call.
    if unsafe { RegisterClassW(&raw const window_class) } == 0 {
        return Err(Error::from_thread());
    }

    // SAFETY: the registered class and instance remain valid until process exit.
    let window = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            WINDOW_CLASS,
            w!("resticpal"),
            WS_OVERLAPPED,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
    }?;

    add_tray_icon(window)?;
    run_message_loop()
}

fn add_tray_icon(window: HWND) -> Result<()> {
    // SAFETY: loading a predefined application icon returns a shared handle.
    let icon = unsafe { LoadIconW(None, IDI_APPLICATION) }?;
    let mut data = NOTIFYICONDATAW {
        cbSize: u32::try_from(size_of::<NOTIFYICONDATAW>())
            .expect("NOTIFYICONDATAW size fits in u32"),
        hWnd: window,
        uID: TRAY_ICON_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
        uCallbackMessage: TRAY_CALLBACK,
        hIcon: icon,
        ..NOTIFYICONDATAW::default()
    };
    let tooltip = fetch_status_tooltip()
        .unwrap_or_else(|_| "resticpal: backup service unavailable".to_owned());
    copy_wide(&tooltip, &mut data.szTip);

    // SAFETY: data is fully initialized and points to the live hidden window.
    if unsafe { Shell_NotifyIconW(NIM_ADD, &raw const data) }.as_bool() {
        Ok(())
    } else {
        Err(Error::from_thread())
    }
}

fn run_message_loop() -> Result<()> {
    let mut message = MSG::default();
    loop {
        // SAFETY: message points to writable storage for the duration of the call.
        let result = unsafe { GetMessageW(&raw mut message, None, 0, 0) };
        if result.0 == -1 {
            return Err(Error::from_thread());
        }
        if !result.as_bool() {
            return Ok(());
        }

        // SAFETY: GetMessageW initialized the message.
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        TRAY_CALLBACK => {
            match u32::try_from(lparam.0).unwrap_or_default() {
                WM_LBUTTONDBLCLK => launch_ui(window),
                WM_RBUTTONUP => {
                    if let Err(error) = show_context_menu(window) {
                        show_error(window, &error.to_string());
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // SAFETY: window is the live HWND supplied by Windows to this callback.
            let _ = unsafe { DestroyWindow(window) };
            LRESULT(0)
        }
        WM_DESTROY => {
            remove_tray_icon(window);
            // SAFETY: called from the UI thread's window procedure.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => {
            // SAFETY: unhandled messages are delegated to the system default procedure.
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        }
    }
}

fn show_context_menu(window: HWND) -> Result<()> {
    // SAFETY: Windows owns the implementation; the returned menu is destroyed below.
    let menu = unsafe { CreatePopupMenu() }?;
    let result = (|| {
        let backup_running = backup_is_running().unwrap_or(false);
        // SAFETY: labels are static null-terminated strings and menu is valid.
        unsafe {
            AppendMenuW(menu, MF_STRING, MENU_OPEN, w!("Open resticpal"))?;
            if backup_running {
                AppendMenuW(menu, MF_STRING, MENU_RUN_BACKUP, w!("Cancel backup"))?;
            } else {
                AppendMenuW(menu, MF_STRING, MENU_RUN_BACKUP, w!("Run backup now"))?;
            }
            AppendMenuW(menu, MF_STRING, MENU_EXIT, w!("Exit tray"))?;
        }

        let mut cursor = POINT::default();
        // SAFETY: cursor points to valid writable storage.
        unsafe { GetCursorPos(&raw mut cursor) }?;
        // SAFETY: foreground activation and menu tracking use our live hidden window.
        unsafe {
            let _ = SetForegroundWindow(window);
        }
        // SAFETY: the menu and owner window remain live during this synchronous call.
        let command = unsafe {
            TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY,
                cursor.x,
                cursor.y,
                None,
                window,
                None,
            )
        };

        match usize::try_from(command.0).unwrap_or_default() {
            MENU_OPEN => launch_ui(window),
            MENU_RUN_BACKUP => {
                if backup_running {
                    send_backup_action(window, RequestCommand::CancelBackup);
                } else {
                    send_backup_action(window, RequestCommand::RunBackupNow);
                }
            }
            MENU_EXIT => {
                // SAFETY: window is owned by this UI thread.
                unsafe { DestroyWindow(window) }?;
            }
            _ => {}
        }
        Ok(())
    })();

    // SAFETY: menu was created in this function and is no longer displayed.
    let destroy_result = unsafe { DestroyMenu(menu) };
    result.and(destroy_result)
}

fn launch_ui(window: HWND) {
    let result = std::env::current_exe().and_then(|mut executable| {
        executable.set_file_name("resticpal-ui.exe");
        Command::new(executable).spawn().map(|_| ())
    });

    if let Err(error) = result {
        show_error(
            window,
            &format!("The resticpal settings application could not be opened.\n\n{error}"),
        );
    }
}

fn send_backup_action(window: HWND, command: RequestCommand) {
    match send_request(command) {
        Ok(Response {
            payload: ResponsePayload::Accepted { message },
            ..
        }) => {
            show_information(window, &message);
            let _ = refresh_tray_status(window);
        }
        Ok(Response {
            payload: ResponsePayload::Rejected { message, .. },
            ..
        }) => show_error(window, &message),
        Ok(_) => show_error(
            window,
            "The backup service returned an unexpected response.",
        ),
        Err(error) => show_error(
            window,
            &format!("The backup service could not be reached.\n\n{error}"),
        ),
    }
}

fn fetch_status_tooltip() -> std::result::Result<String, NamedPipeError> {
    let response = send_request(RequestCommand::GetStatus)?;
    Ok(match response.payload {
        ResponsePayload::Status { status } => match status.state {
            BackupState::Unconfigured => "resticpal: setup required".to_owned(),
            BackupState::Idle | BackupState::Succeeded => "resticpal: protected".to_owned(),
            BackupState::Waiting { .. } => "resticpal: backup waiting".to_owned(),
            BackupState::Running { .. } => status
                .progress
                .as_ref()
                .and_then(|progress| {
                    progress
                        .percent_done
                        .map(|percent| format!("resticpal: backup running ({percent}%)"))
                })
                .unwrap_or_else(|| "resticpal: backup running".to_owned()),
            BackupState::SucceededWithWarnings => {
                "resticpal: backup completed with warnings".to_owned()
            }
            BackupState::Failed { .. } => "resticpal: backup needs attention".to_owned(),
            BackupState::Cancelled => "resticpal: last backup cancelled".to_owned(),
            BackupState::Paused => "resticpal: backups paused".to_owned(),
        },
        ResponsePayload::Rejected { message, .. } => format!("resticpal: {message}"),
        ResponsePayload::Accepted { .. } => "resticpal: connected".to_owned(),
    })
}

fn backup_is_running() -> std::result::Result<bool, NamedPipeError> {
    let response = send_request(RequestCommand::GetStatus)?;
    Ok(matches!(
        response.payload,
        ResponsePayload::Status {
            status: resticpal_core::status::ServiceStatus {
                state: BackupState::Running { .. },
                ..
            }
        }
    ))
}

fn refresh_tray_status(window: HWND) -> std::result::Result<(), NamedPipeError> {
    let tooltip = fetch_status_tooltip()?;
    let mut data = NOTIFYICONDATAW {
        cbSize: u32::try_from(size_of::<NOTIFYICONDATAW>())
            .expect("NOTIFYICONDATAW size fits in u32"),
        hWnd: window,
        uID: TRAY_ICON_ID,
        uFlags: NIF_TIP | NIF_SHOWTIP,
        ..NOTIFYICONDATAW::default()
    };
    copy_wide(&tooltip, &mut data.szTip);

    // SAFETY: data identifies our live notification icon and contains a valid tip.
    if unsafe { Shell_NotifyIconW(NIM_MODIFY, &raw const data) }.as_bool() {
        Ok(())
    } else {
        Err(windows::core::Error::from_thread().into())
    }
}

fn send_request(command: RequestCommand) -> std::result::Result<Response, NamedPipeError> {
    let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    NamedPipeClient::request(&Request::new(request_id, command))
}

fn remove_tray_icon(window: HWND) {
    let data = NOTIFYICONDATAW {
        cbSize: u32::try_from(size_of::<NOTIFYICONDATAW>())
            .expect("NOTIFYICONDATAW size fits in u32"),
        hWnd: window,
        uID: TRAY_ICON_ID,
        ..NOTIFYICONDATAW::default()
    };
    // SAFETY: identifies the icon previously added for this live window.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &raw const data);
    }
}

fn show_information(window: HWND, message: &str) {
    show_message(window, message, MB_OK | MB_ICONINFORMATION);
}

fn show_error(window: HWND, message: &str) {
    show_message(window, message, MB_OK | MB_ICONERROR);
}

fn show_message(
    window: HWND,
    message: &str,
    style: windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE,
) {
    let message = wide_null(message);
    // SAFETY: the window is live and the string remains valid through the call.
    unsafe {
        MessageBoxW(
            Some(window),
            windows::core::PCWSTR(message.as_ptr()),
            w!("resticpal"),
            style,
        );
    }
}

fn copy_wide(value: &str, destination: &mut [u16]) {
    let encoded = value
        .encode_utf16()
        .take(destination.len().saturating_sub(1));
    for (target, source) in destination.iter_mut().zip(encoded) {
        *target = source;
    }
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
