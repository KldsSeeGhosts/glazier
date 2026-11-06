//! Physical-pixel virtual-desktop window: region picker, then the editor.

use crate::capture::{Capture, IRect, pixelate};
use crate::config::Config;
use crate::export::{self, Image, bitmap_props, target_props};
use crate::gfx::{Gfx, rect};
use crate::host;
use crate::hotkey;
use crate::strokes::{self, Painter, Pt, Stroke, Tool, reindex_steps};
use crate::theme::{self, *};
use crate::toolbar::{self, Backdrop, Fonts, Hit, Inputs, Item, Layout, MenuItem, Pop, PopItem, Ui, View};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;
use windows_numerics::{Matrix3x2, Vector2};

const WM_APP_FINISH: u32 = WM_APP + 10;
const TIMER_ANIM: usize = 1;
const TIMER_TIP: usize = 2;
const TIMER_GLIDE: usize = 3;
const TIMER_CLEAR: usize = 4;
const CLASS: PCWSTR = w!("GlazierOverlay");
const PIXEL_BLOCK: i32 = 20;
const ANIM_MS: f32 = 100.0;
const GLIDE_MS: f32 = 300.0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    Cancel = 0,
    Done = 1,
    Recapture = 2,
}

struct Res {
    rt: ID2D1HwndRenderTarget,
    brush: ID2D1SolidColorBrush,
    screen: Vec<(D2D_RECT_F, ID2D1Bitmap)>,
    cache: Option<ID2D1BitmapRenderTarget>,
    pixel: Option<ID2D1Bitmap>,
    /// Blurred backdrops for toolbar surfaces, keyed by their pixel bounds.
    glass: RefCell<Vec<(IRect, ID2D1BitmapBrush)>>,
}

type Rf = (f32, f32, f32, f32);

struct Picker {
    shown: Rf,
    target: IRect,
    from: Rf,
    anim: Option<Instant>,
    opened: Instant,
    press: Option<(f32, f32)>,
    dragging: bool,
}

struct TextEdit {
    x: f32,
    y: f32,
    text: String,
    high: Option<u16>,
}

struct Editor {
    region: IRect,
    monitor: IRect,
    work: IRect,
    base: Image,
    pixel: Option<Vec<u8>>,
    tool: Tool,
    color: u32,
    size: f32,
    strokes: Vec<Stroke>,
    redo: Vec<Stroke>,
    saved: Vec<Stroke>,
    current: Option<Stroke>,
    text: Option<TextEdit>,
    step_counter: u32,
    zoom: f32,
    pan: (f32, f32),
    pan_drag: Option<((f32, f32), (f32, f32))>,
    popover: Option<Pop>,
    /// Tool each compact group button shows while another group is active.
    group_last: [Tool; 3],
    clear_armed: bool,
    /// Selection highlight start, relative to the toolbar, while it glides.
    glide: Option<(D2D_RECT_F, Instant)>,
    tb_user: Option<(f32, f32)>,
    tb_drag: Option<((f32, f32), (f32, f32))>,
    last_mouse: (f32, f32),
    speed: f32,
    cache_valid: bool,
    pressed: Option<Hit>,
    hover: Option<Hit>,
    erasing: bool,
    busy: bool,
}

pub struct Overlay {
    hwnd: HWND,
    gfx: Rc<Gfx>,
    cfg: Rc<Config>,
    cap: Capture,
    k: f32,
    res: Option<Res>,
    fonts: Fonts,
    hud: Hud,
    picker_tint: D2D1_COLOR_F,
    picker_primary: D2D1_COLOR_F,
    picker: Picker,
    ed: Option<Editor>,
    mouse: (f32, f32),
    tip: Option<(Hit, bool)>,
    finished: bool,
    finish_how: Finish,
    recording: bool,
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.gfx.clear_text_cache();
    }
}

fn rf(r: IRect) -> Rf {
    (r.x as f32, r.y as f32, r.w as f32, r.h as f32)
}

fn lparam_xy(lp: LPARAM) -> (f32, f32) {
    ((lp.0 & 0xffff) as i16 as f32, ((lp.0 >> 16) & 0xffff) as i16 as f32)
}

fn key_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetKeyState(vk.0 as i32) < 0 }
}

/// False when Windows "Animation effects" is turned off.
fn animations_enabled() -> bool {
    let mut on = TRUE;
    unsafe {
        let ok = SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&mut on as *mut BOOL).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        ok.is_err() || on.as_bool()
    }
}

pub fn open(gfx: Rc<Gfx>, cfg: Rc<Config>, cap: Capture) -> Result<HWND> {
    open_inner(gfx, cfg, cap, None, false)
}

pub fn open_recording(gfx: Rc<Gfx>, cfg: Rc<Config>, cap: Capture) -> Result<HWND> {
    open_inner(gfx, cfg, cap, None, true)
}

pub fn open_image(gfx: Rc<Gfx>, cfg: Rc<Config>, mut cap: Capture, image: Image, mut region: IRect) -> Result<HWND> {
    region.x -= cap.origin.0;
    region.y -= cap.origin.1;
    // Paint only the visible intersection into the frozen background. The editor
    // retains all exported pixels, including any edges outside the desktop.
    if let Some(visible) = region.intersect(&cap.bounds()) {
        for y in visible.y..visible.y + visible.h {
            let dst = (y as usize * cap.width as usize + visible.x as usize) * 4;
            let src = ((y - region.y) as usize * image.width as usize + (visible.x - region.x) as usize) * 4;
            let count = visible.w as usize * 4;
            cap.pixels[dst..dst + count].copy_from_slice(&image.pixels[src..src + count]);
        }
    }
    open_inner(gfx, cfg, cap, Some((region, image)), false)
}

fn open_inner(gfx: Rc<Gfx>, cfg: Rc<Config>, cap: Capture, image: Option<(IRect, Image)>, recording: bool) -> Result<HWND> {
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        static REGISTER: std::sync::Once = std::sync::Once::new();
        REGISTER.call_once(|| {
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wndproc),
                hInstance: instance,
                hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
                lpszClassName: CLASS,
                ..Default::default()
            };
            RegisterClassExW(&wc);
        });
        let (x, y, w, h) = (cap.origin.0, cap.origin.1, cap.width, cap.height);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            CLASS,
            w!("Glazier"),
            WS_POPUP,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(instance),
            None,
        )?;
        let mut overlay = match Overlay::new(hwnd, gfx, cfg, cap) {
            Ok(o) => o,
            Err(e) => {
                let _ = DestroyWindow(hwnd);
                return Err(e);
            }
        };
        overlay.recording = recording;
        if let Some((region, image)) = image {
            let monitor =
                overlay.cap.monitor_at(region.x as f32 + region.w as f32 / 2.0, region.y as f32 + region.h as f32 / 2.0);
            overlay.start_editor_image(region, image, monitor);
        }
        // Keep a local owner too: presenting the initial frame can pump a
        // nested destruction before open has returned to the host.
        let state = Rc::new(RefCell::new(overlay));
        let cell = Rc::into_raw(state.clone());
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, cell as isize);
        hotkey::set_overlay(Some(hwnd));

        // Present the first frame while cloaked so the freeze is seamless.
        let cloak = TRUE;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &cloak as *const _ as *const _, 4);
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_SHOWWINDOW | SWP_NOACTIVATE);
        state.borrow_mut().render(None);
        let _ = DwmFlush();
        let uncloak = FALSE;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &uncloak as *const _ as *const _, 4);
        if IsWindow(Some(hwnd)).as_bool() {
            host::force_foreground(hwnd);
            if state.borrow().ed.is_none() {
                SetTimer(Some(hwnd), TIMER_ANIM, 10, None);
            }
        }
        Ok(hwnd)
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let cell = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<Overlay>;
        if cell.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        match msg {
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                hotkey::set_overlay(None);
                let state = Rc::from_raw(cell);
                let how = state.try_borrow().map_or(Finish::Cancel, |o| o.finish_how);
                host::overlay_finished(how as usize);
                return DefWindowProcW(hwnd, msg, wp, lp);
            }
            WM_APP_FINISH => {
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            _ => {}
        }
        // A nested Win32/COM dispatch may destroy the HWND. Keep its Rust state
        // alive until this dispatch (and its RefCell borrow) has returned.
        Rc::increment_strong_count(cell);
        let state = Rc::from_raw(cell);
        let Ok(mut overlay) = state.try_borrow_mut() else {
            if msg == hotkey::WM_APP_ESCAPE || msg == WM_CLOSE {
                let _ = PostMessageW(Some(hwnd), msg, wp, lp);
                return LRESULT(0);
            }
            return DefWindowProcW(hwnd, msg, wp, lp);
        };
        match overlay.handle(msg, wp, lp) {
            Some(r) => r,
            None => {
                drop(overlay);
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
    }
}

impl Overlay {
    fn new(hwnd: HWND, gfx: Rc<Gfx>, cfg: Rc<Config>, cap: Capture) -> Result<Self> {
        let k = cap.monitor_at(cap.cursor.0 as f32, cap.cursor.1 as f32).scale;
        let fonts = Fonts::new(&gfx, k)?;
        let tokens = gfx.theme();
        let hud = tokens.as_ref().map_or(*theme::hud(&cfg.toolbar_theme), |tokens| tokens.hud());
        let picker_tint = tokens
            .as_ref()
            .map_or(HOVER, |t| theme::rgb(crate::theme_tokens::parse_colour(&t.colours.surface_container_highest).unwrap()));
        let picker_primary =
            tokens.as_ref().map_or(PRIMARY, |t| theme::rgb(crate::theme_tokens::parse_colour(&t.colours.primary).unwrap()));
        let (cx, cy) = (cap.cursor.0 as f32, cap.cursor.1 as f32);
        let target = cap.hover_target(cx, cy);
        let now = Instant::now();
        let picker =
            Picker { shown: rf(target), target, from: rf(target), anim: None, opened: now, press: None, dragging: false };
        Ok(Self {
            hwnd,
            gfx,
            cfg,
            cap,
            k,
            res: None,
            fonts,
            hud,
            picker_tint,
            picker_primary,
            picker,
            ed: None,
            mouse: (cx, cy),
            tip: None,
            finished: false,
            finish_how: Finish::Cancel,
            recording: false,
        })
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn finish(&mut self, how: Finish) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.finish_how = how;
        unsafe {
            if GetCapture() == self.hwnd {
                let _ = ReleaseCapture();
            }
            let _ = PostMessageW(Some(self.hwnd), WM_APP_FINISH, WPARAM(how as usize), LPARAM(0));
        }
    }

    fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        if self.finished && msg != WM_PAINT {
            return None;
        }
        match msg {
            hotkey::WM_APP_ESCAPE | WM_CLOSE => self.key(VK_ESCAPE),
            // The canvas stays at the captured desktop's physical bounds.
            // UI scale is chosen explicitly from the active monitor.
            WM_DPICHANGED => {}
            WM_DISPLAYCHANGE => self.finish(Finish::Cancel),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                unsafe {
                    BeginPaint(self.hwnd, &mut ps);
                    self.render(Some(ps.rcPaint));
                    let _ = EndPaint(self.hwnd, &ps);
                }
            }
            WM_ERASEBKGND => return Some(LRESULT(1)),
            WM_MOUSEMOVE => {
                let (x, y) = lparam_xy(lp);
                self.mouse = (x, y);
                if self.ed.is_some() {
                    self.editor_move(x, y, wp);
                } else {
                    self.picker_move(x, y);
                }
            }
            WM_LBUTTONDOWN => {
                let (x, y) = lparam_xy(lp);
                unsafe { SetCapture(self.hwnd) };
                if self.ed.is_some() {
                    self.editor_down(x, y, false);
                } else {
                    self.picker.press = Some((x, y));
                    self.picker.dragging = false;
                }
            }
            WM_LBUTTONUP => {
                let (x, y) = lparam_xy(lp);
                if self.ed.is_some() {
                    self.editor_up(x, y);
                } else {
                    self.picker_move(x, y);
                    self.picker_up();
                }
                unsafe {
                    if GetCapture() == self.hwnd {
                        let _ = ReleaseCapture();
                    }
                }
            }
            WM_RBUTTONDOWN => {
                let (x, y) = lparam_xy(lp);
                if self.ed.is_some() {
                    unsafe { SetCapture(self.hwnd) };
                    self.editor_down(x, y, true);
                } else {
                    self.finish(Finish::Cancel);
                }
            }
            WM_RBUTTONUP => {
                if let Some(ed) = &mut self.ed {
                    ed.erasing = false;
                }
                unsafe {
                    if GetCapture() == self.hwnd {
                        let _ = ReleaseCapture();
                    }
                }
            }
            WM_CAPTURECHANGED | WM_CANCELMODE | WM_KILLFOCUS => {
                self.cancel_drag();
                if msg != WM_CAPTURECHANGED {
                    unsafe {
                        if GetCapture() == self.hwnd {
                            let _ = ReleaseCapture();
                        }
                    }
                }
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                self.key(VIRTUAL_KEY(wp.0 as u16));
                if msg == WM_SYSKEYDOWN {
                    return None;
                }
            }
            WM_CHAR => self.char(wp.0 as u16),
            WM_TIMER => self.timer(wp.0),
            WM_SETCURSOR if (lp.0 & 0xffff) as u32 == HTCLIENT => {
                let id = self.cursor();
                unsafe {
                    if let Ok(c) = LoadCursorW(None, id) {
                        SetCursor(Some(c));
                    }
                }
                return Some(LRESULT(1));
            }
            WM_MOUSEACTIVATE => return Some(LRESULT(MA_ACTIVATE as isize)),
            _ => return None,
        }
        Some(LRESULT(0))
    }

    // ---- picker -------------------------------------------------------

    fn cancel_drag(&mut self) {
        self.picker.press = None;
        self.picker.dragging = false;
        self.picker.anim = None;
        self.picker.shown = rf(self.picker.target);
        if let Some(ed) = &mut self.ed {
            ed.current = None;
            ed.tb_drag = None;
            ed.pan_drag = None;
            ed.pressed = None;
            ed.erasing = false;
        }
        self.invalidate();
    }

    fn picker_move(&mut self, x: f32, y: f32) {
        let (w, h) = (self.cap.width as f32, self.cap.height as f32);
        let p = &mut self.picker;
        if let Some((px, py)) = p.press {
            if !p.dragging && ((x - px).abs() > 3.0 * self.k || (y - py).abs() > 3.0 * self.k) {
                p.dragging = true;
            }
            if p.dragging {
                let (cx, cy) = (x.clamp(0.0, w), y.clamp(0.0, h));
                p.anim = None;
                p.shown = (px.min(cx), py.min(cy), (cx - px).abs(), (cy - py).abs());
                self.invalidate();
            }
            return;
        }
        self.k = self.cap.monitor_at(x, y).scale;
        let target = self.cap.hover_target(x, y);
        if target != p.target {
            p.target = target;
            p.from = p.shown;
            p.anim = Some(Instant::now());
            self.invalidate();
        }
    }

    fn picker_up(&mut self) {
        let p = &mut self.picker;
        if p.press.take().is_none() {
            return;
        }
        let region = if p.dragging {
            p.dragging = false;
            let (x, y, w, h) = p.shown;
            IRect { x: x.ceil() as i32, y: y.ceil() as i32, w: w.floor() as i32, h: h.floor() as i32 }
        } else {
            p.target
        };
        match region.intersect(&self.cap.bounds()) {
            Some(r) => self.start_editor(r),
            None => {
                self.picker.shown = rf(self.picker.target);
                self.invalidate();
            }
        }
    }

    fn start_editor(&mut self, region: IRect) {
        if self.recording {
            let monitor = self.cap.monitor_for(region);
            let absolute = |r: IRect| IRect { x: r.x + self.cap.origin.0, y: r.y + self.cap.origin.1, ..r };
            if let Some(region) = crate::recording::clamp_region(absolute(region), absolute(monitor.bounds)) {
                host::recording_selected(region, absolute(monitor.bounds), absolute(monitor.work), monitor.scale);
                self.finish(Finish::Done);
            }
            return;
        }
        let base = Image { width: region.w as u32, height: region.h as u32, pixels: self.cap.crop(region) };
        self.start_editor_image(region, base, self.cap.monitor_for(region));
    }

    fn start_editor_image(&mut self, region: IRect, base: Image, monitor: crate::capture::Monitor) {
        self.k = monitor.scale;
        if let Ok(fonts) = Fonts::new(&self.gfx, self.k) {
            self.fonts = fonts;
        }
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_ANIM);
        }
        let cfg = &self.cfg;
        let tool = Tool::parse(&cfg.default_tool).unwrap_or(Tool::Pencil);
        let mut group_last = [Tool::Pencil, Tool::Arrow, Tool::Text];
        group_last[tool.group()] = tool;
        self.ed = Some(Editor {
            region,
            monitor: monitor.bounds,
            work: monitor.work,
            base,
            pixel: None,
            tool,
            color: theme::parse_hex(&cfg.default_color).unwrap_or(0xFF4444),
            size: if cfg.default_size.is_finite() { cfg.default_size.max(1.0) } else { 3.0 },
            strokes: Vec::new(),
            redo: Vec::new(),
            saved: Vec::new(),
            current: None,
            text: None,
            step_counter: 1,
            zoom: 1.0,
            pan: (0.0, 0.0),
            pan_drag: None,
            popover: None,
            group_last,
            clear_armed: false,
            glide: None,
            tb_user: None,
            tb_drag: None,
            last_mouse: (0.0, 0.0),
            speed: 0.0,
            cache_valid: false,
            pressed: None,
            hover: None,
            erasing: false,
            busy: false,
        });
        if let Some(res) = &mut self.res {
            res.cache = None;
            res.pixel = None;
            res.glass.borrow_mut().clear();
        }
        if tool == Tool::Blur {
            self.ensure_pixel();
        }
        self.invalidate();
    }

    // ---- editor input -------------------------------------------------

    fn layout(&self) -> Option<Layout> {
        let ed = self.ed.as_ref()?;
        let inputs = Inputs {
            region: ed.region,
            work: ed.work,
            user_pos: ed.tb_user,
            show_magnifier: self.cfg.show_magnifier,
            show_tool_options: self.cfg.show_tool_options,
            popover: ed.popover,
            clear_armed: ed.clear_armed,
            zoomed: ed.zoom > 1.0,
        };
        Some(toolbar::layout(&inputs, &self.fonts, self.k))
    }

    fn editor_down(&mut self, x: f32, y: f32, right: bool) {
        let Some(layout) = self.layout() else { return };
        let ed = self.ed.as_mut().unwrap();
        if ed.busy {
            return;
        }
        if !right {
            if let Some(hit) = layout.hit(x, y) {
                ed.pressed = Some(hit);
                self.tip = None;
                self.invalidate();
                return;
            }
            // Empty space on the bar moves it.
            if layout.on_bar(x, y) {
                ed.tb_drag = Some(((x, y), (layout.rect.left, layout.rect.top)));
                ed.popover = None;
                self.tip = None;
                self.invalidate();
                return;
            }
            if layout.covers(x, y) {
                return;
            }
            // The first click elsewhere only dismisses an open popover.
            if ed.popover.take().is_some() {
                self.invalidate();
                return;
            }
        }
        let region = ed.region;
        if !region.contains(x, y) {
            if !right {
                self.finish(Finish::Cancel);
            }
            return;
        }
        if ed.zoom > 1.0 {
            if !right {
                ed.pan_drag = Some(((x, y), ed.pan));
            }
            return;
        }
        let (lx, ly) = (x - region.x as f32, y - region.y as f32);
        if right {
            ed.erasing = true;
            ed.current = None;
            self.erase_at(lx, ly);
            return;
        }
        self.canvas_press(lx, ly);
    }

    fn canvas_press(&mut self, lx: f32, ly: f32) {
        let ed = self.ed.as_mut().unwrap();
        ed.popover = None;
        let tool = ed.tool;
        match tool {
            Tool::Step => {
                let mut s = Stroke::new(Tool::Step, ed.color, ed.size, lx, ly);
                s.step = ed.step_counter;
                ed.step_counter += 1;
                self.commit(s);
            }
            Tool::Text => {
                self.commit_text();
                let ed = self.ed.as_mut().unwrap();
                ed.text = Some(TextEdit { x: lx, y: ly, text: String::new(), high: None });
            }
            _ => {
                if tool == Tool::Blur {
                    self.ensure_pixel();
                }
                let ed = self.ed.as_mut().unwrap();
                let mut s = Stroke::new(tool, ed.color, ed.size, lx, ly);
                if tool.freehand() {
                    ed.last_mouse = (lx, ly);
                    ed.speed = ed.size;
                    s.points.push(Pt { x: lx, y: ly, w: ed.size });
                }
                ed.current = Some(s);
            }
        }
        self.invalidate();
    }

    fn editor_move(&mut self, x: f32, y: f32, wp: WPARAM) {
        let k = self.k;
        let ed = self.ed.as_mut().unwrap();
        if let Some((start, origin)) = ed.tb_drag {
            ed.tb_user = Some((origin.0 + x - start.0, origin.1 + y - start.1));
            self.invalidate();
            return;
        }
        if let Some((start, origin)) = ed.pan_drag {
            ed.pan = (origin.0 + x - start.0, origin.1 + y - start.1);
            self.invalidate();
            return;
        }
        let (lx, ly) = (x - ed.region.x as f32, y - ed.region.y as f32);
        if ed.erasing && (wp.0 & 0x0002) != 0 {
            self.erase_at(lx, ly);
            return;
        }
        if let Some(cur) = &mut ed.current {
            let mut dirty = None;
            if cur.tool.freehand() {
                if (lx - ed.last_mouse.0).hypot(ly - ed.last_mouse.1) < 0.5 {
                    return;
                }
                let pad = cur.size * k * if cur.tool == Tool::Highlighter { 3.0 } else { 1.0 } + 3.0;
                let (mut left, mut top, mut right, mut bottom) = (lx, ly, lx, ly);
                for p in cur.points.iter().rev().take(3) {
                    left = left.min(p.x);
                    top = top.min(p.y);
                    right = right.max(p.x);
                    bottom = bottom.max(p.y);
                }
                dirty = Some(RECT {
                    left: ed.region.x + (left - pad).floor() as i32,
                    top: ed.region.y + (top - pad).floor() as i32,
                    right: ed.region.x + (right + pad).ceil() as i32,
                    bottom: ed.region.y + (bottom + pad).ceil() as i32,
                });
                let mut w = cur.size;
                if cur.tool == Tool::Pencil {
                    let (dx, dy) = ((lx - ed.last_mouse.0) / k, (ly - ed.last_mouse.1) / k);
                    ed.speed = ed.speed * 0.7 + (dx * dx + dy * dy).sqrt() * 0.3;
                    let (min_w, max_w) = ((cur.size * 0.35).max(1.0), cur.size * 1.6);
                    let t = ed.speed.min(20.0) / 20.0;
                    w = max_w - t * (max_w - min_w);
                }
                ed.last_mouse = (lx, ly);
                cur.points.push(Pt { x: lx, y: ly, w });
                if cur.points.len() > 2000 {
                    let last = *cur.points.last().unwrap();
                    let mut next = Stroke::new(cur.tool, cur.color, cur.size, lx, ly);
                    next.points.push(last);
                    let done = std::mem::replace(cur, next);
                    self.commit(done);
                }
            } else {
                cur.x2 = lx;
                cur.y2 = ly;
            }
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd), dirty.as_ref().map(|r| r as *const RECT), false);
            }
            return;
        }
        let hover = self.layout().and_then(|l| l.hit(x, y));
        let ed = self.ed.as_mut().unwrap();
        if hover != ed.hover {
            ed.hover = hover;
            if ed.clear_armed && !hover.is_some_and(Hit::is_clear) {
                ed.clear_armed = false;
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), TIMER_CLEAR);
                }
            }
            self.tip = hover.map(|h| (h, false));
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER_TIP);
                if hover.is_some() {
                    SetTimer(Some(self.hwnd), TIMER_TIP, 450, None);
                }
            }
            self.invalidate();
        }
    }

    fn editor_up(&mut self, x: f32, y: f32) {
        let layout = self.layout();
        let ed = self.ed.as_mut().unwrap();
        let toolbar_drag = ed.tb_drag.take().is_some();
        let pan_drag = ed.pan_drag.take().is_some();
        if toolbar_drag || pan_drag {
            self.invalidate();
            return;
        }
        if let Some(pressed) = ed.pressed.take() {
            if layout.and_then(|l| l.hit(x, y)) == Some(pressed) {
                self.activate(pressed);
            }
            self.invalidate();
            return;
        }
        if let Some(mut s) = ed.current.take() {
            if s.tool == Tool::Blur {
                let (bx, by, bw, bh) = s.bounds();
                s.x1 = bx;
                s.y1 = by;
                s.x2 = bx + bw;
                s.y2 = by + bh;
            }
            self.commit(s);
        }
        self.invalidate();
    }

    fn activate(&mut self, hit: Hit) {
        let ed = self.ed.as_mut().unwrap();
        let (zoom, tool) = (ed.zoom, ed.tool);
        if !hit.is_clear() {
            self.disarm_clear();
        }
        let ed = self.ed.as_mut().unwrap();
        match hit {
            Hit::Bar(Item::Tool(t)) | Hit::Pop(PopItem::Tool(t)) => {
                if zoom > 1.0 {
                    return;
                }
                ed.popover = None;
                self.set_tool(t);
            }
            Hit::Bar(Item::Group(g)) => {
                if zoom > 1.0 {
                    return;
                }
                let open = ed.popover == Some(Pop::Group(g));
                ed.popover = (!open).then_some(Pop::Group(g));
                let next = if tool.group() == g { tool } else { ed.group_last[g] };
                self.set_tool(next);
            }
            Hit::Bar(Item::Color) => ed.popover = if ed.popover == Some(Pop::Ink) { None } else { Some(Pop::Ink) },
            Hit::Bar(Item::More) => ed.popover = if ed.popover == Some(Pop::More) { None } else { Some(Pop::More) },
            Hit::Bar(Item::ZoomOut) if zoom > 1.0 => self.set_zoom((zoom - 1.0).max(1.0)),
            Hit::Bar(Item::ZoomIn) if zoom < 5.0 => self.set_zoom((zoom + 1.0).min(5.0)),
            Hit::Bar(Item::ZoomDone) => self.set_zoom(1.0),
            Hit::Bar(Item::Undo) => self.undo(),
            Hit::Bar(Item::Redo) => self.redo(),
            Hit::Bar(Item::Clear) => self.confirm_clear(),
            Hit::Bar(Item::Copy) => self.copy(),
            Hit::Bar(Item::Save) => self.save(),
            Hit::Bar(Item::Refresh) => self.finish(Finish::Recapture),
            Hit::Bar(Item::Close) => self.finish(Finish::Cancel),
            Hit::Pop(PopItem::Color(i)) => ed.color = DRAW_COLORS[i],
            Hit::Pop(PopItem::Size(i)) => ed.size = DRAW_SIZES[i].0,
            Hit::Pop(PopItem::Menu(m)) => {
                if m != MenuItem::Clear {
                    ed.popover = None;
                }
                match m {
                    MenuItem::Redo => self.redo(),
                    MenuItem::Refresh => self.finish(Finish::Recapture),
                    MenuItem::Save => self.save(),
                    MenuItem::Clear => self.confirm_clear(),
                }
            }
            _ => {}
        }
        self.invalidate();
    }

    /// Selects a tool, gliding the toolbar highlight from the previous one.
    fn set_tool(&mut self, tool: Tool) {
        let from = self.layout().and_then(|l| {
            let r = self.indicator(&l)?;
            Some(rect(r.left - l.rect.left, r.top - l.rect.top, r.right - r.left, r.bottom - r.top))
        });
        self.commit_text();
        let ed = self.ed.as_mut().unwrap();
        ed.group_last[tool.group()] = tool;
        if ed.tool == tool {
            return;
        }
        ed.tool = tool;
        if let Some(from) = from.filter(|_| animations_enabled()) {
            ed.glide = Some((from, Instant::now()));
            unsafe {
                SetTimer(Some(self.hwnd), TIMER_GLIDE, 10, None);
            }
        }
        if tool == Tool::Blur {
            self.ensure_pixel();
        }
        self.invalidate();
    }

    /// The selection highlight's current rect, partway through a glide.
    fn indicator(&self, l: &Layout) -> Option<D2D_RECT_F> {
        let ed = self.ed.as_ref()?;
        let to = l.selected_rect(ed.tool)?;
        let Some((from, start)) = ed.glide else {
            return Some(to);
        };
        let t = (start.elapsed().as_secs_f32() * 1000.0 / GLIDE_MS).min(1.0);
        // Ease out with a slight overshoot.
        let (c1, c3) = (1.25, 2.25);
        let e = 1.0 + c3 * (t - 1.0).powi(3) + c1 * (t - 1.0).powi(2);
        let lerp = |a: f32, b: f32| a + (b - a) * e;
        let (ox, oy) = (l.rect.left, l.rect.top);
        Some(D2D_RECT_F {
            left: lerp(from.left + ox, to.left),
            top: lerp(from.top + oy, to.top),
            right: lerp(from.right + ox, to.right),
            bottom: lerp(from.bottom + oy, to.bottom),
        })
    }

    fn disarm_clear(&mut self) {
        if let Some(ed) = self.ed.as_mut() {
            ed.clear_armed = false;
        }
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_CLEAR);
        }
    }

    fn confirm_clear(&mut self) {
        let ed = self.ed.as_mut().unwrap();
        if ed.clear_armed {
            ed.popover = None;
            self.disarm_clear();
            self.clear_all();
        } else {
            ed.clear_armed = true;
            unsafe {
                SetTimer(Some(self.hwnd), TIMER_CLEAR, 2400, None);
            }
        }
    }

    fn clear_all(&mut self) {
        let ed = self.ed.as_mut().unwrap();
        ed.strokes.clear();
        ed.redo.clear();
        ed.text = None;
        ed.current = None;
        ed.step_counter = 1;
        ed.cache_valid = false;
        self.invalidate();
    }

    fn set_zoom(&mut self, zoom: f32) {
        self.commit_text();
        let ed = self.ed.as_mut().unwrap();
        ed.redo.clear();
        if zoom <= 1.0 {
            ed.strokes = std::mem::take(&mut ed.saved);
            ed.step_counter = ed.strokes.iter().filter(|s| s.tool == Tool::Step).count() as u32 + 1;
            ed.zoom = 1.0;
            ed.cache_valid = false;
        } else {
            if ed.zoom <= 1.0 {
                ed.saved = std::mem::take(&mut ed.strokes);
                ed.current = None;
                ed.step_counter = 1;
                ed.cache_valid = false;
            }
            ed.zoom = zoom;
        }
        ed.pan = (0.0, 0.0);
    }

    fn undo(&mut self) {
        let ed = self.ed.as_mut().unwrap();
        if let Some(s) = ed.strokes.pop() {
            ed.redo.push(s);
            ed.step_counter = reindex_steps(&mut ed.strokes);
            ed.cache_valid = false;
            self.invalidate();
        }
    }

    fn redo(&mut self) {
        let ed = self.ed.as_mut().unwrap();
        if let Some(s) = ed.redo.pop() {
            ed.strokes.push(s);
            ed.step_counter = reindex_steps(&mut ed.strokes);
            ed.cache_valid = false;
            self.invalidate();
        }
    }

    fn erase_at(&mut self, lx: f32, ly: f32) {
        let k = self.k;
        let ed = self.ed.as_mut().unwrap();
        if let Some(i) = strokes::hit_test(&ed.strokes, lx, ly, ed.size, k) {
            ed.strokes.remove(i);
            ed.redo.clear();
            ed.step_counter = reindex_steps(&mut ed.strokes);
            ed.cache_valid = false;
            self.invalidate();
        }
    }

    /// Appends a finished stroke and paints it into the stroke cache.
    fn commit(&mut self, s: Stroke) {
        let ed = self.ed.as_mut().unwrap();
        let mut device_lost = false;
        ed.redo.clear();
        if ed.cache_valid {
            if let Some(res) = &self.res {
                if let Some(cache) = &res.cache {
                    unsafe {
                        cache.BeginDraw();
                        let painter = Painter {
                            gfx: &self.gfx,
                            rt: cache,
                            brush: &res.brush,
                            pixel: res.pixel.as_ref(),
                            scale: self.k,
                            clip: None,
                        };
                        painter.draw(&s);
                        if let Err(e) = cache.EndDraw(None, None) {
                            ed.cache_valid = false;
                            device_lost = e.code() == D2DERR_RECREATE_TARGET;
                        }
                    }
                }
            }
        }
        ed.strokes.push(s);
        if device_lost {
            self.res = None;
        }
        self.invalidate();
    }

    fn commit_text(&mut self) {
        let Some(ed) = self.ed.as_mut() else { return };
        if let Some(te) = ed.text.take() {
            if !te.text.trim().is_empty() {
                let mut s = Stroke::new(Tool::Text, ed.color, ed.size, te.x, te.y);
                s.text = te.text;
                self.commit(s);
            }
        }
    }

    fn ensure_pixel(&mut self) {
        let ed = self.ed.as_mut().unwrap();
        if ed.pixel.is_none() {
            let (w, h) = (ed.base.width as i32, ed.base.height as i32);
            ed.pixel = Some(pixelate(&ed.base.pixels, w, h, PIXEL_BLOCK));
        }
        let pixel = ed.pixel.as_ref().unwrap();
        if let Some(res) = &mut self.res {
            if res.pixel.is_none() {
                let size = D2D_SIZE_U { width: ed.base.width, height: ed.base.height };
                res.pixel =
                    unsafe { res.rt.CreateBitmap(size, Some(pixel.as_ptr().cast()), ed.base.width * 4, &bitmap_props()).ok() };
                ed.cache_valid = false;
            }
        }
    }

    fn key(&mut self, vk: VIRTUAL_KEY) {
        if vk == VK_ESCAPE {
            let discard = self.ed.as_mut().is_some_and(|ed| ed.text.take().is_some() || ed.popover.take().is_some());
            if discard {
                self.invalidate();
            } else {
                self.finish(Finish::Cancel);
            }
            return;
        }
        let ctrl = key_down(VK_CONTROL);
        let shift = key_down(VK_SHIFT);
        let Some(ed) = self.ed.as_mut() else {
            match vk {
                VK_RETURN => {
                    let r = self.picker.target;
                    self.start_editor(r);
                }
                _ => {}
            }
            return;
        };
        if ed.busy {
            return;
        }
        if let Some(te) = &mut ed.text {
            match vk {
                VK_RETURN => self.commit_text(),
                VK_BACK => {
                    te.text.pop();
                }
                _ => {}
            }
            self.invalidate();
            return;
        }
        if !ctrl && !key_down(VK_MENU) && ed.zoom <= 1.0 {
            if let Some(tool) = char::from_u32(vk.0 as u32).and_then(Tool::from_key) {
                self.disarm_clear();
                self.ed.as_mut().unwrap().popover = None;
                self.set_tool(tool);
                return;
            }
        }
        match (vk, ctrl) {
            (VIRTUAL_KEY(0x43), true) => self.copy(),
            (VIRTUAL_KEY(0x53), true) => self.save(),
            (VIRTUAL_KEY(0x5A), true) if shift => self.redo(),
            (VIRTUAL_KEY(0x5A), true) => self.undo(),
            (VIRTUAL_KEY(0x59), true) => self.redo(),
            _ => {}
        }
    }

    fn char(&mut self, unit: u16) {
        let Some(te) = self.ed.as_mut().and_then(|e| e.text.as_mut()) else {
            return;
        };
        if (0xD800..0xDC00).contains(&unit) {
            te.high = Some(unit);
            return;
        }
        let units: Vec<u16> = match te.high.take() {
            Some(h) if (0xDC00..0xE000).contains(&unit) => vec![h, unit],
            _ => vec![unit],
        };
        let s = String::from_utf16_lossy(&units);
        if s.chars().all(|c| !c.is_control()) {
            te.text.push_str(&s);
            self.invalidate();
        }
    }

    fn timer(&mut self, id: usize) {
        match id {
            TIMER_TIP => {
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), TIMER_TIP);
                }
                if let Some((hit, false)) = self.tip {
                    self.tip = Some((hit, true));
                    self.invalidate();
                }
            }
            TIMER_GLIDE => {
                let done = self
                    .ed
                    .as_ref()
                    .and_then(|ed| ed.glide)
                    .is_none_or(|(_, start)| start.elapsed().as_secs_f32() * 1000.0 >= GLIDE_MS);
                if done {
                    if let Some(ed) = self.ed.as_mut() {
                        ed.glide = None;
                    }
                    unsafe {
                        let _ = KillTimer(Some(self.hwnd), TIMER_GLIDE);
                    }
                }
                self.invalidate();
            }
            TIMER_CLEAR => {
                self.disarm_clear();
                self.invalidate();
            }
            TIMER_ANIM => {
                let p = &mut self.picker;
                let fading = p.opened.elapsed().as_secs_f32() * 1000.0 < ANIM_MS * 1.5;
                if let Some(start) = p.anim {
                    let t = (start.elapsed().as_secs_f32() * 1000.0 / ANIM_MS).min(1.0);
                    let e = 1.0 - (1.0 - t).powi(3);
                    let to = rf(p.target);
                    let lerp = |a: f32, b: f32| a + (b - a) * e;
                    p.shown = (lerp(p.from.0, to.0), lerp(p.from.1, to.1), lerp(p.from.2, to.2), lerp(p.from.3, to.3));
                    if t >= 1.0 {
                        p.anim = None;
                    }
                    self.invalidate();
                } else if fading {
                    self.invalidate();
                }
            }
            _ => {}
        }
    }

    fn cursor(&self) -> PCWSTR {
        let Some(ed) = &self.ed else { return IDC_CROSS };
        if ed.tb_drag.is_some() || ed.pan_drag.is_some() {
            return IDC_SIZEALL;
        }
        let (x, y) = self.mouse;
        if let Some(layout) = self.layout() {
            match layout.hit(x, y) {
                Some(Hit::Bar(Item::Tool(_) | Item::Group(_))) if ed.zoom > 1.0 => {
                    return IDC_ARROW;
                }
                Some(Hit::Bar(Item::Undo)) if ed.strokes.is_empty() => return IDC_ARROW,
                Some(Hit::Bar(Item::Redo) | Hit::Pop(PopItem::Menu(MenuItem::Redo))) if ed.redo.is_empty() => {
                    return IDC_ARROW;
                }
                Some(Hit::Bar(Item::ZoomOut)) if ed.zoom <= 1.0 => return IDC_ARROW,
                Some(Hit::Bar(Item::ZoomIn)) if ed.zoom >= 5.0 => return IDC_ARROW,
                Some(_) => return IDC_HAND,
                None if layout.on_bar(x, y) => return IDC_SIZEALL,
                None if layout.covers(x, y) => return IDC_ARROW,
                None => {}
            }
        }
        if ed.region.contains(x, y) {
            if ed.zoom > 1.0 {
                IDC_SIZEALL
            } else if ed.tool == Tool::Text {
                IDC_IBEAM
            } else {
                IDC_CROSS
            }
        } else {
            IDC_ARROW
        }
    }

    // ---- export -------------------------------------------------------

    fn export_image(&mut self) -> Result<Image> {
        let has_blur = self.ed.as_ref().unwrap().strokes.iter().any(|s| s.tool == Tool::Blur);
        if has_blur {
            self.ensure_pixel();
        }
        let ed = self.ed.as_ref().unwrap();
        if ed.zoom > 1.0 {
            let (w, h) = (ed.base.width as f32 * ed.zoom, ed.base.height as f32 * ed.zoom);
            return export::resize(&self.gfx, &ed.base, w.round() as u32, h.round() as u32);
        }
        if ed.strokes.is_empty() {
            return Ok(Image { width: ed.base.width, height: ed.base.height, pixels: ed.base.pixels.clone() });
        }
        export::flatten(&self.gfx, &ed.base, &ed.strokes, self.k, ed.pixel.as_deref())
    }

    fn begin_export(&mut self) -> Option<Image> {
        self.commit_text();
        let ed = self.ed.as_mut()?;
        if ed.busy {
            return None;
        }
        ed.busy = true;
        match self.export_image() {
            Ok(img) => Some(img),
            Err(e) => {
                self.export_failed("Could not render the annotation", &e.message());
                None
            }
        }
    }

    fn export_failed(&mut self, title: &str, message: &str) {
        if let Some(ed) = self.ed.as_mut() {
            ed.busy = false;
        }
        self.invalidate();
        host::notify(title, message, None, true);
    }

    fn copy(&mut self) {
        let Some(img) = self.begin_export() else {
            return;
        };
        let result = export::encode_png(&self.gfx, &img).and_then(|png| {
            export::copy_to_clipboard(host::hwnd(), &img, &png)?;
            self.thumbnail(&img, &png, None);
            Ok(())
        });
        match result {
            Ok(()) => self.finish(Finish::Done),
            Err(e) => self.export_failed("Could not copy annotation", &e.message()),
        }
    }

    fn save(&mut self) {
        let Some(img) = self.begin_export() else {
            return;
        };
        match export::encode_png(&self.gfx, &img).map_err(|e| e.message()).and_then(|png| {
            let path = export::save_png(&self.cfg, &png)?;
            self.thumbnail(&img, &png, Some(path.clone()));
            Ok(path)
        }) {
            Ok(path) => {
                host::saved(&path);
                self.finish(Finish::Done);
            }
            Err(e) => self.export_failed("Could not save annotation", &e),
        }
    }

    fn thumbnail(&self, img: &Image, png: &[u8], path: Option<std::path::PathBuf>) {
        let Some(ed) = &self.ed else { return };
        let mut region = ed.region;
        region.x += self.cap.origin.0;
        region.y += self.cap.origin.1;
        let mut monitor = ed.monitor;
        monitor.x += self.cap.origin.0;
        monitor.y += self.cap.origin.1;
        host::exported(img, png, path, region, monitor, self.k);
    }

    // ---- rendering ----------------------------------------------------

    fn ensure_res(&mut self) -> Result<()> {
        if self.res.is_some() {
            return Ok(());
        }
        let (w, h) = (self.cap.width as u32, self.cap.height as u32);
        unsafe {
            let hprops = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd: self.hwnd,
                pixelSize: D2D_SIZE_U { width: w, height: h },
                presentOptions: D2D1_PRESENT_OPTIONS_RETAIN_CONTENTS,
            };
            let rt = self.gfx.d2d.CreateHwndRenderTarget(&target_props(), &hprops)?;
            rt.SetDpi(96.0, 96.0);
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            // A virtual desktop can be wider than one GPU bitmap.
            let tile = rt.GetMaximumBitmapSize().min(4096).max(1);
            let mut screen = Vec::new();
            for y in (0..h).step_by(tile as usize) {
                for x in (0..w).step_by(tile as usize) {
                    let (tw, th) = (tile.min(w - x), tile.min(h - y));
                    let offset = (y as usize * w as usize + x as usize) * 4;
                    let bitmap = rt.CreateBitmap(
                        D2D_SIZE_U { width: tw, height: th },
                        Some(self.cap.pixels.as_ptr().add(offset).cast()),
                        w * 4,
                        &bitmap_props(),
                    )?;
                    screen.push((rect(x as f32, y as f32, tw as f32, th as f32), bitmap));
                }
            }
            let brush = rt.CreateSolidColorBrush(&WHITE, None)?;
            self.res = Some(Res { rt, brush, screen, cache: None, pixel: None, glass: RefCell::new(Vec::new()) });
        }
        if let Some(ed) = &mut self.ed {
            ed.cache_valid = false;
            if ed.pixel.is_some() {
                self.ensure_pixel();
            }
        }
        Ok(())
    }

    fn rebuild_cache(&mut self) {
        let Some(ed) = self.ed.as_mut() else { return };
        if ed.cache_valid || ed.zoom > 1.0 || ed.strokes.is_empty() {
            return;
        }
        let Some(res) = self.res.as_mut() else { return };
        unsafe {
            if res.cache.is_none() {
                let max = res.rt.GetMaximumBitmapSize();
                if ed.base.width > max || ed.base.height > max {
                    return; // Draw strokes directly when a single cache cannot fit.
                }
                let size = D2D_SIZE_F { width: ed.base.width as f32, height: ed.base.height as f32 };
                let px = D2D_SIZE_U { width: ed.base.width, height: ed.base.height };
                res.cache = res
                    .rt
                    .CreateCompatibleRenderTarget(Some(&size), Some(&px), None, D2D1_COMPATIBLE_RENDER_TARGET_OPTIONS_NONE)
                    .ok();
                if let Some(c) = &res.cache {
                    c.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
                }
            }
            let Some(cache) = &res.cache else { return };
            cache.BeginDraw();
            cache.Clear(Some(&theme::with_alpha(BLACK, 0.0)));
            let painter =
                Painter { gfx: &self.gfx, rt: cache, brush: &res.brush, pixel: res.pixel.as_ref(), scale: self.k, clip: None };
            for s in &ed.strokes {
                painter.draw(s);
            }
            match cache.EndDraw(None, None) {
                Ok(()) => ed.cache_valid = true,
                Err(e) if e.code() == D2DERR_RECREATE_TARGET => self.res = None,
                Err(_) => res.cache = None,
            }
        }
    }

    fn render(&mut self, dirty: Option<RECT>) {
        let retained = self.res.is_some();
        if let Err(e) = self.ensure_res() {
            host::notify("Could not display the annotation", &e.message(), None, true);
            self.finish(Finish::Cancel);
            return;
        }
        self.rebuild_cache();
        let Some(res) = self.res.as_ref() else {
            self.invalidate();
            return;
        };
        let rt = &res.rt;
        unsafe {
            rt.BeginDraw();
            rt.SetTransform(&Matrix3x2::identity());
            let full = rect(0.0, 0.0, self.cap.width as f32, self.cap.height as f32);
            let clip = if retained {
                dirty
                    .map(|r| rect(r.left as f32, r.top as f32, (r.right - r.left) as f32, (r.bottom - r.top) as f32))
                    .unwrap_or(full)
            } else {
                full
            };
            rt.PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_ALIASED);
            draw_screen(res, &full, &full, D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR);
            let ui = Ui { rt, brush: &res.brush, k: self.k };
            if self.ed.is_some() {
                self.render_editor(res, &ui, clip);
            } else {
                self.render_picker(&ui);
            }
            rt.PopAxisAlignedClip();
            if let Err(e) = rt.EndDraw(None, None) {
                self.res = None;
                if e.code() == D2DERR_RECREATE_TARGET {
                    self.invalidate();
                } else {
                    host::notify("Could not draw the annotation", &e.message(), None, true);
                    self.finish(Finish::Cancel);
                }
            }
        }
    }

    fn render_picker(&self, ui: &Ui) {
        let p = &self.picker;
        let fade = (p.opened.elapsed().as_secs_f32() * 1000.0 / ANIM_MS).min(1.0);
        let fade = 1.0 - (1.0 - fade).powi(3);
        let (x, y, w, h) = p.shown;
        let (sw, sh) = (self.cap.width as f32, self.cap.height as f32);
        let tint = theme::with_alpha(self.picker_tint, 0.3 * fade);
        for r in [rect(0.0, 0.0, sw, y), rect(0.0, y + h, sw, sh - y - h), rect(0.0, y, x, h), rect(x + w, y, sw - x - w, h)] {
            ui.fill_rect(&r, tint);
        }
        let bw = 2.0 * self.k;
        unsafe {
            ui.brush.SetColor(&theme::with_alpha(self.picker_primary, fade));
            ui.rt.DrawRectangle(&rect(x - bw / 2.0, y - bw / 2.0, w + bw, h + bw), ui.brush, bw, None::<&ID2D1StrokeStyle>);
        }
    }

    fn render_editor(&self, res: &Res, ui: &Ui, clip: D2D_RECT_F) {
        let ed = self.ed.as_ref().unwrap();
        let k = self.k;
        let rt = &res.rt;
        let (sw, sh) = (self.cap.width as f32, self.cap.height as f32);
        ui.fill_rect(&rect(0.0, 0.0, sw, sh), theme::rgba(0x000000, 0.55));
        let (rx, ry, rw, rh) = rf(ed.region);
        let region = rect(rx, ry, rw, rh);
        unsafe {
            if ed.zoom > 1.0 {
                let (iw, ih) = (rw * ed.zoom, rh * ed.zoom);
                let ox = (rw - iw).max(((rw - iw) / 2.0 + ed.pan.0).min(0.0));
                let oy = (rh - ih).max(((rh - ih) / 2.0 + ed.pan.1).min(0.0));
                let dest = rect(rx + ox, ry + oy, iw, ih);
                rt.PushAxisAlignedClip(&region, D2D1_ANTIALIAS_MODE_ALIASED);
                draw_screen(res, &dest, &region, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR);
                rt.PopAxisAlignedClip();
            } else {
                draw_screen(res, &region, &region, D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR);
                if let Some(cache) = &res.cache {
                    if ed.cache_valid {
                        if let Ok(bmp) = cache.GetBitmap() {
                            rt.DrawBitmap(&bmp, Some(&region), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR, None);
                        }
                    }
                }
                rt.PushAxisAlignedClip(&region, D2D1_ANTIALIAS_MODE_ALIASED);
                rt.SetTransform(&Matrix3x2::translation(rx, ry));
                let painter = Painter {
                    gfx: &self.gfx,
                    rt,
                    brush: &res.brush,
                    pixel: res.pixel.as_ref(),
                    scale: k,
                    clip: Some(rect(clip.left - rx, clip.top - ry, clip.right - clip.left, clip.bottom - clip.top)),
                };
                // Allocation/device loss must not hide committed annotations.
                if !ed.cache_valid {
                    for stroke in &ed.strokes {
                        painter.draw(stroke);
                    }
                }
                if let Some(cur) = &ed.current {
                    if cur.tool == Tool::Blur {
                        let (bx, by, bw, bh) = cur.bounds();
                        ui.brush.SetColor(&theme::with_alpha(WHITE, 0.8));
                        rt.DrawRectangle(&rect(bx, by, bw, bh), ui.brush, 2.0 * k, None::<&ID2D1StrokeStyle>);
                    } else {
                        painter.draw(cur);
                    }
                }
                if let Some(te) = &ed.text {
                    let font = strokes::text_font_size(ed.size, k);
                    ui.brush.SetColor(&theme::rgb(ed.color));
                    painter.text(&te.text, te.x, te.y, font);
                    let fmt = self.gfx.text_format(font, windows::Win32::Graphics::DirectWrite::DWRITE_FONT_WEIGHT_BOLD, false);
                    let width = fmt.map(|f| self.gfx.measure(&te.text, &f).0).unwrap_or(0.0);
                    ui.brush.SetColor(&theme::rgb(ed.color));
                    rt.DrawLine(
                        Vector2 { X: te.x + width + 1.0 * k, Y: te.y - font * 0.85 },
                        Vector2 { X: te.x + width + 1.0 * k, Y: te.y + font * 0.2 },
                        ui.brush,
                        1.5 * k,
                        None::<&ID2D1StrokeStyle>,
                    );
                }
                rt.SetTransform(&Matrix3x2::identity());
                rt.PopAxisAlignedClip();
            }
        }

        let Some(layout) = self.layout() else { return };
        let view = View {
            tool: ed.tool,
            group_last: ed.group_last,
            color: ed.color,
            size: ed.size,
            zoom: ed.zoom,
            can_undo: !ed.strokes.is_empty(),
            can_redo: !ed.redo.is_empty(),
            clear_armed: ed.clear_armed,
            hover: if ed.tb_drag.is_some() { None } else { ed.hover },
            indicator: self.indicator(&layout),
        };
        let backdrop = Frozen { cap: &self.cap, region: ed.region, res, k };
        ui.toolbar(&layout, &view, &self.fonts, &self.hud, &backdrop);
        if let Some((Hit::Bar(item), true)) = self.tip {
            if let (Some((text, key)), Some(anchor)) = (item.tip(ed.clear_armed), layout.item_rect(Hit::Bar(item))) {
                let (fonts, hud) = (&self.fonts, &self.hud);
                ui.tooltip(text, key.as_deref(), &anchor, &layout, ed.work, fonts, &self.gfx, hud, &backdrop);
            }
        }
    }
}

/// Blurs the frozen capture under toolbar surfaces, dimmed outside the selection
/// as the editor draws it.
struct Frozen<'a> {
    cap: &'a Capture,
    region: IRect,
    res: &'a Res,
    k: f32,
}

impl Backdrop for Frozen<'_> {
    fn brush(&self, r: &D2D_RECT_F) -> Option<ID2D1BitmapBrush> {
        let left = r.left.floor() as i32;
        let top = r.top.floor() as i32;
        let key = IRect { x: left, y: top, w: r.right.ceil() as i32 - left, h: r.bottom.ceil() as i32 - top };
        let mut cache = self.res.glass.borrow_mut();
        if let Some((_, brush)) = cache.iter().find(|(k, _)| *k == key) {
            return Some(brush.clone());
        }
        let brush = blurred_backdrop(&self.res.rt, self.cap, key, self.region, self.k).ok()??;
        if cache.len() == 8 {
            cache.remove(0);
        }
        cache.push((key, brush.clone()));
        Some(brush)
    }
}

/// Downsamples, dims and box-blurs the capture around `r` into a bitmap brush
/// positioned in canvas coordinates.
fn blurred_backdrop(rt: &ID2D1RenderTarget, cap: &Capture, r: IRect, region: IRect, k: f32) -> Result<Option<ID2D1BitmapBrush>> {
    let f = (4.0 * k).round().max(2.0) as i32;
    let margin = 6 * f;
    let wanted = IRect { x: r.x - margin, y: r.y - margin, w: r.w + margin * 2, h: r.h + margin * 2 };
    let Some(area) = wanted.intersect(&cap.bounds()) else {
        return Ok(None);
    };
    let (dw, dh) = (((area.w + f - 1) / f) as usize, ((area.h + f - 1) / f) as usize);
    let stride = cap.width as usize * 4;
    let mut px = vec![0f32; dw * dh * 3];
    for dy in 0..dh {
        for dx in 0..dw {
            let (x0, y0) = (area.x + dx as i32 * f, area.y + dy as i32 * f);
            let (x1, y1) = ((x0 + f).min(area.x + area.w), (y0 + f).min(area.y + area.h));
            let mut sum = [0f32; 3];
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = y as usize * stride + x as usize * 4;
                    // Matches the 55% black wash drawn outside the selection.
                    let dim = if region.contains(x as f32, y as f32) { 1.0 } else { 0.45 };
                    for c in 0..3 {
                        sum[c] += cap.pixels[i + c] as f32 * dim;
                    }
                }
            }
            let n = ((x1 - x0) * (y1 - y0)).max(1) as f32;
            let o = (dy * dw + dx) * 3;
            for c in 0..3 {
                px[o + c] = sum[c] / n;
            }
        }
    }
    // Three box passes each way approximate a Gaussian.
    let mut tmp = vec![0f32; px.len()];
    for _ in 0..3 {
        box_blur(&px, &mut tmp, dw, dh, 2, true);
        box_blur(&tmp, &mut px, dw, dh, 2, false);
    }
    let bytes: Vec<u8> = px.chunks(3).flat_map(|c| [c[0].round() as u8, c[1].round() as u8, c[2].round() as u8, 255]).collect();
    unsafe {
        let size = D2D_SIZE_U { width: dw as u32, height: dh as u32 };
        let bitmap = rt.CreateBitmap(size, Some(bytes.as_ptr().cast()), dw as u32 * 4, &bitmap_props())?;
        let props = D2D1_BITMAP_BRUSH_PROPERTIES {
            extendModeX: D2D1_EXTEND_MODE_CLAMP,
            extendModeY: D2D1_EXTEND_MODE_CLAMP,
            interpolationMode: D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
        };
        let brush = rt.CreateBitmapBrush(&bitmap, Some(&props), None)?;
        let f = f as f32;
        brush.SetTransform(&Matrix3x2 { M11: f, M12: 0.0, M21: 0.0, M22: f, M31: area.x as f32, M32: area.y as f32 });
        Ok(Some(brush))
    }
}

/// One box-blur pass over RGB floats, clamping at the edges.
fn box_blur(src: &[f32], dst: &mut [f32], w: usize, h: usize, radius: i32, horizontal: bool) {
    let n = (radius * 2 + 1) as f32;
    let (len, lines) = if horizontal { (w, h) } else { (h, w) };
    let at = |line: usize, i: usize| {
        if horizontal { (line * w + i) * 3 } else { (i * w + line) * 3 }
    };
    for line in 0..lines {
        for i in 0..len {
            let mut sum = [0f32; 3];
            for d in -radius..=radius {
                let j = (i as i32 + d).clamp(0, len as i32 - 1) as usize;
                let o = at(line, j);
                for c in 0..3 {
                    sum[c] += src[o + c];
                }
            }
            let o = at(line, i);
            for c in 0..3 {
                dst[o + c] = sum[c] / n;
            }
        }
    }
}

fn draw_screen(res: &Res, dest: &D2D_RECT_F, source: &D2D_RECT_F, mode: D2D1_BITMAP_INTERPOLATION_MODE) {
    let (sx, sy) =
        ((dest.right - dest.left) / (source.right - source.left), (dest.bottom - dest.top) / (source.bottom - source.top));
    for (tile, bitmap) in &res.screen {
        let (left, top) = (source.left.max(tile.left), source.top.max(tile.top));
        let (right, bottom) = (source.right.min(tile.right), source.bottom.min(tile.bottom));
        if right <= left || bottom <= top {
            continue;
        }
        let src = rect(left - tile.left, top - tile.top, right - left, bottom - top);
        let dst = rect(
            dest.left + (left - source.left) * sx,
            dest.top + (top - source.top) * sy,
            (right - left) * sx,
            (bottom - top) * sy,
        );
        unsafe {
            res.rt.DrawBitmap(bitmap, Some(&dst), 1.0, mode, Some(&src));
        }
    }
}

#[cfg(test)]
pub fn check_reentrant_lifetime(gfx: &Rc<Gfx>) {
    unsafe {
        // A synthetic hidden window tests destruction during a live dispatch
        // borrow without changing desktop focus or installing another hook.
        let instance: HINSTANCE = GetModuleHandleW(None).unwrap().into();
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: w!("GlazierLifetimeTest"),
            ..Default::default()
        });
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("GlazierLifetimeTest"),
            w!("Glazier lifetime test"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(instance),
            None,
        )
        .unwrap();
        let cap = Capture {
            origin: (0, 0),
            width: 1,
            height: 1,
            pixels: vec![0, 0, 0, 255],
            cursor: (0, 0),
            windows: Vec::new(),
            monitors: vec![crate::capture::Monitor {
                bounds: IRect { x: 0, y: 0, w: 1, h: 1 },
                work: IRect { x: 0, y: 0, w: 1, h: 1 },
                scale: 1.0,
            }],
        };
        let refs = Rc::strong_count(gfx);
        let state = Rc::new(RefCell::new(Overlay::new(hwnd, gfx.clone(), Rc::new(Config::default()), cap).unwrap()));
        let weak = Rc::downgrade(&state);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Rc::into_raw(state.clone()) as isize);
        let borrow = state.borrow_mut();
        DestroyWindow(hwnd).unwrap();
        assert!(weak.upgrade().is_some());
        drop(borrow);
        drop(state);
        assert!(weak.upgrade().is_none(), "window state survived the final dispatch");
        assert_eq!(Rc::strong_count(gfx), refs, "overlay retained shared factories");
        UnregisterClassW(w!("GlazierLifetimeTest"), Some(instance)).unwrap();
        println!("Reentrant destruction releases overlay state after its live borrow ends");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};

    fn with_editor(test: impl FnOnce(&mut Overlay)) {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
            {
                // Keep invalidation and timers confined to a hidden test window.
                let hwnd = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!("Glazier editor test"),
                    WS_POPUP,
                    0,
                    0,
                    320,
                    180,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap();
                let region = IRect { x: 0, y: 0, w: 320, h: 180 };
                let monitor = crate::capture::Monitor { bounds: region, work: region, scale: 1.0 };
                let cap = Capture {
                    origin: (0, 0),
                    width: region.w,
                    height: region.h,
                    pixels: vec![255; 320 * 180 * 4],
                    monitors: vec![monitor],
                    windows: Vec::new(),
                    cursor: (0, 0),
                };
                let base = Image { width: 320, height: 180, pixels: cap.crop(region) };
                let mut overlay = Overlay::new(hwnd, Rc::new(Gfx::new().unwrap()), Rc::new(Config::default()), cap).unwrap();
                overlay.start_editor_image(region, base, monitor);
                test(&mut overlay);
                DestroyWindow(hwnd).unwrap();
            }
            CoUninitialize();
        }
    }

    #[test]
    fn compact_clear_confirms_and_discards_pending_text() {
        with_editor(|overlay| {
            overlay.commit(Stroke::new(Tool::Step, 0, 3.0, 10.0, 10.0));
            let ed = overlay.ed.as_mut().unwrap();
            ed.popover = Some(Pop::More);
            ed.text = Some(TextEdit { x: 20.0, y: 20.0, text: "pending".into(), high: None });
            let clear = Hit::Pop(PopItem::Menu(MenuItem::Clear));
            overlay.activate(clear);
            let ed = overlay.ed.as_ref().unwrap();
            assert_eq!(ed.strokes.len(), 1, "the first click must only ask for confirmation");
            assert!(ed.clear_armed);
            assert_eq!(ed.popover, Some(Pop::More));
            let r = overlay.layout().unwrap().item_rect(clear).unwrap();
            overlay.editor_move((r.left + r.right) / 2.0, (r.top + r.bottom) / 2.0, WPARAM(0));
            assert!(overlay.ed.as_ref().unwrap().clear_armed, "moving within Clear must keep it armed");
            overlay.activate(clear);
            overlay.commit_text();
            let ed = overlay.ed.as_ref().unwrap();
            assert!(ed.strokes.is_empty(), "pending text must not reappear after clearing");
            assert!(ed.text.is_none());
            assert!(!ed.clear_armed);
            assert_eq!(ed.popover, None);
        });
    }

    #[test]
    fn erasing_discards_stale_redo_history() {
        with_editor(|overlay| {
            overlay.commit(Stroke::new(Tool::Step, 0, 3.0, 10.0, 10.0));
            overlay.commit(Stroke::new(Tool::Step, 0, 3.0, 100.0, 100.0));
            overlay.undo();
            overlay.erase_at(10.0, 10.0);
            overlay.redo();
            let ed = overlay.ed.as_ref().unwrap();
            assert!(ed.strokes.is_empty(), "redo must not restore strokes from before an erase");
            assert!(ed.redo.is_empty());
        });
    }

    #[test]
    fn failed_save_preserves_annotations_and_allows_retry() {
        with_editor(|overlay| {
            let dir = std::env::temp_dir().join(format!("glazier-export-retry-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let blocked = dir.join("not-a-directory");
            std::fs::write(&blocked, b"keep").unwrap();
            overlay.cfg = Rc::new(Config {
                save_directory: blocked.to_string_lossy().into_owned(),
                filename_format: "retry".into(),
                show_thumbnail: false,
                ..Config::default()
            });
            overlay.commit(Stroke::new(Tool::Step, 0xff0000, 3.0, 40.0, 40.0));
            overlay.save();
            assert!(!overlay.finished, "a failed save must keep the editor open");
            let ed = overlay.ed.as_ref().unwrap();
            assert!(!ed.busy, "the user must be able to retry or copy instead");
            assert_eq!(ed.strokes.len(), 1);
            assert_eq!(std::fs::read(&blocked).unwrap(), b"keep");

            Rc::make_mut(&mut overlay.cfg).save_directory = dir.to_string_lossy().into_owned();
            overlay.save();
            assert!(overlay.finished, "a successful retry closes the editor");
            let saved = dir.join("retry.png");
            let decoded = export::decode_png(&overlay.gfx, &saved).unwrap();
            assert_eq!((decoded.width, decoded.height), (320, 180));
            assert!(decoded.pixels.as_chunks::<4>().0.contains(&[0, 0, 255, 255]));
            std::fs::remove_file(saved).unwrap();
            std::fs::remove_file(blocked).unwrap();
            std::fs::remove_dir(dir).unwrap();
        });
    }
}
