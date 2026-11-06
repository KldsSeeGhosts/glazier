//! Editor toolbar, its popovers and the zoom control: translucent surfaces drawn
//! over a blurred copy of the frozen capture. Sizes are logical pixels multiplied
//! by the monitor scale `k`; hairlines stay one physical pixel.

use crate::capture::IRect;
use crate::gfx::{Gfx, icon, rect, wide};
use crate::placement;
use crate::strokes::Tool;
use crate::theme::{self, *};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::core::Result;
use windows_numerics::Vector2;

pub const GROUP_NAMES: [&str; 3] = ["Freehand", "Shapes", "Markup"];

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Item {
    Tool(Tool),
    /// A collapsed tool group in the compact bar.
    Group(usize),
    Sep,
    Color,
    Undo,
    Redo,
    Clear,
    Refresh,
    Save,
    Copy,
    More,
    Close,
    ZoomOut,
    ZoomLabel,
    ZoomIn,
    ZoomDone,
}

impl Item {
    /// Tooltip text and its keyboard shortcut.
    pub fn tip(self, clear_armed: bool) -> Option<(&'static str, Option<String>)> {
        let key = |s: &str| Some(s.to_owned());
        Some(match self {
            Item::Tool(t) => (t.tip(), Some(t.key().to_string())),
            Item::Group(g) => (GROUP_NAMES[g], None),
            Item::Color => ("Color and line size", None),
            Item::Undo => ("Undo", key("Ctrl+Z")),
            Item::Redo => ("Redo", key("Ctrl+Y")),
            Item::Clear if clear_armed => ("Click again to clear all", None),
            Item::Clear => ("Clear all", None),
            Item::Refresh => ("New capture", None),
            Item::Save => ("Save", key("Ctrl+S")),
            Item::Copy => ("Copy annotated image", key("Ctrl+C")),
            Item::More => ("More", None),
            Item::Close => ("Close", key("Esc")),
            Item::ZoomOut => ("Zoom out", None),
            Item::ZoomIn => ("Zoom in", None),
            Item::Sep | Item::ZoomLabel | Item::ZoomDone => return None,
        })
    }
}

/// Which popover is open.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pop {
    Ink,
    Group(usize),
    More,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MenuItem {
    Redo,
    Refresh,
    Save,
    Clear,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PopItem {
    Color(usize),
    Size(usize),
    Tool(Tool),
    Menu(MenuItem),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    Bar(Item),
    Pop(PopItem),
}

impl Hit {
    pub fn is_clear(self) -> bool {
        matches!(self, Hit::Bar(Item::Clear) | Hit::Pop(PopItem::Menu(MenuItem::Clear)))
    }
}

pub struct Fonts {
    pub icon: IDWriteTextFormat,
    pub icon_small: IDWriteTextFormat,
    pub icon_tiny: IDWriteTextFormat,
    pub label: IDWriteTextFormat,
    pub menu: IDWriteTextFormat,
    pub small_left: IDWriteTextFormat,
    pub small_bold: IDWriteTextFormat,
    pub small_bold_left: IDWriteTextFormat,
    copy_w: f32,
    clear_w: f32,
    done_w: f32,
}

impl Fonts {
    pub fn new(gfx: &Gfx, k: f32) -> Result<Self> {
        let semibold = DWRITE_FONT_WEIGHT_SEMI_BOLD;
        let fonts = Self {
            icon: gfx.icon_format(18.0 * k)?,
            icon_small: gfx.icon_format(16.0 * k)?,
            icon_tiny: gfx.icon_format(10.0 * k)?,
            label: gfx.hud_format(13.0 * k, semibold, false)?,
            menu: gfx.hud_format(13.0 * k, DWRITE_FONT_WEIGHT_NORMAL, false)?,
            small_left: gfx.hud_format(12.0 * k, DWRITE_FONT_WEIGHT_NORMAL, false)?,
            small_bold: gfx.hud_format(12.0 * k, semibold, true)?,
            small_bold_left: gfx.hud_format(12.0 * k, semibold, false)?,
            copy_w: 0.0,
            clear_w: 0.0,
            done_w: 0.0,
        };
        Ok(Self {
            copy_w: 39.0 * k + gfx.measure("Copy", &fonts.label).0,
            clear_w: 39.0 * k + gfx.measure(CLEAR_ARMED, &fonts.small_bold_left).0,
            done_w: 18.0 * k + gfx.measure("Done", &fonts.small_bold).0,
            ..fonts
        })
    }
}

const CLEAR_ARMED: &str = "Clear all?";

pub struct Inputs {
    pub region: IRect,
    /// Monitor work area (no taskbar), in the same coordinates as `region`.
    pub work: IRect,
    pub user_pos: Option<(f32, f32)>,
    pub show_magnifier: bool,
    pub show_tool_options: bool,
    pub popover: Option<Pop>,
    pub clear_armed: bool,
    pub zoomed: bool,
}

pub struct Popover {
    pub kind: Pop,
    pub rect: D2D_RECT_F,
    pub items: Vec<(PopItem, D2D_RECT_F)>,
    /// Hairlines between sections.
    pub dividers: Vec<D2D_RECT_F>,
    /// Background of the line-size control.
    pub track: Option<D2D_RECT_F>,
}

pub struct Layout {
    pub rect: D2D_RECT_F,
    pub compact: bool,
    pub items: Vec<(Item, D2D_RECT_F)>,
    pub chip: Option<D2D_RECT_F>,
    pub popover: Option<Popover>,
}

fn inside(r: &D2D_RECT_F, x: f32, y: f32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn overlaps(a: &D2D_RECT_F, b: &D2D_RECT_F) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

impl Layout {
    pub fn hit(&self, x: f32, y: f32) -> Option<Hit> {
        if let Some(p) = &self.popover {
            if let Some((item, _)) = p.items.iter().find(|(_, r)| inside(r, x, y)) {
                return Some(Hit::Pop(*item));
            }
        }
        self.items
            .iter()
            .find(|(item, r)| !matches!(item, Item::Sep | Item::ZoomLabel) && inside(r, x, y))
            .map(|(item, _)| Hit::Bar(*item))
    }

    /// True on the toolbar itself, where empty space drags it.
    pub fn on_bar(&self, x: f32, y: f32) -> bool {
        inside(&self.rect, x, y)
    }

    /// True when the point is on the toolbar, the zoom control or the open popover.
    pub fn covers(&self, x: f32, y: f32) -> bool {
        self.on_bar(x, y)
            || self.chip.is_some_and(|r| inside(&r, x, y))
            || self.popover.as_ref().is_some_and(|p| inside(&p.rect, x, y))
    }

    pub fn item_rect(&self, hit: Hit) -> Option<D2D_RECT_F> {
        match hit {
            Hit::Bar(i) => self.items.iter().find(|(it, _)| *it == i).map(|(_, r)| *r),
            Hit::Pop(p) => self.popover.as_ref()?.items.iter().find(|(it, _)| *it == p).map(|(_, r)| *r),
        }
    }

    /// Where the selection highlight sits for `tool`.
    pub fn selected_rect(&self, tool: Tool) -> Option<D2D_RECT_F> {
        let item = if self.compact { Item::Group(tool.group()) } else { Item::Tool(tool) };
        self.item_rect(Hit::Bar(item))
    }
}

/// Item sizes along the bar, each with the gap before it.
struct Specs {
    items: Vec<(Item, f32, f32, f32)>,
    gap: f32,
    k: f32,
}

impl Specs {
    fn add(&mut self, item: Item, w: f32, h: f32) {
        let before = if self.items.is_empty() { 0.0 } else { self.gap };
        self.items.push((item, w, h, before));
        self.gap = 2.0 * self.k;
    }

    fn gap(&mut self, g: f32) {
        self.gap = g * self.k;
    }

    fn sep(&mut self) {
        self.gap(6.0);
        self.add(Item::Sep, 1.0, 18.0 * self.k);
        self.gap(6.0);
    }

    fn main(&self) -> f32 {
        self.items.iter().map(|s| s.3 + s.1).sum()
    }
}

fn specs(inp: &Inputs, fonts: &Fonts, k: f32, compact: bool) -> Specs {
    let btn = 30.0 * k;
    let mut s = Specs { items: Vec::new(), gap: 0.0, k };
    if compact {
        for g in 0..Tool::GROUPS.len() {
            if g > 0 {
                s.gap(4.0);
            }
            s.add(Item::Group(g), 42.0 * k, btn);
        }
    } else {
        for (g, tools) in Tool::GROUPS.iter().enumerate() {
            if g > 0 {
                s.gap(8.0);
            }
            for &t in *tools {
                s.add(Item::Tool(t), btn, btn);
            }
        }
    }
    if inp.show_tool_options {
        s.sep();
        s.add(Item::Color, btn, btn);
    }
    s.sep();
    s.add(Item::Undo, btn, btn);
    let square = (34.0 * k, btn);
    if compact {
        s.add(Item::More, btn, btn);
        s.gap(4.0);
        s.add(Item::Copy, square.0, square.1);
    } else {
        s.add(Item::Redo, btn, btn);
        let clear_w = if inp.clear_armed { fonts.clear_w } else { btn };
        s.add(Item::Clear, clear_w, btn);
        s.sep();
        s.add(Item::Refresh, btn, btn);
        s.add(Item::Save, btn, btn);
        s.gap(4.0);
        s.add(Item::Copy, fonts.copy_w, btn);
    }
    s.gap(4.0);
    s.add(Item::Close, btn, btn);
    s
}

/// Places items along one axis, centering each across the other axis.
fn flow(specs: &Specs, origin: (f32, f32), cross: f32) -> Vec<(Item, D2D_RECT_F)> {
    let mut pos = 0.0;
    specs
        .items
        .iter()
        .map(|&(item, w, h, before)| {
            pos += before;
            let r = rect(origin.0 + pos, origin.1 + (cross - h) / 2.0, w, h);
            pos += w;
            (item, r)
        })
        .collect()
}

fn shift(r: &mut D2D_RECT_F, dx: f32, dy: f32) {
    r.left += dx;
    r.right += dx;
    r.top += dy;
    r.bottom += dy;
}

pub fn layout(inp: &Inputs, fonts: &Fonts, k: f32) -> Layout {
    let r = inp.region;
    let rw = r.w as f32;
    let (thick, gap, pad, margin) = (44.0 * k, 10.0 * k, 6.0 * k, 6.0 * k);

    // Collapse the tool groups when the full bar would be wider than the selection.
    let full = specs(inp, fonts, k, false);
    let length = full.main() + pad * 2.0;
    let compact = length > rw + 90.0 * k || length > inp.work.w as f32 - margin * 2.0;
    let specs = if compact { specs(inp, fonts, k, true) } else { full };

    let main = specs.main();
    let (w, h) = (main + pad * 2.0, thick);
    let auto = placement::controls(inp.work, r, w, h, gap, margin);
    let (x, y) = match inp.user_pos {
        Some(pos) => placement::clamp(inp.work, pos.0, pos.1, w, h, margin),
        None => (auto.x, auto.y),
    };
    let bar = rect(x, y, w, h);
    let mut items = flow(&specs, (x + pad, y), h);

    let chip = inp.show_magnifier.then(|| zoom_chip(inp, fonts, k, &bar, &mut items));
    let popover = inp.popover.and_then(|kind| {
        let anchor = match kind {
            Pop::Ink => Item::Color,
            Pop::Group(g) => Item::Group(g),
            Pop::More => Item::More,
        };
        let anchor = items.iter().find(|(it, _)| *it == anchor)?.1;
        Some(popover(kind, &bar, &anchor, inp.work, k))
    });
    Layout { rect: bar, compact, items, chip, popover }
}

/// The zoom control sits inside the selection's bottom-right corner, or next to
/// the selection when it is too small to hold it.
fn zoom_chip(inp: &Inputs, fonts: &Fonts, k: f32, bar: &D2D_RECT_F, items: &mut Vec<(Item, D2D_RECT_F)>) -> D2D_RECT_F {
    let r = inp.region;
    let (rx, ry, rw, rh) = (r.x as f32, r.y as f32, r.w as f32, r.h as f32);
    let mut s = Specs { items: Vec::new(), gap: 0.0, k };
    s.add(Item::ZoomOut, 24.0 * k, 24.0 * k);
    s.gap(1.0);
    s.add(Item::ZoomLabel, 28.0 * k, 24.0 * k);
    s.gap(1.0);
    s.add(Item::ZoomIn, 24.0 * k, 24.0 * k);
    if inp.zoomed {
        s.gap(3.0);
        s.add(Item::ZoomDone, fonts.done_w, 22.0 * k);
    }
    let (pad, inset, margin) = (3.0 * k, 10.0 * k, 6.0 * k);
    let (w, h) = (s.main() + pad * 2.0, 30.0 * k);
    let x = (rx + rw - w - inset).max(rx + inset);
    let (x, y) = placement::clamp(inp.work, x, ry + rh - h - inset, w, h, margin);
    let inner = rect(x, y, w, h);
    let above = {
        let (x, y) = placement::clamp(inp.work, x, ry - h - 8.0 * k, w, h, margin);
        rect(x, y, w, h)
    };
    let below = {
        let (x, y) = placement::clamp(inp.work, x, ry + rh + 8.0 * k, w, h, margin);
        rect(x, y, w, h)
    };
    let roomy = rw >= w + 40.0 * k && rh >= h + 40.0 * k;
    let on_work = |c: &D2D_RECT_F| {
        c.top >= inp.work.y as f32 + margin && c.bottom <= (inp.work.y + inp.work.h) as f32 - margin
    };
    let chip = if roomy && !overlaps(&inner, bar) {
        inner
    } else {
        [above, below].into_iter().find(|c| on_work(c) && !overlaps(c, bar)).unwrap_or(inner)
    };
    items.extend(flow(&s, (chip.left + pad, chip.top), h));
    chip
}

fn popover(kind: Pop, bar: &D2D_RECT_F, anchor: &D2D_RECT_F, work: IRect, k: f32) -> Popover {
    let mut items = Vec::new();
    let mut dividers = Vec::new();
    let mut track = None;
    // Content is laid out from (0, 0), then moved beside the anchor.
    let (w, h) = match kind {
        Pop::Ink => {
            let (pad, dot, gap) = (12.0 * k, 20.0 * k, 8.0 * k);
            let n = DRAW_COLORS.len() as f32;
            let cw = n * dot + (n - 1.0) * gap;
            for i in 0..DRAW_COLORS.len() {
                let x = pad + i as f32 * (dot + gap);
                items.push((PopItem::Color(i), rect(x - 2.0 * k, pad - 2.0 * k, dot + 4.0 * k, dot + 4.0 * k)));
            }
            let line = pad + dot + 12.0 * k;
            dividers.push(rect(pad, line, cw, 1.0));
            let t = rect(pad, line + 1.0 + 12.0 * k, cw, 32.0 * k);
            let seg = (cw - 4.0 * k) / DRAW_SIZES.len() as f32;
            for i in 0..DRAW_SIZES.len() {
                items.push((PopItem::Size(i), rect(t.left + 2.0 * k + i as f32 * seg, t.top + 2.0 * k, seg, 28.0 * k)));
            }
            track = Some(t);
            (cw + pad * 2.0, t.bottom + pad)
        }
        Pop::Group(g) => {
            let (pad, btn, gap) = (6.0 * k, 30.0 * k, 2.0 * k);
            let tools = Tool::GROUPS[g];
            for (i, &t) in tools.iter().enumerate() {
                items.push((PopItem::Tool(t), rect(pad + i as f32 * (btn + gap), pad, btn, btn)));
            }
            let n = tools.len() as f32;
            (pad * 2.0 + n * btn + (n - 1.0) * gap, pad * 2.0 + btn)
        }
        Pop::More => {
            let (pad, row, width) = (5.0 * k, 30.0 * k, 184.0 * k);
            let mut y = pad;
            for m in [MenuItem::Redo, MenuItem::Refresh, MenuItem::Save, MenuItem::Clear] {
                if m == MenuItem::Clear {
                    dividers.push(rect(pad + 4.0 * k, y + 5.0 * k, width - pad * 2.0 - 8.0 * k, 1.0));
                    y += 11.0 * k;
                }
                items.push((PopItem::Menu(m), rect(pad, y, width - pad * 2.0, row)));
                y += row;
            }
            (width, y + pad)
        }
    };
    let (gap, margin) = (8.0 * k, 6.0 * k);
    let acx = (anchor.left + anchor.right) / 2.0;
    let y = if bar.top - gap - h >= work.y as f32 + margin { bar.top - gap - h } else { bar.bottom + gap };
    let (x, y) = placement::clamp(work, acx - w / 2.0, y, w, h, margin);
    for (_, r) in &mut items {
        shift(r, x, y);
    }
    for r in &mut dividers {
        shift(r, x, y);
    }
    if let Some(t) = &mut track {
        shift(t, x, y);
    }
    Popover { kind, rect: rect(x, y, w, h), items, dividers, track }
}

/// Supplies a blurred copy of what lies beneath a surface.
pub trait Backdrop {
    fn brush(&self, r: &D2D_RECT_F) -> Option<ID2D1BitmapBrush>;
}

/// Editor state the toolbar reflects.
pub struct View {
    pub tool: Tool,
    /// Tool each compact group button shows when it is not active.
    pub group_last: [Tool; 3],
    pub color: u32,
    pub size: f32,
    pub zoom: f32,
    pub can_undo: bool,
    pub can_redo: bool,
    pub clear_armed: bool,
    pub hover: Option<Hit>,
    /// Current position of the gliding selection highlight.
    pub indicator: Option<D2D_RECT_F>,
}

pub struct Ui<'a> {
    pub rt: &'a ID2D1RenderTarget,
    pub brush: &'a ID2D1SolidColorBrush,
    pub k: f32,
}

fn fade(c: D2D1_COLOR_F, opacity: f32) -> D2D1_COLOR_F {
    with_alpha(c, c.a * opacity)
}

fn lighten(c: D2D1_COLOR_F, t: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: c.r + (1.0 - c.r) * t, g: c.g + (1.0 - c.g) * t, b: c.b + (1.0 - c.b) * t, a: c.a }
}

fn grow(r: &D2D_RECT_F, d: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: r.left - d, top: r.top - d, right: r.right + d, bottom: r.bottom + d }
}

fn center(r: &D2D_RECT_F) -> (f32, f32) {
    ((r.left + r.right) / 2.0, (r.top + r.bottom) / 2.0)
}

fn size_label_colour(hud: &Hud, selected: bool) -> D2D1_COLOR_F {
    if selected { hud.on_thumb } else { hud.fg2 }
}

/// Diameter of the color button's dot, which also shows the line size.
fn dot_diameter(size: f32, k: f32) -> f32 {
    (8.0 + 1.3 * size).clamp(9.0, 19.0) * k
}

impl Ui<'_> {
    pub fn fill_rr(&self, r: &D2D_RECT_F, radius: f32, color: D2D1_COLOR_F) {
        unsafe {
            self.brush.SetColor(&color);
            self.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: *r, radiusX: radius, radiusY: radius }, self.brush);
        }
    }

    pub fn stroke_rr(&self, r: &D2D_RECT_F, radius: f32, color: D2D1_COLOR_F, width: f32) {
        let h = width / 2.0;
        let r = D2D_RECT_F { left: r.left + h, top: r.top + h, right: r.right - h, bottom: r.bottom - h };
        unsafe {
            self.brush.SetColor(&color);
            self.rt.DrawRoundedRectangle(
                &D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius },
                self.brush,
                width,
                None::<&ID2D1StrokeStyle>,
            );
        }
    }

    pub fn fill_rect(&self, r: &D2D_RECT_F, color: D2D1_COLOR_F) {
        unsafe {
            self.brush.SetColor(&color);
            self.rt.FillRectangle(r, self.brush);
        }
    }

    pub fn circle(&self, cx: f32, cy: f32, radius: f32, fill: D2D1_COLOR_F, border: Option<(D2D1_COLOR_F, f32)>) {
        let e = D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: radius, radiusY: radius };
        unsafe {
            self.brush.SetColor(&fill);
            self.rt.FillEllipse(&e, self.brush);
            if let Some((c, w)) = border {
                let e = D2D1_ELLIPSE { radiusX: radius - w / 2.0, radiusY: radius - w / 2.0, ..e };
                self.brush.SetColor(&c);
                self.rt.DrawEllipse(&e, self.brush, w, None::<&ID2D1StrokeStyle>);
            }
        }
    }

    pub fn text(&self, s: &str, r: &D2D_RECT_F, format: &IDWriteTextFormat, color: D2D1_COLOR_F) {
        unsafe {
            self.brush.SetColor(&color);
            self.rt.DrawText(&wide(s), format, r, self.brush, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
        }
    }

    /// Soft drop shadow approximating the QML MultiEffect.
    pub fn shadow(&self, r: &D2D_RECT_F, radius: f32) {
        let k = self.k;
        let base = D2D_RECT_F { top: r.top + 3.0 * k, bottom: r.bottom + 3.0 * k, ..*r };
        for i in (1..=6).rev() {
            let d = i as f32 * 1.5 * k;
            self.fill_rr(&grow(&base, d), radius + d, theme::rgba(0x000000, 0.045));
        }
    }

    pub fn panel(&self, r: &D2D_RECT_F) {
        let radius = 20.0 * self.k;
        self.shadow(r, radius);
        self.fill_rr(r, radius, SURFACE);
        self.stroke_rr(r, radius, CAPSULE_BORDER, 1.0 * self.k);
    }

    /// A translucent surface: long soft shadow, blurred backdrop, tint and a hairline edge.
    fn glass(&self, r: &D2D_RECT_F, radius: f32, hud: &Hud, backdrop: Option<ID2D1BitmapBrush>) {
        let k = self.k;
        let deep = D2D_RECT_F { top: r.top + 14.0 * k, bottom: r.bottom + 6.0 * k, ..grow(r, -6.0 * k) };
        for i in 1..=8 {
            let d = i as f32 * 3.0 * k;
            self.fill_rr(&grow(&deep, d), radius + d, theme::rgba(0x000000, 0.045 * hud.shadow));
        }
        let near = D2D_RECT_F { top: r.top + 3.0 * k, bottom: r.bottom + 3.0 * k, ..*r };
        for i in 1..=3 {
            let d = i as f32 * 1.5 * k;
            self.fill_rr(&grow(&near, d), radius + d, theme::rgba(0x000000, 0.05 * hud.shadow));
        }
        let rr = D2D1_ROUNDED_RECT { rect: *r, radiusX: radius, radiusY: radius };
        match backdrop {
            Some(b) => unsafe { self.rt.FillRoundedRectangle(&rr, &b) },
            // Without a backdrop, a nearly opaque tint keeps text legible.
            None => self.fill_rr(r, radius, hud.solid),
        }
        self.fill_rr(r, radius, hud.tint);
        self.stroke_rr(r, radius, hud.edge, 1.0);
        let y = r.top + 1.5;
        unsafe {
            self.brush.SetColor(&hud.highlight);
            self.rt.DrawLine(
                Vector2 { X: r.left + radius, Y: y },
                Vector2 { X: r.right - radius, Y: y },
                self.brush,
                1.0,
                None::<&ID2D1StrokeStyle>,
            );
        }
    }

    fn icon_button(&self, r: &D2D_RECT_F, glyph: &str, fonts: &Fonts, hud: &Hud, hovered: bool, enabled: bool) {
        if hovered && enabled {
            self.fill_rr(r, hud.radius_small * self.k, hud.hover);
        }
        self.text(icon(glyph), r, &fonts.icon, fade(hud.fg, if enabled { 1.0 } else { 0.3 }));
    }

    /// The color button's dot, ringed so it reads on any backdrop.
    fn color_dot(&self, cx: f32, cy: f32, d: f32, color: u32, hud: &Hud) {
        let k = self.k;
        self.circle(cx, cy, d / 2.0 + 2.5 * k, hud.divider, None);
        self.circle(cx, cy, d / 2.0 + 1.5 * k, hud.solid, None);
        self.circle(cx, cy, d / 2.0, theme::rgb(color), None);
    }

    pub fn toolbar(&self, l: &Layout, v: &View, fonts: &Fonts, hud: &Hud, backdrop: &dyn Backdrop) {
        let k = self.k;
        self.glass(&l.rect, hud.radius_large * k, hud, backdrop.brush(&l.rect));
        if let Some(c) = &l.chip {
            self.glass(c, (c.bottom - c.top) / 2.0, hud, backdrop.brush(c));
        }
        let zoomed = v.zoom > 1.0;
        let tool_alpha = if zoomed { 0.35 } else { 1.0 };
        if let Some(ind) = &v.indicator {
            self.fill_rr(ind, hud.radius_small * k, fade(hud.selected, tool_alpha));
        }
        for &(item, r) in &l.items {
            let hovered = v.hover == Some(Hit::Bar(item));
            match item {
                Item::Sep => self.fill_rect(&r, hud.divider),
                Item::Tool(t) => {
                    let selected = v.tool == t;
                    if hovered && !selected && !zoomed {
                        self.fill_rr(&r, hud.radius_small * k, hud.hover);
                    }
                    let fg = if selected { hud.on_selected } else { hud.fg };
                    self.text(icon(t.icon()), &r, &fonts.icon, fade(fg, tool_alpha));
                }
                Item::Group(g) => {
                    let selected = v.tool.group() == g;
                    if hovered && !selected && !zoomed {
                        self.fill_rr(&r, hud.radius_small * k, hud.hover);
                    }
                    let shown = if selected { v.tool } else { v.group_last[g] };
                    let fg = fade(if selected { hud.on_selected } else { hud.fg }, tool_alpha);
                    let h = r.bottom - r.top;
                    let glyph = rect(r.left + 5.0 * k, r.top, 20.0 * k, h);
                    let chevron = rect(r.left + 25.0 * k, r.top, 11.0 * k, h);
                    self.text(icon(shown.icon()), &glyph, &fonts.icon, fg);
                    self.text(icon("chevron-down"), &chevron, &fonts.icon_tiny, fade(fg, 0.55));
                }
                Item::Color => {
                    let open = l.popover.as_ref().is_some_and(|p| p.kind == Pop::Ink);
                    if hovered || open {
                        self.fill_rr(&r, hud.radius_small * k, hud.hover);
                    }
                    let (cx, cy) = center(&r);
                    self.color_dot(cx, cy, dot_diameter(v.size, k), v.color, hud);
                }
                Item::Clear if v.clear_armed => {
                    self.fill_rr(&r, hud.radius_small * k, hud.danger_fill);
                    let glyph = rect(r.left + 8.0 * k, r.top, 16.0 * k, r.bottom - r.top);
                    self.text(icon("trash"), &glyph, &fonts.icon_small, hud.danger);
                    let label = D2D_RECT_F { left: r.left + 29.0 * k, ..r };
                    self.text(CLEAR_ARMED, &label, &fonts.small_bold_left, hud.danger);
                }
                Item::Copy => {
                    let fill = if hovered { lighten(hud.action, 0.08) } else { hud.action };
                    self.fill_rr(&r, hud.radius_small * k, fill);
                    if l.compact {
                        self.text(icon("copy"), &r, &fonts.icon_small, hud.on_action);
                    } else {
                        let glyph = rect(r.left + 10.0 * k, r.top, 16.0 * k, r.bottom - r.top);
                        self.text(icon("copy"), &glyph, &fonts.icon_small, hud.on_action);
                        let label = D2D_RECT_F { left: r.left + 32.0 * k, ..r };
                        self.text("Copy", &label, &fonts.label, hud.on_action);
                    }
                }
                Item::More => {
                    let open = l.popover.as_ref().is_some_and(|p| p.kind == Pop::More);
                    self.icon_button(&r, "dots", fonts, hud, hovered || open, true);
                }
                Item::ZoomOut | Item::ZoomIn => {
                    let enabled = if item == Item::ZoomOut { v.zoom > 1.0 } else { v.zoom < 5.0 };
                    if hovered && enabled {
                        self.fill_rr(&r, (r.bottom - r.top) / 2.0, hud.hover);
                    }
                    let glyph = if item == Item::ZoomOut { "minus" } else { "plus" };
                    self.text(icon(glyph), &r, &fonts.icon_small, fade(hud.fg, if enabled { 1.0 } else { 0.3 }));
                }
                Item::ZoomLabel => {
                    let label = format!("{}×", v.zoom.round() as i32);
                    self.text(&label, &r, &fonts.small_bold, if zoomed { hud.fg } else { hud.fg2 });
                }
                Item::ZoomDone => {
                    let fill = if hovered { lighten(hud.action, 0.08) } else { hud.action };
                    self.fill_rr(&r, (r.bottom - r.top) / 2.0, fill);
                    self.text("Done", &r, &fonts.small_bold, hud.on_action);
                }
                _ => {
                    let enabled = match item {
                        Item::Undo => v.can_undo,
                        Item::Redo => v.can_redo,
                        _ => true,
                    };
                    let glyph = match item {
                        Item::Undo => "arrow-back-up",
                        Item::Redo => "arrow-forward-up",
                        Item::Clear => "trash",
                        Item::Refresh => "capture",
                        Item::Save => "download",
                        _ => "x",
                    };
                    self.icon_button(&r, glyph, fonts, hud, hovered, enabled);
                }
            }
        }
        if let Some(p) = &l.popover {
            self.popover(p, v, fonts, hud, backdrop);
        }
    }

    fn popover(&self, p: &Popover, v: &View, fonts: &Fonts, hud: &Hud, backdrop: &dyn Backdrop) {
        let k = self.k;
        self.glass(&p.rect, hud.radius_large * k, hud, backdrop.brush(&p.rect));
        for d in &p.dividers {
            self.fill_rect(d, hud.divider);
        }
        if let Some(t) = &p.track {
            self.fill_rr(t, 9.0 * k, hud.track);
        }
        for &(item, r) in &p.items {
            let hovered = v.hover == Some(Hit::Pop(item));
            match item {
                PopItem::Color(i) => {
                    let color = DRAW_COLORS[i];
                    let (cx, cy) = center(&r);
                    let radius = 10.0 * k * if hovered { 1.12 } else { 1.0 };
                    if v.color == color {
                        self.circle(cx, cy, radius + 3.5 * k, hud.fg, None);
                        self.circle(cx, cy, radius + 2.0 * k, hud.solid, None);
                    }
                    self.circle(cx, cy, radius, theme::rgb(color), Some((theme::rgba(0x000000, 0.18), 1.0)));
                }
                PopItem::Size(i) => {
                    let (size, label) = DRAW_SIZES[i];
                    let selected = v.size == size;
                    if selected {
                        let lift = D2D_RECT_F { top: r.top + 1.0 * k, bottom: r.bottom + 1.0 * k, ..r };
                        self.fill_rr(&lift, hud.radius_tooltip * k, theme::rgba(0x000000, 0.12));
                        self.fill_rr(&r, hud.radius_tooltip * k, hud.thumb);
                    } else if hovered {
                        self.fill_rr(&r, hud.radius_tooltip * k, hud.hover);
                    }
                    let (cx, cy) = center(&r);
                    let (line_w, text_w, gap) = (22.0 * k, 8.0 * k, 8.0 * k);
                    let x0 = cx - (line_w + gap + text_w) / 2.0;
                    let lh = size * k;
                    self.fill_rr(&rect(x0, cy - lh / 2.0, line_w, lh), lh / 2.0, theme::rgb(v.color));
                    let tr = rect(x0 + line_w + gap, r.top, text_w + 4.0 * k, r.bottom - r.top);
                    self.text(label, &tr, &fonts.small_left, size_label_colour(hud, selected));
                }
                PopItem::Tool(t) => {
                    let selected = v.tool == t;
                    if selected {
                        self.fill_rr(&r, hud.radius_small * k, hud.selected);
                    } else if hovered {
                        self.fill_rr(&r, hud.radius_small * k, hud.hover);
                    }
                    let fg = if selected { hud.on_selected } else { hud.fg };
                    self.text(icon(t.icon()), &r, &fonts.icon, fg);
                }
                PopItem::Menu(m) => {
                    let enabled = m != MenuItem::Redo || v.can_redo;
                    if hovered && enabled {
                        self.fill_rr(&r, hud.radius_tooltip * k, hud.hover);
                    }
                    let (glyph, label, key) = match m {
                        MenuItem::Redo => ("arrow-forward-up", "Redo", Some("Ctrl+Y")),
                        MenuItem::Refresh => ("capture", "New capture", None),
                        MenuItem::Save => ("download", "Save", Some("Ctrl+S")),
                        MenuItem::Clear => ("trash", if v.clear_armed { CLEAR_ARMED } else { "Clear all" }, None),
                    };
                    let fg = if m == MenuItem::Clear { hud.danger } else { hud.fg };
                    let fg = fade(fg, if enabled { 1.0 } else { 0.4 });
                    let h = r.bottom - r.top;
                    self.text(icon(glyph), &rect(r.left + 8.0 * k, r.top, 18.0 * k, h), &fonts.icon_small, fg);
                    self.text(label, &D2D_RECT_F { left: r.left + 36.0 * k, ..r }, &fonts.menu, fg);
                    if let Some(key) = key {
                        let w = 60.0 * k;
                        let kr = rect(r.right - 9.0 * k - w, r.top, w, h);
                        self.text_right(key, &kr, &fonts.small_left, fade(hud.fg2, if enabled { 1.0 } else { 0.6 }));
                    }
                }
            }
        }
    }

    fn text_right(&self, s: &str, r: &D2D_RECT_F, format: &IDWriteTextFormat, color: D2D1_COLOR_F) {
        unsafe {
            let _ = format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING);
            self.text(s, r, format, color);
            let _ = format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
        }
    }

    /// Name and shortcut beside the hovered control.
    #[allow(clippy::too_many_arguments)]
    pub fn tooltip(
        &self,
        text: &str,
        key: Option<&str>,
        anchor: &D2D_RECT_F,
        l: &Layout,
        work: IRect,
        fonts: &Fonts,
        gfx: &Gfx,
        hud: &Hud,
        backdrop: &dyn Backdrop,
    ) {
        let k = self.k;
        let (pad, gap) = (9.0 * k, 10.0 * k);
        let tw = gfx.measure(text, &fonts.small_left).0;
        let kw = key.map_or(0.0, |key| gfx.measure(key, &fonts.small_left).0 + gap);
        let (w, h) = (tw + kw + pad * 2.0, 26.0 * k);
        let space = 8.0 * k;
        // Tooltips sit outside the surface that holds the control.
        let host = if inside(&l.rect, anchor.left + 1.0, anchor.top + 1.0) { l.rect } else { l.chip.unwrap_or(l.rect) };
        let cx = center(anchor).0;
        let y = if host.top - space - h >= work.y as f32 + 6.0 * k { host.top - space - h } else { host.bottom + space };
        let (x, y) = placement::clamp(work, cx - w / 2.0, y, w, h, 6.0 * k);
        let r = rect(x, y, w, h);
        self.glass(&r, hud.radius_tooltip * k, hud, backdrop.brush(&r));
        self.text(text, &rect(x + pad, y, tw + 2.0 * k, h), &fonts.small_left, hud.fg);
        if let Some(key) = key {
            self.text(key, &rect(x + pad + tw + gap, y, kw, h), &fonts.small_left, hud.fg2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gfx() -> Gfx {
        use windows::Win32::System::Com::*;
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        Gfx::new().unwrap()
    }

    fn inputs(region: IRect, popover: Option<Pop>) -> Inputs {
        Inputs {
            region,
            work: IRect { x: 0, y: 0, w: 1920, h: 1040 },
            user_pos: None,
            show_magnifier: true,
            show_tool_options: true,
            popover,
            clear_armed: false,
            zoomed: false,
        }
    }

    #[test]
    fn size_label_uses_on_primary_only_for_external_theme() {
        let tokens = crate::theme_tokens::ThemeTokens::parse(crate::theme_tokens::tests::VALID).unwrap();
        let themed = tokens.hud();
        assert_eq!(size_label_colour(&themed, true), theme::rgb(0x654321));
        assert_eq!(size_label_colour(&themed, false), themed.fg2);
        for builtin in [&theme::HUD_DARK, &theme::HUD_LIGHT] {
            assert_eq!(size_label_colour(builtin, true), builtin.fg);
            assert_eq!(size_label_colour(builtin, false), builtin.fg2);
        }
    }

    #[test]
    fn narrow_selection_collapses_tool_groups() {
        let gfx = gfx();
        let fonts = Fonts::new(&gfx, 1.0).unwrap();
        let wide = layout(&inputs(IRect { x: 300, y: 200, w: 900, h: 400 }, None), &fonts, 1.0);
        assert!(!wide.compact);
        assert!(wide.item_rect(Hit::Bar(Item::Tool(Tool::Ruler))).is_some());
        assert!(wide.rect.top >= 610.0, "bar sits below the selection");

        let narrow = layout(&inputs(IRect { x: 300, y: 200, w: 300, h: 400 }, Some(Pop::Group(1))), &fonts, 1.0);
        assert!(narrow.compact);
        assert!(narrow.item_rect(Hit::Bar(Item::Tool(Tool::Arrow))).is_none());
        let pop = narrow.popover.as_ref().unwrap();
        assert_eq!(pop.items.len(), Tool::GROUPS[1].len());
        assert!(pop.rect.bottom <= narrow.rect.top, "flyout opens above the bar");
        let (x, y) = center(&narrow.item_rect(Hit::Pop(PopItem::Tool(Tool::Rect))).unwrap());
        assert_eq!(narrow.hit(x, y), Some(Hit::Pop(PopItem::Tool(Tool::Rect))));
    }

    #[test]
    fn full_height_selection_keeps_the_bar_and_chip_above_the_taskbar() {
        let gfx = gfx();
        let fonts = Fonts::new(&gfx, 1.0).unwrap();
        let region = IRect { x: 100, y: 0, w: 1200, h: 1080 };
        let l = layout(&inputs(region, Some(Pop::Ink)), &fonts, 1.0);
        assert!(l.rect.bottom <= 1034.0, "bar stays inside the work area: {:?}", l.rect);
        assert!(l.rect.top >= 0.0);
        let chip = l.chip.unwrap();
        assert!(chip.right <= 1920.0 && chip.bottom <= 1040.0 && chip.left >= 100.0);
        let pop = l.popover.as_ref().unwrap();
        assert!(pop.rect.top >= 0.0 && pop.rect.bottom <= 1040.0);
        assert_eq!(pop.items.iter().filter(|(i, _)| matches!(i, PopItem::Color(_))).count(), DRAW_COLORS.len());
    }

    #[test]
    fn full_monitor_selection_puts_the_bar_inside_the_bottom_edge() {
        let gfx = gfx();
        for (k, work, region) in [
            (1.0, IRect { x: 0, y: 0, w: 1920, h: 1040 }, IRect { x: 0, y: 0, w: 1920, h: 1080 }),
            (1.5, IRect { x: -2880, y: 0, w: 2880, h: 1560 }, IRect { x: -2880, y: 0, w: 2880, h: 1620 }),
        ] {
            let fonts = Fonts::new(&gfx, k).unwrap();
            let mut inp = inputs(region, None);
            inp.work = work;
            let l = layout(&inp, &fonts, k);
            assert!(l.rect.left >= work.x as f32 && l.rect.top >= work.y as f32);
            assert!(l.rect.right <= (work.x + work.w) as f32 && l.rect.bottom <= (work.y + work.h) as f32);
            assert!(l.rect.bottom >= (work.y + work.h) as f32 - 44.0 * k, "bar sits at the selection's bottom edge");
        }
    }

    #[test]
    fn dragged_bar_is_clamped_to_the_work_area() {
        let gfx = gfx();
        let fonts = Fonts::new(&gfx, 1.0).unwrap();
        let mut inp = inputs(IRect { x: 300, y: 200, w: 900, h: 400 }, None);
        inp.user_pos = Some((-500.0, 2000.0));
        let l = layout(&inp, &fonts, 1.0);
        assert!(l.rect.left >= 0.0 && l.rect.top >= 0.0);
        assert!(l.rect.right <= 1920.0 && l.rect.bottom <= 1040.0);
    }
}
