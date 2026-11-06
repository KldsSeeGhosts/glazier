//! Global shortcuts through a low-level keyboard hook, so that keys owned by
//! the shell (PrintScreen, Win+Shift+S) can be taken over.

use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub vk: u16,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
}

pub fn parse(spec: &str) -> Option<Hotkey> {
    let mut hk = Hotkey { vk: 0, ctrl: false, alt: false, shift: false, win: false };
    for part in spec.split('+').map(|p| p.trim().to_ascii_lowercase()) {
        match part.as_str() {
            "ctrl" | "control" => hk.ctrl = true,
            "alt" => hk.alt = true,
            "shift" => hk.shift = true,
            "win" | "super" | "meta" | "windows" => hk.win = true,
            key => {
                if hk.vk != 0 {
                    return None;
                }
                hk.vk = key_code(key)?;
            }
        }
    }
    (hk.vk != 0).then_some(hk)
}

fn key_code(key: &str) -> Option<u16> {
    let named = match key {
        "printscreen" | "prtsc" | "print" | "prtscn" => VK_SNAPSHOT,
        "insert" | "ins" => VK_INSERT,
        "delete" | "del" => VK_DELETE,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" | "pgup" => VK_PRIOR,
        "pagedown" | "pgdn" => VK_NEXT,
        "pause" => VK_PAUSE,
        "scrolllock" => VK_SCROLL,
        "space" => VK_SPACE,
        _ => VIRTUAL_KEY(0),
    };
    if named.0 != 0 {
        return Some(named.0);
    }
    let bytes = key.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii_alphanumeric() {
        return Some(bytes[0].to_ascii_uppercase() as u16);
    }
    if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u16>().ok()) {
        if (1..=24).contains(&n) {
            return Some(VK_F1.0 + n - 1);
        }
    }
    None
}

static HOTKEYS: Mutex<Vec<Hotkey>> = Mutex::new(Vec::new());
static RECORDING: Mutex<Option<Hotkey>> = Mutex::new(None);
static TARGET: AtomicIsize = AtomicIsize::new(0);
static OVERLAY: AtomicIsize = AtomicIsize::new(0);

pub const WM_APP_HOTKEY: u32 = WM_APP + 1;
pub const WM_APP_ESCAPE: u32 = WM_APP + 2;
pub const WM_APP_RECORD: u32 = WM_APP + 6;

/// Tags our own injected mask key so the hook can ignore it.
const MASK_TAG: usize = 0x474C_415A;

thread_local! {
    static SWALLOWED: Cell<[bool; 256]> = const { Cell::new([false; 256]) };
}

pub fn set(keys: Vec<Hotkey>, recording: Option<Hotkey>) {
    *HOTKEYS.lock().unwrap() = keys;
    *RECORDING.lock().unwrap() = recording;
}

pub fn set_overlay(hwnd: Option<HWND>) {
    OVERLAY.store(hwnd.map_or(0, |h| h.0 as isize), Ordering::SeqCst);
}

pub fn start(target: HWND) {
    TARGET.store(target.0 as isize, Ordering::SeqCst);
    std::thread::spawn(|| unsafe {
        let module = GetModuleHandleW(None).ok().map(|m| HINSTANCE(m.0));
        let Ok(hook) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), module, 0) else {
            return;
        };
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(hook);
    });
}

fn held(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000 != 0 }
}

/// Keeps the Start menu or a menu bar from opening when the swallowed key was
/// pressed with Win or Alt held.
fn send_mask_key() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0xFF), dwFlags: flags, dwExtraInfo: MASK_TAG, ..Default::default() },
        },
    };
    unsafe {
        SendInput(&[key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)], size_of::<INPUT>() as i32);
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code == HC_ACTION as i32 {
            let k = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            if k.dwExtraInfo != MASK_TAG && swallow(wparam.0 as u32, k.vkCode as u16) {
                return LRESULT(1);
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

fn swallow(msg: u32, vk: u16) -> bool {
    let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
    if vk >= 256 {
        return false;
    }
    let mut swallowed = SWALLOWED.get();
    if swallowed[vk as usize] {
        if up {
            swallowed[vk as usize] = false;
            SWALLOWED.set(swallowed);
        }
        return true; // key repeat or the matching release
    }
    if !down {
        return false;
    }
    let overlay = OVERLAY.load(Ordering::SeqCst);
    if vk == VK_ESCAPE.0 && overlay != 0 {
        swallowed[vk as usize] = true;
        SWALLOWED.set(swallowed);
        // Always route Escape here, including when a different app has focus.
        // Swallow repeats and the release so one press cannot both discard
        // pending text and close the editor, or escape into the previous app.
        unsafe {
            let _ = PostMessageW(Some(HWND(overlay as *mut _)), WM_APP_ESCAPE, WPARAM(0), LPARAM(0));
        }
        return true;
    }
    let pressed = Hotkey {
        vk,
        ctrl: held(VK_CONTROL),
        alt: held(VK_MENU),
        shift: held(VK_SHIFT),
        win: held(VK_LWIN) || held(VK_RWIN),
    };
    let Ok(keys) = HOTKEYS.try_lock() else {
        return false;
    };
    let Ok(recording) = RECORDING.try_lock() else {
        return false;
    };
    let record = *recording == Some(pressed);
    if !record && !keys.contains(&pressed) {
        return false;
    }
    drop(keys);
    drop(recording);
    swallowed[vk as usize] = true;
    SWALLOWED.set(swallowed);
    if pressed.win || pressed.alt {
        send_mask_key();
    }
    let target = HWND(TARGET.load(Ordering::SeqCst) as *mut _);
    unsafe {
        let _ = PostMessageW(Some(target), if record { WM_APP_RECORD } else { WM_APP_HOTKEY }, WPARAM(0), LPARAM(0));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_specs() {
        let h = parse("Win+Shift+S").unwrap();
        assert_eq!((h.vk, h.win, h.shift, h.ctrl, h.alt), (b'S' as u16, true, true, false, false));
        assert_eq!(parse("PrintScreen").unwrap().vk, VK_SNAPSHOT.0);
        let r = parse(&crate::config::Config::default().recording_hotkey).unwrap();
        assert_eq!(r, Hotkey { vk: b'R' as u16, win: true, shift: true, ctrl: false, alt: false });
        assert_eq!(parse("ctrl + f12").unwrap().vk, VK_F12.0);
        assert!(parse("Ctrl+Shift").is_none());
        assert!(parse("A+B").is_none());
        assert!(parse("Hyper+Q").is_none());
    }
}
