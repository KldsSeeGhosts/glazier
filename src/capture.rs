//! Freezes the virtual desktop in physical pixels and records window bounds.

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct IRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl IRect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x as f32 && y >= self.y as f32 && x < (self.x + self.w) as f32 && y < (self.y + self.h) as f32
    }

    pub fn intersect(&self, o: &IRect) -> Option<IRect> {
        let l = self.x.max(o.x);
        let t = self.y.max(o.y);
        let r = (self.x + self.w).min(o.x + o.w);
        let b = (self.y + self.h).min(o.y + o.h);
        (r > l && b > t).then_some(IRect { x: l, y: t, w: r - l, h: b - t })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Monitor {
    /// Relative to the virtual desktop's top-left, in physical pixels.
    pub bounds: IRect,
    /// Monitor bounds minus taskbars and other appbars.
    pub work: IRect,
    pub scale: f32,
}

/// A still frame of all monitors in physical pixels, including desktop gaps.
pub struct Capture {
    pub origin: (i32, i32),
    pub width: i32,
    pub height: i32,
    /// Top-down BGRA, fully opaque.
    pub pixels: Vec<u8>,
    pub monitors: Vec<Monitor>,
    /// Visible top-level windows, topmost first, relative to the virtual desktop.
    pub windows: Vec<IRect>,
    pub cursor: (i32, i32),
}

impl Capture {
    pub fn bounds(&self) -> IRect {
        IRect { x: 0, y: 0, w: self.width, h: self.height }
    }

    pub fn crop(&self, r: IRect) -> Vec<u8> {
        assert_eq!(r.intersect(&self.bounds()), Some(r));
        let mut out = Vec::with_capacity(r.w as usize * r.h as usize * 4);
        let stride = (self.width * 4) as usize;
        for y in r.y..r.y + r.h {
            let start = y as usize * stride + r.x as usize * 4;
            out.extend_from_slice(&self.pixels[start..start + r.w as usize * 4]);
        }
        out
    }

    pub fn monitor_at(&self, x: f32, y: f32) -> Monitor {
        self.monitors.iter().find(|m| m.bounds.contains(x, y)).copied().unwrap_or(self.monitors[0])
    }

    pub fn monitor_for(&self, region: IRect) -> Monitor {
        *self.monitors.iter().max_by_key(|m| m.bounds.intersect(&region).map_or(0, |r| r.w as i64 * r.h as i64)).unwrap()
    }

    pub fn hover_target(&self, x: f32, y: f32) -> IRect {
        self.windows.iter().find(|r| r.contains(x, y)).copied().unwrap_or(self.monitor_at(x, y).bounds)
    }
}

pub fn capture_desktop() -> Result<Capture> {
    unsafe {
        let mut pt = POINT::default();
        GetCursorPos(&mut pt)?;
        let mut monitors = Vec::<Monitor>::new();
        EnumDisplayMonitors(None, None, Some(enum_monitor), LPARAM(&mut monitors as *mut _ as isize)).ok()?;
        let screen = desktop_bounds(&monitors).ok_or_else(|| Error::from(E_FAIL))?;
        let (width, height) = (screen.w, screen.h);
        // One blit freezes all displays before any overlay is shown.
        let mut pixels = grab(screen.x, screen.y, width, height)?;
        for m in &mut monitors {
            m.bounds.x -= screen.x;
            m.bounds.y -= screen.y;
            m.work.x -= screen.x;
            m.work.y -= screen.y;
        }
        blacken_gaps(&mut pixels, width, height, &monitors);
        let windows = visible_windows(&screen);
        Ok(Capture {
            origin: (screen.x, screen.y),
            width,
            height,
            pixels,
            monitors,
            windows,
            cursor: (pt.x - screen.x, pt.y - screen.y),
        })
    }
}

fn blacken_gaps(pixels: &mut [u8], width: i32, height: i32, monitors: &[Monitor]) {
    let mut spans = Vec::with_capacity(monitors.len());
    for y in 0..height {
        let row = &mut pixels[y as usize * width as usize * 4..(y + 1) as usize * width as usize * 4];
        spans.clear();
        spans.extend(
            monitors
                .iter()
                .filter(|m| y >= m.bounds.y && y < m.bounds.y + m.bounds.h)
                .map(|m| (m.bounds.x, m.bounds.x + m.bounds.w)),
        );
        spans.sort_unstable();
        let mut end = 0;
        for (left, right) in spans.iter().copied().chain(std::iter::once((width, width))) {
            for px in row[end as usize * 4..left.max(end) as usize * 4].chunks_exact_mut(4) {
                px.copy_from_slice(&[0, 0, 0, 255]);
            }
            end = end.max(right);
        }
    }
}

fn desktop_bounds(monitors: &[Monitor]) -> Option<IRect> {
    let first = monitors.first()?.bounds;
    Some(monitors.iter().skip(1).fold(first, |r, m| {
        let (x, y) = (r.x.min(m.bounds.x), r.y.min(m.bounds.y));
        IRect { x, y, w: (r.x + r.w).max(m.bounds.x + m.bounds.w) - x, h: (r.y + r.h).max(m.bounds.y + m.bounds.h) - y }
    }))
}

unsafe extern "system" fn enum_monitor(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    unsafe {
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            let (mut x, mut y) = (96, 96);
            let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y);
            let r = info.rcMonitor;
            let w = info.rcWork;
            (*(data.0 as *mut Vec<Monitor>)).push(Monitor {
                bounds: IRect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top },
                work: IRect { x: w.left, y: w.top, w: w.right - w.left, h: w.bottom - w.top },
                scale: x as f32 / 96.0,
            });
        }
        TRUE
    }
}

unsafe fn grab(x: i32, y: i32, w: i32, h: i32) -> Result<Vec<u8>> {
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return Err(Error::from_thread());
        }
        let mem = CreateCompatibleDC(Some(screen));
        if mem.is_invalid() {
            let error = Error::from_thread();
            ReleaseDC(None, screen);
            return Err(error);
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let result = CreateDIBSection(Some(mem), &info, DIB_RGB_COLORS, &mut bits, None, 0).and_then(|dib| {
            let old = SelectObject(mem, dib.into());
            let blit = BitBlt(mem, 0, 0, w, h, Some(screen), x, y, SRCCOPY | CAPTUREBLT);
            let _ = GdiFlush();
            let mut pixels = Vec::new();
            if blit.is_ok() {
                pixels = std::slice::from_raw_parts(bits as *const u8, w as usize * h as usize * 4).to_vec();
                for px in pixels.chunks_exact_mut(4) {
                    px[3] = 255;
                }
            }
            SelectObject(mem, old);
            let _ = DeleteObject(dib.into());
            blit.map(|_| pixels)
        });
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_origins_and_mixed_dpi_stay_in_physical_pixels() {
        let monitors = vec![
            Monitor {
                bounds: IRect { x: -3840, y: -200, w: 3840, h: 2160 },
                work: IRect { x: -3840, y: -200, w: 3840, h: 2120 },
                scale: 1.75,
            },
            Monitor {
                bounds: IRect { x: 0, y: 0, w: 3840, h: 2160 },
                work: IRect { x: 0, y: 0, w: 3840, h: 2120 },
                scale: 1.0,
            },
        ];
        assert_eq!(desktop_bounds(&monitors), Some(IRect { x: -3840, y: -200, w: 7680, h: 2360 }));
    }

    #[test]
    fn crop_spans_monitor_seam_without_scaling() {
        let cap = Capture {
            origin: (-2, 0),
            width: 4,
            height: 2,
            cursor: (0, 0),
            pixels: (0..8).flat_map(|n| [n, 0, 0, 255]).collect(),
            monitors: vec![
                Monitor { bounds: IRect { x: 0, y: 0, w: 2, h: 2 }, work: IRect { x: 0, y: 0, w: 2, h: 2 }, scale: 1.75 },
                Monitor { bounds: IRect { x: 2, y: 0, w: 2, h: 2 }, work: IRect { x: 2, y: 0, w: 2, h: 2 }, scale: 1.0 },
            ],
            windows: vec![IRect { x: 1, y: 0, w: 3, h: 1 }],
        };
        assert_eq!(cap.crop(IRect { x: 1, y: 0, w: 2, h: 2 }), vec![1, 0, 0, 255, 2, 0, 0, 255, 5, 0, 0, 255, 6, 0, 0, 255]);
        assert_eq!(cap.hover_target(3.0, 0.0), cap.windows[0]);
        assert_eq!(cap.hover_target(3.0, 1.0), cap.monitors[1].bounds);
        assert_eq!(cap.monitor_for(IRect { x: 1, y: 0, w: 3, h: 2 }).scale, 1.0);
    }

    #[test]
    fn offset_monitor_gaps_are_black_and_opaque() {
        let mut pixels = vec![99; 4 * 2 * 4];
        let monitors = [
            Monitor { bounds: IRect { x: 0, y: 0, w: 2, h: 1 }, work: IRect { x: 0, y: 0, w: 2, h: 1 }, scale: 1.75 },
            Monitor { bounds: IRect { x: 2, y: 1, w: 2, h: 1 }, work: IRect { x: 2, y: 1, w: 2, h: 1 }, scale: 1.0 },
        ];
        blacken_gaps(&mut pixels, 4, 2, &monitors);
        assert_eq!(&pixels[..8], &[99; 8]);
        assert_eq!(&pixels[8..24], &[0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255]);
        assert_eq!(&pixels[24..], &[99; 8]);
    }
}

struct Enum {
    screen: IRect,
    found: Vec<IRect>,
}

fn visible_windows(screen: &IRect) -> Vec<IRect> {
    let mut state = Enum { screen: *screen, found: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut state as *mut Enum as isize));
    }
    state.found
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    unsafe {
        let state = &mut *(lparam.0 as *mut Enum);
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return TRUE;
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, 4);
        if cloaked != 0 {
            return TRUE;
        }
        let mut class = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut class) as usize;
        let class = String::from_utf16_lossy(&class[..n]);
        if class == "Progman" || class == "WorkerW" {
            return TRUE;
        }
        let ex = WINDOW_EX_STYLE(GetWindowLongW(hwnd, GWL_EXSTYLE) as u32);
        let taskbar = class == "Shell_TrayWnd" || class == "Shell_SecondaryTrayWnd";
        if !taskbar && (ex.contains(WS_EX_TOOLWINDOW) || ex.contains(WS_EX_TRANSPARENT) || ex.contains(WS_EX_NOACTIVATE)) {
            return TRUE;
        }
        let mut r = RECT::default();
        if DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut RECT as *mut _, size_of::<RECT>() as u32)
            .is_err()
            && GetWindowRect(hwnd, &mut r).is_err()
        {
            return TRUE;
        }
        let rect = IRect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top };
        if let Some(clip) = rect.intersect(&state.screen) {
            state.found.push(IRect { x: clip.x - state.screen.x, y: clip.y - state.screen.y, ..clip });
        }
        TRUE
    }
}

/// Box-filtered mosaic, equivalent to ImageMagick `-scale 5% -scale 2000%`.
pub fn pixelate(src: &[u8], w: i32, h: i32, block: i32) -> Vec<u8> {
    let mut out = vec![0u8; src.len()];
    let stride = (w * 4) as usize;
    for by in (0..h).step_by(block as usize) {
        for bx in (0..w).step_by(block as usize) {
            let (bw, bh) = (block.min(w - bx), block.min(h - by));
            let mut sum = [0u64; 4];
            for y in by..by + bh {
                let row = y as usize * stride;
                for x in bx..bx + bw {
                    let i = row + x as usize * 4;
                    for c in 0..4 {
                        sum[c] += src[i + c] as u64;
                    }
                }
            }
            let n = (bw * bh) as u64;
            let avg = [(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8, 255];
            for y in by..by + bh {
                let row = y as usize * stride;
                for x in bx..bx + bw {
                    let i = row + x as usize * 4;
                    out[i..i + 4].copy_from_slice(&avg);
                }
            }
        }
    }
    out
}
