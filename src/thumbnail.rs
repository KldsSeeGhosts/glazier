//! Nonactivating, layered export previews. Only scaled pixels and a path persist.

use crate::capture::IRect;
use crate::config::Config;
use crate::export::{self, Image};
use crate::gfx::{Gfx, icon, rect, wide};
use crate::{drag, host, theme};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

pub const CLASS: PCWSTR = w!("GlazierThumbnail");
const MAX: usize = 5;
const ANIMATION: f32 = 0.20;

thread_local! {
    static WINDOWS: RefCell<Vec<HWND>> = const { RefCell::new(Vec::new()) };
    static HIDDEN: Cell<bool> = const { Cell::new(false) };
}

struct Thumbnail {
    hwnd: HWND,
    gfx: Rc<Gfx>,
    palette: theme::Palette,
    themed: bool,
    preview: Image,
    path: PathBuf,
    saved: bool,
    video: bool,
    origin: IRect,
    work: IRect,
    k: f32,
    size: (i32, i32),
    position: (f32, f32),
    target: (i32, i32),
    born: Instant,
    closing: Option<Instant>,
    last_tick: Instant,
    elapsed: Duration,
    seconds: u64,
    hover: bool,
    button: Option<usize>,
    press: Option<(i32, i32, Option<usize>)>,
    dragging: bool,
    busy: bool,
    frame: Option<Image>,
    presented: Option<(i32, i32, u8)>,
}

#[derive(Clone, Copy)]
enum Action {
    Reopen,
    Copy,
    Save,
    Reveal,
    Close,
    Drag,
    Destroy,
}

fn cache_dir() -> PathBuf {
    std::env::temp_dir().join("Glazier-thumbnails")
}

fn temp_name(pid: u32, time: u128, id: u64) -> String {
    format!("glazier-{pid}-{time}-{id}.png")
}

fn owned_name(name: &str) -> bool {
    let Some(stem) = name.strip_prefix("glazier-").and_then(|s| s.strip_suffix(".png")) else {
        return false;
    };
    let parts: Vec<_> = stem.split('-').collect();
    parts.len() == 3 && parts.iter().all(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
}

fn cleanup_dir(dir: &Path) {
    if let Ok(files) = std::fs::read_dir(dir) {
        for file in files.flatten() {
            if owned_name(&file.file_name().to_string_lossy()) && file.file_type().is_ok_and(|t| t.is_file()) {
                let _ = std::fs::remove_file(file.path());
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}

/// Called after acquiring the single-instance mutex, and again on clean exit.
pub fn cleanup() {
    cleanup_dir(&cache_dir());
}

fn write_temp(png: &[u8]) -> Result<PathBuf> {
    use std::io::Write;
    static ID: AtomicU64 = AtomicU64::new(0);
    let dir = cache_dir();
    std::fs::create_dir_all(&dir).map_err(io_error)?;
    let name = temp_name(
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos(),
        ID.fetch_add(1, Ordering::Relaxed),
    );
    let path = dir.join(name);
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(io_error)?;
    if let Err(e) = file.write_all(png) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(io_error(e));
    }
    Ok(path)
}

fn io_error(e: std::io::Error) -> Error {
    Error::new(E_FAIL, e.to_string())
}

/// Newest first in this calculation; the newest preview sits at the bottom.
fn stack_layout(work: IRect, sizes: &[(i32, i32)], gap: i32) -> Vec<IRect> {
    let mut bottom = work.y + work.h - gap;
    sizes
        .iter()
        .map(|&(w, h)| {
            let r = IRect { x: (work.x + work.w - gap - w).max(work.x), y: bottom - h, w, h };
            bottom = r.y - gap;
            r
        })
        .collect()
}

fn preview_size(work: IRect, k: f32) -> (i32, i32) {
    let gap = (12.0 * k).round() as i32;
    let width = (260.0 * k).round().min((work.w - 2 * gap).max(1) as f32) as i32;
    let height = (174.0 * k).round().min(((work.h - gap * (MAX as i32 + 1)) / MAX as i32).max(1) as f32) as i32;
    (width, height)
}

fn expired(elapsed: Duration, delta: Duration, paused: bool, seconds: u64) -> (Duration, bool) {
    let elapsed = if paused { elapsed } else { elapsed.saturating_add(delta) };
    (elapsed, seconds != 0 && elapsed >= Duration::from_secs(seconds))
}

/// Original screen position when it fits, otherwise centered on that monitor.
/// Large images retain their original pixels, with offscreen edges clipped.
pub fn reopen_region(origin: IRect, work: IRect, width: u32, height: u32) -> IRect {
    let (w, h) = (width as i32, height as i32);
    let fits = w <= work.w && h <= work.h;
    let x = if fits { origin.x.clamp(work.x, work.x + work.w - w) } else { work.x + (work.w - w) / 2 };
    let y = if fits { origin.y.clamp(work.y, work.y + work.h - h) } else { work.y + (work.h - h) / 2 };
    IRect { x, y, w, h }
}

fn windows() -> Vec<HWND> {
    WINDOWS.with_borrow(|w| w.clone())
}

fn state(hwnd: HWND) -> Option<Rc<RefCell<Thumbnail>>> {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<Thumbnail>;
        if ptr.is_null() {
            return None;
        }
        Rc::increment_strong_count(ptr);
        Some(Rc::from_raw(ptr))
    }
}

pub fn is_dragging() -> bool {
    windows().into_iter().any(|h| state(h).is_some_and(|s| s.try_borrow().is_ok_and(|s| s.dragging)))
}

pub fn hide(hidden: bool) {
    HIDDEN.set(hidden);
    for hwnd in windows() {
        unsafe {
            let _ = ShowWindow(hwnd, if hidden { SW_HIDE } else { SW_SHOWNOACTIVATE });
        }
    }
}

pub fn reload(cfg: &Config) {
    for hwnd in windows() {
        if let Some(s) = state(hwnd) {
            let mut s = s.borrow_mut();
            s.seconds = cfg.thumbnail_seconds;
            s.elapsed = Duration::ZERO;
            if !cfg.show_thumbnail {
                s.closing = Some(Instant::now());
            }
        }
    }
}

pub fn reload_theme(gfx: &Gfx) {
    let tokens = gfx.theme();
    for hwnd in windows() {
        if let Some(state) = state(hwnd) {
            if let Ok(mut state) = state.try_borrow_mut() {
                state.palette = tokens.as_ref().map_or_else(theme::Palette::default, |t| t.palette());
                state.themed = tokens.is_some();
                state.frame = None;
                state.presented = None;
            }
        }
    }
}

pub fn shutdown() {
    for hwnd in windows() {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
    cleanup();
}

pub fn show(gfx: Rc<Gfx>, cfg: &Config, img: &Image, png: &[u8], saved: Option<PathBuf>, origin: IRect, monitor: IRect, k: f32) {
    if !cfg.show_thumbnail {
        return;
    }
    if let Err(e) = open(gfx, cfg, img, png, saved, origin, monitor, k, false) {
        host::notify("Could not show thumbnail", &e.message(), None, true);
    }
}

pub fn show_video(gfx: Rc<Gfx>, cfg: &Config, poster: &Image, path: PathBuf, origin: IRect, monitor: IRect, k: f32) {
    if cfg.show_thumbnail {
        if let Err(e) = open(gfx, cfg, poster, &[], Some(path), origin, monitor, k, true) {
            host::notify("Could not show recording preview", &e.message(), None, true);
        }
    }
}

fn open(
    gfx: Rc<Gfx>,
    cfg: &Config,
    img: &Image,
    png: &[u8],
    saved: Option<PathBuf>,
    origin: IRect,
    monitor: IRect,
    k: f32,
    video: bool,
) -> Result<()> {
    unsafe {
        let mon = MonitorFromRect(
            &RECT { left: monitor.x, top: monitor.y, right: monitor.x + monitor.w, bottom: monitor.y + monitor.h },
            MONITOR_DEFAULTTONEAREST,
        );
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        GetMonitorInfoW(mon, &mut info).ok()?;
        let r = info.rcWork;
        let work = IRect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top };
        // Five previews fit even on a short monitor. Dimensions are physical px.
        let (width, height) = preview_size(work, k);
        let factor = ((width as f32 - 20.0 * k).max(1.0) / img.width as f32)
            .min((height as f32 - 20.0 * k).max(1.0) / img.height as f32)
            .min(1.0);
        let preview = export::resize(
            &gfx,
            img,
            (img.width as f32 * factor).round().max(1.0) as u32,
            (img.height as f32 * factor).round().max(1.0) as u32,
        )?;
        let saved_file = saved.is_some();
        let path = match saved {
            Some(path) => path,
            None => write_temp(png)?,
        };
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        static REGISTER: std::sync::Once = std::sync::Once::new();
        REGISTER.call_once(|| {
            RegisterClassExW(&WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                hInstance: instance,
                lpfnWndProc: Some(wndproc),
                hCursor: LoadCursorW(None, IDC_HAND).unwrap_or_default(),
                lpszClassName: CLASS,
                ..Default::default()
            });
        });
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            CLASS,
            if video { w!("Glazier recording preview") } else { w!("Glazier thumbnail") },
            WS_POPUP,
            work.x + work.w,
            work.y + work.h - height,
            width,
            height,
            None,
            None,
            Some(instance),
            None,
        )?;
        let tokens = gfx.theme();
        let palette = tokens.as_ref().map_or_else(theme::Palette::default, |t| t.palette());
        let themed = tokens.is_some();
        let now = Instant::now();
        let s = Rc::new(RefCell::new(Thumbnail {
            hwnd,
            gfx,
            palette,
            themed,
            preview,
            path,
            saved: saved_file,
            video,
            origin,
            work,
            k,
            size: (width, height),
            position: ((work.x + work.w) as f32, (work.y + work.h - height) as f32),
            target: (work.x + work.w - width, work.y + work.h - height),
            born: now,
            closing: None,
            last_tick: now,
            elapsed: Duration::ZERO,
            seconds: cfg.thumbnail_seconds,
            hover: false,
            button: None,
            press: None,
            dragging: false,
            busy: false,
            frame: None,
            presented: None,
        }));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Rc::into_raw(s.clone()) as isize);
        // Display affinity is defense in depth; the host also hides every
        // preview and flushes DWM before freezing the desktop.
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        let old = WINDOWS.with_borrow_mut(|w| {
            w.push(hwnd);
            (w.len() > MAX).then(|| w.remove(0))
        });
        if let Some(old) = old {
            let _ = DestroyWindow(old);
        }
        reflow();
        let presented = s.borrow_mut().present();
        if let Err(e) = presented {
            let _ = DestroyWindow(hwnd);
            return Err(e);
        }
        if !HIDDEN.get() {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        SetTimer(Some(hwnd), 1, 16, None);
        Ok(())
    }
}

fn reflow() {
    let entries: Vec<_> = windows().into_iter().filter_map(state).collect();
    // Per-monitor stacks share physical work-area coordinates.
    for entry in &entries {
        let work = entry.borrow().work;
        let group: Vec<_> = entries.iter().rev().filter(|s| s.borrow().work == work).collect();
        let sizes: Vec<_> = group.iter().map(|s| s.borrow().size).collect();
        let gap = (12.0 * entry.borrow().k).round() as i32;
        for (s, r) in group.iter().zip(stack_layout(work, &sizes, gap)) {
            s.borrow_mut().target = (r.x, r.y);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCHITTEST {
            return LRESULT(HTCLIENT as isize);
        }
        if msg == WM_MOUSEACTIVATE {
            return LRESULT(MA_NOACTIVATE as isize);
        }
        if msg == WM_NCDESTROY {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<Thumbnail>;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if !ptr.is_null() {
                drop(Rc::from_raw(ptr));
            }
            WINDOWS.with_borrow_mut(|w| w.retain(|h| *h != hwnd));
            reflow();
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let Some(s) = state(hwnd) else {
            return DefWindowProcW(hwnd, msg, wp, lp);
        };
        let action = {
            let Ok(mut s) = s.try_borrow_mut() else {
                return DefWindowProcW(hwnd, msg, wp, lp);
            };
            let x = (lp.0 & 0xffff) as i16 as i32;
            let y = ((lp.0 >> 16) & 0xffff) as i16 as i32;
            match msg {
                WM_TIMER => s.tick(),
                WM_MOUSEMOVE => {
                    let was_hover = s.hover;
                    s.hover = true;
                    let button = s.hit(x, y);
                    if !was_hover || s.button != button {
                        s.frame = None;
                    }
                    s.button = button;
                    if let Some((px, py, None)) = s.press {
                        if (x - px).abs() >= GetSystemMetrics(SM_CXDRAG) || (y - py).abs() >= GetSystemMetrics(SM_CYDRAG) {
                            s.press = None;
                            s.dragging = true;
                            Some(Action::Drag)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                WM_LBUTTONDOWN if s.closing.is_none() && !s.busy => {
                    SetCapture(hwnd);
                    s.press = Some((x, y, s.hit(x, y)));
                    None
                }
                WM_LBUTTONUP => {
                    let press = s.press.take();
                    if GetCapture() == hwnd {
                        let _ = ReleaseCapture();
                    }
                    press.and_then(|(_, _, b)| {
                        if b != s.hit(x, y) || x < 0 || y < 0 || x >= s.size.0 || y >= s.size.1 {
                            return None;
                        }
                        Some(match b {
                            Some(0) if s.video => Action::Reveal,
                            Some(1) if s.video => Action::Copy,
                            Some(2) if s.video => Action::Close,
                            Some(0) => Action::Copy,
                            Some(1) => Action::Save,
                            Some(2) => Action::Reveal,
                            Some(_) => Action::Close,
                            None => Action::Reopen,
                        })
                    })
                }
                WM_CAPTURECHANGED | WM_CANCELMODE => {
                    s.press = None;
                    None
                }
                WM_CLOSE => Some(Action::Close),
                WM_DISPLAYCHANGE | WM_DPICHANGED => Some(Action::Close),
                WM_ERASEBKGND | WM_PAINT => return DefWindowProcW(hwnd, msg, wp, lp),
                _ => return DefWindowProcW(hwnd, msg, wp, lp),
            }
        };
        // OLE, WIC and the editor can pump nested messages. No RefCell borrow or
        // collection borrow survives an action that calls into those systems.
        if let Some(action) = action {
            perform(s, action);
        }
        LRESULT(0)
    }
}

fn perform(s: Rc<RefCell<Thumbnail>>, action: Action) {
    if matches!(action, Action::Close) {
        s.borrow_mut().closing.get_or_insert_with(Instant::now);
        return;
    }
    let (hwnd, gfx, path, origin, work, saved, video) = {
        let s = s.borrow();
        (s.hwnd, s.gfx.clone(), s.path.clone(), s.origin, s.work, s.saved, s.video)
    };
    if matches!(action, Action::Destroy) {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return;
    }
    {
        let mut s = s.borrow_mut();
        if s.busy {
            return;
        }
        s.busy = true;
    }
    let result: Result<()> = (|| {
        match action {
            Action::Drag => {
                unsafe {
                    if GetCapture() == hwnd {
                        let _ = ReleaseCapture();
                    }
                }
                let preview = {
                    let s = s.borrow();
                    Image { width: s.preview.width, height: s.preview.height, pixels: s.preview.pixels.clone() }
                };
                let result = drag::start(&gfx, &path, &preview);
                let mut s = s.borrow_mut();
                s.dragging = false;
                s.last_tick = Instant::now();
                result?;
            }
            Action::Reopen if video => host::open_video(&path)?,
            Action::Reopen => host::reopen(&path, origin, work)?,
            Action::Copy if video => drag::copy_file(&path)?,
            Action::Copy => {
                let png = std::fs::read(&path).map_err(io_error)?;
                let img = export::decode_png_bytes(&gfx, &png)?;
                export::copy_to_clipboard(host::hwnd(), &img, &png)?;
            }
            Action::Save | Action::Reveal => {
                let dest = if saved {
                    path
                } else {
                    let png = std::fs::read(path).map_err(io_error)?;
                    let dest = host::save_thumbnail(&png).map_err(|e| Error::new(E_FAIL, e))?;
                    let mut s = s.borrow_mut();
                    s.path = dest.clone();
                    s.saved = true;
                    dest
                };
                if matches!(action, Action::Reveal) {
                    host::reveal(&dest);
                }
            }
            _ => {}
        }
        Ok(())
    })();
    {
        let mut s = s.borrow_mut();
        s.busy = false;
        s.last_tick = Instant::now();
    }
    if let Err(e) = result {
        // Also reset after a failed lazy image load, before DoDragDrop started.
        s.borrow_mut().dragging = false;
        host::notify("Thumbnail action failed", &e.message(), None, true);
    }
}

impl Thumbnail {
    fn hit(&self, x: i32, y: i32) -> Option<usize> {
        let k = self.k;
        let width = if self.video { 96.0 } else { 128.0 };
        let left = self.size.0 as f32 / 2.0 - width / 2.0 * k;
        let top = self.size.1 as f32 - 42.0 * k;
        (self.hover && y as f32 >= top && (y as f32) < top + 30.0 * k && x as f32 >= left && (x as f32) < left + width * k)
            .then(|| ((x as f32 - left) / (32.0 * k)) as usize)
    }

    fn tick(&mut self) -> Option<Action> {
        let now = Instant::now();
        let delta = now.duration_since(self.last_tick);
        self.last_tick = now;
        let mut pt = POINT::default();
        let mut r = RECT::default();
        unsafe {
            let _ = GetCursorPos(&mut pt);
            let _ = GetWindowRect(self.hwnd, &mut r);
        }
        let hover = !HIDDEN.get()
            && pt.x >= r.left
            && pt.x < r.right
            && pt.y >= r.top
            && pt.y < r.bottom
            && unsafe { WindowFromPoint(pt) == self.hwnd };
        if hover != self.hover {
            self.hover = hover;
            self.frame = None;
        }
        if !hover {
            self.button = None;
        }
        let (elapsed, dismiss) = expired(
            self.elapsed,
            delta,
            hover || self.dragging || self.busy || self.press.is_some() || HIDDEN.get(),
            self.seconds,
        );
        self.elapsed = elapsed;
        if dismiss && !self.busy {
            self.closing.get_or_insert(now);
        }
        if self.closing.is_some_and(|t| now.duration_since(t).as_secs_f32() >= ANIMATION) && !self.busy {
            return Some(Action::Destroy);
        }
        let _ = self.present();
        None
    }

    fn present(&mut self) -> Result<()> {
        let entering = (self.born.elapsed().as_secs_f32() / ANIMATION).min(1.0);
        let leaving = self.closing.map_or(0.0, |t| (t.elapsed().as_secs_f32() / ANIMATION).min(1.0));
        let alpha = (entering * (1.0 - leaving) * 255.0).round() as u8;
        let dx = self.target.0 as f32 - self.position.0;
        let dy = self.target.1 as f32 - self.position.1;
        self.position.0 += dx * 0.24;
        self.position.1 += dy * 0.24;
        let pos = POINT { x: (self.position.0 + leaving * 60.0 * self.k).round() as i32, y: self.position.1.round() as i32 };
        if self.frame.is_some() && self.presented == Some((pos.x, pos.y, alpha)) {
            return Ok(());
        }
        if self.frame.is_none() {
            self.frame = Some(self.render()?);
        }
        let bitmap = drag::bitmap(self.frame.as_ref().unwrap())?;
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                let _ = DeleteObject(bitmap.into());
                return Err(Error::from_thread());
            }
            let old = SelectObject(dc, bitmap.into());
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                SourceConstantAlpha: alpha,
                AlphaFormat: AC_SRC_ALPHA as u8,
                BlendFlags: 0,
            };
            let result = UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&pos),
                Some(&SIZE { cx: self.size.0, cy: self.size.1 }),
                Some(dc),
                Some(&POINT::default()),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            SelectObject(dc, old);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(dc);
            if result.is_ok() {
                self.presented = Some((pos.x, pos.y, alpha));
            }
            result
        }
    }

    fn render(&self) -> Result<Image> {
        let (w, h) = (self.size.0 as u32, self.size.1 as u32);
        let k = self.k;
        unsafe {
            let target = self.gfx.wic.CreateBitmap(w, h, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad)?;
            let rt = self.gfx.d2d.CreateWicBitmapRenderTarget(&target, &export::target_props())?;
            let brush = rt.CreateSolidColorBrush(&theme::BLACK, None)?;
            let surface = rect(6.0 * k, 5.0 * k, w as f32 - 12.0 * k, h as f32 - 14.0 * k);
            rt.BeginDraw();
            rt.Clear(Some(&D2D1_COLOR_F::default()));
            for n in (1..=5).rev() {
                let d = n as f32 * k;
                brush.SetColor(&theme::with_alpha(theme::BLACK, 0.035));
                rt.FillRoundedRectangle(
                    &D2D1_ROUNDED_RECT {
                        rect: rect(
                            surface.left - d,
                            surface.top - d + 3.0 * k,
                            surface.right - surface.left + d * 2.0,
                            surface.bottom - surface.top + d * 2.0,
                        ),
                        radiusX: 12.0 * k,
                        radiusY: 12.0 * k,
                    },
                    &brush,
                );
            }
            let radius = self.palette.radius_normal * k;
            let rounded = D2D1_ROUNDED_RECT { rect: surface, radiusX: radius, radiusY: radius };
            brush.SetColor(&self.palette.surface);
            rt.FillRoundedRectangle(&rounded, &brush);
            brush.SetColor(&self.palette.primary_container);
            rt.DrawRoundedRectangle(&rounded, &brush, k, None::<&ID2D1StrokeStyle>);
            let bitmap = rt.CreateBitmap(
                D2D_SIZE_U { width: self.preview.width, height: self.preview.height },
                Some(self.preview.pixels.as_ptr().cast()),
                self.preview.width * 4,
                &export::bitmap_props(),
            )?;
            let (iw, ih) = (self.preview.width as f32, self.preview.height as f32);
            let dest = rect((w as f32 - iw) / 2.0, (h as f32 - ih) / 2.0 - 2.0 * k, iw, ih);
            rt.DrawBitmap(&bitmap, Some(&dest), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None);
            if self.hover {
                let font = self.gfx.icon_format(17.0 * k)?;
                let width = if self.video { 96.0 } else { 128.0 };
                let left = w as f32 / 2.0 - width / 2.0 * k;
                let top = h as f32 - 42.0 * k;
                brush.SetColor(&self.palette.surface_variant);
                rt.FillRoundedRectangle(
                    &D2D1_ROUNDED_RECT {
                        rect: rect(left - 3.0 * k, top - 3.0 * k, (width + 6.0) * k, 36.0 * k),
                        radiusX: radius,
                        radiusY: radius,
                    },
                    &brush,
                );
                let glyphs: &[&str] =
                    if self.video { &["folder", "copy", "x"] } else { &["copy", "device-floppy", "folder", "x"] };
                for (i, glyph) in glyphs.iter().enumerate() {
                    let r = rect(left + i as f32 * 32.0 * k, top, 30.0 * k, 30.0 * k);
                    if self.button == Some(i) {
                        brush.SetColor(&self.palette.hover);
                        rt.FillRoundedRectangle(
                            &D2D1_ROUNDED_RECT {
                                rect: r,
                                radiusX: if self.themed { self.palette.radius_small } else { 6.0 } * k,
                                radiusY: if self.themed { self.palette.radius_small } else { 6.0 } * k,
                            },
                            &brush,
                        );
                    }
                    brush.SetColor(&self.palette.primary);
                    rt.DrawText(
                        &wide(icon(glyph)),
                        &font,
                        &r,
                        &brush,
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                        DWRITE_MEASURING_MODE_NATURAL,
                    );
                }
                if let Some(i) = self.button {
                    let font = if self.themed {
                        self.gfx.hud_format(11.0 * k, DWRITE_FONT_WEIGHT_NORMAL, true)?
                    } else {
                        self.gfx.text_format(11.0 * k, DWRITE_FONT_WEIGHT_NORMAL, true)?
                    };
                    let r = rect(left - 2.0 * k, top - 25.0 * k, 132.0 * k, 21.0 * k);
                    brush.SetColor(&self.palette.surface_variant);
                    let radius = if self.themed { self.palette.radius_small } else { 5.0 } * k;
                    rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius }, &brush);
                    brush.SetColor(&self.palette.on_surface);
                    rt.DrawText(
                        &wide(if self.video {
                            ["Show in folder", "Copy", "Close"][i]
                        } else {
                            ["Copy", "Save", "Show in folder", "Close"][i]
                        }),
                        &font,
                        &r,
                        &brush,
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                        DWRITE_MEASURING_MODE_NATURAL,
                    );
                }
            }
            rt.EndDraw(None, None)?;
            let mut pixels = vec![0; w as usize * h as usize * 4];
            target.CopyPixels(std::ptr::null(), w * 4, &mut pixels)?;
            Ok(Image { width: w, height: h, pixels })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stacks_are_inside_negative_origin_work_area() {
        let work = IRect { x: -1920, y: -200, w: 1920, h: 1040 };
        let sizes = vec![(325, 190); MAX];
        let layout = stack_layout(work, &sizes, 15);
        for r in &layout {
            assert_eq!(r.intersect(&work), Some(*r));
        }
        assert_eq!(layout[0].y + layout[0].h, work.y + work.h - 15);
        for pair in layout.windows(2) {
            assert!(pair[1].y + pair[1].h < pair[0].y);
        }
    }

    #[test]
    fn five_previews_fit_short_work_areas_at_each_dpi() {
        for k in [1.0, 1.25, 1.5, 1.75, 2.0, 3.0] {
            let work = IRect { x: -1920, y: 40, w: 1920, h: 720 };
            let size = preview_size(work, k);
            let gap = (12.0 * k).round() as i32;
            for r in stack_layout(work, &[size; MAX], gap) {
                assert_eq!(r.intersect(&work), Some(r));
            }
        }
    }

    #[test]
    fn reopen_clamps_and_centers_without_resampling() {
        let work = IRect { x: 100, y: 50, w: 800, h: 600 };
        let origin = IRect { x: 850, y: 600, w: 200, h: 100 };
        assert_eq!(reopen_region(origin, work, 200, 100), IRect { x: 700, y: 550, w: 200, h: 100 });
        assert_eq!(reopen_region(origin, work, 1000, 800), IRect { x: 0, y: -50, w: 1000, h: 800 });
    }

    #[test]
    fn dismissal_counts_only_unpaused_time_and_zero_never_expires() {
        let five = Duration::from_secs(5);
        assert_eq!(expired(five, Duration::from_secs(20), true, 6), (five, false));
        assert_eq!(expired(five, Duration::from_secs(1), false, 6), (Duration::from_secs(6), true));
        assert!(!expired(five, Duration::from_secs(999), false, 0).1);
    }

    #[test]
    fn temp_names_are_unique_and_cleanup_preserves_foreign_files() {
        let a = temp_name(1, 42, 0);
        let b = temp_name(1, 42, 1);
        assert_ne!(a, b);
        assert!(owned_name(&a));
        assert!(!owned_name("glazier-../../photo.png"));
        assert!(!owned_name("photo.png"));
        let dir = std::env::temp_dir().join(format!("glazier-thumbnail-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(&a), b"stale").unwrap();
        std::fs::write(dir.join("keep.txt"), b"foreign").unwrap();
        cleanup_dir(&dir);
        assert!(!dir.join(a).exists());
        assert!(dir.join("keep.txt").exists());
        std::fs::remove_file(dir.join("keep.txt")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
