#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs;
use std::mem::size_of;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use resticpal_core::status::BackupState;
use resticpal_protocol::{Request, RequestCommand, Response, ResponsePayload};
use resticpal_windows::named_pipe::{NamedPipeClient, NamedPipeError};
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIN_BALLOONUSERCLICK, NOTIFYICONDATAW, Shell_NotifyIconW, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CW_USEDEFAULT, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW,
    DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, FindWindowW,
    GetCursorPos, GetMessageW, HICON, IDC_ARROW, IDI_APPLICATION, KillTimer, LR_DEFAULTCOLOR,
    LoadCursorW, LoadIconW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MENU_ITEM_FLAGS, MSG,
    MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassW, SW_SHOWNORMAL, SetForegroundWindow,
    SetTimer, TPM_NONOTIFY, TPM_RETURNCMD, TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE,
    WM_APP, WM_CLOSE, WM_DESTROY, WM_LBUTTONDBLCLK, WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
    WS_OVERLAPPED,
};
use windows::core::{Error, Result, w};

const WINDOW_CLASS: windows::core::PCWSTR = w!("ResticPalTrayWindow");
const TRAY_CALLBACK: u32 = WM_APP + 1;
const TRAY_ICON_ID: u32 = 1;
const MENU_OPEN: usize = 1;
const MENU_RUN_BACKUP: usize = 2;
const MENU_EXIT: usize = 3;
const MENU_UPDATE: usize = 4;
const ONBOARDING_TIMER_ID: usize = 2;
const ONBOARDING_RETRY_INTERVAL_MS: u32 = 1_000;
const ONBOARDING_MAX_ATTEMPTS: u64 = 120;
const UPDATE_TIMER_ID: usize = 3;
const UPDATE_CHECK_INTERVAL_MS: u32 = 6 * 60 * 60 * 1_000;
const UPDATE_AVAILABLE_MESSAGE: u32 = WM_APP + 2;
const UPDATE_PROMPT_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const UPDATE_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_UPDATE_APPCAST_BYTES: usize = 256 * 1024;
const MAX_UPDATE_SIGNATURE_BYTES: usize = 1024;
const UPDATE_APPCAST_URL: &str =
    "https://github.com/theatrus/resticpal/releases/latest/download/appcast.xml";
const UPDATE_APPCAST_SIGNATURE_URL: &str =
    "https://github.com/theatrus/resticpal/releases/latest/download/appcast.xml.signature";
const UPDATE_PUBLIC_KEY: &str = include_str!("../../../config/update-public-key.txt");
const MF_STRING: MENU_ITEM_FLAGS = MENU_ITEM_FLAGS(0);
const TRAY_ICON_BYTES: &[u8] = include_bytes!("../../../assets/resticpal.ico");
const PREFERRED_TRAY_ICON_SIZE: u16 = 32;
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
static ONBOARDING_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static UPDATE_CHECK_RUNNING: AtomicBool = AtomicBool::new(false);
static UPDATE_AVAILABLE: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy)]
enum UiDestination {
    Default,
    Setup,
    Updates,
}

struct TrayIcon {
    handle: HICON,
    owned: bool,
}

impl TrayIcon {
    fn load() -> Result<Self> {
        if let Some(image) = select_icon_image(TRAY_ICON_BYTES, PREFERRED_TRAY_ICON_SIZE) {
            // SAFETY: `image` is a bounded image resource from the embedded ICO and
            // remains live for the duration of this synchronous call.
            if let Ok(handle) =
                unsafe { CreateIconFromResourceEx(image, true, 0x0003_0000, 0, 0, LR_DEFAULTCOLOR) }
            {
                return Ok(Self {
                    handle,
                    owned: true,
                });
            }
        }

        // SAFETY: loading a predefined Windows resource returns a shared handle.
        let handle = unsafe { LoadIconW(None, IDI_APPLICATION) }?;
        Ok(Self {
            handle,
            owned: false,
        })
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: an owned handle is created exactly once above and remains live
            // until this process has removed its notification-area icon.
            let _ = unsafe { DestroyIcon(self.handle) };
        }
    }
}

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
    // A machine-wide Run entry and the post-install launch can overlap during
    // upgrades. Keep one notification icon per interactive session.
    if unsafe { FindWindowW(WINDOW_CLASS, None) }.is_ok() {
        return Ok(());
    }

    // SAFETY: None asks Windows for the module containing this executable.
    let module = unsafe { GetModuleHandleW(None) }?;
    let instance = HINSTANCE(module.0);
    // SAFETY: loading predefined Windows resources does not transfer ownership.
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }?;
    let tray_icon = TrayIcon::load()?;

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

    add_tray_icon(window, tray_icon.handle)?;
    if !maybe_launch_onboarding(window) {
        // The service can briefly report validation/waiting state while a fresh
        // install settles. Retry from the normal Windows message loop so the
        // notification icon remains responsive during that grace period.
        unsafe {
            SetTimer(
                Some(window),
                ONBOARDING_TIMER_ID,
                ONBOARDING_RETRY_INTERVAL_MS,
                None,
            );
        }
    }
    start_update_check(window);
    // The timer consumes no CPU between the tray's bounded signed-feed checks.
    unsafe {
        SetTimer(
            Some(window),
            UPDATE_TIMER_ID,
            UPDATE_CHECK_INTERVAL_MS,
            None,
        );
    }
    run_message_loop()
}

fn add_tray_icon(window: HWND, icon: HICON) -> Result<()> {
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

fn select_icon_image(bytes: &[u8], preferred_size: u16) -> Option<&[u8]> {
    if read_u16(bytes, 0)? != 0 || read_u16(bytes, 2)? != 1 {
        return None;
    }

    let count = usize::from(read_u16(bytes, 4)?);
    let directory_end = 6usize.checked_add(count.checked_mul(16)?)?;
    if count == 0 || directory_end > bytes.len() {
        return None;
    }

    let preferred_size = u32::from(preferred_size);
    let mut selected: Option<(u32, &[u8])> = None;
    for index in 0..count {
        let entry_offset = 6 + index * 16;
        let entry = bytes.get(entry_offset..entry_offset + 16)?;
        let width = if entry[0] == 0 {
            256
        } else {
            u32::from(entry[0])
        };
        let height = if entry[1] == 0 {
            256
        } else {
            u32::from(entry[1])
        };
        let image_size = usize::try_from(read_u32(entry, 8)?).ok()?;
        let image_offset = usize::try_from(read_u32(entry, 12)?).ok()?;
        let image_end = image_offset.checked_add(image_size)?;
        let image = bytes.get(image_offset..image_end)?;
        let score = width.abs_diff(preferred_size) + height.abs_diff(preferred_size);

        if selected
            .as_ref()
            .is_none_or(|(selected_score, _)| score < *selected_score)
        {
            selected = Some((score, image));
        }
    }

    selected.map(|(_, image)| image)
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let raw: [u8; 2] = bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?;
    Some(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(raw))
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
                WM_LBUTTONDBLCLK => {
                    let _ = launch_ui(window, UiDestination::Default);
                }
                NIN_BALLOONUSERCLICK => {
                    let _ = launch_ui(window, UiDestination::Updates);
                }
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
        WM_TIMER if wparam.0 == ONBOARDING_TIMER_ID => {
            let attempts = ONBOARDING_ATTEMPTS.fetch_add(1, Ordering::Relaxed) + 1;
            if maybe_launch_onboarding(window) || attempts >= ONBOARDING_MAX_ATTEMPTS {
                // SAFETY: this removes only the timer created for this window.
                let _ = unsafe { KillTimer(Some(window), ONBOARDING_TIMER_ID) };
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == UPDATE_TIMER_ID => {
            start_update_check(window);
            LRESULT(0)
        }
        UPDATE_AVAILABLE_MESSAGE => {
            if wparam.0 != 0 {
                let _ = show_update_notification(window);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: harmless if the onboarding timer has already been removed.
            let _ = unsafe { KillTimer(Some(window), ONBOARDING_TIMER_ID) };
            let _ = unsafe { KillTimer(Some(window), UPDATE_TIMER_ID) };
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
            if UPDATE_AVAILABLE.load(Ordering::Relaxed) {
                AppendMenuW(menu, MF_STRING, MENU_UPDATE, w!("Update available…"))?;
            }
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
            MENU_OPEN => {
                let _ = launch_ui(window, UiDestination::Default);
            }
            MENU_UPDATE => {
                let _ = launch_ui(window, UiDestination::Updates);
            }
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

fn launch_ui(window: HWND, destination: UiDestination) -> bool {
    let Ok(mut executable) = std::env::current_exe() else {
        show_error(
            window,
            "The resticpal installation path could not be found.",
        );
        return false;
    };
    executable.set_file_name("resticpal-ui.exe");
    let executable = wide_null(&executable.to_string_lossy());
    let arguments = match destination {
        UiDestination::Default => w!(""),
        UiDestination::Setup => w!("--setup"),
        UiDestination::Updates => w!("--updates"),
    };
    // SAFETY: the executable path is live and null terminated. ShellExecute
    // handles the UAC consent flow required by the settings application.
    let result = unsafe {
        ShellExecuteW(
            Some(window),
            w!("runas"),
            windows::core::PCWSTR(executable.as_ptr()),
            arguments,
            w!(""),
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize <= 32 {
        show_error(
            window,
            &format!(
                "The resticpal settings application could not be opened.\n\nShell error {}",
                result.0 as isize
            ),
        );
        false
    } else {
        true
    }
}

fn maybe_launch_onboarding(window: HWND) -> bool {
    let marker_exists = onboarding_marker_path().is_some_and(|path| path.is_file());
    if marker_exists {
        return true;
    }
    let Ok(response) = send_request(RequestCommand::GetStatus) else {
        return false;
    };
    if let ResponsePayload::Status { status } = response.payload {
        if should_launch_onboarding(marker_exists, &status.state) {
            let _ = launch_ui(window, UiDestination::Setup);
            return true;
        }
        return !matches!(status.state, BackupState::Waiting { .. });
    }
    false
}

fn onboarding_marker_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|root| {
        PathBuf::from(root)
            .join("resticpal")
            .join("onboarding-shown-v1")
    })
}

fn should_launch_onboarding(marker_exists: bool, state: &BackupState) -> bool {
    !marker_exists && matches!(state, BackupState::Unconfigured)
}

fn start_update_check(window: HWND) {
    if UPDATE_CHECK_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    let window_value = window.0 as isize;
    std::thread::spawn(move || {
        match check_for_update() {
            Some(true) => {
                UPDATE_AVAILABLE.store(true, Ordering::Relaxed);
                let prompt = record_update_prompt_if_due(SystemTime::now());
                // SAFETY: the integer originated from the live HWND. A posted
                // message safely fails if the tray exits before the check.
                let window = HWND(window_value as *mut core::ffi::c_void);
                let _ = unsafe {
                    PostMessageW(
                        Some(window),
                        UPDATE_AVAILABLE_MESSAGE,
                        WPARAM(usize::from(prompt)),
                        LPARAM(0),
                    )
                };
            }
            Some(false) => UPDATE_AVAILABLE.store(false, Ordering::Relaxed),
            None => {}
        }
        UPDATE_CHECK_RUNNING.store(false, Ordering::Release);
    });
}

fn check_for_update() -> Option<bool> {
    let agent = update_http_agent();
    let appcast = fetch_bounded(&agent, UPDATE_APPCAST_URL, MAX_UPDATE_APPCAST_BYTES)?;
    let signature = fetch_bounded(
        &agent,
        UPDATE_APPCAST_SIGNATURE_URL,
        MAX_UPDATE_SIGNATURE_BYTES,
    )?;
    let signature = std::str::from_utf8(&signature).ok()?;
    if !verify_signed_document(&appcast, signature, UPDATE_PUBLIC_KEY) {
        return None;
    }

    let appcast = std::str::from_utf8(&appcast).ok()?;
    let available = extract_appcast_version(appcast)?;
    let current = parse_product_version(env!("CARGO_PKG_VERSION"))?;
    Some(available > current)
}

fn update_http_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(UPDATE_HTTP_TIMEOUT))
        .tls_config(
            TlsConfig::builder()
                .provider(TlsProvider::NativeTls)
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .user_agent(concat!("resticpal/", env!("CARGO_PKG_VERSION")))
        .build();
    config.into()
}

fn fetch_bounded(agent: &ureq::Agent, url: &str, max_bytes: usize) -> Option<Vec<u8>> {
    let mut response = agent.get(url).call().ok()?;
    let body = response
        .body_mut()
        .with_config()
        .limit(u64::try_from(max_bytes.checked_add(1)?).ok()?)
        .read_to_vec()
        .ok()?;
    (body.len() <= max_bytes).then_some(body)
}

fn verify_signed_document(document: &[u8], signature: &str, public_key: &str) -> bool {
    let Ok(public_key) = STANDARD.decode(public_key.trim()) else {
        return false;
    };
    let public_key: [u8; 32] = match public_key.try_into() {
        Ok(public_key) => public_key,
        Err(_) => return false,
    };
    let Ok(verifying_key) = VerifyingKey::from_bytes(&public_key) else {
        return false;
    };
    let Ok(signature) = STANDARD.decode(signature.trim()) else {
        return false;
    };
    let signature: [u8; 64] = match signature.try_into() {
        Ok(signature) => signature,
        Err(_) => return false,
    };

    verifying_key
        .verify_strict(document, &Signature::from_bytes(&signature))
        .is_ok()
}

fn extract_appcast_version(document: &str) -> Option<[u64; 3]> {
    const OPEN: &str = "<sparkle:version>";
    const CLOSE: &str = "</sparkle:version>";
    let value_start = document.find(OPEN)?.checked_add(OPEN.len())?;
    let value_end = value_start.checked_add(document.get(value_start..)?.find(CLOSE)?)?;
    parse_product_version(document.get(value_start..value_end)?.trim())
}

fn parse_product_version(value: &str) -> Option<[u64; 3]> {
    let mut components = value.split('.');
    let version = [
        parse_version_component(components.next()?)?,
        parse_version_component(components.next()?)?,
        parse_version_component(components.next()?)?,
    ];
    components.next().is_none().then_some(version)
}

fn parse_version_component(value: &str) -> Option<u64> {
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

fn update_prompt_marker_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|root| {
        PathBuf::from(root)
            .join("resticpal")
            .join("update-prompted")
    })
}

fn record_update_prompt_if_due(now: SystemTime) -> bool {
    let Some(path) = update_prompt_marker_path() else {
        return true;
    };
    let last_prompt = fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .ok();
    if !update_prompt_is_due(now, last_prompt) {
        return false;
    }
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, b"1");
    true
}

fn update_prompt_is_due(now: SystemTime, last_prompt: Option<SystemTime>) -> bool {
    last_prompt.is_none_or(|last| {
        now.duration_since(last)
            .is_ok_and(|elapsed| elapsed >= UPDATE_PROMPT_INTERVAL)
    })
}

fn show_update_notification(window: HWND) -> Result<()> {
    let mut data = NOTIFYICONDATAW {
        cbSize: u32::try_from(size_of::<NOTIFYICONDATAW>())
            .expect("NOTIFYICONDATAW size fits in u32"),
        hWnd: window,
        uID: TRAY_ICON_ID,
        uFlags: NIF_INFO,
        dwInfoFlags: NIIF_INFO,
        ..NOTIFYICONDATAW::default()
    };
    copy_wide("resticpal update available", &mut data.szInfoTitle);
    copy_wide(
        "A signed resticpal update is ready. Click to review and install it.",
        &mut data.szInfo,
    );
    data.Anonymous.uTimeout = 10_000;

    // SAFETY: data identifies the notification icon owned by the live window.
    if unsafe { Shell_NotifyIconW(NIM_MODIFY, &raw const data) }.as_bool() {
        Ok(())
    } else {
        Err(Error::from_thread())
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
        ResponsePayload::RunHistory { .. }
        | ResponsePayload::Management { .. }
        | ResponsePayload::BackupSources { .. }
        | ResponsePayload::DiscoveredBackupSources { .. }
        | ResponsePayload::Repository { .. }
        | ResponsePayload::Schedule { .. }
        | ResponsePayload::Retention { .. }
        | ResponsePayload::Diagnostics { .. } => {
            "resticpal: unexpected service response".to_owned()
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn embedded_tray_icon_contains_the_preferred_png_image() {
        let image = select_icon_image(TRAY_ICON_BYTES, PREFERRED_TRAY_ICON_SIZE)
            .expect("embedded tray icon should contain a valid image");

        assert_eq!(image.get(..8), Some(b"\x89PNG\r\n\x1a\n".as_slice()));
    }

    #[test]
    fn windows_can_create_the_embedded_tray_icon() {
        let image = select_icon_image(TRAY_ICON_BYTES, PREFERRED_TRAY_ICON_SIZE)
            .expect("embedded tray icon should contain a valid image");
        // SAFETY: `image` is a complete, bounded icon image owned by the static ICO.
        let icon =
            unsafe { CreateIconFromResourceEx(image, true, 0x0003_0000, 0, 0, LR_DEFAULTCOLOR) }
                .expect("Windows should create an icon from the embedded image");

        // SAFETY: the icon was created by this test and has not been destroyed yet.
        unsafe { DestroyIcon(icon) }.expect("Windows should destroy the test icon");
    }

    #[test]
    fn icon_selection_rejects_truncated_directories() {
        let truncated = [0, 0, 1, 0, 1, 0, 32, 32];

        assert!(select_icon_image(&truncated, 32).is_none());
    }

    #[test]
    fn onboarding_is_offered_once_for_an_unconfigured_user() {
        assert!(should_launch_onboarding(false, &BackupState::Unconfigured));
        assert!(!should_launch_onboarding(true, &BackupState::Unconfigured));
        assert!(!should_launch_onboarding(false, &BackupState::Idle));
    }

    #[test]
    fn update_prompt_is_bounded_to_once_per_day() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(200_000);
        assert!(update_prompt_is_due(now, None));
        assert!(!update_prompt_is_due(
            now,
            Some(now - Duration::from_secs(60 * 60))
        ));
        assert!(update_prompt_is_due(
            now,
            Some(now - UPDATE_PROMPT_INTERVAL)
        ));
        assert!(!update_prompt_is_due(
            now,
            Some(now + Duration::from_secs(60))
        ));
    }

    #[test]
    fn appcast_versions_are_strictly_parsed_and_compared() {
        let appcast = "<rss><sparkle:version>1.2.30</sparkle:version></rss>";
        assert_eq!(extract_appcast_version(appcast), Some([1, 2, 30]));
        assert!(extract_appcast_version(appcast).unwrap() > [1, 2, 3]);

        assert_eq!(parse_product_version("1.0.1"), Some([1, 0, 1]));
        assert_eq!(parse_product_version("1.0"), None);
        assert_eq!(parse_product_version("1.0.1.0"), None);
        assert_eq!(parse_product_version("1.0-beta"), None);
        assert_eq!(extract_appcast_version("<rss />"), None);
    }

    #[test]
    fn update_client_uses_windows_native_tls_and_platform_roots() {
        let agent = update_http_agent();
        let tls = agent.config().tls_config();

        assert_eq!(tls.provider(), TlsProvider::NativeTls);
        assert!(matches!(tls.root_certs(), RootCerts::PlatformVerifier));
    }

    #[test]
    fn detached_update_signature_rejects_tampering() {
        let signing_key = SigningKey::from_bytes(&[42; 32]);
        let document = b"<rss><sparkle:version>1.0.2</sparkle:version></rss>";
        let signature = signing_key.sign(document);
        let signature = STANDARD.encode(signature.to_bytes());
        let public_key = STANDARD.encode(signing_key.verifying_key().to_bytes());

        assert!(verify_signed_document(document, &signature, &public_key));
        assert!(!verify_signed_document(
            b"<rss><sparkle:version>9.0.0</sparkle:version></rss>",
            &signature,
            &public_key
        ));
        assert!(!verify_signed_document(document, "not-base64", &public_key));
    }

    #[test]
    #[ignore = "contacts the live signed release feed"]
    fn live_signed_update_feed_is_valid() {
        assert!(check_for_update().is_some());
    }
}
