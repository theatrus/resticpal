#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::VecDeque;
use std::fs;
use std::mem::size_of;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicIsize, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use resticpal_core::status::{BackupState, ServiceStatus, WaitingReason};
use resticpal_protocol::{Request, RequestCommand, Response, ResponsePayload, UpdatePackage};
use resticpal_windows::named_pipe::{NamedPipeClient, NamedPipeError};
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{LIM_SMALL, LoadIconMetric};
use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NIN_BALLOONHIDE, NIN_BALLOONSHOW, NIN_BALLOONTIMEOUT,
    NIN_BALLOONUSERCLICK, NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
    ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CW_USEDEFAULT, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DestroyWindow, DispatchMessageW, FindWindowW, GetCursorPos, GetMessageTime,
    GetMessageW, HICON, IDC_ARROW, KillTimer, LoadCursorW, MB_ICONERROR, MB_OK, MENU_ITEM_FLAGS,
    MSG, MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
    SW_SHOWNORMAL, SetForegroundWindow, SetTimer, TPM_NONOTIFY, TPM_RETURNCMD, TrackPopupMenu,
    TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY,
    WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::{Error, PCWSTR, Result, w};

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
const UPDATE_RETRY_TIMER_ID: usize = 4;
const BACKUP_STATUS_TIMER_ID: usize = 5;
const BACKUP_STATUS_INTERVAL_MS: u32 = 1_000;
const BACKUP_WAITING_INTERVAL_MS: u32 = 30_000;
const BACKUP_FAST_POLL_TICKS: u64 = 15;
const BACKUP_MONITOR_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);
const UPDATE_CHECK_INTERVAL_MS: u32 = 6 * 60 * 60 * 1_000;
const UPDATE_RETRY_INTERVAL_MS: u32 = 5 * 60 * 1_000;
const UPDATE_ACCEPTED_FOLLOW_UP_INTERVAL_MS: u32 = UPDATE_CHECK_INTERVAL_MS;
const UPDATE_AVAILABLE_MESSAGE: u32 = WM_APP + 2;
const NIN_KEYSELECT_EVENT: u32 = NIN_SELECT | 1;
const MAX_PENDING_BALLOON_ROUTES: usize = 16;
const UPDATE_PROMPT_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const UPDATE_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_UPDATE_APPCAST_BYTES: usize = 256 * 1024;
const MAX_UPDATE_SIGNATURE_BYTES: usize = 1024;
const UPDATE_APPCAST_SOURCES: &[UpdateSource] = &[
    UpdateSource {
        appcast_url: "https://updates.resticpal.com/appcast-v2.xml",
        signature_url: "https://updates.resticpal.com/appcast-v2.xml.signature",
    },
    UpdateSource {
        appcast_url: "https://github.com/theatrus/resticpal/releases/latest/download/appcast-v2.xml",
        signature_url: "https://github.com/theatrus/resticpal/releases/latest/download/appcast-v2.xml.signature",
    },
];
const UPDATE_PUBLIC_KEY: &str = include_str!("../../../config/update-public-key.txt");
const MF_STRING: MENU_ITEM_FLAGS = MENU_ITEM_FLAGS(0);
const TRAY_ICON_RESOURCE_ID: usize = 1;
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
static ONBOARDING_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static LAST_LEFT_CLICK_MESSAGE_TIME: AtomicU64 = AtomicU64::new(u64::MAX);
static UI_LAUNCH_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static TASKBAR_CREATED_MESSAGE: AtomicU64 = AtomicU64::new(0);
static TRAY_ICON_HANDLE: AtomicIsize = AtomicIsize::new(0);
static UPDATE_CHECK_RUNNING: AtomicBool = AtomicBool::new(false);
static UPDATE_AVAILABLE: AtomicBool = AtomicBool::new(false);
static BALLOON_ROUTES: OnceLock<Mutex<BalloonRouteState>> = OnceLock::new();
static BACKUP_MONITOR_BASELINE_ATTEMPT: AtomicI64 = AtomicI64::new(i64::MIN);
static BACKUP_MONITOR_BASELINE_KNOWN: AtomicBool = AtomicBool::new(false);
static BACKUP_MONITOR_BASELINE_HAS_ATTEMPT: AtomicBool = AtomicBool::new(false);
static BACKUP_MONITOR_TICKS: AtomicU64 = AtomicU64::new(0);
static BACKUP_MONITOR_STARTED: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
static BACKUP_MONITOR_OBSERVED_RUNNING: AtomicBool = AtomicBool::new(false);
static BACKUP_STARTED_NOTIFICATION_SHOWN: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UiDestination {
    Default,
    Setup,
    Updates,
}

#[derive(Default)]
struct BalloonRouteState {
    pending: VecDeque<UiDestination>,
    active: Option<UiDestination>,
}

impl BalloonRouteState {
    fn reserve(&mut self, destination: UiDestination) -> bool {
        if self.pending.len() >= MAX_PENDING_BALLOON_ROUTES {
            return false;
        }
        self.pending.push_back(destination);
        true
    }

    fn cancel_latest(&mut self) {
        self.pending.pop_back();
    }

    fn shown(&mut self) {
        if let Some(destination) = self.pending.pop_front() {
            self.active = Some(destination);
        }
    }

    fn dismissed(&mut self) {
        if self.active.take().is_none() {
            self.pending.pop_front();
        }
    }

    fn clicked(&mut self) -> UiDestination {
        self.active
            .take()
            .or_else(|| self.pending.pop_front())
            .unwrap_or(UiDestination::Default)
    }

    fn clear(&mut self) {
        self.pending.clear();
        self.active = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrayIconAction {
    OpenSettings,
    OpenUpdates,
    ShowContextMenu,
}

#[derive(Clone, Copy)]
struct UpdateSource {
    appcast_url: &'static str,
    signature_url: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BackupAttemptBaseline {
    known: bool,
    attempt: Option<i64>,
}

impl BackupAttemptBaseline {
    const UNKNOWN: Self = Self {
        known: false,
        attempt: None,
    };

    fn from_status(status: ServiceStatus) -> Self {
        Self {
            known: true,
            attempt: status
                .last_attempt
                .map(|attempt| attempt.timestamp_millis()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UpdateCheck {
    Current,
    Available(UpdatePackage),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutomaticUpdateSetting {
    Enabled,
    Disabled,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AvailableUpdateAction {
    RequestSilentInstall,
    Prompt,
    RetrySilently,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserUpdateAction {
    OpenManualUi,
    CheckSilently,
    RetrySilently,
}

const fn available_update_action(setting: AutomaticUpdateSetting) -> AvailableUpdateAction {
    match setting {
        AutomaticUpdateSetting::Enabled => AvailableUpdateAction::RequestSilentInstall,
        AutomaticUpdateSetting::Disabled => AvailableUpdateAction::Prompt,
        AutomaticUpdateSetting::Unavailable => AvailableUpdateAction::RetrySilently,
    }
}

const fn automatic_update_follow_up_interval(accepted: bool) -> u32 {
    if accepted {
        // Acceptance queues asynchronous work. If it later fails, retry at the
        // normal six-hour cadence rather than redownloading a bad 512 MiB asset
        // every five minutes. A successful upgrade exits this tray first.
        UPDATE_ACCEPTED_FOLLOW_UP_INTERVAL_MS
    } else {
        UPDATE_RETRY_INTERVAL_MS
    }
}

const fn silent_recheck_follow_up_required(_check_started: bool) -> bool {
    true
}

const fn user_update_action(setting: AutomaticUpdateSetting) -> UserUpdateAction {
    match setting {
        AutomaticUpdateSetting::Disabled => UserUpdateAction::OpenManualUi,
        AutomaticUpdateSetting::Enabled => UserUpdateAction::CheckSilently,
        AutomaticUpdateSetting::Unavailable => UserUpdateAction::RetrySilently,
    }
}

struct TrayIcon {
    handle: HICON,
}

struct LaunchGuard<'a> {
    flag: &'a AtomicBool,
}

impl<'a> LaunchGuard<'a> {
    fn try_acquire(flag: &'a AtomicBool) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .ok()
            .map(|_| Self { flag })
    }
}

impl Drop for LaunchGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}

impl TrayIcon {
    fn load(instance: HINSTANCE) -> Result<Self> {
        // MAKEINTRESOURCEW encodes a numeric resource identifier in a PCWSTR.
        // LoadIconMetric selects an exact small-icon frame when available and
        // otherwise downsamples the next larger frame for the active DPI.
        let resource = PCWSTR(TRAY_ICON_RESOURCE_ID as *const u16);
        // SAFETY: resource ID 1 is the ICO embedded by build.rs, and the v6
        // common-controls dependency is activated by the required manifest.
        let handle = unsafe { LoadIconMetric(Some(instance), resource, LIM_SMALL) }?;
        Ok(Self { handle })
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        // SAFETY: LoadIconMetric transfers an owned icon handle, created once
        // above and kept live until the notification-area icon is removed.
        let _ = unsafe { DestroyIcon(self.handle) };
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
    let tray_icon = TrayIcon::load(instance)?;
    // Explorer broadcasts this registered message after recreating its taskbar.
    // The notification icon otherwise disappears while this process keeps running.
    let taskbar_created_message = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    if taskbar_created_message == 0 {
        return Err(Error::from_thread());
    }
    TASKBAR_CREATED_MESSAGE.store(u64::from(taskbar_created_message), Ordering::Relaxed);
    TRAY_ICON_HANDLE.store(tray_icon.handle.0 as isize, Ordering::Relaxed);

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
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &raw const data) }.as_bool() {
        return Err(Error::from_thread());
    }

    data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    if !unsafe { Shell_NotifyIconW(NIM_SETVERSION, &raw const data) }.as_bool() {
        remove_tray_icon(window);
        return Err(Error::from_thread());
    }
    Ok(())
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
    // Windows APIs such as the UAC consent flow can dispatch nested window
    // messages. Never allow a Rust panic to cross this FFI callback boundary.
    catch_unwind(AssertUnwindSafe(|| {
        window_proc_inner(window, message, wparam, lparam)
    }))
    .unwrap_or_else(|_| {
        // SAFETY: after a failed handler, let Windows provide the normal
        // fallback behavior for the original message.
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    })
}

fn window_proc_inner(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if is_taskbar_created_message(
        message,
        u32::try_from(TASKBAR_CREATED_MESSAGE.load(Ordering::Relaxed)).unwrap_or_default(),
    ) {
        let icon = TRAY_ICON_HANDLE.load(Ordering::Relaxed);
        if icon != 0 {
            with_balloon_routes(BalloonRouteState::clear);
            let _ = add_tray_icon(window, HICON(icon as *mut core::ffi::c_void));
        }
        return LRESULT(0);
    }

    match message {
        TRAY_CALLBACK => {
            let event = tray_callback_event(lparam.0);
            if event == NIN_BALLOONSHOW {
                with_balloon_routes(BalloonRouteState::shown);
                return LRESULT(0);
            }
            if event == NIN_BALLOONHIDE || event == NIN_BALLOONTIMEOUT {
                with_balloon_routes(BalloonRouteState::dismissed);
                return LRESULT(0);
            }
            let balloon_opens_updates = event == NIN_BALLOONUSERCLICK
                && with_balloon_routes(BalloonRouteState::clicked) == UiDestination::Updates;
            match tray_icon_action(event, balloon_opens_updates) {
                Some(TrayIconAction::OpenSettings) => {
                    // SAFETY: these functions only read system input timing state.
                    let message_time = unsafe { GetMessageTime() } as u32;
                    let double_click_time = unsafe { GetDoubleClickTime() };
                    let previous = LAST_LEFT_CLICK_MESSAGE_TIME.load(Ordering::Relaxed);
                    let previous = (previous != u64::MAX).then_some(previous as u32);
                    if tray_left_click_is_distinct(previous, message_time, double_click_time) {
                        LAST_LEFT_CLICK_MESSAGE_TIME
                            .store(u64::from(message_time), Ordering::Relaxed);
                        let _ = launch_ui(window, UiDestination::Default);
                    }
                }
                Some(TrayIconAction::OpenUpdates) => {
                    handle_user_update_request(window);
                }
                Some(TrayIconAction::ShowContextMenu) => {
                    if let Err(error) = show_context_menu(window) {
                        show_error(window, &error.to_string());
                    }
                }
                None => {}
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
        WM_TIMER if wparam.0 == UPDATE_RETRY_TIMER_ID => {
            // SAFETY: this removes only the one-shot retry timer below.
            let _ = unsafe { KillTimer(Some(window), UPDATE_RETRY_TIMER_ID) };
            start_update_check(window);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == BACKUP_STATUS_TIMER_ID => {
            poll_backup_status(window);
            LRESULT(0)
        }
        UPDATE_AVAILABLE_MESSAGE => {
            if wparam.0 != 0 {
                match user_update_action(automatic_update_setting()) {
                    UserUpdateAction::OpenManualUi => {
                        let _ = show_update_notification(window);
                    }
                    UserUpdateAction::CheckSilently => {
                        check_update_silently(window);
                    }
                    UserUpdateAction::RetrySilently => {
                        UPDATE_AVAILABLE.store(false, Ordering::Relaxed);
                        schedule_update_retry(window);
                    }
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: harmless if the onboarding timer has already been removed.
            let _ = unsafe { KillTimer(Some(window), ONBOARDING_TIMER_ID) };
            let _ = unsafe { KillTimer(Some(window), UPDATE_TIMER_ID) };
            let _ = unsafe { KillTimer(Some(window), UPDATE_RETRY_TIMER_ID) };
            let _ = unsafe { KillTimer(Some(window), BACKUP_STATUS_TIMER_ID) };
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

fn is_taskbar_created_message(message: u32, registered_message: u32) -> bool {
    registered_message != 0 && message == registered_message
}

fn with_balloon_routes<T>(action: impl FnOnce(&mut BalloonRouteState) -> T) -> T {
    let routes = BALLOON_ROUTES.get_or_init(|| Mutex::new(BalloonRouteState::default()));
    let mut routes = routes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    action(&mut routes)
}

fn show_context_menu(window: HWND) -> Result<()> {
    // SAFETY: Windows owns the implementation; the returned menu is destroyed below.
    let menu = unsafe { CreatePopupMenu() }?;
    let result = (|| {
        let backup_running = backup_is_running().unwrap_or(false);
        // SAFETY: labels are static null-terminated strings and menu is valid.
        let update_available = if UPDATE_AVAILABLE.load(Ordering::Relaxed) {
            match user_update_action(automatic_update_setting()) {
                UserUpdateAction::OpenManualUi => true,
                UserUpdateAction::CheckSilently => {
                    check_update_silently(window);
                    false
                }
                UserUpdateAction::RetrySilently => {
                    UPDATE_AVAILABLE.store(false, Ordering::Relaxed);
                    schedule_update_retry(window);
                    false
                }
            }
        } else {
            false
        };
        unsafe {
            AppendMenuW(menu, MF_STRING, MENU_OPEN, w!("Open resticpal"))?;
            if update_available {
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
                handle_user_update_request(window);
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

fn handle_user_update_request(window: HWND) {
    match user_update_action(automatic_update_setting()) {
        UserUpdateAction::OpenManualUi => {
            let _ = launch_ui(window, UiDestination::Updates);
        }
        UserUpdateAction::CheckSilently => {
            check_update_silently(window);
        }
        UserUpdateAction::RetrySilently => {
            UPDATE_AVAILABLE.store(false, Ordering::Relaxed);
            schedule_update_retry(window);
        }
    }
}

fn check_update_silently(window: HWND) {
    UPDATE_AVAILABLE.store(false, Ordering::Relaxed);
    let check_started = start_update_check(window);
    // A policy revalidation can arrive while the prior check's worker still
    // owns the single-flight flag. Coalesce a bounded retry either way so the
    // silent handoff cannot be lost until the six-hour periodic timer.
    if silent_recheck_follow_up_required(check_started) {
        schedule_update_retry(window);
    }
}

fn tray_left_click_is_distinct(
    previous_message_time: Option<u32>,
    message_time: u32,
    double_click_time: u32,
) -> bool {
    previous_message_time
        .is_none_or(|previous| message_time.wrapping_sub(previous) > double_click_time)
}

fn tray_callback_event(raw_lparam: isize) -> u32 {
    // NOTIFYICON_VERSION_4 packs the event into LOWORD(lParam) and the icon ID
    // into HIWORD(lParam); older versions put the event in the full value.
    u32::try_from((raw_lparam as usize) & usize::from(u16::MAX)).unwrap_or_default()
}

fn tray_icon_action(event: u32, balloon_opens_updates: bool) -> Option<TrayIconAction> {
    match event {
        WM_LBUTTONUP | WM_LBUTTONDBLCLK | NIN_SELECT | NIN_KEYSELECT_EVENT => {
            Some(TrayIconAction::OpenSettings)
        }
        NIN_BALLOONUSERCLICK if balloon_opens_updates => Some(TrayIconAction::OpenUpdates),
        NIN_BALLOONUSERCLICK => Some(TrayIconAction::OpenSettings),
        WM_RBUTTONUP | WM_CONTEXTMENU => Some(TrayIconAction::ShowContextMenu),
        _ => None,
    }
}

fn launch_ui(window: HWND, destination: UiDestination) -> bool {
    // ShellExecuteW pumps window messages while Windows displays UAC consent.
    // A timer or click delivered during that nested loop must not start another
    // elevation request on the same call stack.
    let Some(_launch_guard) = LaunchGuard::try_acquire(&UI_LAUNCH_IN_PROGRESS) else {
        return true;
    };

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
            // Stop retry delivery before ShellExecuteW enters the nested UAC
            // message loop. A rejected prompt remains user-recoverable from
            // the tray without generating another prompt automatically.
            let _ = unsafe { KillTimer(Some(window), ONBOARDING_TIMER_ID) };
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

fn start_update_check(window: HWND) -> bool {
    if UPDATE_CHECK_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return false;
    }

    let window_value = window.0 as isize;
    std::thread::spawn(move || {
        match check_for_update() {
            Some(UpdateCheck::Available(package)) => {
                let window = HWND(window_value as *mut core::ffi::c_void);
                match available_update_action(automatic_update_setting()) {
                    AvailableUpdateAction::RequestSilentInstall => {
                        // Automatic mode has no manual notification or menu
                        // fallback, including while the service is busy. Keep a
                        // bounded follow-up even after the service accepts: the
                        // download, signature check, or MSI can still fail
                        // asynchronously. A successful upgrade exits this tray.
                        UPDATE_AVAILABLE.store(false, Ordering::Relaxed);
                        let accepted = request_automatic_update(package);
                        schedule_update_retry_after(
                            window,
                            automatic_update_follow_up_interval(accepted),
                        );
                    }
                    AvailableUpdateAction::Prompt => {
                        UPDATE_AVAILABLE.store(true, Ordering::Relaxed);
                        if record_update_prompt_if_due(SystemTime::now()) {
                            // SAFETY: the integer originated from the live HWND. A
                            // posted message safely fails if the tray exits first.
                            let _ = unsafe {
                                PostMessageW(
                                    Some(window),
                                    UPDATE_AVAILABLE_MESSAGE,
                                    WPARAM(1),
                                    LPARAM(0),
                                )
                            };
                        }
                    }
                    AvailableUpdateAction::RetrySilently => {
                        UPDATE_AVAILABLE.store(false, Ordering::Relaxed);
                        schedule_update_retry(window);
                    }
                }
            }
            Some(UpdateCheck::Current) => UPDATE_AVAILABLE.store(false, Ordering::Relaxed),
            None => {}
        }
        UPDATE_CHECK_RUNNING.store(false, Ordering::Release);
    });
    true
}

fn schedule_update_retry(window: HWND) {
    schedule_update_retry_after(window, UPDATE_RETRY_INTERVAL_MS);
}

fn schedule_update_retry_after(window: HWND, interval_ms: u32) {
    // SAFETY: `window` originated from the live tray HWND. The timer is removed
    // on delivery or window destruction, and using the same ID coalesces retries.
    unsafe {
        SetTimer(Some(window), UPDATE_RETRY_TIMER_ID, interval_ms, None);
    }
}

fn check_for_update() -> Option<UpdateCheck> {
    let agent = update_http_agent();
    check_update_sources(UPDATE_APPCAST_SOURCES, |source| {
        check_update_source(&agent, source)
    })
}

fn check_update_sources(
    sources: &[UpdateSource],
    mut check: impl FnMut(UpdateSource) -> Option<UpdateCheck>,
) -> Option<UpdateCheck> {
    let mut current_feed_seen = false;
    for source in sources.iter().copied() {
        match check(source) {
            Some(available @ UpdateCheck::Available(_)) => return Some(available),
            Some(UpdateCheck::Current) => current_feed_seen = true,
            None => {}
        }
    }
    current_feed_seen.then_some(UpdateCheck::Current)
}

fn check_update_source(agent: &ureq::Agent, source: UpdateSource) -> Option<UpdateCheck> {
    let appcast = fetch_bounded(agent, source.appcast_url, MAX_UPDATE_APPCAST_BYTES)?;
    let signature = fetch_bounded(agent, source.signature_url, MAX_UPDATE_SIGNATURE_BYTES)?;
    let signature = std::str::from_utf8(&signature).ok()?;
    if !verify_signed_document(&appcast, signature, UPDATE_PUBLIC_KEY) {
        return None;
    }

    let appcast = std::str::from_utf8(&appcast).ok()?;
    let package = extract_update_package(appcast)?;
    let available = parse_product_version(&package.version)?;
    let current = parse_product_version(env!("CARGO_PKG_VERSION"))?;
    Some(if available > current {
        UpdateCheck::Available(package)
    } else {
        UpdateCheck::Current
    })
}

fn automatic_update_setting() -> AutomaticUpdateSetting {
    match send_request(RequestCommand::GetUpdateSettings) {
        Ok(Response {
            payload: ResponsePayload::UpdateSettings { configuration },
            ..
        }) => {
            if configuration.automatic_install {
                AutomaticUpdateSetting::Enabled
            } else {
                AutomaticUpdateSetting::Disabled
            }
        }
        _ => AutomaticUpdateSetting::Unavailable,
    }
}

fn request_automatic_update(package: UpdatePackage) -> bool {
    matches!(
        send_request(RequestCommand::InstallUpdate { package }),
        Ok(Response {
            payload: ResponsePayload::Accepted { .. },
            ..
        })
    )
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

#[cfg(test)]
fn extract_appcast_version(document: &str) -> Option<[u64; 3]> {
    const OPEN: &str = "<sparkle:version>";
    const CLOSE: &str = "</sparkle:version>";
    let value_start = document.find(OPEN)?.checked_add(OPEN.len())?;
    let value_end = value_start.checked_add(document.get(value_start..)?.find(CLOSE)?)?;
    parse_product_version(document.get(value_start..value_end)?.trim())
}

fn extract_update_package(document: &str) -> Option<UpdatePackage> {
    let version = extract_between(document, "<sparkle:version>", "</sparkle:version>")?
        .trim()
        .to_owned();
    parse_product_version(&version)?;
    let enclosure_start = document.find("<enclosure ")?;
    let enclosure = document.get(enclosure_start..)?;
    let enclosure_end = enclosure.find('>')?;
    let enclosure = enclosure.get(..=enclosure_end)?;
    Some(UpdatePackage {
        version,
        url: extract_xml_attribute(enclosure, "url")?.to_owned(),
        signature: extract_xml_attribute(enclosure, "sparkle:signature")?.to_owned(),
        length: extract_xml_attribute(enclosure, "length")?.parse().ok()?,
    })
}

fn extract_between<'a>(document: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = document.find(open)?.checked_add(open.len())?;
    let end = start.checked_add(document.get(start..)?.find(close)?)?;
    document.get(start..end)
}

fn extract_xml_attribute<'a>(element: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!(" {name}=\"");
    let start = element.find(&marker)?.checked_add(marker.len())?;
    let end = start.checked_add(element.get(start..)?.find('"')?)?;
    element.get(start..end).filter(|value| !value.is_empty())
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
    show_tray_notification(
        window,
        "resticpal update available",
        "A signed resticpal update is ready. Click to review and install it.",
        UiDestination::Updates,
    )
}

fn show_tray_notification(
    window: HWND,
    title: &str,
    message: &str,
    destination: UiDestination,
) -> Result<()> {
    let mut data = NOTIFYICONDATAW {
        cbSize: u32::try_from(size_of::<NOTIFYICONDATAW>())
            .expect("NOTIFYICONDATAW size fits in u32"),
        hWnd: window,
        uID: TRAY_ICON_ID,
        uFlags: NIF_INFO,
        dwInfoFlags: NIIF_INFO,
        ..NOTIFYICONDATAW::default()
    };
    copy_wide(title, &mut data.szInfoTitle);
    copy_wide(message, &mut data.szInfo);
    data.Anonymous.uTimeout = 10_000;
    if !with_balloon_routes(|routes| routes.reserve(destination)) {
        return Ok(());
    }

    // SAFETY: data identifies the notification icon owned by the live window.
    if unsafe { Shell_NotifyIconW(NIM_MODIFY, &raw const data) }.as_bool() {
        Ok(())
    } else {
        with_balloon_routes(BalloonRouteState::cancel_latest);
        Err(Error::from_thread())
    }
}

fn send_backup_action(window: HWND, command: RequestCommand) {
    let run_backup = matches!(&command, RequestCommand::RunBackupNow);
    let baseline_attempt = if run_backup {
        fetch_service_status().ok().flatten().map_or(
            BackupAttemptBaseline::UNKNOWN,
            BackupAttemptBaseline::from_status,
        )
    } else {
        BackupAttemptBaseline::UNKNOWN
    };
    match send_request(command) {
        Ok(Response {
            payload: ResponsePayload::Accepted { message },
            ..
        }) => {
            let _ = show_tray_notification(
                window,
                if run_backup {
                    "Backup requested"
                } else {
                    "Cancellation requested"
                },
                &message,
                UiDestination::Default,
            );
            let _ = refresh_tray_status(window);
            start_backup_status_monitor(window, baseline_attempt, !run_backup);
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
    fetch_service_status().map(|status| {
        status.map_or_else(
            || "resticpal: unexpected service response".to_owned(),
            |status| status_tooltip(&status),
        )
    })
}

fn fetch_service_status() -> std::result::Result<Option<ServiceStatus>, NamedPipeError> {
    let response = send_request(RequestCommand::GetStatus)?;
    match response.payload {
        ResponsePayload::Status { status } => Ok(Some(status)),
        _ => Ok(None),
    }
}

fn status_tooltip(status: &ServiceStatus) -> String {
    match status.state {
        BackupState::Unconfigured => "resticpal: setup required".to_owned(),
        BackupState::Idle | BackupState::Succeeded => "resticpal: protected".to_owned(),
        BackupState::Waiting {
            reason: WaitingReason::Battery,
        } => "resticpal: backup waiting for AC power".to_owned(),
        BackupState::Waiting {
            reason: WaitingReason::Network,
        } => "resticpal: backup waiting for network".to_owned(),
        BackupState::Waiting {
            reason: WaitingReason::MeteredNetwork,
        } => "resticpal: backup waiting for an unmetered network".to_owned(),
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
    }
}

fn backup_is_running() -> std::result::Result<bool, NamedPipeError> {
    Ok(matches!(
        fetch_service_status()?,
        Some(ServiceStatus {
            state: BackupState::Running { .. },
            ..
        })
    ))
}

fn refresh_tray_status(window: HWND) -> std::result::Result<Option<ServiceStatus>, NamedPipeError> {
    let Some(status) = fetch_service_status()? else {
        return Ok(None);
    };
    let tooltip = status_tooltip(&status);
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
        Ok(Some(status))
    } else {
        Err(windows::core::Error::from_thread().into())
    }
}

fn start_backup_status_monitor(
    window: HWND,
    baseline: BackupAttemptBaseline,
    observed_running: bool,
) {
    BACKUP_MONITOR_BASELINE_ATTEMPT.store(baseline.attempt.unwrap_or(i64::MIN), Ordering::Relaxed);
    BACKUP_MONITOR_BASELINE_KNOWN.store(baseline.known, Ordering::Relaxed);
    BACKUP_MONITOR_BASELINE_HAS_ATTEMPT.store(baseline.attempt.is_some(), Ordering::Relaxed);
    BACKUP_MONITOR_TICKS.store(0, Ordering::Relaxed);
    with_backup_monitor_started(|started| *started = Some(Instant::now()));
    BACKUP_MONITOR_OBSERVED_RUNNING.store(observed_running, Ordering::Relaxed);
    BACKUP_STARTED_NOTIFICATION_SHOWN.store(observed_running, Ordering::Relaxed);
    // SAFETY: the timer belongs to the tray's live hidden window and is replaced
    // when another tray backup action starts.
    unsafe {
        SetTimer(
            Some(window),
            BACKUP_STATUS_TIMER_ID,
            BACKUP_STATUS_INTERVAL_MS,
            None,
        );
    }
}

fn poll_backup_status(window: HWND) {
    let status = match refresh_tray_status(window) {
        Ok(Some(status)) => status,
        _ => {
            let ticks = BACKUP_MONITOR_TICKS.fetch_add(1, Ordering::Relaxed) + 1;
            if backup_monitor_timed_out(Instant::now()) {
                finish_backup_status_monitor(
                    window,
                    "Backup status unavailable",
                    "resticpal stopped the dedicated status watch. Open the app to review the service.",
                );
            } else if ticks == BACKUP_FAST_POLL_TICKS {
                // SAFETY: resetting the same live timer reduces polling while
                // the service is temporarily unavailable.
                unsafe {
                    SetTimer(
                        Some(window),
                        BACKUP_STATUS_TIMER_ID,
                        BACKUP_WAITING_INTERVAL_MS,
                        None,
                    );
                }
            }
            return;
        }
    };
    let ticks = BACKUP_MONITOR_TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    if backup_monitor_timed_out(Instant::now()) {
        finish_backup_status_monitor(
            window,
            "Backup still pending",
            "resticpal stopped the dedicated status watch after two hours. Open the app to review current conditions.",
        );
        return;
    }
    let observed_running = BACKUP_MONITOR_OBSERVED_RUNNING.load(Ordering::Relaxed);
    let baseline_known = BACKUP_MONITOR_BASELINE_KNOWN.load(Ordering::Relaxed);
    let baseline_attempt = BACKUP_MONITOR_BASELINE_HAS_ATTEMPT
        .load(Ordering::Relaxed)
        .then(|| BACKUP_MONITOR_BASELINE_ATTEMPT.load(Ordering::Relaxed));
    let attempt_changed = backup_attempt_changed(
        baseline_known,
        baseline_attempt,
        status
            .last_attempt
            .map(|attempt| attempt.timestamp_millis()),
    );
    if ticks == BACKUP_FAST_POLL_TICKS
        && !observed_running
        && !attempt_changed
        && !matches!(status.state, BackupState::Running { .. })
    {
        let _ = show_tray_notification(
            window,
            "Backup waiting",
            "The request is still queued. resticpal will keep watching its power, network, and service conditions.",
            UiDestination::Default,
        );
        // SAFETY: resetting the same live timer reduces idle polling while a
        // start condition remains unavailable.
        unsafe {
            SetTimer(
                Some(window),
                BACKUP_STATUS_TIMER_ID,
                BACKUP_WAITING_INTERVAL_MS,
                None,
            );
        }
    }

    match status.state {
        BackupState::Running { .. } => {
            BACKUP_MONITOR_OBSERVED_RUNNING.store(true, Ordering::Relaxed);
            if !BACKUP_STARTED_NOTIFICATION_SHOWN.swap(true, Ordering::Relaxed) {
                let _ = show_tray_notification(
                    window,
                    "Backup started",
                    "resticpal is now protecting this PC.",
                    UiDestination::Default,
                );
            }
        }
        BackupState::Succeeded if observed_running || attempt_changed => {
            finish_backup_status_monitor(
                window,
                "Backup complete",
                "The latest backup finished successfully.",
            );
        }
        BackupState::SucceededWithWarnings if observed_running || attempt_changed => {
            finish_backup_status_monitor(
                window,
                "Backup completed with warnings",
                "The backup finished, but some files need attention. Open resticpal for details.",
            );
        }
        BackupState::Failed { .. } if observed_running || attempt_changed => {
            finish_backup_status_monitor(
                window,
                "Backup needs attention",
                "The backup did not complete. Open resticpal diagnostics for details.",
            );
        }
        BackupState::Cancelled if observed_running || attempt_changed => {
            finish_backup_status_monitor(
                window,
                "Backup cancelled",
                "The active backup was cancelled.",
            );
        }
        BackupState::Unconfigured | BackupState::Paused => {
            finish_backup_status_monitor(
                window,
                "Backup request stopped",
                "The service can no longer run the requested backup. Open resticpal to review its configuration.",
            );
        }
        _ => {}
    }
}

fn backup_monitor_timed_out(now: Instant) -> bool {
    with_backup_monitor_started(|started| {
        started.is_some_and(|started| {
            now.checked_duration_since(started)
                .is_some_and(|elapsed| elapsed >= BACKUP_MONITOR_TIMEOUT)
        })
    })
}

fn with_backup_monitor_started<T>(action: impl FnOnce(&mut Option<Instant>) -> T) -> T {
    let started = BACKUP_MONITOR_STARTED.get_or_init(|| Mutex::new(None));
    let mut started = started
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    action(&mut started)
}

fn backup_attempt_changed(
    baseline_known: bool,
    baseline_attempt: Option<i64>,
    last_attempt: Option<i64>,
) -> bool {
    baseline_known && last_attempt.is_some() && last_attempt != baseline_attempt
}

fn finish_backup_status_monitor(window: HWND, title: &str, message: &str) {
    // SAFETY: this removes only the backup status timer owned by this window.
    let _ = unsafe { KillTimer(Some(window), BACKUP_STATUS_TIMER_ID) };
    with_backup_monitor_started(|started| *started = None);
    let _ = show_tray_notification(window, title, message, UiDestination::Default);
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
    use windows::Win32::UI::HiDpi::{
        AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        GetThreadDpiAwarenessContext,
    };

    #[test]
    fn launch_guard_rejects_reentrant_launches_and_resets_on_drop() {
        let flag = AtomicBool::new(false);
        let guard = LaunchGuard::try_acquire(&flag).expect("first launch should acquire the guard");

        assert!(LaunchGuard::try_acquire(&flag).is_none());
        drop(guard);
        assert!(LaunchGuard::try_acquire(&flag).is_some());
    }

    #[test]
    fn tray_left_clicks_open_once_per_system_double_click_window() {
        assert!(tray_left_click_is_distinct(None, 1_000, 500));
        assert!(!tray_left_click_is_distinct(Some(1_000), 1_200, 500));
        assert!(tray_left_click_is_distinct(Some(1_000), 1_501, 500));
        assert!(!tray_left_click_is_distinct(Some(u32::MAX - 100), 100, 500));
    }

    #[test]
    fn tray_mouse_events_map_to_open_and_context_actions() {
        let version_four_left_click = isize::try_from(
            (usize::try_from(TRAY_ICON_ID).unwrap() << 16) | usize::try_from(WM_LBUTTONUP).unwrap(),
        )
        .unwrap();
        assert_eq!(tray_callback_event(version_four_left_click), WM_LBUTTONUP);
        assert_eq!(
            tray_icon_action(WM_LBUTTONUP, false),
            Some(TrayIconAction::OpenSettings)
        );
        assert_eq!(
            tray_icon_action(WM_LBUTTONDBLCLK, false),
            Some(TrayIconAction::OpenSettings)
        );
        assert_eq!(
            tray_icon_action(WM_RBUTTONUP, false),
            Some(TrayIconAction::ShowContextMenu)
        );
        assert_eq!(
            tray_icon_action(NIN_BALLOONUSERCLICK, false),
            Some(TrayIconAction::OpenSettings)
        );
        assert_eq!(
            tray_icon_action(NIN_BALLOONUSERCLICK, true),
            Some(TrayIconAction::OpenUpdates)
        );
        assert_eq!(
            tray_icon_action(NIN_SELECT, false),
            Some(TrayIconAction::OpenSettings)
        );
        assert_eq!(
            tray_icon_action(NIN_KEYSELECT_EVENT, false),
            Some(TrayIconAction::OpenSettings)
        );
        assert_eq!(
            tray_icon_action(WM_CONTEXTMENU, false),
            Some(TrayIconAction::ShowContextMenu)
        );
    }

    #[test]
    fn balloon_clicks_follow_the_route_that_became_visible() {
        let mut routes = BalloonRouteState::default();
        assert!(routes.reserve(UiDestination::Updates));
        assert!(routes.reserve(UiDestination::Default));

        routes.shown();
        assert_eq!(routes.clicked(), UiDestination::Updates);
        routes.shown();
        assert_eq!(routes.clicked(), UiDestination::Default);
        assert_eq!(routes.clicked(), UiDestination::Default);
    }

    #[test]
    fn explorer_restart_message_is_matched_only_after_registration() {
        assert!(!is_taskbar_created_message(0, 0));
        assert!(!is_taskbar_created_message(42, 0));
        assert!(is_taskbar_created_message(42, 42));
        assert!(!is_taskbar_created_message(41, 42));
    }

    #[test]
    fn embedded_manifest_sets_the_tray_process_to_per_monitor_v2() {
        // SAFETY: this only queries the current test thread's inherited process
        // default, which comes from the same build-script manifest as the tray.
        let current = unsafe { GetThreadDpiAwarenessContext() };
        // SAFETY: both values are predefined, valid awareness contexts.
        assert!(
            unsafe {
                AreDpiAwarenessContextsEqual(current, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
            }
            .as_bool()
        );
    }

    #[test]
    fn windows_can_load_the_embedded_metric_tray_icon() {
        // SAFETY: None asks Windows for the module containing this test binary,
        // which receives the same resources from the package build script.
        let module = unsafe { GetModuleHandleW(None) }.expect("test module should be available");
        let icon = TrayIcon::load(HINSTANCE(module.0))
            .expect("Windows should load resource 1 at the active small-icon metric");
        drop(icon);
    }

    #[test]
    fn onboarding_is_offered_once_for_an_unconfigured_user() {
        assert!(should_launch_onboarding(false, &BackupState::Unconfigured));
        assert!(!should_launch_onboarding(true, &BackupState::Unconfigured));
        assert!(!should_launch_onboarding(false, &BackupState::Idle));
    }

    #[test]
    fn backup_completion_requires_a_known_changed_attempt_or_observed_run() {
        assert!(!backup_attempt_changed(false, None, Some(1_000)));
        assert!(backup_attempt_changed(true, None, Some(1_000)));
        assert!(!backup_attempt_changed(true, Some(1_000), Some(1_000)));
        assert!(backup_attempt_changed(true, Some(1_000), Some(2_000)));
        assert!(!backup_attempt_changed(true, Some(1_000), None));
        let started = Instant::now();
        with_backup_monitor_started(|value| *value = Some(started));
        assert!(!backup_monitor_timed_out(
            started + BACKUP_MONITOR_TIMEOUT - Duration::from_millis(1)
        ));
        assert!(backup_monitor_timed_out(started + BACKUP_MONITOR_TIMEOUT));
        with_backup_monitor_started(|value| *value = None);
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
    fn available_updates_follow_the_effective_automatic_update_setting() {
        assert_eq!(
            available_update_action(AutomaticUpdateSetting::Enabled),
            AvailableUpdateAction::RequestSilentInstall,
            "enabled policy must use the service-owned silent installer"
        );
        assert_eq!(
            available_update_action(AutomaticUpdateSetting::Disabled),
            AvailableUpdateAction::Prompt,
            "only an explicit disabled setting may notify the user"
        );
        assert_eq!(
            available_update_action(AutomaticUpdateSetting::Unavailable),
            AvailableUpdateAction::RetrySilently,
            "an unavailable service must not be mistaken for prompted mode"
        );
        assert_eq!(
            automatic_update_follow_up_interval(true),
            UPDATE_ACCEPTED_FOLLOW_UP_INTERVAL_MS
        );
        assert_eq!(
            automatic_update_follow_up_interval(false),
            UPDATE_RETRY_INTERVAL_MS
        );
        assert!(silent_recheck_follow_up_required(true));
        assert!(silent_recheck_follow_up_required(false));
        assert_eq!(
            user_update_action(AutomaticUpdateSetting::Disabled),
            UserUpdateAction::OpenManualUi
        );
        assert_eq!(
            user_update_action(AutomaticUpdateSetting::Enabled),
            UserUpdateAction::CheckSilently
        );
        assert_eq!(
            user_update_action(AutomaticUpdateSetting::Unavailable),
            UserUpdateAction::RetrySilently
        );
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
    fn signed_appcast_metadata_preserves_the_msi_identity() {
        let appcast = r#"<rss xmlns:sparkle="http://example.test/sparkle">
            <channel><item><sparkle:version>2.3.4</sparkle:version>
            <enclosure url="https://github.com/theatrus/resticpal/releases/download/v2.3.4/resticpal-2.3.4-x64.msi"
                length="83329024" sparkle:signature="c2lnbmF0dXJl" /></item></channel></rss>"#;

        assert_eq!(
            extract_update_package(appcast),
            Some(UpdatePackage {
                version: "2.3.4".to_owned(),
                url: "https://github.com/theatrus/resticpal/releases/download/v2.3.4/resticpal-2.3.4-x64.msi".to_owned(),
                signature: "c2lnbmF0dXJl".to_owned(),
                length: 83_329_024,
            })
        );
    }

    #[test]
    fn update_sources_try_the_primary_before_the_github_fallback() {
        let mut checked = Vec::new();
        let available = check_update_sources(UPDATE_APPCAST_SOURCES, |source| {
            checked.push(source.appcast_url);
            (checked.len() == 2).then_some(UpdateCheck::Available(UpdatePackage {
                version: "9.0.0".to_owned(),
                url: "https://example.test/update.msi".to_owned(),
                signature: "signature".to_owned(),
                length: 1,
            }))
        });

        assert!(matches!(available, Some(UpdateCheck::Available(_))));
        assert_eq!(
            checked,
            vec![
                "https://updates.resticpal.com/appcast-v2.xml",
                "https://github.com/theatrus/resticpal/releases/latest/download/appcast-v2.xml",
            ]
        );

        checked.clear();
        let fallback_available = check_update_sources(UPDATE_APPCAST_SOURCES, |source| {
            checked.push(source.appcast_url);
            if checked.len() == 1 {
                Some(UpdateCheck::Current)
            } else {
                Some(UpdateCheck::Available(UpdatePackage {
                    version: "9.0.0".to_owned(),
                    url: "https://example.test/update.msi".to_owned(),
                    signature: "signature".to_owned(),
                    length: 1,
                }))
            }
        });
        assert!(matches!(
            fallback_available,
            Some(UpdateCheck::Available(_))
        ));
        assert_eq!(checked.len(), 2);

        checked.clear();
        let current = check_update_sources(UPDATE_APPCAST_SOURCES, |source| {
            checked.push(source.appcast_url);
            Some(UpdateCheck::Current)
        });
        assert_eq!(current, Some(UpdateCheck::Current));
        assert_eq!(
            checked,
            vec![
                "https://updates.resticpal.com/appcast-v2.xml",
                "https://github.com/theatrus/resticpal/releases/latest/download/appcast-v2.xml",
            ]
        );
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
    #[ignore = "contacts both live signed release feeds"]
    fn live_signed_update_feeds_are_valid() {
        let agent = update_http_agent();
        for source in UPDATE_APPCAST_SOURCES {
            assert!(
                check_update_source(&agent, *source).is_some(),
                "{} should serve a valid signed appcast",
                source.appcast_url
            );
        }
    }
}
