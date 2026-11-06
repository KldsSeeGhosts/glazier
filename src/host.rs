//! Hidden host window: tray icon, menu, notifications and capture requests.

use crate::config::{self, Config};
use crate::control::{self, Command, Options, Phase};
use crate::gfx::Gfx;
use crate::hotkey::{self, WM_APP_HOTKEY};
use crate::overlay::{self, Finish};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

pub const CLASS: PCWSTR = w!("GlazierHost");
pub const WM_APP_CAPTURE: u32 = WM_APP + 4;
pub const WM_APP_ALREADY_RUNNING: u32 = WM_APP + 5;
const WM_APP_TRAY: u32 = WM_APP + 3;
const TIMER_RECAPTURE: usize = 1;
const TRAY_ID: u32 = 1;
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("Glazier");

const CMD_CAPTURE: usize = 1;
const CMD_FOLDER: usize = 2;
const CMD_SETTINGS: usize = 3;
const CMD_RELOAD: usize = 4;
const CMD_AUTOSTART: usize = 5;
const CMD_EXIT: usize = 6;
const CMD_RECORD: usize = 7;
const WM_APP_RECORDING_CHANGED: u32 = WM_APP + 7;

static HOST: AtomicIsize = AtomicIsize::new(0);
static EXITING: AtomicBool = AtomicBool::new(false);

struct App {
    gfx: Rc<Gfx>,
    cfg: Rc<Config>,
    /// --theme-file, also applied after a settings reload.
    theme_file: Option<String>,
    /// --no-hotkeys: leave the global shortcuts to the host shell.
    no_hotkeys: bool,
    overlay: Option<HWND>,
    opening: bool,
    icon: HICON,
    recording_icon: HICON,
    recording_focus: Option<HWND>,
    recording_audio: Option<(Option<bool>, Option<bool>)>,
    cancel_selection: bool,
    taskbar_created: u32,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static LAST_SAVED: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

pub fn hwnd() -> HWND {
    HWND(HOST.load(Ordering::SeqCst) as *mut _)
}

fn load_config(theme_file: Option<&str>) -> (Config, Option<String>) {
    let (mut cfg, error) = config::load();
    if let Some(path) = theme_file {
        cfg.theme_file = path.to_owned();
    }
    (cfg, error)
}

pub fn run(options: Options) -> Result<()> {
    crate::thumbnail::cleanup();
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let gfx = Rc::new(Gfx::new()?);
        let icon = make_icon(&gfx, false).unwrap_or_else(|_| LoadIconW(None, IDI_APPLICATION).unwrap_or_default());
        let recording_icon = make_icon(&gfx, true).unwrap_or(icon);
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            hIcon: icon,
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("Glazier"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )?;
        HOST.store(hwnd.0 as isize, Ordering::SeqCst);

        let (cfg, error) = load_config(options.theme_file.as_deref());
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        APP.with_borrow_mut(|app| {
            *app = Some(App {
                gfx,
                cfg: Rc::new(cfg.clone()),
                theme_file: options.theme_file.clone(),
                no_hotkeys: options.no_hotkeys,
                overlay: None,
                opening: false,
                icon,
                recording_icon,
                recording_focus: None,
                recording_audio: None,
                cancel_selection: false,
                taskbar_created,
            });
        });
        control::start(hwnd)?;
        control::directory(&cfg);
        add_tray(hwnd, icon);
        apply_hotkeys(&cfg, error, options.no_hotkeys);
        // The hook still routes Escape from the editor when the shell owns the
        // capture shortcuts; --no-hotkeys only empties the shortcut list.
        hotkey::start(hwnd);
        if let Some(command) = options.command {
            if let Err(e) = control(command) {
                control::error(e.clone());
                notify("Glazier control failed", &e, None, true);
            }
        }

        let mut msg = MSG::default();
        while !EXITING.load(Ordering::SeqCst) && GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        remove_tray(hwnd);
        let overlay = APP.with_borrow(|a| a.as_ref().and_then(|a| a.overlay));
        if let Some(overlay) = overlay {
            let _ = DestroyWindow(overlay);
        }
        crate::recording::shutdown();
        crate::thumbnail::shutdown();
        let _ = DestroyWindow(hwnd);
        APP.with_borrow_mut(|app| {
            if let Some(app) = app.take() {
                let _ = DestroyIcon(app.icon);
                if app.recording_icon != app.icon {
                    let _ = DestroyIcon(app.recording_icon);
                }
            }
        });
        Ok(())
    }
}

fn apply_hotkeys(cfg: &Config, error: Option<String>, no_hotkeys: bool) {
    let mut bad = Vec::new();
    if no_hotkeys {
        hotkey::set(Vec::new(), None);
    } else {
        let keys = cfg
            .hotkeys
            .iter()
            .filter_map(|s| {
                let k = hotkey::parse(s);
                if k.is_none() {
                    bad.push(s.as_str());
                }
                k
            })
            .collect();
        let recording = if cfg.recording_hotkey.trim().is_empty() {
            None
        } else {
            let key = hotkey::parse(&cfg.recording_hotkey);
            if key.is_none() {
                bad.push(&cfg.recording_hotkey);
            }
            key
        };
        hotkey::set(keys, recording);
    }
    if let Some(e) = error {
        notify("Glazier settings were not loaded", &e, None, true);
    } else if !bad.is_empty() {
        notify("Unknown shortcut in settings", &bad.join(", "), None, true);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_APP_HOTKEY | WM_APP_CAPTURE => start_capture(),
            hotkey::WM_APP_RECORD => {
                if crate::recording::running() {
                    crate::recording::stop();
                } else if !crate::recording::exists() {
                    start_picker(true);
                }
            }
            WM_APP_RECORDING_CHANGED => update_recording_tray(),
            control::WM_CONTROL => control::dispatch(),
            WM_APP_ALREADY_RUNNING => notify("Glazier is running", &hotkey_hint(), None, false),
            WM_TIMER if wp.0 == TIMER_RECAPTURE => {
                let _ = KillTimer(Some(hwnd), TIMER_RECAPTURE);
                start_capture();
            }
            WM_APP_TRAY => match (lp.0 & 0xffff) as u32 {
                WM_CONTEXTMENU | WM_RBUTTONUP => show_menu(hwnd),
                NIN_SELECT | NIN_KEYSELECT => start_capture(),
                NIN_BALLOONUSERCLICK => {
                    if let Some(path) = LAST_SAVED.with_borrow(|p| p.clone()) {
                        reveal(&path);
                    }
                }
                _ => {}
            },
            WM_DESTROY => {
                EXITING.store(true, Ordering::SeqCst);
                PostQuitMessage(0);
            }
            _ => {
                let created = APP.with_borrow(|a| a.as_ref().map(|a| (a.taskbar_created, a.icon)));
                if let Some((id, icon)) = created {
                    if msg == id && id != 0 {
                        add_tray(hwnd, icon);
                        recording_changed();
                    }
                }
                return DefWindowProcW(hwnd, msg, wp, lp);
            }
        }
        LRESULT(0)
    }
}

fn start_capture() {
    start_picker(false);
}

fn start_picker(recording: bool) {
    if EXITING.load(Ordering::SeqCst) || crate::thumbnail::is_dragging() {
        return;
    }
    let state = APP.with_borrow_mut(|a| {
        let a = a.as_mut()?;
        if a.opening || a.overlay.is_some() {
            return None;
        }
        // GPU/COM initialization can pump messages before open returns.
        a.opening = true;
        if recording {
            a.cancel_selection = false;
            a.recording_focus = Some(unsafe { GetForegroundWindow() });
        }
        Some((a.gfx.clone(), a.cfg.clone()))
    });
    let Some((gfx, cfg)) = state else { return };
    gfx.reload_theme(&cfg.theme_file);
    crate::recording::reload_theme(&gfx);
    crate::thumbnail::reload_theme(&gfx);
    if recording {
        control::change(Phase::Selecting, None, None, None);
    }
    unsafe {
        let _ = KillTimer(Some(hwnd()), TIMER_RECAPTURE);
    }
    crate::thumbnail::hide(true);
    crate::recording::hide(true);
    unsafe {
        let _ = DwmFlush();
    }
    let result = crate::capture::capture_desktop()
        .and_then(|cap| if recording { overlay::open_recording(gfx, cfg, cap) } else { overlay::open(gfx, cfg, cap) });
    APP.with_borrow_mut(|a| {
        if let Some(a) = a {
            a.opening = false;
            a.overlay = result.as_ref().ok().copied().filter(|h| unsafe { IsWindow(Some(*h)).as_bool() });
        }
    });
    match result {
        Ok(_) => {}
        Err(e) => {
            if recording {
                control::change(Phase::Idle, Some("error"), None, Some(e.message().to_string()));
                let foreground = APP.with_borrow_mut(|a| a.as_mut().and_then(|a| a.recording_focus.take()));
                if let Some(foreground) = foreground {
                    force_foreground(foreground);
                }
                APP.with_borrow_mut(|a| {
                    if let Some(a) = a {
                        a.recording_audio = None;
                    }
                });
            }
            crate::thumbnail::hide(false);
            crate::recording::hide(false);
            notify("Could not capture the screen", &e.message(), None, true);
        }
    }
    let cancelled = recording && APP.with_borrow(|a| a.as_ref().is_some_and(|a| a.cancel_selection));
    if cancelled {
        let overlay = APP.with_borrow(|a| a.as_ref().and_then(|a| a.overlay));
        if let Some(overlay) = overlay {
            unsafe { let _ = PostMessageW(Some(overlay), WM_CLOSE, WPARAM(0), LPARAM(0)); }
        }
    }
}

/// Called by the overlay after its window is destroyed.
pub fn overlay_finished(how: usize) {
    let foreground = APP.with_borrow_mut(|a| {
        if let Some(a) = a {
            a.overlay = None;
            a.recording_focus.take()
        } else {
            None
        }
    });
    if let Some(foreground) = foreground {
        force_foreground(foreground);
    }
    crate::thumbnail::hide(false);
    crate::recording::hide(false);
    let app = APP.with_borrow(|a| a.as_ref().map(|a| (a.gfx.clone(), a.cfg.clone())));
    if let Some((gfx, cfg)) = app {
        let audio = APP.with_borrow_mut(|a| a.as_mut().and_then(|a| a.recording_audio.take()));
        crate::recording::show_pending(gfx, cfg, audio);
        if control::phase() == Phase::Selecting {
            control::change(Phase::Idle, Some("cancelled"), None, None);
        }
    }
    if how == Finish::Recapture as usize {
        // Let the compositor drop the editor before freezing a new frame.
        unsafe {
            let _ = DwmFlush();
            SetTimer(Some(hwnd()), TIMER_RECAPTURE, 120, None);
        }
    }
}

/// Public commands are explicit actions, never the tray/hotkey record toggle.
pub fn control(command: Command) -> std::result::Result<(), String> {
    if !control::accepts(control::phase(), &command) {
        return Err(format!("Command is not valid while {:?}", control::phase()));
    }
    match command {
        Command::Status => {}
        Command::Capture => start_capture(),
        Command::Stop => {
            if control::phase() == Phase::Selecting {
                let overlay = APP.with_borrow_mut(|a| a.as_mut().and_then(|a| {
                    a.cancel_selection = true;
                    a.overlay
                }));
                if let Some(overlay) = overlay {
                    unsafe {
                        let _ = PostMessageW(Some(overlay), WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                }
            } else {
                crate::recording::stop();
            }
        }
        Command::Pause => crate::recording::pause(true),
        Command::Resume => crate::recording::pause(false),
        Command::Record { fullscreen, system_sound, mic } => {
            let ready = APP.with_borrow(|a| a.as_ref().is_some_and(|a| !a.opening && a.overlay.is_none()));
            if !ready || crate::recording::exists() {
                return Err("Glazier is busy".into());
            }
            if fullscreen {
                let (gfx, cfg) = APP.with_borrow(|a| a.as_ref().map(|a| (a.gfx.clone(), a.cfg.clone()))).unwrap();
                gfx.reload_theme(&cfg.theme_file);
                crate::recording::reload_theme(&gfx);
                crate::thumbnail::reload_theme(&gfx);
                crate::recording::fullscreen(gfx, cfg, system_sound, mic).map_err(|e| e.message().to_string())?;
            } else {
                APP.with_borrow_mut(|a| a.as_mut().unwrap().recording_audio = Some((system_sound, mic)));
                start_picker(true);
                if control::phase() != Phase::Selecting {
                    return Err("Could not open the recording picker".into());
                }
            }
        }
    }
    Ok(())
}

pub fn recording_selected(
    region: crate::capture::IRect,
    monitor: crate::capture::IRect,
    work: crate::capture::IRect,
    k: f32,
) {
    if !APP.with_borrow(|a| a.as_ref().is_some_and(|a| a.cancel_selection)) {
        crate::recording::select(region, monitor, work, k);
    }
}

pub fn recording_changed() {
    unsafe {
        let _ = PostMessageW(Some(hwnd()), WM_APP_RECORDING_CHANGED, WPARAM(0), LPARAM(0));
    }
}

fn update_recording_tray() {
    let recording = crate::recording::running();
    let mut data = tray_data(hwnd());
    data.uFlags = NIF_TIP | NIF_ICON;
    fill(&mut data.szTip, if recording { "Glazier - Recording" } else { "Glazier" });
    data.hIcon = APP.with_borrow(|a| a.as_ref().map(|a| if recording { a.recording_icon } else { a.icon })).unwrap_or_default();
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
    }
}

fn hotkey_hint() -> String {
    let (keys, no_hotkeys) = APP.with_borrow(|a| {
        a.as_ref().map_or((String::new(), false), |a| (a.cfg.hotkeys.join(" or "), a.no_hotkeys))
    });
    if no_hotkeys || keys.is_empty() {
        "Use the tray icon to capture a region.".into()
    } else {
        format!("Press {keys} to capture a region.")
    }
}

fn fill(dst: &mut [u16], s: &str) {
    let n = dst.len() - 1;
    for (d, c) in dst.iter_mut().zip(s.encode_utf16().take(n).chain(std::iter::once(0))) {
        *d = c;
    }
}

fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: TRAY_ID, ..Default::default() }
}

fn add_tray(hwnd: HWND, icon: HICON) {
    let mut data = tray_data(hwnd);
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
    data.uCallbackMessage = WM_APP_TRAY;
    data.hIcon = icon;
    fill(&mut data.szTip, "Glazier");
    data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &data);
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
    }
}

fn remove_tray(hwnd: HWND) {
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &tray_data(hwnd));
    }
}

/// Shows a tray notification. Clicking it reveals `path` when given.
pub fn notify(title: &str, body: &str, path: Option<PathBuf>, error: bool) {
    LAST_SAVED.with_borrow_mut(|p| *p = path);
    let mut data = tray_data(hwnd());
    data.uFlags = NIF_INFO;
    fill(&mut data.szInfoTitle, title);
    fill(&mut data.szInfo, if body.is_empty() { " " } else { body });
    data.dwInfoFlags = if error { NIIF_ERROR } else { NIIF_INFO } | NIIF_NOSOUND;
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
    }
}

pub fn reveal(path: &std::path::Path) {
    let args = HSTRING::from(format!("/select,\"{}\"", path.display()));
    unsafe {
        ShellExecuteW(None, w!("open"), w!("explorer.exe"), &args, PCWSTR::null(), SW_SHOWNORMAL);
    }
}

pub fn open_video(path: &std::path::Path) -> Result<()> {
    unsafe {
        let result =
            ShellExecuteW(None, w!("open"), &HSTRING::from(path.as_os_str()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
        if result.0 as isize <= 32 {
            return Err(Error::new(E_FAIL, "Windows could not open the recording in its default player"));
        }
        Ok(())
    }
}

fn open_path(path: &std::path::Path) {
    let target = HSTRING::from(path.as_os_str());
    unsafe {
        let r = ShellExecuteW(None, w!("open"), &target, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
        if r.0 as isize <= 32 {
            ShellExecuteW(None, w!("open"), w!("notepad.exe"), &target, PCWSTR::null(), SW_SHOWNORMAL);
        }
    }
}

fn autostart_enabled() -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, None) == ERROR_SUCCESS }
}

fn set_autostart(enable: bool) {
    unsafe {
        if enable {
            let exe = std::env::current_exe().unwrap_or_default();
            let value: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().chain(std::iter::once(0)).collect();
            let _ = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_VALUE,
                REG_SZ.0,
                Some(value.as_ptr().cast()),
                (value.len() * 2) as u32,
            );
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
        }
    }
}

fn show_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let (keys, no_hotkeys) = APP.with_borrow(|a| {
            a.as_ref().map_or((String::new(), false), |a| (a.cfg.hotkeys.join(", "), a.no_hotkeys))
        });
        let capture = HSTRING::from(
            if no_hotkeys || keys.is_empty() { "Capture region".to_string() } else { format!("Capture region\t{keys}") },
        );
        let _ = AppendMenuW(menu, MF_STRING, CMD_CAPTURE, &capture);
        let _ = AppendMenuW(menu, MF_STRING, CMD_FOLDER, w!("Open screenshots folder"));
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            CMD_RECORD,
            if crate::recording::running() { w!("Stop recording") } else { w!("New recording") },
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, CMD_SETTINGS, w!("Edit settings"));
        let _ = AppendMenuW(menu, MF_STRING, CMD_RELOAD, w!("Reload settings"));
        let check = if autostart_enabled() { MF_CHECKED } else { MF_UNCHECKED };
        let _ = AppendMenuW(menu, MF_STRING | check, CMD_AUTOSTART, w!("Start with Windows"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, CMD_EXIT, w!("Exit"));

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenuEx(menu, (TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON).0, pt.x, pt.y, hwnd, None).0 as usize;
        let _ = DestroyMenu(menu);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        match cmd {
            CMD_RECORD => {
                if crate::recording::running() {
                    crate::recording::stop();
                } else {
                    let _ = PostMessageW(Some(hwnd), hotkey::WM_APP_RECORD, WPARAM(0), LPARAM(0));
                }
            }
            CMD_CAPTURE => {
                let _ = SetTimer(Some(hwnd), TIMER_RECAPTURE, 150, None);
            }
            CMD_FOLDER => {
                let cfg = APP.with_borrow(|a| a.as_ref().map(|a| a.cfg.clone()));
                if let Some((dir, create)) = cfg.and_then(|c| crate::export::save_dir(&c)) {
                    if create {
                        let _ = std::fs::create_dir_all(&dir);
                    }
                    open_path(&dir);
                }
            }
            CMD_SETTINGS => {
                let _ = config::load();
                open_path(&config::path());
            }
            CMD_RELOAD => {
                let (theme_file, no_hotkeys) = APP.with_borrow(|a| {
                    a.as_ref().map_or((None, false), |a| (a.theme_file.clone(), a.no_hotkeys))
                });
                let (cfg, error) = load_config(theme_file.as_deref());
                control::directory(&cfg);
                apply_hotkeys(&cfg, error.clone(), no_hotkeys);
                crate::thumbnail::reload(&cfg);
                APP.with_borrow_mut(|a| {
                    if let Some(a) = a {
                        a.cfg = Rc::new(cfg);
                    }
                });
                if error.is_none() {
                    notify("Settings reloaded", &hotkey_hint(), None, false);
                }
            }
            CMD_AUTOSTART => set_autostart(!autostart_enabled()),
            CMD_EXIT => {
                // COM can dispatch the menu inside an export or render. Let
                // that live borrow finish before destroying any app windows.
                EXITING.store(true, Ordering::SeqCst);
                let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            }
            _ => {}
        }
    }
}

pub fn exported(
    img: &crate::export::Image,
    png: &[u8],
    saved: Option<PathBuf>,
    origin: crate::capture::IRect,
    monitor: crate::capture::IRect,
    k: f32,
) {
    let app = APP.with_borrow(|a| a.as_ref().map(|a| (a.gfx.clone(), a.cfg.clone())));
    if let Some((gfx, cfg)) = app {
        crate::thumbnail::show(gfx, &cfg, img, png, saved, origin, monitor, k);
    }
}

pub fn save_thumbnail(png: &[u8]) -> std::result::Result<PathBuf, String> {
    let cfg = APP.with_borrow(|a| a.as_ref().map(|a| a.cfg.clone())).ok_or("Glazier is shutting down")?;
    let path = crate::export::save_png(&cfg, png)?;
    saved(&path);
    Ok(path)
}

pub fn saved(path: &std::path::Path) {
    let thumbnail = APP.with_borrow(|a| a.as_ref().is_some_and(|a| a.cfg.show_thumbnail));
    if !thumbnail {
        notify("Annotation saved", &format!("Saved to {}", path.display()), Some(path.to_owned()), false);
    }
}

pub fn reopen(path: &std::path::Path, origin: crate::capture::IRect, work: crate::capture::IRect) -> Result<()> {
    if EXITING.load(Ordering::SeqCst) {
        return Ok(());
    }
    let app = APP.with_borrow_mut(|a| {
        let a = a.as_mut()?;
        if a.opening || a.overlay.is_some() {
            return None;
        }
        a.opening = true;
        Some((a.gfx.clone(), a.cfg.clone()))
    });
    let Some((gfx, cfg)) = app else { return Ok(()) };
    crate::thumbnail::hide(true);
    crate::recording::hide(true);
    unsafe {
        let _ = DwmFlush();
    }
    let result = (|| {
        let image = crate::export::decode_png(&gfx, path)?;
        let region = crate::thumbnail::reopen_region(origin, work, image.width, image.height);
        let cap = crate::capture::capture_desktop()?;
        overlay::open_image(gfx, cfg, cap, image, region)
    })();
    APP.with_borrow_mut(|a| {
        if let Some(a) = a {
            a.opening = false;
            a.overlay = result.as_ref().ok().copied().filter(|h| unsafe { IsWindow(Some(*h)).as_bool() });
        }
    });
    if result.is_err() {
        crate::thumbnail::hide(false);
        crate::recording::hide(false);
    }
    result.map(|_| ())
}

/// Temporarily shares the foreground input queue, then detaches immediately.
/// Foreground activation can still be denied; the hook independently routes Esc.
pub fn force_foreground(target: HWND) {
    unsafe {
        if !SetForegroundWindow(target).as_bool() {
            let foreground = GetForegroundWindow();
            let foreground_thread = GetWindowThreadProcessId(foreground, None);
            let current_thread = GetCurrentThreadId();
            let attached = foreground_thread != 0
                && foreground_thread != current_thread
                && AttachThreadInput(current_thread, foreground_thread, true).as_bool();
            let _ = SetForegroundWindow(target);
            if GetForegroundWindow() == target {
                let _ = SetFocus(Some(target));
            }
            if attached {
                let _ = AttachThreadInput(current_thread, foreground_thread, false);
            }
        }
        if GetForegroundWindow() == target {
            let _ = SetFocus(Some(target));
        }
    }
}

/// Renders the app icon: the pink rounded square with a white "G".
fn make_icon(gfx: &Gfx, recording: bool) -> Result<HICON> {
    unsafe {
        let size = GetSystemMetrics(SM_CXSMICON).max(16) as u32;
        let target = gfx.wic.CreateBitmap(size, size, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad)?;
        let rt = gfx.d2d.CreateWicBitmapRenderTarget(&target, &crate::export::target_props())?;
        let brush = rt.CreateSolidColorBrush(&crate::theme::rgb(if recording { 0xdc2626 } else { 0xdb2777 }), None)?;
        let s = size as f32;
        let font = gfx.text_format(s * 0.62, DWRITE_FONT_WEIGHT_SEMI_BOLD, true)?;
        rt.BeginDraw();
        rt.Clear(Some(&D2D1_COLOR_F::default()));
        let r = crate::gfx::rect(0.0, 0.0, s, s);
        rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: s * 0.22, radiusY: s * 0.22 }, &brush);
        brush.SetColor(&crate::theme::WHITE);
        rt.DrawText(
            &crate::gfx::wide(if recording { "R" } else { "G" }),
            &font,
            &crate::gfx::rect(0.0, -s * 0.03, s, s),
            &brush,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );
        rt.EndDraw(None, None)?;
        let mut pixels = vec![0u8; (size * size * 4) as usize];
        target.CopyPixels(std::ptr::null(), size * 4, &mut pixels)?;
        // Icons take straight alpha.
        for px in pixels.chunks_exact_mut(4) {
            let a = px[3] as u32;
            if a > 0 && a < 255 {
                for c in &mut px[..3] {
                    *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
        let color = CreateBitmap(size as i32, size as i32, 1, 32, Some(pixels.as_ptr().cast()));
        let mask_bits = vec![0u8; (size.div_ceil(16) * 2 * size) as usize];
        let mask = CreateBitmap(size as i32, size as i32, 1, 1, Some(mask_bits.as_ptr().cast()));
        let info = ICONINFO { fIcon: TRUE, hbmMask: mask, hbmColor: color, ..Default::default() };
        let icon = CreateIconIndirect(&info);
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}
