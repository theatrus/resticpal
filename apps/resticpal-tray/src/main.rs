#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::mem::size_of;
use std::process::Command;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
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
    copy_wide("resticpal: connecting to backup service", &mut data.szTip);

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
        // SAFETY: labels are static null-terminated strings and menu is valid.
        unsafe {
            AppendMenuW(menu, MF_STRING, MENU_OPEN, w!("Open resticpal"))?;
            AppendMenuW(menu, MF_STRING, MENU_RUN_BACKUP, w!("Run backup now"))?;
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
            MENU_RUN_BACKUP => show_information(
                window,
                "The tray is ready. Service IPC will wire up Run backup now next.",
            ),
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
