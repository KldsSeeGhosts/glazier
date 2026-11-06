#![windows_subsystem = "windows"]

mod capture;
mod config;
mod control;
#[cfg(test)]
mod desktop_tests;
mod drag;
mod export;
mod gfx;
mod host;
mod hotkey;
mod overlay;
mod placement;
mod recording;
mod recording_media;
mod strokes;
mod theme;
mod theme_tokens;
mod thumbnail;
mod toolbar;

use windows::Win32::Foundation::*;
use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

fn main() {
    let options = match control::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(e) => std::process::exit(control::print(Err(e))),
    };
    // A read or stop must never launch an idle tray host.
    if let Some(
        command @ (control::Command::Status
        | control::Command::Stop
        | control::Command::Pause
        | control::Command::Resume),
    ) = &options.command
    {
        std::process::exit(control::print(control::client(command)));
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance = match CreateMutexW(None, false, w!(r"Local\Glazier.Instance")) {
            Ok(handle) => handle,
            Err(e) => {
                MessageBoxW(None, &HSTRING::from(e.message()), w!("Glazier could not start"), MB_ICONERROR);
                return;
            }
        };
        let _instance = Instance(instance);
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Some(command) = &options.command {
                std::process::exit(control::print(control::client(command)));
            }
            if let Ok(running) = FindWindowW(host::CLASS, None) {
                let _ = PostMessageW(Some(running), host::WM_APP_ALREADY_RUNNING, WPARAM(0), LPARAM(0));
            }
            return;
        }
        let result = OleInitialize(None);
        let initialized = result.is_ok();
        if let Err(e) = result.and_then(|_| host::run(options)) {
            let text = HSTRING::from(format!("Glazier could not start: {}", e.message()));
            MessageBoxW(None, &text, w!("Glazier"), MB_ICONERROR);
        }
        if initialized {
            OleUninitialize();
        }
    }
}

struct Instance(HANDLE);

impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
