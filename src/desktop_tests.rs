//! Opt-in live desktop regression. Requires the release app already running.
//! cargo test real_input_desktop -- --ignored --nocapture --test-threads=1

use crate::{capture, gfx, host, toolbar};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::CF_DIB;
use windows::Win32::System::ProcessStatus::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

fn pump() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn wait(mut check: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(4) {
        pump();
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    false
}

fn input(inputs: &[INPUT]) {
    unsafe {
        assert_eq!(SendInput(inputs, size_of::<INPUT>() as i32), inputs.len() as u32);
    }
    std::thread::sleep(Duration::from_millis(45));
    pump();
}

fn key(vk: VIRTUAL_KEY, up: bool) {
    let extended = matches!(
        vk,
        VK_END
            | VK_HOME
            | VK_LEFT
            | VK_RIGHT
            | VK_UP
            | VK_DOWN
            | VK_INSERT
            | VK_DELETE
            | VK_PRIOR
            | VK_NEXT
            | VK_LWIN
            | VK_RWIN
            | VK_RCONTROL
            | VK_RMENU
    );
    input(&[INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                dwFlags: (if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) })
                    | (if extended { KEYEVENTF_EXTENDEDKEY } else { KEYBD_EVENT_FLAGS(0) }),
                ..Default::default()
            },
        },
    }]);
}

fn press(vk: VIRTUAL_KEY) {
    key(vk, false);
    key(vk, true);
}

fn mouse(x: i32, y: i32, flags: MOUSE_EVENT_FLAGS) {
    unsafe {
        let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        input(&[INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: (((x - left) as f64 + 0.5) * 65536.0 / width as f64) as i32,
                    dy: (((y - top) as f64 + 0.5) * 65536.0 / height as f64) as i32,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    ..Default::default()
                },
            },
        }]);
        if flags.0 != 0 {
            input(&[INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 { mi: MOUSEINPUT { dwFlags: flags, ..Default::default() } },
            }]);
        }
    }
}

fn overlay() -> Option<HWND> {
    unsafe { FindWindowW(w!("GlazierOverlay"), None).ok() }
}

fn region_pixels(region: capture::IRect) -> Vec<u8> {
    unsafe {
        DwmFlush().unwrap();
    }
    capture::capture_desktop().unwrap().crop(region)
}

fn resources(log: bool) -> (u32, u32, u32) {
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(FindWindowW(host::CLASS, None).unwrap(), Some(&mut pid));
        let process = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid).unwrap();
        let mut memory = PROCESS_MEMORY_COUNTERS_EX::default();
        GetProcessMemoryInfo(
            process,
            &mut memory as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
            size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
        .unwrap();
        let mut handles = 0;
        GetProcessHandleCount(process, &mut handles).unwrap();
        let gdi = GetGuiResources(process, GR_GDIOBJECTS);
        let user = GetGuiResources(process, GR_USEROBJECTS);
        CloseHandle(process).unwrap();
        if log {
            println!(
                "Idle resources: {handles} handles, {gdi} GDI, {user} USER; {} MiB working set, {} MiB private",
                memory.WorkingSetSize / 1024 / 1024,
                memory.PrivateUsage / 1024 / 1024
            );
        }
        (handles, gdi, user)
    }
}

fn open(win_shift_s: bool) -> HWND {
    assert!(overlay().is_none());
    let start = Instant::now();
    if win_shift_s {
        key(VK_LWIN, false);
        key(VK_SHIFT, false);
        press(VIRTUAL_KEY(b'S' as u16));
        key(VK_SHIFT, true);
        key(VK_LWIN, true);
    } else {
        press(VK_SNAPSHOT);
    }
    assert!(
        wait(|| overlay().is_some_and(|h| unsafe { IsWindowVisible(h).as_bool() && GetForegroundWindow() == h })),
        "capture did not open and acquire foreground"
    );
    let hwnd = overlay().unwrap();
    hold(Duration::from_millis(120));
    println!(
        "Capture via {}: {:?}, foreground acquired",
        if win_shift_s { "Win+Shift+S" } else { "PrintScreen" },
        start.elapsed()
    );
    hwnd
}

struct Cleanup {
    sink: HWND,
    previous: HWND,
    cursor: POINT,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        unsafe {
            // Cleanup on assertion failure too, including against the old app.
            if let Some(h) = overlay() {
                press(VK_ESCAPE);
                if IsWindow(h.into()).as_bool() {
                    let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
                    wait(|| overlay().is_none());
                }
            }
            let _ = DestroyWindow(self.sink);
            let _ = SetCursorPos(self.cursor.x, self.cursor.y);
            host::force_foreground(self.previous);
            CoUninitialize();
        }
    }
}

#[test]
#[ignore = "briefly controls the real desktop and writes the clipboard"]
fn real_input_desktop() {
    unsafe {
        assert!(FindWindowW(host::CLASS, None).is_ok(), "start target/release/glazier.exe first");
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
        let previous = GetForegroundWindow();
        let mut cursor = POINT::default();
        GetCursorPos(&mut cursor).unwrap();
        let sink = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("Glazier input regression"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            80,
            80,
            420,
            220,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let _cleanup = Cleanup { sink, previous, cursor };
        host::force_foreground(sink);
        assert!(wait(|| GetForegroundWindow() == sink));
        let cap = capture::capture_desktop().unwrap();
        println!("Desktop {:?}, {}x{}, monitors {:?}", cap.origin, cap.width, cap.height, cap.monitors);

        open(false);
        press(VK_ESCAPE);
        assert!(wait(|| overlay().is_none()), "Escape failed in picker");
        open(true);
        press(VK_RETURN);
        press(VK_ESCAPE);
        assert!(wait(|| overlay().is_none()), "Escape failed in editor");
        println!("Picker and editor Escape passed using SendInput");

        open(false);
        host::force_foreground(sink);
        assert!(wait(|| GetForegroundWindow() == sink));
        press(VK_ESCAPE);
        assert!(wait(|| overlay().is_none()), "Escape failed after another app took focus");
        println!("Escape after foreground loss passed using SendInput");

        let g = std::rc::Rc::new(gfx::Gfx::new().unwrap());
        for across in [false, true] {
            let m = cap.monitors.iter().find(|m| m.bounds.x == 0).unwrap();
            let region = if across && cap.monitors.len() > 1 {
                capture::IRect { x: m.bounds.x + m.bounds.w - 120, y: 350, w: 260, h: 200 }
            } else {
                capture::IRect { x: m.bounds.x + 600, y: m.bounds.y + 350, w: 400, h: 250 }
            };
            let (x, y) = (cap.origin.0 + region.x, cap.origin.1 + region.y);
            // A static reference window makes a byte-for-byte freeze check
            // independent of clocks, video and the user's other applications.
            if across {
                SetWindowPos(sink, Some(HWND_TOPMOST), x - 20, y - 60, region.w + 40, region.h + 100, SWP_SHOWWINDOW)
                    .unwrap();
                let _ = UpdateWindow(sink);
                DwmFlush().unwrap();
            }
            let expected = if across { Some(capture::capture_desktop().unwrap().crop(region)) } else { None };
            // Start from the primary monitor, then select on the negative one.
            mouse(400, 300, MOUSE_EVENT_FLAGS(0));
            let hwnd = open(false);
            let mut bounds = RECT::default();
            GetWindowRect(hwnd, &mut bounds).unwrap();
            assert_eq!(
                (bounds.left, bounds.top, bounds.right - bounds.left, bounds.bottom - bounds.top),
                (cap.origin.0, cap.origin.1, cap.width, cap.height)
            );
            mouse(x, y, MOUSEEVENTF_LEFTDOWN);
            mouse(x + region.w, y + region.h, MOUSE_EVENT_FLAGS(0));
            mouse(x + region.w, y + region.h, MOUSEEVENTF_LEFTUP);

            // Text mode: one Escape discards text, a second closes.
            if !across {
                let monitor = cap.monitor_for(region);
                let fonts = toolbar::Fonts::new(&g, monitor.scale).unwrap();
                let cfg = crate::config::load().0;
                let layout = |popover| {
                    toolbar::layout(
                        &toolbar::Inputs {
                            region,
                            work: monitor.work,
                            user_pos: None,
                            show_magnifier: cfg.show_magnifier,
                            show_tool_options: cfg.show_tool_options,
                            popover,
                            clear_armed: false,
                            zoomed: false,
                        },
                        &fonts,
                        monitor.scale,
                    )
                };
                let click = |r: windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F| {
                    let (cx, cy) = (
                        cap.origin.0 + ((r.left + r.right) / 2.0) as i32,
                        cap.origin.1 + ((r.top + r.bottom) / 2.0) as i32,
                    );
                    mouse(cx, cy, MOUSEEVENTF_LEFTDOWN);
                    mouse(cx, cy, MOUSEEVENTF_LEFTUP);
                };
                // A compact bar reaches a tool through its group's flyout.
                let pick = |tool: crate::strokes::Tool| {
                    let bar = layout(None);
                    match bar.item_rect(toolbar::Hit::Bar(toolbar::Item::Tool(tool))) {
                        Some(r) => click(r),
                        None => {
                            let g = tool.group();
                            click(bar.item_rect(toolbar::Hit::Bar(toolbar::Item::Group(g))).unwrap());
                            let flyout = layout(Some(toolbar::Pop::Group(g)));
                            click(flyout.item_rect(toolbar::Hit::Pop(toolbar::PopItem::Tool(tool))).unwrap());
                        }
                    }
                };
                pick(crate::strokes::Tool::Pencil);
                let pristine = region_pixels(region);
                mouse(x + 50, y + 120, MOUSEEVENTF_LEFTDOWN);
                for n in 1..20 {
                    mouse(x + 50 + n * 10, y + 120 + (n % 4) * 5, MOUSE_EVENT_FLAGS(0));
                }
                mouse(x + 250, y + 120, MOUSEEVENTF_LEFTUP);
                let mut drawn = pristine.clone();
                assert!(
                    wait(|| {
                        drawn = region_pixels(region);
                        drawn != pristine
                    }),
                    "freehand stroke was not displayed"
                );
                key(VK_CONTROL, false);
                press(VIRTUAL_KEY(b'Z' as u16));
                assert!(wait(|| region_pixels(region) == pristine), "undo left stale stroke pixels");
                press(VIRTUAL_KEY(b'Y' as u16));
                key(VK_CONTROL, true);
                assert!(wait(|| region_pixels(region) == drawn), "redo did not restore stroke cache pixels");
                pick(crate::strokes::Tool::Text);
                mouse(x + 50, y + 70, MOUSEEVENTF_LEFTDOWN);
                mouse(x + 50, y + 70, MOUSEEVENTF_LEFTUP);
                press(VIRTUAL_KEY(b'A' as u16));
                press(VK_ESCAPE);
                assert!(overlay().is_some(), "first text Escape should only discard");
                press(VK_ESCAPE);
                assert!(wait(|| overlay().is_none()), "second text Escape should close");
                println!("Negative-monitor selection, freehand drawing, undo/redo, text discard then close passed");
            } else {
                key(VK_CONTROL, false);
                press(VIRTUAL_KEY(b'C' as u16));
                key(VK_CONTROL, true);
                assert!(wait(|| overlay().is_none()), "copy did not finish");
                OpenClipboard(Some(sink)).unwrap();
                let handle = GetClipboardData(CF_DIB.0 as u32).unwrap();
                let data = GlobalLock(HGLOBAL(handle.0));
                assert!(!data.is_null());
                let header = &*(data as *const BITMAPINFOHEADER);
                let dimensions = (header.biWidth, header.biHeight);
                let bytes = std::slice::from_raw_parts(
                    (data as *const u8).add(size_of::<BITMAPINFOHEADER>()),
                    region.w as usize * region.h as usize * 4,
                );
                let pixels: Vec<u8> = bytes.chunks_exact(region.w as usize * 4).rev().flatten().copied().collect();
                let _ = GlobalUnlock(HGLOBAL(handle.0));
                CloseClipboard().unwrap();
                assert_eq!(dimensions, (region.w, region.h), "export must use physical pixels");
                assert!(
                    pixels == expected.unwrap(),
                    "frozen image changed, rescaled or shifted across the monitor seam"
                );
                println!(
                    "Spanning-monitor export: {}x{} physical pixels, every BGRA byte matches frozen reference",
                    dimensions.0, dimensions.1
                );
            }
        }
        hold(Duration::from_millis(200));
        let before = resources(true);
        for _ in 0..8 {
            open(false);
            press(VK_ESCAPE);
            assert!(wait(|| overlay().is_none()));
        }
        // FindWindow stops seeing a window before its final nested COM/Win32
        // destruction has necessarily returned. Keep the same resource bound,
        // but wait for that pending cleanup instead of sampling mid-dispatch.
        let mut after = before;
        assert!(
            wait(|| {
                after = resources(false);
                after.0 <= before.0 + 8 && after.1 <= before.1 && after.2 <= before.2
            }),
            "capture/close leaked Win32 resources: {before:?} -> {after:?}"
        );
        resources(true);
        println!("Eight repeated capture/close cycles passed");
        crate::overlay::check_reentrant_lifetime(&g);
    }
}

fn thumbnails() -> Vec<HWND> {
    unsafe extern "system" fn collect(hwnd: HWND, lp: LPARAM) -> BOOL {
        unsafe {
            let mut class = [0; 64];
            let n = GetClassNameW(hwnd, &mut class) as usize;
            if String::from_utf16_lossy(&class[..n]) == "GlazierThumbnail" {
                (*(lp.0 as *mut Vec<HWND>)).push(hwnd);
            }
        }
        TRUE
    }
    let mut windows = Vec::new();
    unsafe {
        EnumWindows(Some(collect), LPARAM(&mut windows as *mut _ as isize)).unwrap();
    }
    windows
}

fn hold(duration: Duration) {
    let start = Instant::now();
    while start.elapsed() < duration {
        pump();
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn shortcut(vk: u8) {
    key(VK_CONTROL, false);
    press(VIRTUAL_KEY(vk as u16));
    key(VK_CONTROL, true);
}

fn select(region: capture::IRect) {
    mouse(region.x, region.y, MOUSEEVENTF_LEFTDOWN);
    mouse(region.x + region.w, region.y + region.h, MOUSE_EVENT_FLAGS(0));
    mouse(region.x + region.w, region.y + region.h, MOUSEEVENTF_LEFTUP);
}

fn bounds(hwnd: HWND) -> RECT {
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(hwnd, &mut rect).unwrap();
    }
    rect
}

fn png_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|f| f.path())
        .filter(|p| p.extension().is_some_and(|s| s == "png"))
        .collect()
}

fn explorer_named(name: &str) -> Option<HWND> {
    struct Search<'a> {
        name: &'a str,
        hwnd: Option<HWND>,
    }
    unsafe extern "system" fn find(hwnd: HWND, lp: LPARAM) -> BOOL {
        unsafe {
            let search = &mut *(lp.0 as *mut Search<'_>);
            let mut class = [0; 64];
            let n = GetClassNameW(hwnd, &mut class) as usize;
            if String::from_utf16_lossy(&class[..n]) == "CabinetWClass" {
                let mut title = [0; 1024];
                let n = GetWindowTextW(hwnd, &mut title) as usize;
                if String::from_utf16_lossy(&title[..n]).contains(search.name) {
                    search.hwnd = Some(hwnd);
                    return FALSE;
                }
            }
            TRUE
        }
    }
    let mut search = Search { name, hwnd: None };
    unsafe {
        let _ = EnumWindows(Some(find), LPARAM(&mut search as *mut _ as isize));
    }
    search.hwnd
}

#[test]
#[ignore = "controls the real desktop, opens a scratch Explorer folder and writes the clipboard"]
fn real_input_thumbnails() {
    use windows::Win32::UI::Shell::*;
    unsafe {
        assert!(FindWindowW(host::CLASS, None).is_ok(), "start target/release/glazier.exe first");
        let cfg = crate::config::load().0;
        assert!(cfg.show_thumbnail && cfg.thumbnail_seconds == 6, "this check expects default thumbnail settings");
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
        let previous = GetForegroundWindow();
        let mut cursor = POINT::default();
        GetCursorPos(&mut cursor).unwrap();
        unsafe extern "system" fn sink_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        let instance: HINSTANCE = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap().into();
        RegisterClassExW(&WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            hInstance: instance,
            lpszClassName: w!("GlazierRegressionSink"),
            lpfnWndProc: Some(sink_proc),
            hbrBackground: HBRUSH(GetStockObject(LTGRAY_BRUSH).0),
            ..Default::default()
        });
        let sink = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("GlazierRegressionSink"),
            w!("Glazier thumbnail regression"),
            WS_POPUP | WS_VISIBLE,
            100,
            100,
            500,
            400,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let _cleanup = Cleanup { sink, previous, cursor };
        let info = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: FindWindowW(host::CLASS, None).unwrap(),
            uID: 1,
            uFlags: NIF_INFO,
            ..Default::default()
        };
        let _ = Shell_NotifyIconW(NIM_MODIFY, &info);
        for hwnd in thumbnails() {
            PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)).unwrap();
        }
        assert!(wait(|| thumbnails().is_empty()));
        let cap = capture::capture_desktop().unwrap();
        let (save_dir, create) = crate::export::save_dir(&cfg).unwrap();
        if create {
            std::fs::create_dir_all(&save_dir).unwrap();
        }
        let old_files = png_files(&save_dir);
        let mut saved_paths = Vec::new();
        for (i, monitor) in cap.monitors.iter().enumerate().rev() {
            let b = monitor.bounds;
            let (mx, my) = (cap.origin.0 + b.x, cap.origin.1 + b.y);
            let mon = MonitorFromPoint(POINT { x: mx + b.w / 2, y: my + b.h / 2 }, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            GetMonitorInfoW(mon, &mut info).ok().unwrap();
            let work = info.rcWork;
            SetWindowPos(
                sink,
                Some(HWND_TOPMOST),
                work.left,
                work.top,
                work.right - work.left,
                work.bottom - work.top,
                SWP_SHOWWINDOW,
            )
            .unwrap();
            let _ = UpdateWindow(sink);
            host::force_foreground(sink);
            DwmFlush().unwrap();
            hold(Duration::from_millis(150));
            let mut reference = None;
            let ready = wait(|| {
                let frame = capture::capture_desktop().unwrap();
                let x = work.right - 100 - frame.origin.0;
                let y = work.bottom - 100 - frame.origin.1;
                let index = (y as usize * frame.width as usize + x as usize) * 4;
                // Wait for the fixture's LTGRAY_BRUSH client background to
                // reach DWM instead of retaining the preceding app's frame.
                if frame.pixels[index..index + 3] == [192; 3] {
                    reference = Some(frame);
                    true
                } else {
                    false
                }
            });
            assert!(ready, "reference window never reached the compositor");
            let reference = reference.unwrap();
            let region = capture::IRect { x: mx + 250, y: my + 220, w: 320, h: 180 };
            open(false);
            select(region);
            shortcut(b'C');
            assert!(wait(|| overlay().is_none() && thumbnails().len() == 1), "Copy preview missing");
            hold(Duration::from_millis(400));
            let thumb = thumbnails()[0];
            let r = bounds(thumb);
            assert!(r.left >= work.left && r.right <= work.right && r.top >= work.top && r.bottom <= work.bottom);
            assert!(((r.right - r.left) as f32 - 260.0 * monitor.scale).abs() <= 1.0);
            assert_ne!(GetForegroundWindow(), thumb, "preview stole focus");
            let style = WINDOW_EX_STYLE(GetWindowLongW(thumb, GWL_EXSTYLE) as u32);
            assert!(style.contains(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST));
            // The reference fixture is itself topmost. Order the already-
            // topmost preview above that test fixture without activating it.
            SetWindowPos(thumb, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE).unwrap();
            println!("Monitor {i}: Copy preview within work area at DPI scale {}", monitor.scale);

            // Hover for longer than its entire lifetime, then use the Copy button.
            let tx = (r.left + r.right) / 2;
            mouse(tx, r.top + 25, MOUSE_EVENT_FLAGS(0));
            hold(Duration::from_millis(6300));
            assert!(IsWindow(Some(thumb)).as_bool(), "hover did not pause dismissal");
            let k = monitor.scale;
            let button_y = r.bottom - (27.0 * k) as i32;
            let button_x = tx - (49.0 * k) as i32;
            OpenClipboard(Some(sink)).unwrap();
            windows::Win32::System::DataExchange::EmptyClipboard().unwrap();
            CloseClipboard().unwrap();
            let sequence = windows::Win32::System::DataExchange::GetClipboardSequenceNumber();
            mouse(button_x, button_y, MOUSEEVENTF_LEFTDOWN);
            mouse(button_x, button_y, MOUSEEVENTF_LEFTUP);
            assert_ne!(GetForegroundWindow(), thumb, "hover button activated preview");
            let copied = wait(|| {
                windows::Win32::System::DataExchange::GetClipboardSequenceNumber() != sequence
                    && windows::Win32::System::DataExchange::IsClipboardFormatAvailable(CF_DIB.0 as u32).is_ok()
            });
            assert!(copied, "hover Copy did not replace clipboard");
            assert!(wait(|| OpenClipboard(Some(sink)).is_ok()), "clipboard remained busy after hover Copy");
            let handle = GetClipboardData(CF_DIB.0 as u32).unwrap();
            let ptr = GlobalLock(HGLOBAL(handle.0));
            let header = &*ptr.cast::<BITMAPINFOHEADER>();
            assert_eq!((header.biWidth, header.biHeight), (region.w, region.h));
            let _ = GlobalUnlock(HGLOBAL(handle.0));
            CloseClipboard().unwrap();
            hold(Duration::from_millis(150));
            println!("Monitor {i}: hover pause >6s, hover Copy and no activation passed");

            // Click image, confirm a new editor (Ctrl+C immediately exports,
            // whereas a picker would still need a selection).
            mouse(tx, r.top + 25, MOUSEEVENTF_LEFTDOWN);
            mouse(tx, r.top + 25, MOUSEEVENTF_LEFTUP);
            let reopened = wait(|| overlay().is_some_and(|h| GetForegroundWindow() == h));
            assert!(
                reopened,
                "preview did not reopen: overlay {:?}, foreground {:?}, thumb {:?}, visible {}",
                overlay(),
                GetForegroundWindow(),
                thumb,
                IsWindowVisible(thumb).as_bool()
            );
            shortcut(b'S');
            assert!(wait(|| overlay().is_none() && thumbnails().len() == 2), "reopened editor Save failed");
            let paths: Vec<_> = png_files(&save_dir)
                .into_iter()
                .filter(|p| !old_files.contains(p) && !saved_paths.contains(p))
                .collect();
            assert_eq!(paths.len(), 1);
            let g = gfx::Gfx::new().unwrap();
            let image = crate::export::decode_png(&g, &paths[0]).unwrap();
            assert_eq!((image.width, image.height), (320, 180));
            saved_paths.extend(paths);
            println!("Monitor {i}: click reopens exported pixels; Save creates a second stacked preview");

            // Freeze the exact area that held the previews and compare all
            // exported pixels against the static window beneath them.
            let thumb = thumbnails()[0];
            hold(Duration::from_millis(400));
            let r = bounds(thumb);
            open(false);
            assert!(thumbnails().iter().all(|h| !IsWindowVisible(*h).as_bool()));
            let region =
                capture::IRect { x: r.left + 2, y: r.top + 2, w: r.right - r.left - 4, h: r.bottom - r.top - 4 };
            select(region);
            shortcut(b'C');
            assert!(wait(|| overlay().is_none()), "capture exclusion selection did not export");
            OpenClipboard(Some(sink)).unwrap();
            let handle = GetClipboardData(CF_DIB.0 as u32).unwrap();
            let ptr = GlobalLock(HGLOBAL(handle.0));
            let header = &*ptr.cast::<BITMAPINFOHEADER>();
            assert_eq!((header.biWidth, header.biHeight), (region.w, region.h));
            let bytes = std::slice::from_raw_parts(
                ptr.cast::<u8>().add(size_of::<BITMAPINFOHEADER>()),
                region.w as usize * region.h as usize * 4,
            );
            let pixels: Vec<_> = bytes.chunks_exact(region.w as usize * 4).rev().flatten().copied().collect();
            let _ = GlobalUnlock(HGLOBAL(handle.0));
            CloseClipboard().unwrap();
            let local = capture::IRect { x: region.x - reference.origin.0, y: region.y - reference.origin.1, ..region };
            let expected = reference.crop(local);
            let differences: Vec<_> = pixels
                .chunks_exact(4)
                .zip(expected.chunks_exact(4))
                .enumerate()
                .filter(|(_, (a, b))| a != b)
                .map(|(i, (a, b))| (i % region.w as usize, i / region.w as usize, a, b))
                .take(5)
                .collect();
            assert!(pixels == expected, "thumbnail leaked into next freeze; first differences {differences:?}");
            for hwnd in thumbnails() {
                PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)).unwrap();
            }
            assert!(wait(|| thumbnails().is_empty()));
            println!("Monitor {i}: next capture excludes thumbnails, pixel-for-pixel");
        }

        // Real OLE drop into a real Explorer folder. Keep only this test's
        // Explorer window and files in the cleanup scope.
        let scratch = std::env::temp_dir().join(format!("Glazier-drop-test-{}", std::process::id()));
        std::fs::create_dir(&scratch).unwrap();
        let args = HSTRING::from(format!("/n,\"{}\"", scratch.display()));
        ShellExecuteW(None, w!("open"), w!("explorer.exe"), &args, PCWSTR::null(), SW_SHOWNORMAL);
        let title = scratch.file_name().unwrap().to_string_lossy();
        assert!(wait(|| explorer_named(&title).is_some()), "scratch Explorer did not open");
        let explorer = explorer_named(&title).unwrap();
        let m =
            cap.monitors.iter().find(|m| m.bounds.contains((-cap.origin.0) as f32, (-cap.origin.1) as f32)).unwrap();
        SetWindowPos(sink, None, 0, 0, 0, 0, SWP_HIDEWINDOW | SWP_NOMOVE | SWP_NOSIZE).unwrap();
        SetWindowPos(explorer, Some(HWND_TOP), 0, 0, 0, 0, SWP_SHOWWINDOW | SWP_NOMOVE | SWP_NOSIZE).unwrap();
        host::force_foreground(explorer);
        open(false);
        select(capture::IRect { x: 300, y: 300, w: 320, h: 180 });
        shortcut(b'C');
        assert!(wait(|| overlay().is_none() && thumbnails().len() == 1));
        hold(Duration::from_millis(500));
        let thumb = thumbnails()[0];
        let r = bounds(thumb);
        // Use Explorer's actual position: a desktop tiling manager may place
        // it on either monitor rather than honoring a requested rectangle.
        let drop = bounds(explorer);
        let drop_x = drop.left + (drop.right - drop.left) * 5 / 8;
        let drop_y = drop.top + (drop.bottom - drop.top) * 3 / 5;
        mouse((r.left + r.right) / 2, r.top + 25, MOUSEEVENTF_LEFTDOWN);
        for n in 1..=24 {
            let x = (r.left + r.right) / 2 + (drop_x - (r.left + r.right) / 2) * n / 24;
            let y = r.top + 25 + (drop_y - r.top - 25) * n / 24;
            mouse(x, y, MOUSE_EVENT_FLAGS(0));
        }
        mouse(drop_x, drop_y, MOUSEEVENTF_LEFTUP);
        assert!(wait(|| !png_files(&scratch).is_empty()), "OLE drop did not create PNG in Explorer");
        let dropped = png_files(&scratch);
        let g = gfx::Gfx::new().unwrap();
        let image = crate::export::decode_png(&g, &dropped[0]).unwrap();
        assert_eq!((image.width, image.height), (320, 180));
        println!("Real DoDragDrop to Explorer copied a decodable 320x180 PNG");

        // Hover Save and Reveal on the copied preview.
        let r = bounds(thumb);
        let tx = (r.left + r.right) / 2;
        let by = r.bottom - (27.0 * m.scale) as i32;
        let folder_title = save_dir.file_name().unwrap().to_string_lossy();
        let original_folder = explorer_named(&folder_title);
        mouse(tx, r.top + 25, MOUSE_EVENT_FLAGS(0));
        mouse(tx - (17.0 * m.scale) as i32, by, MOUSEEVENTF_LEFTDOWN);
        mouse(tx - (17.0 * m.scale) as i32, by, MOUSEEVENTF_LEFTUP);
        assert!(wait(|| png_files(&save_dir).len() > old_files.len() + saved_paths.len()));
        saved_paths.extend(
            png_files(&save_dir)
                .into_iter()
                .filter(|p| !old_files.contains(p) && !saved_paths.contains(p))
                .collect::<Vec<_>>(),
        );
        mouse(tx + (15.0 * m.scale) as i32, by, MOUSEEVENTF_LEFTDOWN);
        mouse(tx + (15.0 * m.scale) as i32, by, MOUSEEVENTF_LEFTUP);
        assert!(wait(|| explorer_named(&folder_title).is_some()), "Show in folder did not reveal the saved capture");
        let revealed = explorer_named(&folder_title);
        hold(Duration::from_millis(150));
        println!("Hover Save created a PNG; Show in folder opened its Explorer folder");
        mouse(tx + (47.0 * m.scale) as i32, by, MOUSEEVENTF_LEFTDOWN);
        mouse(tx + (47.0 * m.scale) as i32, by, MOUSEEVENTF_LEFTUP);
        assert!(wait(|| thumbnails().is_empty()), "hover Close failed");
        PostMessageW(Some(explorer), WM_CLOSE, WPARAM(0), LPARAM(0)).unwrap();
        if let Some(hwnd) = revealed.filter(|h| Some(*h) != original_folder && *h != explorer) {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        for path in dropped {
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_dir(scratch).unwrap();
        for path in saved_paths {
            std::fs::remove_file(path).unwrap();
        }
        println!("Hover Close passed; scratch PNGs and saved test captures cleaned");

        open(false);
        select(capture::IRect { x: 300, y: 300, w: 320, h: 180 });
        shortcut(b'C');
        assert!(wait(|| overlay().is_none() && thumbnails().len() == 1));
        hold(Duration::from_millis(400));
        let before_reveal = png_files(&save_dir);
        let r = bounds(thumbnails()[0]);
        let tx = (r.left + r.right) / 2;
        let by = r.bottom - (27.0 * m.scale) as i32;
        let original_folder = explorer_named(&folder_title);
        mouse(tx, r.top + 25, MOUSE_EVENT_FLAGS(0));
        mouse(tx + (15.0 * m.scale) as i32, by, MOUSEEVENTF_LEFTDOWN);
        mouse(tx + (15.0 * m.scale) as i32, by, MOUSEEVENTF_LEFTUP);
        assert!(
            wait(|| png_files(&save_dir).len() > before_reveal.len()),
            "Show in folder did not save the copied capture"
        );
        let revealed_paths: Vec<_> = png_files(&save_dir).into_iter().filter(|p| !before_reveal.contains(p)).collect();
        assert!(wait(|| explorer_named(&folder_title).is_some()));
        let revealed = explorer_named(&folder_title);
        println!("Show in folder on an unsaved preview saved first and opened Explorer");
        mouse(200, 150, MOUSE_EVENT_FLAGS(0));
        hold(Duration::from_millis(6500));
        assert!(thumbnails().is_empty(), "preview failed to auto-dismiss after six seconds");
        for path in revealed_paths {
            std::fs::remove_file(path).unwrap();
        }
        if let Some(hwnd) = revealed.filter(|h| Some(*h) != original_folder) {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        println!("Unhovered six-second auto-dismiss passed");
    }
}

fn tray_menu_key(vk: VIRTUAL_KEY) {
    tray_menu_command(if vk == VK_END { "Exit" } else { "Reload settings" });
}

fn tray_menu_command(label: &str) {
    unsafe {
        // Inspect native menu rectangles, then select through real mouse input.
        PostMessageW(
            Some(FindWindowW(host::CLASS, None).unwrap()),
            WM_APP + 3,
            WPARAM(0),
            LPARAM(WM_CONTEXTMENU as isize),
        )
        .unwrap();
        assert!(wait(|| FindWindowW(w!("#32768"), None).is_ok()), "tray menu did not open");
        {
            let popup = FindWindowW(w!("#32768"), None).unwrap();
            let menu = HMENU(SendMessageW(popup, 0x01e1, None, None).0 as *mut _);
            let item = (0..GetMenuItemCount(Some(menu)))
                .find(|&i| {
                    let mut text = [0u16; 128];
                    let n = GetMenuStringW(menu, i as u32, Some(&mut text), MF_BYPOSITION) as usize;
                    String::from_utf16_lossy(&text[..n]) == label
                })
                .expect("tray command missing");
            let mut r = RECT::default();
            GetMenuItemRect(None, menu, item as u32, &mut r).unwrap();
            let (x, y) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
            mouse(x, y, MOUSE_EVENT_FLAGS(0));
            mouse(x, y, MOUSEEVENTF_LEFTDOWN);
            mouse(x, y, MOUSEEVENTF_LEFTUP);
        }
        assert!(wait(|| FindWindowW(w!("#32768"), None).is_err()), "tray menu did not close");
    }
}

#[test]
#[ignore = "temporarily edits and restores settings, controls desktop input and exits the release app"]
fn real_input_thumbnail_settings_and_exit() {
    struct Backup {
        path: std::path::PathBuf,
        bytes: Vec<u8>,
    }
    impl Drop for Backup {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.path, &self.bytes);
        }
    }
    unsafe {
        assert!(FindWindowW(host::CLASS, None).is_ok());
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
        let previous = GetForegroundWindow();
        let mut cursor = POINT::default();
        GetCursorPos(&mut cursor).unwrap();
        let sink = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("Glazier settings regression"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            100,
            100,
            450,
            300,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let _cleanup = Cleanup { sink, previous, cursor };
        let path = crate::config::path();
        let backup = Backup { bytes: std::fs::read(&path).unwrap(), path };
        let mut cfg: toml::Table = std::str::from_utf8(&backup.bytes).unwrap().parse().unwrap();
        for hwnd in thumbnails() {
            PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)).unwrap();
        }
        assert!(wait(|| thumbnails().is_empty()));
        open(false);
        select(capture::IRect { x: 300, y: 300, w: 320, h: 180 });
        shortcut(b'C');
        assert!(wait(|| overlay().is_none() && thumbnails().len() == 1));
        let thumb = thumbnails()[0];
        cfg.insert("thumbnail_seconds".into(), toml::Value::Integer(0));
        cfg.insert("show_thumbnail".into(), toml::Value::Boolean(true));
        std::fs::write(&backup.path, toml::to_string(&cfg).unwrap()).unwrap();
        tray_menu_key(VIRTUAL_KEY(b'R' as u16));
        mouse(200, 150, MOUSE_EVENT_FLAGS(0));
        hold(Duration::from_millis(6600));
        assert!(IsWindow(Some(thumb)).as_bool(), "reload did not apply thumbnail_seconds = 0 to an existing preview");
        println!("Reload applied zero lifetime to an existing preview; it stayed beyond six seconds");
        cfg.insert("show_thumbnail".into(), toml::Value::Boolean(false));
        std::fs::write(&backup.path, toml::to_string(&cfg).unwrap()).unwrap();
        tray_menu_key(VIRTUAL_KEY(b'R' as u16));
        assert!(wait(|| thumbnails().is_empty()), "reload did not disable existing previews");
        std::fs::write(&backup.path, &backup.bytes).unwrap();
        tray_menu_key(VIRTUAL_KEY(b'R' as u16));
        open(false);
        select(capture::IRect { x: 300, y: 300, w: 320, h: 180 });
        shortcut(b'C');
        assert!(wait(|| overlay().is_none() && thumbnails().len() == 1));
        println!("Reload disabled existing previews and restored preview creation after settings restoration");
        let cache = std::env::temp_dir().join("Glazier-thumbnails");
        assert!(!png_files(&cache).is_empty());
        tray_menu_key(VK_END);
        assert!(wait(|| FindWindowW(host::CLASS, None).is_err()), "tray Exit did not stop Glazier");
        assert!(wait(|| png_files(&cache).is_empty()), "clean exit left temporary PNGs");
        println!("Native tray Exit removed the temporary PNG cache");
    }
}

fn record_key() {
    key(VK_LWIN, false);
    key(VK_SHIFT, false);
    press(VIRTUAL_KEY(b'R' as u16));
    key(VK_SHIFT, true);
    key(VK_LWIN, true);
}

fn recording_bar() -> Option<HWND> {
    unsafe { FindWindowW(crate::recording::CLASS, None).ok() }
}

fn recording_button(index: usize, active: bool) {
    let hwnd = recording_bar().expect("recording controls disappeared");
    use crate::recording::Button::*;
    let r = bounds(hwnd);
    let button = if active { [Pause, Mic, Sound, Stop, Discard][index] } else { [Record, Mic, Sound, Cancel][index] };
    let b = crate::recording::button_rect(button, active, r.right - r.left).unwrap();
    let (x, y) = (r.left + ((b.left + b.right) / 2.0) as i32, r.top + ((b.top + b.bottom) / 2.0) as i32);
    mouse(x, y, MOUSEEVENTF_LEFTDOWN);
    mouse(x, y, MOUSEEVENTF_LEFTUP);
}

fn mp4_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|f| f.path())
        .filter(|p| {
            p.extension().is_some_and(|s| s == "mp4") && !p.file_name().unwrap().to_string_lossy().starts_with('.')
        })
        .collect()
}

fn read_recording(
    path: &std::path::Path,
    expected: (u32, u32),
    audio_expected: bool,
    check_pixels: bool,
) -> (f64, usize, i16) {
    use windows::Win32::Media::MediaFoundation::*;
    unsafe {
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 1).unwrap();
        let attrs = attrs.unwrap();
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1).unwrap();
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attrs).unwrap();
        let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let audio = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
        let native = reader.GetNativeMediaType(video, 0).unwrap();
        assert_eq!(native.GetGUID(&MF_MT_SUBTYPE).unwrap(), MFVideoFormat_H264);
        let dimensions = native.GetUINT64(&MF_MT_FRAME_SIZE).unwrap();
        assert_eq!(((dimensions >> 32) as u32, dimensions as u32), expected);
        assert_eq!(expected.0 % 2, 0);
        assert_eq!(expected.1 % 2, 0);
        assert_eq!(reader.GetNativeMediaType(audio, 0).is_ok(), audio_expected, "unexpected audio stream");
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false).unwrap();
        reader.SetStreamSelection(video, true).unwrap();
        let decoded = MFCreateMediaType().unwrap();
        decoded.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).unwrap();
        decoded.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).unwrap();
        reader.SetCurrentMediaType(video, None, &decoded).unwrap();
        let (mut end, mut count) = (0, 0);
        loop {
            let (mut flags, mut time, mut sample) = (0, 0, None);
            reader.ReadSample(video, 0, None, Some(&mut flags), Some(&mut time), Some(&mut sample)).unwrap();
            if let Some(s) = sample {
                count += 1;
                end = end.max(time + s.GetSampleDuration().unwrap_or(0));
                if check_pixels || (expected == (640, 360) && count <= 30) {
                    let buffer = s.ConvertToContiguousBuffer().unwrap();
                    let (mut data, mut len) = (std::ptr::null_mut(), 0);
                    buffer.Lock(&mut data, None, Some(&mut len)).unwrap();
                    let size = expected.0 as usize * expected.1 as usize * 4;
                    assert!(len as usize >= size);
                    // Decoders can append macroblock padding below the frame.
                    let bytes = std::slice::from_raw_parts(data, size);
                    // The reference window is solid RGB(40, 80, 120). A bar or
                    // border inside the selected area creates thousands of
                    // clearly different pixels even after H.264 conversion.
                    let bad = bytes
                        .chunks_exact(4)
                        .filter(|p| {
                            (p[0] as i32 - 120).abs() > 16
                                || (p[1] as i32 - 80).abs() > 16
                                || (p[2] as i32 - 40).abs() > 16
                        })
                        .count();
                    buffer.Unlock().unwrap();
                    assert!(
                        bad < expected.0 as usize * expected.1 as usize / 200,
                        "captured controls or unexpected pixels in frame {count}: {bad}"
                    );
                }
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
        }
        assert!(count > 10, "not enough decodable video frames");
        let mut peak = 0i16;
        if audio_expected {
            let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None).unwrap();
            assert_eq!(
                reader.GetNativeMediaType(audio, 0).unwrap().GetGUID(&MF_MT_SUBTYPE).unwrap(),
                MFAudioFormat_AAC
            );
            reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false).unwrap();
            reader.SetStreamSelection(audio, true).unwrap();
            let decoded = MFCreateMediaType().unwrap();
            decoded.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).unwrap();
            decoded.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM).unwrap();
            decoded.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).unwrap();
            reader.SetCurrentMediaType(audio, None, &decoded).unwrap();
            loop {
                let (mut flags, mut sample) = (0, None);
                reader.ReadSample(audio, 0, None, Some(&mut flags), None, Some(&mut sample)).unwrap();
                if let Some(s) = sample {
                    let buffer = s.ConvertToContiguousBuffer().unwrap();
                    let (mut data, mut len) = (std::ptr::null_mut(), 0);
                    buffer.Lock(&mut data, None, Some(&mut len)).unwrap();
                    for bytes in std::slice::from_raw_parts(data, len as usize).chunks_exact(2) {
                        peak = peak.max(i16::from_le_bytes([bytes[0], bytes[1]]).saturating_abs());
                    }
                    buffer.Unlock().unwrap();
                }
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    break;
                }
            }
        }
        (end as f64 / 10_000_000.0, count, peak)
    }
}

#[test]
#[ignore = "records both monitors using real input, plays a tone, writes clipboard and temporarily restores settings"]
fn real_input_recording() {
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_LOOP, SND_NODEFAULT};
    use windows::Win32::Media::MediaFoundation::*;
    struct Files {
        config: Vec<u8>,
        state: Option<Vec<u8>>,
        dir: std::path::PathBuf,
    }
    impl Drop for Files {
        fn drop(&mut self) {
            unsafe {
                let _ = PlaySoundW(PCWSTR::null(), None, SND_ASYNC);
                if let Some(bar) = recording_bar() {
                    let _ = PostMessageW(Some(bar), WM_CLOSE, WPARAM(0), LPARAM(0));
                    wait(|| recording_bar().is_none());
                }
                for h in thumbnails() {
                    let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
            let _ = std::fs::write(crate::config::path(), &self.config);
            let path = crate::config::dir().join("state.toml");
            if let Some(bytes) = &self.state {
                let _ = std::fs::write(path, bytes);
            } else {
                let _ = std::fs::remove_file(path);
            }
            if !std::thread::panicking() {
                tray_menu_key(VIRTUAL_KEY(b'R' as u16));
            }
            // Only remove files created in this uniquely named test directory.
            if let Ok(files) = std::fs::read_dir(&self.dir) {
                for f in files.flatten() {
                    let _ = std::fs::remove_file(f.path());
                }
            }
            let _ = std::fs::remove_dir(&self.dir);
            unsafe {
                let _ = MFShutdown();
            }
        }
    }
    unsafe {
        assert!(FindWindowW(host::CLASS, None).is_ok(), "start target/release/glazier.exe first");
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
        MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap();
        let previous = GetForegroundWindow();
        let mut cursor = POINT::default();
        GetCursorPos(&mut cursor).unwrap();
        let instance: HINSTANCE = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap().into();
        let brush = CreateSolidBrush(COLORREF(40 | (80 << 8) | (120 << 16)));
        unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        RegisterClassExW(&WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            hInstance: instance,
            lpszClassName: w!("GlazierRecordingRegression"),
            lpfnWndProc: Some(proc),
            hbrBackground: brush,
            ..Default::default()
        });
        let sink = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("GlazierRecordingRegression"),
            w!("Recording test"),
            WS_POPUP | WS_VISIBLE,
            100,
            100,
            1200,
            1000,
            None,
            None,
            Some(instance),
            None,
        )
        .unwrap();
        let _cleanup = Cleanup { sink, previous, cursor };
        SetWindowPos(sink, Some(HWND_TOPMOST), 100, 100, 1200, 1000, SWP_SHOWWINDOW).unwrap();
        mouse(150, 150, MOUSEEVENTF_LEFTDOWN);
        mouse(150, 150, MOUSEEVENTF_LEFTUP);
        host::force_foreground(sink);
        let dir = std::env::temp_dir().join(format!(
            "glazier-recording-test-{}-{}",
            std::process::id(),
            crate::export::file_stem("%H%M%S")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let files = Files {
            config: std::fs::read(crate::config::path()).unwrap(),
            state: std::fs::read(crate::config::dir().join("state.toml")).ok(),
            dir,
        };
        let mut cfg: toml::Table = std::str::from_utf8(&files.config).unwrap().parse().unwrap();
        cfg.insert("recording_directory".into(), toml::Value::String(files.dir.display().to_string()));
        cfg.insert("recording_hotkey".into(), toml::Value::String("Win+Shift+R".into()));
        cfg.insert("recording_fps".into(), toml::Value::Integer(30));
        cfg.insert("thumbnail_seconds".into(), toml::Value::Integer(0));
        cfg.insert("show_thumbnail".into(), toml::Value::Boolean(true));
        std::fs::write(crate::config::path(), toml::to_string(&cfg).unwrap()).unwrap();
        std::fs::write(crate::config::dir().join("state.toml"), "mic = false\nsound = true\n").unwrap();
        tray_menu_key(VIRTUAL_KEY(b'R' as u16));
        let cap = capture::capture_desktop().unwrap();
        println!("Recording desktop: {:?}, monitors {:?}", cap.origin, cap.monitors);
        assert!(cap.monitors.len() >= 2, "two monitors required");
        match std::thread::spawn(crate::recording_media::probe_microphone).join().unwrap() {
            Ok(peak) => {
                println!("Microphone WASAPI stream opened, observed peak {peak:.6}; a real mic signal is not required")
            }
            Err(e) => {
                println!("Microphone unavailable on this PC: {}; live toggling must still preserve video", e.message())
            }
        }

        // A deterministic two-second tone exercises real WASAPI loopback.
        let tone = files.dir.join("tone.wav");
        let mut pcm = Vec::new();
        for n in 0..96_000 {
            let v = ((n as f64 * std::f64::consts::TAU * 440.0 / 48_000.0).sin() * 6000.0) as i16;
            pcm.extend_from_slice(&v.to_le_bytes());
            pcm.extend_from_slice(&v.to_le_bytes());
        }
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36u32 + pcm.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&48_000u32.to_le_bytes());
        wav.extend_from_slice(&192_000u32.to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
        wav.extend_from_slice(&pcm);
        std::fs::write(&tone, wav).unwrap();

        for (index, m) in cap.monitors.iter().enumerate() {
            for h in thumbnails() {
                PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0)).unwrap();
            }
            assert!(wait(|| thumbnails().is_empty()));
            let x = cap.origin.0 + m.bounds.x + 400;
            let y = cap.origin.1 + m.bounds.y + 300;
            SetWindowPos(sink, Some(HWND_TOPMOST), x - 80, y - 80, 1200, 1000, SWP_SHOWWINDOW).unwrap();
            let _ = InvalidateRect(Some(sink), None, true);
            let _ = UpdateWindow(sink);
            host::force_foreground(sink);
            mouse(x - 40, y - 40, MOUSE_EVENT_FLAGS(0));
            record_key();
            assert!(wait(|| overlay().is_some()), "Win+Shift+R picker did not open");
            select(capture::IRect { x, y, w: 641, h: 361 });
            assert!(
                wait(|| overlay().is_none() && recording_bar().is_some()),
                "selection did not unfreeze into controls"
            );
            let bar = recording_bar().unwrap();
            let border = FindWindowW(w!("GlazierRecordingBorder"), None).unwrap();
            let br = bounds(border);
            assert_eq!((br.left + 2, br.top + 2, br.right - br.left - 4, br.bottom - br.top - 4), (x, y, 640, 360));
            for h in [bar, border] {
                let mut affinity = 0u32;
                GetWindowDisplayAffinity(h, &mut affinity).unwrap();
                assert_eq!(affinity, WDA_EXCLUDEFROMCAPTURE.0);
            }
            // Deliberately put both windows inside the video, then prove they
            // are absent in decoded samples and screenshot exports.
            let r = bounds(bar);
            SetWindowPos(bar, Some(HWND_TOPMOST), x + 10, y + 170, r.right - r.left, r.bottom - r.top, SWP_NOACTIVATE)
                .unwrap();
            SetWindowPos(border, Some(HWND_TOPMOST), x + 3, y + 3, 634, 354, SWP_NOACTIVATE).unwrap();
            recording_button(0, false);
            assert!(
                wait(|| {
                    let mut text = [0u16; 256];
                    let n = GetWindowTextW(bar, &mut text) as usize;
                    let text = String::from_utf16_lossy(&text[..n]);
                    if text.contains('|') {
                        println!("Monitor {index}: {text}");
                        true
                    } else {
                        false
                    }
                }),
                "encoder did not start"
            );
            assert_eq!(GetForegroundWindow(), sink, "recording controls stole focus");
            let start = Instant::now();
            assert!(
                PlaySoundW(&HSTRING::from(tone.as_os_str()), None, SND_FILENAME | SND_ASYNC | SND_LOOP | SND_NODEFAULT)
                    .as_bool()
            );
            mouse(x - 40, y - 40, MOUSE_EVENT_FLAGS(0));
            hold(Duration::from_millis(1300));
            recording_button(0, true);
            let pause = Instant::now();
            hold(Duration::from_millis(1000));
            recording_button(0, true);
            let paused = pause.elapsed();
            // Live mute/unmute keeps the same recording and audio clients.
            recording_button(2, true);
            recording_button(2, true);
            recording_button(1, true);
            hold(Duration::from_millis(250));
            recording_button(1, true);
            mouse(x - 40, y - 40, MOUSE_EVENT_FLAGS(0));
            if index == 0 {
                open(true);
                select(capture::IRect { x, y, w: 640, h: 360 });
                shortcut(b'C');
                assert!(wait(|| overlay().is_none()), "screenshot during recording failed");
                OpenClipboard(Some(sink)).unwrap();
                let handle = GetClipboardData(CF_DIB.0 as u32).unwrap();
                let ptr = GlobalLock(HGLOBAL(handle.0));
                let header = (ptr as *const BITMAPINFOHEADER).read();
                assert_eq!((header.biWidth, header.biHeight), (640, 360));
                let bytes =
                    std::slice::from_raw_parts((ptr as *const u8).add(size_of::<BITMAPINFOHEADER>()), 640 * 360 * 4);
                assert!(bytes.chunks_exact(4).all(|p| p[..3] == [120, 80, 40]), "bar or border appeared in screenshot");
                let _ = GlobalUnlock(HGLOBAL(handle.0));
                CloseClipboard().unwrap();
                println!("Screenshot during recording: exact 640x360 pixels, controls excluded");
            }
            hold(Duration::from_millis(1100));
            let expected = start.elapsed().saturating_sub(paused).as_secs_f64();
            if index == 0 {
                record_key();
            } else {
                recording_button(3, true);
            }
            let _ = PlaySoundW(PCWSTR::null(), None, SND_ASYNC);
            assert!(wait(|| recording_bar().is_none()), "stop did not finalize within four seconds");
            let paths = mp4_files(&files.dir);
            assert_eq!(paths.len(), index + 1);
            let path = paths.iter().max_by_key(|p| std::fs::metadata(p).unwrap().modified().unwrap()).unwrap();
            // Screenshot editing briefly freezes the video on monitor zero.
            let (duration, frames, peak) = read_recording(path, (640, 360), true, index != 0);
            assert!((duration - expected).abs() < 0.65, "pause timestamps: duration={duration}, expected={expected}");
            assert!(peak > 100, "system loopback was silent: {peak}");
            println!(
                "Monitor {index}: finalized H.264/AAC 640x360, {frames} frames, {duration:.3}s (expected {expected:.3}s excluding pause), audio peak {peak}; stop via {}",
                if index == 0 { "hotkey" } else { "button" }
            );
            assert!(wait(|| !thumbnails().is_empty()), "video poster was not shown");
            hold(Duration::from_millis(350));
            let video = FindWindowW(crate::thumbnail::CLASS, w!("Glazier recording preview")).unwrap();
            let r = bounds(video);
            mouse((r.left + r.right) / 2, r.top + 25, MOUSE_EVENT_FLAGS(0));
            hold(Duration::from_millis(300));
            let k = GetDpiForWindow(video) as f32 / 96.0;
            let cx = (r.left + r.right) / 2;
            let cy = r.bottom - (27.0 * k) as i32;
            mouse(cx, cy, MOUSEEVENTF_LEFTDOWN);
            mouse(cx, cy, MOUSEEVENTF_LEFTUP);
            OpenClipboard(Some(sink)).unwrap();
            assert!(
                IsClipboardFormatAvailable(windows::Win32::System::Ole::CF_HDROP.0 as u32).is_ok(),
                "video Copy must offer CF_HDROP"
            );
            CloseClipboard().unwrap();
        }
        // Neither source enabled means a video-only final MP4. Unmute is also
        // supported after starting muted because stream creation is deferred
        // only at publication, through a lossless remux.
        std::fs::write(crate::config::dir().join("state.toml"), "mic = false\nsound = false\n").unwrap();
        record_key();
        assert!(wait(|| overlay().is_some()));
        select(capture::IRect { x: 500, y: 500, w: 320, h: 180 });
        assert!(wait(|| recording_bar().is_some() && overlay().is_none()));
        recording_button(0, false);
        hold(Duration::from_millis(1500));
        recording_button(3, true);
        assert!(wait(|| recording_bar().is_none()));
        let paths = mp4_files(&files.dir);
        let path = paths.iter().max_by_key(|p| std::fs::metadata(p).unwrap().modified().unwrap()).unwrap();
        let (duration, _, _) = read_recording(path, (320, 180), false, false);
        println!("Both sources off: video-only MP4 finalized, {duration:.3}s");
        let before = paths.len();
        record_key();
        assert!(wait(|| overlay().is_some()));
        select(capture::IRect { x: 500, y: 500, w: 320, h: 180 });
        assert!(wait(|| recording_bar().is_some() && overlay().is_none()));
        recording_button(0, false);
        hold(Duration::from_millis(350));
        assert!(
            PlaySoundW(&HSTRING::from(tone.as_os_str()), None, SND_FILENAME | SND_ASYNC | SND_LOOP | SND_NODEFAULT)
                .as_bool()
        );
        recording_button(2, true);
        hold(Duration::from_millis(1200));
        tray_menu_command("Stop recording");
        let _ = PlaySoundW(PCWSTR::null(), None, SND_ASYNC);
        assert!(wait(|| recording_bar().is_none()), "tray Stop recording failed");
        let paths = mp4_files(&files.dir);
        assert_eq!(paths.len(), before + 1);
        let path = paths.iter().max_by_key(|p| std::fs::metadata(p).unwrap().modified().unwrap()).unwrap();
        let (_, _, peak) = read_recording(path, (320, 180), true, false);
        assert!(peak > 100);
        println!("Live unmute from both sources off produced non-silent AAC (peak {peak}); tray Stop finalized it");
        let count = paths.len();
        record_key();
        assert!(wait(|| overlay().is_some()));
        select(capture::IRect { x: 500, y: 500, w: 320, h: 180 });
        assert!(wait(|| recording_bar().is_some() && overlay().is_none()));
        recording_button(0, false);
        hold(Duration::from_millis(1200));
        recording_button(4, true);
        assert!(wait(|| recording_bar().is_none()));
        assert_eq!(mp4_files(&files.dir).len(), count, "Discard published a file");
        assert!(
            std::fs::read_dir(&files.dir)
                .unwrap()
                .flatten()
                .all(|f| !f.file_name().to_string_lossy().starts_with(".glazier-")),
            "temporary recording files remained"
        );
        println!("Discard removed partial MP4; all test recordings and settings will be cleaned up");
        resources(true);
        drop(files);
    }
}
