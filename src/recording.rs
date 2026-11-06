//! Recording controls, persisted audio preferences and a separate picker flow.

use crate::capture::IRect;
use crate::config::{self, Config};
use crate::control::{self, Phase};
use crate::export::{self, Image};
use crate::gfx::{Gfx, icon, rect};
use crate::theme::*;
use crate::toolbar::Ui;
use crate::{drag, host, recording_media as media};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

pub const CLASS: PCWSTR = w!("GlazierRecording");
const BORDER: PCWSTR = w!("GlazierRecordingBorder");
const FINISH: u32 = WM_APP + 41;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct AudioState {
    pub mic: bool,
    pub sound: bool,
}

impl Default for AudioState {
    fn default() -> Self {
        Self { mic: false, sound: true }
    }
}

impl AudioState {
    fn load() -> Self {
        Self::read(&config::dir().join("state.toml"))
    }

    fn read(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
    }

    fn save(self) -> std::io::Result<()> {
        std::fs::create_dir_all(config::dir())?;
        self.write(&config::dir().join("state.toml"))
    }

    fn write(self, path: &std::path::Path) -> std::io::Result<()> {
        let temp = path.with_extension("toml.tmp");
        std::fs::write(&temp, toml::to_string(&self).map_err(std::io::Error::other)?)?;
        std::fs::rename(temp, path)
    }
}

/// Crop the last odd pixel, keeping the border and encoded area identical.
pub fn clamp_region(region: IRect, monitor: IRect) -> Option<IRect> {
    let mut r = region.intersect(&monitor)?;
    r.w &= !1;
    r.h &= !1;
    (r.w >= 2 && r.h >= 2).then_some(r)
}

/// A single timebase for samples and the timer, measured in 100 ns units.
#[derive(Default)]
pub struct Clock {
    paused_at: Option<i64>,
    paused: i64,
}

impl Clock {
    pub fn pause(&mut self, now: i64) {
        self.paused_at.get_or_insert(now);
    }

    pub fn resume(&mut self, now: i64) {
        if let Some(start) = self.paused_at.take() {
            self.paused += (now - start).max(0);
        }
    }

    pub fn time(&self, now: i64) -> i64 {
        (self.paused_at.unwrap_or(now) - self.paused).max(0)
    }
}

pub enum Command {
    Pause(bool),
    Audio(AudioState),
    Stop,
    Discard,
}

pub enum Event {
    Started(String),
    Warning(String),
    Finished(std::result::Result<Option<media::Completed>, String>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Button {
    Record,
    Pause,
    Stop,
    Mic,
    Sound,
    Discard,
    Cancel,
}

#[derive(Clone, Copy, PartialEq)]
enum Item {
    Button(Button),
    Timer,
    Sep,
}

// Logical window size. The bar is centered in it, leaving room for its shadow
// and for a tooltip above or below. Transparent pixels pass clicks through.
const WIDTH: f32 = 320.0;
const HEIGHT: f32 = 140.0;
const BAR_H: f32 = 52.0;

fn specs(recording: bool) -> Vec<(Item, f32)> {
    use Button::*;
    let b = |b| (Item::Button(b), 34.0);
    let sep = (Item::Sep, 1.0);
    if recording {
        vec![(Item::Timer, 78.0), sep, b(Pause), b(Stop), sep, b(Mic), b(Sound), sep, b(Discard)]
    } else {
        vec![b(Record), sep, b(Mic), b(Sound), sep, b(Cancel)]
    }
}

/// The bar and its items in window pixels, laid out like the editor toolbar.
fn layout(recording: bool, k: f32) -> (D2D_RECT_F, Vec<(Item, D2D_RECT_F)>) {
    let specs = specs(recording);
    let (gap, pad) = (4.0, 6.0);
    let main = specs.iter().map(|s| s.1).sum::<f32>() + gap * (specs.len() - 1) as f32;
    let w = main + pad * 2.0;
    let (x, y) = ((WIDTH - w) / 2.0, (HEIGHT - BAR_H) / 2.0);
    let mut left = x + pad;
    let items = specs
        .iter()
        .map(|&(item, iw)| {
            let ih = if item == Item::Sep { 28.0 } else { 34.0 };
            let r = rect(left * k, (y + (BAR_H - ih) / 2.0) * k, iw * k, ih * k);
            left += iw + gap;
            (item, r)
        })
        .collect();
    (rect(x * k, y * k, w * k, BAR_H * k), items)
}

/// Where a button sits in a bar window `width` pixels wide.
#[cfg(test)]
pub fn button_rect(button: Button, recording: bool, width: i32) -> Option<D2D_RECT_F> {
    let k = width as f32 / WIDTH;
    layout(recording, k).1.into_iter().find(|(item, _)| *item == Item::Button(button)).map(|(_, r)| r)
}

struct Controls {
    hwnd: HWND,
    border: HWND,
    gfx: Rc<Gfx>,
    palette: Palette,
    themed: bool,
    cfg: Rc<Config>,
    region: IRect,
    monitor: IRect,
    work: IRect,
    k: f32,
    icons: IDWriteTextFormat,
    tips_below: bool,
    audio: AudioState,
    worker: Option<Sender<Command>>,
    events: Option<Receiver<Event>>,
    started: Option<Instant>,
    clock: Clock,
    paused: bool,
    finishing: bool,
    hover: Option<Button>,
    pressed: Option<Button>,
    presented: Option<ControlsView>,
}

#[derive(Clone, Copy, PartialEq)]
struct ControlsView {
    recording: bool,
    started: bool,
    paused: bool,
    finishing: bool,
    audio: AudioState,
    hover: Option<Button>,
    tick: i64,
}

thread_local! {
    static ACTIVE: RefCell<Option<Rc<RefCell<Controls>>>> = const { RefCell::new(None) };
    static PENDING: RefCell<Option<(IRect, IRect, IRect, f32)>> = const { RefCell::new(None) };
}

pub fn select(region: IRect, monitor: IRect, work: IRect, k: f32) {
    PENDING.with_borrow_mut(|p| *p = Some((region, monitor, work, k)));
}

pub fn exists() -> bool {
    ACTIVE.with_borrow(|s| s.is_some())
}

pub fn running() -> bool {
    ACTIVE.with_borrow(|s| s.as_ref().is_some_and(|s| s.try_borrow().map_or(true, |s| s.worker.is_some())))
}

pub fn stop() {
    if let Some(s) = ACTIVE.with_borrow(Clone::clone) {
        if let Ok(mut s) = s.try_borrow_mut() {
            s.stop(false);
        }
    }
}

pub fn pause(paused: bool) {
    if let Some(s) = ACTIVE.with_borrow(Clone::clone) {
        if let Ok(mut s) = s.try_borrow_mut() {
            if s.worker.is_some() && s.started.is_some() && !s.finishing && s.paused != paused {
                s.action(Button::Pause);
            }
        }
    }
}

pub fn update_status() {
    if let Some(s) = ACTIVE.with_borrow(Clone::clone) {
        if let Ok(s) = s.try_borrow() {
            control::update((s.clock.time(s.now()) / 10_000) as u64, s.audio);
        }
    }
}

pub fn fullscreen(gfx: Rc<Gfx>, cfg: Rc<Config>, sound: Option<bool>, mic: Option<bool>) -> Result<()> {
    unsafe {
        let mut point = POINT::default();
        GetCursorPos(&mut point)?;
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return Err(Error::from_thread());
        }
        let r = info.rcMonitor;
        let w = info.rcWork;
        let monitor = IRect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top };
        let work = IRect { x: w.left, y: w.top, w: w.right - w.left, h: w.bottom - w.top };
        let region = clamp_region(monitor, monitor).ok_or_else(|| Error::new(E_FAIL, "Monitor is too small"))?;
        let mut x = 96;
        let mut y = 96;
        let _ = windows::Win32::UI::HiDpi::GetDpiForMonitor(
            monitor_handle(point),
            windows::Win32::UI::HiDpi::MDT_EFFECTIVE_DPI,
            &mut x,
            &mut y,
        );
        open(gfx, cfg, region, monitor, work, x as f32 / 96.0)?;
        configure(sound, mic, true);
        Ok(())
    }
}

fn monitor_handle(point: POINT) -> HMONITOR {
    unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) }
}

fn configure(sound: Option<bool>, mic: Option<bool>, start: bool) {
    if let Some(s) = ACTIVE.with_borrow(Clone::clone) {
        let mut s = s.borrow_mut();
        if let Some(sound) = sound {
            s.audio.sound = sound;
        }
        if let Some(mic) = mic {
            s.audio.mic = mic;
        }
        control::update(0, s.audio);
        if start {
            s.action(Button::Record);
        }
    }
}

pub fn hide(hidden: bool) {
    if let Some(s) = ACTIVE.with_borrow(Clone::clone) {
        let Ok(s) = s.try_borrow() else {
            return;
        };
        unsafe {
            let show = if hidden { SW_HIDE } else { SW_SHOWNOACTIVATE };
            let _ = ShowWindow(s.hwnd, show);
            let _ = ShowWindow(s.border, show);
        }
    }
}

pub fn reload_theme(gfx: &Gfx) {
    let tokens = gfx.theme();
    if let Some(state) = ACTIVE.with_borrow(Clone::clone) {
        if let Ok(mut state) = state.try_borrow_mut() {
            state.palette = tokens.as_ref().map_or_else(Palette::default, |t| t.palette());
            state.themed = tokens.is_some();
            state.presented = None;
            unsafe {
                let _ = InvalidateRect(Some(state.border), None, false);
            }
        }
    }
}

pub fn show_pending(gfx: Rc<Gfx>, cfg: Rc<Config>, audio: Option<(Option<bool>, Option<bool>)>) {
    if let Some((region, monitor, work, k)) = PENDING.with_borrow_mut(Option::take) {
        if let Err(e) = open(gfx, cfg, region, monitor, work, k) {
            control::change(Phase::Idle, Some("error"), None, Some(e.message().to_string()));
            host::notify("Could not prepare recording", &e.message(), None, true);
        } else if let Some((sound, mic)) = audio {
            // API-selected regions start after selection; the tray keeps its review step.
            configure(sound, mic, true);
        }
    }
}

pub fn shutdown() {
    stop();
    // Only application shutdown waits here. Encoding stays on the media thread.
    let start = Instant::now();
    while running() && start.elapsed().as_secs() < 15 {
        poll();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    close();
}

fn close() {
    if let Some(s) = ACTIVE.with_borrow_mut(Option::take) {
        let s = s.borrow();
        unsafe {
            let _ = DestroyWindow(s.hwnd);
            let _ = DestroyWindow(s.border);
        }
        host::recording_changed();
    }
}

fn poll() {
    let Some(s) = ACTIVE.with_borrow(Clone::clone) else {
        return;
    };
    let events: Vec<_> = {
        let Ok(s) = s.try_borrow() else { return };
        s.events.as_ref().map(|e| e.try_iter().collect()).unwrap_or_default()
    };
    for event in events {
        match event {
            Event::Started(encoder) => {
                let mut s = s.borrow_mut();
                s.started = Some(Instant::now());
                control::change(
                    if s.finishing { Phase::Finalizing } else { Phase::Recording },
                    Some("started"),
                    None,
                    None,
                );
                unsafe {
                    let _ = SetWindowTextW(s.hwnd, &HSTRING::from(format!("Glazier recording | {encoder}")));
                }
                host::recording_changed();
            }
            Event::Warning(text) => {
                control::error(text.clone());
                host::notify("Recording", &text, None, true);
            }
            Event::Finished(result) => {
                let (gfx, cfg, region, monitor, k) = {
                    let mut s = s.borrow_mut();
                    control::update((s.clock.time(s.now()) / 10_000) as u64, s.audio);
                    s.worker = None;
                    (s.gfx.clone(), s.cfg.clone(), s.region, s.monitor, s.k)
                };
                close();
                match result {
                    Ok(Some(done)) => {
                        control::change(
                            Phase::Idle,
                            Some("finished"),
                            Some(done.path.to_string_lossy().into_owned()),
                            None,
                        );
                        if let Some(warning) = done.warning {
                            host::notify("Recording recovered", &warning, Some(done.path.clone()), true);
                        } else if !cfg.show_thumbnail {
                            host::notify("Recording saved", &done.path.display().to_string(), Some(done.path.clone()), false);
                        }
                        crate::thumbnail::show_video(gfx, &cfg, &done.poster, done.path, region, monitor, k);
                    }
                    Ok(None) => control::change(Phase::Idle, Some("cancelled"), None, None),
                    Err(e) => {
                        control::change(Phase::Idle, Some("error"), None, Some(e.clone()));
                        host::notify("Recording failed", &e, None, true);
                    }
                }
                return;
            }
        }
    }
}

/// Window origin for the review bar: below the selection, above it, or just
/// inside its bottom edge, always within the monitor's work area.
fn bar_placement(work: IRect, region: IRect, recording: bool, k: f32) -> (i32, i32, bool) {
    let specs = specs(recording);
    let main = specs.iter().map(|s| s.1).sum::<f32>() + 4.0 * (specs.len() - 1) as f32;
    let bar_w = (main + 12.0) * k;
    let bar_h = BAR_H * k;
    let placed = crate::placement::controls(work, region, bar_w, bar_h, 8.0 * k, 6.0 * k);
    let x = placed.x - (WIDTH * k - bar_w) / 2.0;
    let y = placed.y - (HEIGHT * k - bar_h) / 2.0;
    let tips_below = (work.y + work.h) as f32 - (placed.y + bar_h) >= 34.0 * k;
    (x.round() as i32, y.round() as i32, tips_below)
}

fn open(gfx: Rc<Gfx>, cfg: Rc<Config>, region: IRect, monitor: IRect, work: IRect, k: f32) -> Result<()> {
    if exists() {
        return Ok(());
    }
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        static REGISTER: std::sync::Once = std::sync::Once::new();
        REGISTER.call_once(|| {
            let classes: [(PCWSTR, WNDPROC); 2] = [(CLASS, Some(wndproc)), (BORDER, Some(border_proc))];
            for (class, proc) in classes {
                RegisterClassExW(&WNDCLASSEXW {
                    cbSize: size_of::<WNDCLASSEXW>() as u32,
                    hInstance: instance,
                    lpfnWndProc: proc,
                    hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                    lpszClassName: class,
                    ..Default::default()
                });
            }
        });
        let icons = gfx.icon_format(17.0 * k)?;
        let w = (WIDTH * k).round() as i32;
        let h = (HEIGHT * k).round() as i32;
        // Place the bar itself, not the transparent margin, beside the region.
        let (x, y, tips_below) = bar_placement(work, region, false, k);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            CLASS,
            w!("Glazier recording"),
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
        let border = match CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_LAYERED,
            BORDER,
            w!("Glazier recording border"),
            WS_POPUP,
            region.x - 2,
            region.y - 2,
            region.w + 4,
            region.h + 4,
            None,
            None,
            Some(instance),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                let _ = DestroyWindow(hwnd);
                return Err(e);
            }
        };
        // A hollow region allows input to pass through the entire live desktop.
        let outer = CreateRectRgn(0, 0, region.w + 4, region.h + 4);
        let inner = CreateRectRgn(2, 2, region.w + 2, region.h + 2);
        CombineRgn(Some(outer), Some(outer), Some(inner), RGN_DIFF);
        let _ = DeleteObject(inner.into());
        SetWindowRgn(border, Some(outer), false);
        let _ = SetLayeredWindowAttributes(border, COLORREF(0), 255, LWA_ALPHA);
        let affinity = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)
            .and_then(|_| SetWindowDisplayAffinity(border, WDA_EXCLUDEFROMCAPTURE));
        if let Err(e) = affinity {
            let _ = DestroyWindow(hwnd);
            let _ = DestroyWindow(border);
            return Err(e);
        }
        let tokens = gfx.theme();
        let palette = tokens.as_ref().map_or_else(Palette::default, |t| t.palette());
        let themed = tokens.is_some();
        ACTIVE.with_borrow_mut(|s| {
            *s = Some(Rc::new(RefCell::new(Controls {
                hwnd,
                border,
                gfx,
                palette,
                themed,
                cfg,
                region,
                monitor,
                work,
                k,
                icons,
                tips_below,
                audio: AudioState::load(),
                worker: None,
                events: None,
                started: None,
                clock: Clock::default(),
                paused: false,
                finishing: false,
                hover: None,
                pressed: None,
                presented: None,
            })))
        });
        // A layered window shows nothing until its first frame is presented.
        present_initial()?;
        let _ = ShowWindow(border, SW_SHOWNOACTIVATE);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(Some(hwnd), 1, 50, None);
        control::change(Phase::Ready, None, None, None);
        Ok(())
    }
}

fn present_initial() -> Result<()> {
    let result = ACTIVE.with_borrow(Clone::clone).map_or(Ok(()), |s| s.borrow_mut().present());
    // Release the RefCell borrow before close() borrows it to destroy the windows.
    if result.is_err() {
        close();
    }
    result
}

unsafe extern "system" fn border_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut ps);
                let mut r = RECT::default();
                let _ = GetClientRect(hwnd, &mut r);
                let primary =
                    ACTIVE.with_borrow(|active| active.as_ref().and_then(|s| s.try_borrow().ok()).map(|s| s.palette.primary));
                let color = primary
                    .map_or(0xffc1c2, |c| (c.r * 255.0) as u32 | (((c.g * 255.0) as u32) << 8) | (((c.b * 255.0) as u32) << 16));
                let pen = CreatePen(PS_DASH, 1, COLORREF(color));
                let old = SelectObject(dc, pen.into());
                SetBkMode(dc, TRANSPARENT);
                let brush = SelectObject(dc, GetStockObject(NULL_BRUSH));
                let _ = Rectangle(dc, 0, 0, r.right, r.bottom);
                let _ = Rectangle(dc, 1, 1, r.right - 1, r.bottom - 1);
                SelectObject(dc, brush);
                SelectObject(dc, old);
                let _ = DeleteObject(pen.into());
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == FINISH {
            close();
            return LRESULT(0);
        }
        if msg == WM_TIMER {
            poll();
        }
        let Some(s) = ACTIVE.with_borrow(Clone::clone) else {
            return DefWindowProcW(hwnd, msg, wp, lp);
        };
        let Ok(mut s) = s.try_borrow_mut() else {
            return DefWindowProcW(hwnd, msg, wp, lp);
        };
        let point = || ((lp.0 & 0xffff) as i16 as f32, ((lp.0 >> 16) & 0xffff) as i16 as f32);
        match msg {
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            WM_TIMER => {
                // Layered windows get no WM_MOUSELEAVE without tracking; poll instead.
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                if WindowFromPoint(pt) != hwnd {
                    s.hover = None;
                }
            }
            WM_MOUSEMOVE => {
                let (x, y) = point();
                s.hover = s.hit(x, y);
            }
            WM_LBUTTONDOWN => {
                let (x, y) = point();
                s.pressed = s.hit(x, y);
            }
            WM_LBUTTONUP => {
                let (x, y) = point();
                let hit = s.hit(x, y);
                if let Some(button) = hit.filter(|_| s.pressed.take() == hit) {
                    s.action(button);
                }
            }
            WM_CLOSE => s.stop(true),
            WM_DISPLAYCHANGE => s.stop(false),
            WM_DPICHANGED => {}
            _ => return DefWindowProcW(hwnd, msg, wp, lp),
        }
        if matches!(msg, WM_TIMER | WM_MOUSEMOVE | WM_LBUTTONUP) {
            if let Err(e) = s.present() {
                if !s.finishing {
                    control::error(e.message().to_string());
                    host::notify("Recording controls failed", &e.message(), None, true);
                }
                s.stop(false);
            }
        }
        LRESULT(0)
    }
}

impl Controls {
    fn now(&self) -> i64 {
        self.started.map_or(0, |t| (t.elapsed().as_nanos() / 100) as i64)
    }

    /// Switches to the recording bar's wider layout and re-places the window.
    fn start_worker(&mut self, worker: Sender<Command>, events: Receiver<Event>) {
        self.worker = Some(worker);
        self.events = Some(events);
        // The bar widens once recording starts; re-place it so its right edge
        // (Discard) stays on screen.
        self.place();
        host::recording_changed();
    }

    /// Repositions the window after the bar's width changes.
    fn place(&mut self) {
        let (x, y, tips_below) = bar_placement(self.work, self.region, self.worker.is_some(), self.k);
        self.tips_below = tips_below;
        unsafe {
            let _ = SetWindowPos(self.hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }

    fn hit(&self, x: f32, y: f32) -> Option<Button> {
        if self.finishing {
            return None;
        }
        layout(self.worker.is_some(), self.k).1.into_iter().find_map(|(item, r)| match item {
            Item::Button(b) if x >= r.left && x < r.right && y >= r.top && y < r.bottom => Some(b),
            _ => None,
        })
    }

    fn action(&mut self, button: Button) {
        if self.worker.is_none() {
            match button {
                Button::Record => {
                    let (send, recv) = mpsc::channel();
                    let (events, results) = mpsc::channel();
                    let cfg = (*self.cfg).clone();
                    let region = self.region;
                    let audio = self.audio;
                    match std::thread::Builder::new().name("Glazier recording".into()).spawn(move || {
                        media::run(cfg, region, audio, recv, events);
                    }) {
                        Ok(_) => self.start_worker(send, results),
                        Err(e) => {
                            control::error(e.to_string());
                            host::notify("Could not start recording", &e.to_string(), None, true);
                        }
                    }
                }
                Button::Mic => self.toggle(true),
                Button::Sound => self.toggle(false),
                _ => self.stop(true),
            }
        } else {
            match button {
                Button::Pause if self.started.is_some() => {
                    self.paused = !self.paused;
                    let now = self.now();
                    if self.paused {
                        self.clock.pause(now);
                    } else {
                        self.clock.resume(now);
                    }
                    let _ = self.worker.as_ref().unwrap().send(Command::Pause(self.paused));
                    control::change(
                        if self.paused { Phase::Paused } else { Phase::Recording },
                        Some(if self.paused { "paused" } else { "resumed" }),
                        None,
                        None,
                    );
                }
                Button::Mic => self.toggle(true),
                Button::Sound => self.toggle(false),
                Button::Stop => self.stop(false),
                Button::Discard => self.stop(true),
                _ => {}
            }
        }
    }

    fn toggle(&mut self, mic: bool) {
        if mic {
            self.audio.mic = !self.audio.mic;
        } else {
            self.audio.sound = !self.audio.sound;
        }
        if let Err(e) = self.audio.save() {
            host::notify("Could not remember audio settings", &e.to_string(), None, true);
        }
        if let Some(worker) = &self.worker {
            let _ = worker.send(Command::Audio(self.audio));
        }
    }

    fn stop(&mut self, discard: bool) {
        if self.finishing {
            return;
        }
        self.finishing = true;
        if let Some(worker) = &self.worker {
            self.clock.pause(self.now());
            control::change(Phase::Finalizing, None, None, None);
            let _ = worker.send(if discard { Command::Discard } else { Command::Stop });
        } else {
            control::change(Phase::Idle, Some("cancelled"), None, None);
            unsafe {
                let _ = PostMessageW(Some(self.hwnd), FINISH, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// Glyph, optional background, foreground and tooltip for a button.
    fn look(&self, button: Button, hovered: bool) -> (&'static str, Option<D2D1_COLOR_F>, D2D1_COLOR_F, &'static str) {
        let p = self.palette;
        let plain = |glyph, tip| (glyph, hovered.then_some(p.hover), if hovered { p.on_hover } else { p.on_surface }, tip);
        // Enabled toggles read like the active tool in the editor toolbar.
        let toggle = |on: bool, glyphs: (&'static str, &'static str), tips: (&'static str, &'static str)| {
            if on {
                (
                    glyphs.0,
                    Some(if hovered { p.secondary } else { p.primary }),
                    if hovered && self.themed { p.on_surface } else { p.on_primary },
                    tips.0,
                )
            } else {
                (glyphs.1, hovered.then_some(p.hover), p.on_surface_variant, tips.1)
            }
        };
        match button {
            Button::Record => (
                "player-record-filled",
                Some(with_alpha(p.record, if hovered { 0.24 } else { 0.12 })),
                p.record,
                "Start recording",
            ),
            Button::Pause if self.paused => plain("player-play-filled", "Resume"),
            Button::Pause => plain("player-pause-filled", "Pause"),
            Button::Stop => {
                ("player-stop-filled", Some(with_alpha(p.record, if hovered { 0.24 } else { 0.12 })), p.record, "Stop and save")
            }
            Button::Mic => toggle(self.audio.mic, ("microphone", "microphone-off"), ("Microphone on", "Microphone off")),
            Button::Sound => toggle(self.audio.sound, ("volume", "volume-off"), ("System sound on", "System sound off")),
            Button::Discard => (
                "trash",
                hovered.then_some(with_alpha(p.error, 0.15)),
                if hovered { p.error } else { p.on_surface },
                "Discard recording",
            ),
            Button::Cancel => plain("x", "Cancel"),
        }
    }

    fn timer(&self, ui: &Ui, r: &D2D_RECT_F) -> Result<()> {
        let k = self.k;
        let (dot_x, cy) = (r.left + 10.0 * k, (r.top + r.bottom) / 2.0);
        let live = self.started.is_some() && !self.paused && !self.finishing;
        let dot = match self.started {
            // A slow pulse marks a live recording.
            Some(t) if live => {
                with_alpha(self.palette.record, 0.6 + 0.4 * (t.elapsed().as_secs_f32() * std::f32::consts::TAU / 1.6).cos())
            }
            _ => self.palette.on_surface_variant,
        };
        ui.circle(dot_x, cy, 5.0 * k, dot, None);
        let sec = self.clock.time(self.now()) / 10_000_000;
        let label = if self.finishing {
            "Saving".into()
        } else if sec >= 3600 {
            format!("{}:{:02}:{:02}", sec / 3600, sec / 60 % 60, sec % 60)
        } else {
            format!("{:02}:{:02}", sec / 60, sec % 60)
        };
        let font = if self.themed {
            self.gfx.hud_format(13.0 * k, DWRITE_FONT_WEIGHT_SEMI_BOLD, true)?
        } else {
            self.gfx.text_format(13.0 * k, DWRITE_FONT_WEIGHT_SEMI_BOLD, true)?
        };
        let text = D2D_RECT_F { left: dot_x + 6.0 * k, ..*r };
        ui.text(&label, &text, &font, if live { self.palette.on_surface } else { self.palette.on_surface_variant });
        Ok(())
    }

    fn tooltip(&self, ui: &Ui, text: &str, anchor: &D2D_RECT_F, bar: &D2D_RECT_F, width: f32) -> Result<()> {
        let k = self.k;
        let font = if self.themed {
            self.gfx.hud_format(12.0 * k, DWRITE_FONT_WEIGHT_NORMAL, true)?
        } else {
            self.gfx.text_format(12.0 * k, DWRITE_FONT_WEIGHT_NORMAL, true)?
        };
        let (w, h) = (self.gfx.measure(text, &font).0 + 16.0 * k, 26.0 * k);
        let x = ((anchor.left + anchor.right - w) / 2.0).clamp(0.0, (width - w).max(0.0));
        let y = if self.tips_below { bar.bottom + 8.0 * k } else { bar.top - 8.0 * k - h };
        let r = rect(x, y, w, h);
        ui.fill_rr(&r, self.palette.radius_small * k, self.palette.surface_variant);
        ui.stroke_rr(&r, self.palette.radius_small * k, self.palette.outline, k);
        ui.text(text, &r, &font, self.palette.on_surface);
        Ok(())
    }

    fn render(&self) -> Result<Image> {
        let k = self.k;
        let (w, h) = ((WIDTH * k).round() as u32, (HEIGHT * k).round() as u32);
        let (bar, items) = layout(self.worker.is_some(), k);
        let dim = if self.finishing { 0.35 } else { 1.0 };
        unsafe {
            let target = self.gfx.wic.CreateBitmap(w, h, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad)?;
            let rt = self.gfx.d2d.CreateWicBitmapRenderTarget(&target, &export::target_props())?;
            let brush = rt.CreateSolidColorBrush(&BLACK, None)?;
            let ui = Ui { rt: &rt, brush: &brush, k };
            rt.BeginDraw();
            rt.Clear(Some(&D2D1_COLOR_F::default()));
            if self.themed {
                ui.shadow(&bar, self.palette.radius_large * k);
                ui.fill_rr(&bar, self.palette.radius_large * k, self.palette.surface);
                ui.stroke_rr(&bar, self.palette.radius_large * k, self.palette.outline, k);
            } else {
                ui.panel(&bar);
            }
            let mut tip = None;
            for &(item, r) in &items {
                match item {
                    Item::Sep => ui.fill_rect(&r, with_alpha(self.palette.on_surface_variant, 0.3)),
                    Item::Timer => self.timer(&ui, &r)?,
                    Item::Button(b) => {
                        let hovered = self.hover == Some(b);
                        let (glyph, bg, fg, text) = self.look(b, hovered);
                        if let Some(bg) = bg {
                            ui.fill_rr(
                                &r,
                                if self.themed { self.palette.radius_small } else { 12.0 } * k,
                                with_alpha(bg, bg.a * dim),
                            );
                        }
                        ui.text(icon(glyph), &r, &self.icons, with_alpha(fg, fg.a * dim));
                        if hovered {
                            tip = Some((text, r));
                        }
                    }
                }
            }
            if let Some((text, r)) = tip {
                self.tooltip(&ui, text, &r, &bar, w as f32)?;
            }
            rt.EndDraw(None, None)?;
            let mut pixels = vec![0; w as usize * h as usize * 4];
            target.CopyPixels(std::ptr::null(), w * 4, &mut pixels)?;
            Ok(Image { width: w, height: h, pixels })
        }
    }

    fn present(&mut self) -> Result<()> {
        let live = self.started.is_some() && !self.paused && !self.finishing;
        let view = ControlsView {
            recording: self.worker.is_some(),
            started: self.started.is_some(),
            paused: self.paused,
            finishing: self.finishing,
            audio: self.audio,
            hover: self.hover,
            // Animate at the timer's 20 Hz even when mouse messages arrive faster.
            tick: if live { self.now() / 500_000 } else { 0 },
        };
        if self.presented == Some(view) {
            return Ok(());
        }
        let image = self.render()?;
        let bitmap = drag::bitmap(&image)?;
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                let _ = DeleteObject(bitmap.into());
                return Err(Error::from_thread());
            }
            let old = SelectObject(dc, bitmap.into());
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
                BlendFlags: 0,
            };
            let result = UpdateLayeredWindow(
                self.hwnd,
                None,
                None,
                Some(&SIZE { cx: image.width as i32, cy: image.height as i32 }),
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
                self.presented = Some(view);
            }
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_controls_share_worker_state_without_capturing() {
        unsafe {
            windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED)
                .ok().unwrap();
        }
        {
            let gfx = Rc::new(Gfx::new().unwrap());
            let (commands, receive) = mpsc::channel();
            let (events, results) = mpsc::channel();
            ACTIVE.with_borrow_mut(|s| *s = Some(Rc::new(RefCell::new(Controls {
                hwnd: HWND::default(), border: HWND::default(),
                icons: gfx.icon_format(17.0).unwrap(), gfx, cfg: Rc::new(Config::default()),
                palette: Palette::default(), themed: false,
                region: IRect { x: 0, y: 0, w: 320, h: 180 },
                monitor: IRect { x: 0, y: 0, w: 1920, h: 1080 },
                work: IRect { x: 0, y: 0, w: 1920, h: 1040 }, k: 1.0, tips_below: false,
                audio: AudioState::default(), worker: Some(commands), events: Some(results),
                started: None, clock: Clock::default(), paused: false, finishing: false,
                hover: None, pressed: None, presented: None,
            }))));
            control::change(Phase::Ready, None, None, None);
            events.send(Event::Started("test encoder, no capture worker".into())).unwrap();
            poll();
            assert_eq!(control::phase(), Phase::Recording);
            assert!(host::control(control::Command::Pause).is_ok());
            assert!(matches!(receive.try_recv(), Ok(Command::Pause(true))));
            assert_eq!(control::phase(), Phase::Paused);
            assert!(host::control(control::Command::Pause).is_err());
            assert!(receive.try_recv().is_err());
            assert!(host::control(control::Command::Resume).is_ok());
            assert!(matches!(receive.try_recv(), Ok(Command::Pause(false))));
            assert_eq!(control::phase(), Phase::Recording);
            assert!(host::control(control::Command::Stop).is_ok());
            assert!(matches!(receive.try_recv(), Ok(Command::Stop)));
            assert_eq!(control::phase(), Phase::Finalizing);
            assert!(host::control(control::Command::Stop).is_ok());
            assert!(receive.try_recv().is_err(), "stop must not toggle or send a second start");
            events.send(Event::Finished(Err("simulated failure".into()))).unwrap();
            poll();
            assert_eq!(control::phase(), Phase::Idle);
            assert!(!exists());
            assert!(host::control(control::Command::Stop).is_ok());
            assert!(!exists(), "idle stop must not start controls or capture");
        }
        unsafe { windows::Win32::System::Com::CoUninitialize(); }
    }

    #[test]
    fn failed_initial_presentation_releases_controls() {
        unsafe {
            windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED)
                .ok()
                .unwrap();
        }
        {
            let gfx = Rc::new(Gfx::new().unwrap());
            let state = Rc::new(RefCell::new(Controls {
                // UpdateLayeredWindow fails for a null HWND, exercising startup cleanup.
                hwnd: HWND::default(),
                border: HWND::default(),
                icons: gfx.icon_format(17.0).unwrap(),
                gfx,
                palette: Palette::default(),
                themed: false,
                cfg: Rc::new(Config::default()),
                region: IRect { x: 0, y: 0, w: 320, h: 180 },
                monitor: IRect { x: 0, y: 0, w: 1920, h: 1080 },
                work: IRect { x: 0, y: 0, w: 1920, h: 1040 },
                k: 1.0,
                tips_below: false,
                audio: AudioState::default(),
                worker: None,
                events: None,
                started: None,
                clock: Clock::default(),
                paused: false,
                finishing: false,
                hover: None,
                pressed: None,
                presented: None,
            }));
            let weak = Rc::downgrade(&state);
            ACTIVE.with_borrow_mut(|s| *s = Some(state));
            assert!(present_initial().is_err());
            assert!(!exists());
            assert!(weak.upgrade().is_none(), "failed controls must release their resources");
        }
        unsafe {
            windows::Win32::System::Com::CoUninitialize();
        }
    }

    #[test]
    fn bar_placement_uses_the_work_area_for_any_selection() {
        let bar = |recording: bool, k: f32| {
            let specs = specs(recording);
            (specs.iter().map(|s| s.1).sum::<f32>() + 4.0 * (specs.len() - 1) as f32 + 12.0) * k
        };
        let work = IRect { x: 0, y: 0, w: 1920, h: 1040 };
        for (recording, k) in [(false, 1.0), (true, 1.0), (true, 1.5), (true, 2.0)] {
            let region = IRect { x: 0, y: 0, w: 1920, h: 1080 };
            let (x, y, _) = bar_placement(work, region, recording, k);
            let (bw, bh) = (bar(recording, k), BAR_H * k);
            let (left, top) = (x as f32 + (WIDTH * k - bw) / 2.0, y as f32 + (HEIGHT * k - bh) / 2.0);
            assert!(left >= 0.0 && top >= 0.0, "{recording} {k}: {left} {top}");
            assert!(left + bw <= 1920.0 && top + bh <= 1040.0, "{recording} {k}: {left} {top}");
        }

        // A small selection at the top leaves room below, so the bar goes there.
        let (_, y, tips) = bar_placement(work, IRect { x: 300, y: 200, w: 500, h: 300 }, false, 1.0);
        assert_eq!(y, (500.0 + 8.0 - (HEIGHT - BAR_H) / 2.0) as i32);
        assert!(tips);
    }

    #[test]
    fn expanding_to_recording_width_repositions_the_bar() {
        unsafe {
            windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED)
                .ok()
                .unwrap();
        }
        {
            let gfx = Rc::new(Gfx::new().unwrap());
            let hwnd = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!("Glazier bar placement test"),
                    WS_POPUP,
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .unwrap();
            let work = IRect { x: 0, y: 0, w: 1920, h: 1040 };
            let region = IRect { x: 1800, y: 100, w: 120, h: 300 };
            let (send, _receive) = mpsc::channel();
            let (_events, results) = mpsc::channel();
            let state = Rc::new(RefCell::new(Controls {
                hwnd,
                border: HWND::default(),
                icons: gfx.icon_format(17.0).unwrap(),
                gfx,
                palette: Palette::default(),
                themed: false,
                cfg: Rc::new(Config::default()),
                region,
                monitor: IRect { x: 0, y: 0, w: 1920, h: 1080 },
                work,
                k: 1.0,
                tips_below: false,
                audio: AudioState::default(),
                worker: None,
                events: None,
                started: None,
                clock: Clock::default(),
                paused: false,
                finishing: false,
                hover: None,
                pressed: None,
                presented: None,
            }));
            let bar = |recording: bool| {
                let state = state.borrow();
                let mut r = RECT::default();
                unsafe { GetWindowRect(state.hwnd, &mut r) }.unwrap();
                let specs = specs(recording);
                let main = specs.iter().map(|s| s.1).sum::<f32>() + 4.0 * (specs.len() - 1) as f32;
                let bw = (main + 12.0) * state.k;
                let left = r.left as f32 + (WIDTH * state.k - bw) / 2.0;
                (left, left + bw)
            };
            state.borrow_mut().place();
            assert!(bar(false).1 <= (work.x + work.w) as f32 - 6.0);

            // The recording bar is 125 px wider than the review bar.
            state.borrow_mut().start_worker(send, results);
            let (left, right) = bar(true);
            assert!(right <= (work.x + work.w) as f32 - 6.0 + 1.0, "Discard must stay on screen: {left}..{right}");
            assert!(left >= work.x as f32 + 6.0, "record button must stay on screen: {left}..{right}");
            unsafe { DestroyWindow(hwnd) }.unwrap();
        }
        unsafe {
            windows::Win32::System::Com::CoUninitialize();
        }
    }

    #[test]
    fn clamps_negative_monitor_and_crops_odd_sizes() {
        let m = IRect { x: -3840, y: 0, w: 3840, h: 2160 };
        assert_eq!(clamp_region(IRect { x: -101, y: 3, w: 201, h: 2159 }, m), Some(IRect { x: -101, y: 3, w: 100, h: 2156 }));
        assert!(clamp_region(IRect { x: 0, y: 0, w: 10, h: 10 }, m).is_none());
        assert!(clamp_region(IRect { x: -1, y: 0, w: 1, h: 9 }, m).is_none());
    }

    #[test]
    fn pause_shifts_timestamps_and_is_idempotent() {
        let mut c = Clock::default();
        assert_eq!(c.time(100), 100);
        c.pause(100);
        c.pause(200);
        assert_eq!(c.time(500), 100);
        c.resume(600);
        c.resume(700);
        assert_eq!(c.time(800), 300);
        c.pause(900);
        c.resume(1200);
        assert_eq!(c.time(1400), 600);
    }

    #[test]
    fn audio_state_file_round_trip() {
        let dir = std::env::temp_dir().join(format!("glazier-state-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("state.toml");
        for value in [AudioState::default(), AudioState { mic: true, sound: false }] {
            value.write(&file).unwrap();
            assert_eq!(AudioState::read(&file), value);
        }
        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
