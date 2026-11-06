//! Annotation model and drawing engine, ported from the Noctalia 1.3.3
//! `_drawStrokeToCtx` and `eraseAt` canvas code.
//!
//! Geometry is stored in physical pixels relative to the selected region.
//! Sizes stay in the original logical units and are multiplied by the monitor
//! scale when drawn, so strokes look the same on any DPI.

use crate::gfx::{Gfx, rect, wide};
use crate::theme;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows_numerics::{Matrix3x2, Vector2};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Pencil,
    Highlighter,
    Line,
    Arrow,
    Rect,
    Circle,
    Text,
    Blur,
    Step,
    Ruler,
}

impl Tool {
    pub const ALL: [Tool; 10] = [
        Tool::Pencil,
        Tool::Highlighter,
        Tool::Line,
        Tool::Arrow,
        Tool::Rect,
        Tool::Circle,
        Tool::Text,
        Tool::Blur,
        Tool::Step,
        Tool::Ruler,
    ];

    pub fn parse(s: &str) -> Option<Tool> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "pencil" => Tool::Pencil,
            "highlighter" => Tool::Highlighter,
            "line" => Tool::Line,
            "arrow" => Tool::Arrow,
            "rect" => Tool::Rect,
            "circle" => Tool::Circle,
            "text" => Tool::Text,
            "blur" => Tool::Blur,
            "step" => Tool::Step,
            "ruler" => Tool::Ruler,
            _ => return None,
        })
    }

    pub fn icon(self) -> &'static str {
        match self {
            Tool::Pencil => "pencil",
            Tool::Highlighter => "highlight",
            Tool::Line => "slash",
            Tool::Arrow => "arrow-up-right",
            Tool::Rect => "square",
            Tool::Circle => "circle",
            Tool::Text => "text-size",
            Tool::Blur => "eye-off",
            Tool::Step => "number-123",
            Tool::Ruler => "ruler-2",
        }
    }

    pub fn tip(self) -> &'static str {
        match self {
            Tool::Pencil => "Pencil",
            Tool::Highlighter => "Highlighter",
            Tool::Line => "Line",
            Tool::Arrow => "Arrow",
            Tool::Rect => "Rectangle",
            Tool::Circle => "Circle",
            Tool::Text => "Text",
            Tool::Blur => "Blur",
            Tool::Step => "Numbered step",
            Tool::Ruler => "Ruler",
        }
    }

    /// Single-key shortcut, also shown in the toolbar tooltip.
    pub fn key(self) -> char {
        match self {
            Tool::Pencil => 'P',
            Tool::Highlighter => 'H',
            Tool::Line => 'L',
            Tool::Arrow => 'A',
            Tool::Rect => 'R',
            Tool::Circle => 'C',
            Tool::Text => 'T',
            Tool::Blur => 'B',
            Tool::Step => 'N',
            Tool::Ruler => 'M',
        }
    }

    pub fn from_key(c: char) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.key() == c)
    }

    /// Toolbar group: freehand, shapes, or markup.
    pub fn group(self) -> usize {
        Tool::GROUPS.iter().position(|g| g.contains(&self)).unwrap_or(0)
    }

    pub const GROUPS: [&'static [Tool]; 3] = [
        &[Tool::Pencil, Tool::Highlighter],
        &[Tool::Line, Tool::Arrow, Tool::Rect, Tool::Circle],
        &[Tool::Text, Tool::Blur, Tool::Step, Tool::Ruler],
    ];

    pub fn freehand(self) -> bool {
        matches!(self, Tool::Pencil | Tool::Highlighter)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Pt {
    pub x: f32,
    pub y: f32,
    pub w: f32,
}

#[derive(Clone, Debug)]
pub struct Stroke {
    pub tool: Tool,
    pub color: u32,
    pub size: f32,
    pub points: Vec<Pt>,
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
    pub text: String,
    pub step: u32,
}

impl Stroke {
    pub fn new(tool: Tool, color: u32, size: f32, x: f32, y: f32) -> Self {
        Self { tool, color, size, points: Vec::new(), x1: x, y1: y, x2: x, y2: y, text: String::new(), step: 0 }
    }

    pub fn bounds(&self) -> (f32, f32, f32, f32) {
        (self.x1.min(self.x2), self.y1.min(self.y2), (self.x2 - self.x1).abs(), (self.y2 - self.y1).abs())
    }
}

/// Renumbers step markers 1..n in stroke order.
pub fn reindex_steps(strokes: &mut [Stroke]) -> u32 {
    let mut n = 1;
    for s in strokes.iter_mut().filter(|s| s.tool == Tool::Step) {
        s.step = n;
        n += 1;
    }
    n
}

pub struct Painter<'a> {
    pub gfx: &'a Gfx,
    pub rt: &'a ID2D1RenderTarget,
    pub brush: &'a ID2D1SolidColorBrush,
    pub pixel: Option<&'a ID2D1Bitmap>,
    pub scale: f32,
    pub clip: Option<D2D_RECT_F>,
}

fn v(x: f32, y: f32) -> Vector2 {
    Vector2 { X: x, Y: y }
}

fn mid(a: &Pt, b: &Pt) -> (f32, f32) {
    ((a.x + b.x) / 2.0, (a.y + b.y) / 2.0)
}

pub fn text_font_size(size: f32, scale: f32) -> f32 {
    (size * 5.0 + 12.0) * scale
}

impl Painter<'_> {
    pub fn draw(&self, s: &Stroke) {
        let k = self.scale;
        let rt = self.rt;
        let color = theme::rgb(s.color);
        unsafe {
            self.brush.SetColor(&color);
            let width = s.size * k;
            match s.tool {
                Tool::Blur => {
                    let (x, y, w, h) = s.bounds();
                    if let (Some(pixel), true) = (self.pixel, w > 0.0 && h > 0.0) {
                        let r = rect(x, y, w, h);
                        rt.DrawBitmap(pixel, Some(&r), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR, Some(&r));
                    }
                }
                Tool::Pencil => self.pencil(&s.points),
                Tool::Highlighter if s.points.len() > 1 => {
                    let pts = &s.points;
                    let Ok(path) = self.gfx.d2d.CreatePathGeometry() else { return };
                    let Ok(sink) = path.Open() else { return };
                    sink.BeginFigure(v(pts[0].x, pts[0].y), D2D1_FIGURE_BEGIN_HOLLOW);
                    for j in 1..pts.len() - 1 {
                        let (mx, my) = mid(&pts[j], &pts[j + 1]);
                        sink.AddQuadraticBezier(&D2D1_QUADRATIC_BEZIER_SEGMENT {
                            point1: v(pts[j].x, pts[j].y),
                            point2: v(mx, my),
                        });
                    }
                    let last = pts[pts.len() - 1];
                    sink.AddLine(v(last.x, last.y));
                    sink.EndFigure(D2D1_FIGURE_END_OPEN);
                    let _ = sink.Close();
                    self.brush.SetColor(&theme::with_alpha(color, 0.35));
                    rt.DrawGeometry(&path, self.brush, width * 6.0, &self.gfx.square);
                }
                Tool::Line => {
                    rt.DrawLine(v(s.x1, s.y1), v(s.x2, s.y2), self.brush, width, &self.gfx.round);
                }
                Tool::Arrow => {
                    let (dx, dy) = (s.x2 - s.x1, s.y2 - s.y1);
                    let len = (dx * dx + dy * dy).sqrt();
                    if len < 2.0 * k {
                        return;
                    }
                    let angle = dy.atan2(dx);
                    let hs = (s.size * 3.5).max(10.0) * k;
                    let hw = std::f32::consts::PI / 5.0;
                    let base = (1.0 - hs / len).max(0.0);
                    rt.DrawLine(v(s.x1, s.y1), v(s.x1 + dx * base, s.y1 + dy * base), self.brush, width, &self.gfx.round);
                    let head = [
                        (s.x2, s.y2),
                        (s.x2 - hs * (angle - hw).cos(), s.y2 - hs * (angle - hw).sin()),
                        (s.x2 - hs * (angle + hw).cos(), s.y2 - hs * (angle + hw).sin()),
                    ];
                    self.fill_polygon(&head);
                }
                Tool::Rect => {
                    let (x, y, w, h) = s.bounds();
                    rt.DrawRectangle(&rect(x, y, w, h), self.brush, width, &self.gfx.round);
                }
                Tool::Circle => {
                    let (rx, ry) = ((s.x2 - s.x1).abs(), (s.y2 - s.y1).abs());
                    if rx > 0.0 || ry > 0.0 {
                        let e = D2D1_ELLIPSE { point: v(s.x1, s.y1), radiusX: rx.max(1.0), radiusY: ry.max(1.0) };
                        rt.DrawEllipse(&e, self.brush, width, &self.gfx.round);
                    }
                }
                Tool::Text => self.text(&s.text, s.x1, s.y1, text_font_size(s.size, k)),
                Tool::Step => {
                    let sr = (s.size * 2.0 + 8.0).max(12.0);
                    let font = (sr * 0.78).round().max(9.0) * k;
                    let sr = sr * k;
                    rt.FillEllipse(&D2D1_ELLIPSE { point: v(s.x1, s.y1), radiusX: sr, radiusY: sr }, self.brush);
                    if let Ok(fmt) = self.gfx.text_format(font, DWRITE_FONT_WEIGHT_BOLD, true) {
                        self.brush.SetColor(&theme::WHITE);
                        rt.DrawText(
                            &wide(&s.step.to_string()),
                            &fmt,
                            &rect(s.x1 - sr, s.y1 - sr, sr * 2.0, sr * 2.0),
                            self.brush,
                            D2D1_DRAW_TEXT_OPTIONS_NONE,
                            DWRITE_MEASURING_MODE_NATURAL,
                        );
                    }
                }
                Tool::Ruler => self.ruler(s),
                _ => {}
            }
        }
    }

    fn pencil(&self, pts: &[Pt]) {
        let k = self.scale;
        let rt = self.rt;
        unsafe {
            if pts.len() == 1 {
                let r = pts[0].w * k / 2.0;
                rt.FillEllipse(&D2D1_ELLIPSE { point: v(pts[0].x, pts[0].y), radiusX: r, radiusY: r }, self.brush);
                return;
            }
            for i in 1..pts.len() {
                let w = pts[i].w * k;
                if let Some(clip) = self.clip {
                    let part = &pts[i.saturating_sub(2)..=i];
                    let left = part.iter().map(|p| p.x).fold(f32::INFINITY, f32::min) - w;
                    let top = part.iter().map(|p| p.y).fold(f32::INFINITY, f32::min) - w;
                    let right = part.iter().map(|p| p.x).fold(f32::NEG_INFINITY, f32::max) + w;
                    let bottom = part.iter().map(|p| p.y).fold(f32::NEG_INFINITY, f32::max) + w;
                    if right < clip.left || bottom < clip.top || left > clip.right || top > clip.bottom {
                        continue;
                    }
                }
                if i == 1 {
                    rt.DrawLine(v(pts[0].x, pts[0].y), v(pts[1].x, pts[1].y), self.brush, w, &self.gfx.round);
                    continue;
                }
                // Quadratic from the previous midpoint, through the previous
                // point, to the current midpoint; flattened into short lines.
                let (sx, sy) = mid(&pts[i - 2], &pts[i - 1]);
                let (cx, cy) = (pts[i - 1].x, pts[i - 1].y);
                let (ex, ey) = mid(&pts[i - 1], &pts[i]);
                let span = ((cx - sx).hypot(cy - sy) + (ex - cx).hypot(ey - cy)) / k;
                let n = (span / 3.0).ceil().clamp(1.0, 8.0) as usize;
                let mut prev = v(sx, sy);
                for j in 1..=n {
                    let t = j as f32 / n as f32;
                    let u = 1.0 - t;
                    let p = v(u * u * sx + 2.0 * u * t * cx + t * t * ex, u * u * sy + 2.0 * u * t * cy + t * t * ey);
                    rt.DrawLine(prev, p, self.brush, w, &self.gfx.round);
                    prev = p;
                }
            }
            if pts.len() > 2 {
                let last = pts[pts.len() - 1];
                let (x, y) = mid(&pts[pts.len() - 2], &last);
                rt.DrawLine(v(x, y), v(last.x, last.y), self.brush, last.w * k, &self.gfx.round);
            }
        }
    }

    fn fill_polygon(&self, pts: &[(f32, f32)]) {
        unsafe {
            let Ok(path) = self.gfx.d2d.CreatePathGeometry() else { return };
            let Ok(sink) = path.Open() else { return };
            sink.BeginFigure(v(pts[0].0, pts[0].1), D2D1_FIGURE_BEGIN_FILLED);
            for p in &pts[1..] {
                sink.AddLine(v(p.0, p.1));
            }
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            let _ = sink.Close();
            self.rt.FillGeometry(&path, self.brush, None::<&ID2D1Brush>);
        }
    }

    /// Draws `text` with its first baseline at `y`, like canvas `fillText`.
    pub fn text(&self, text: &str, x: f32, y: f32, font: f32) {
        if text.is_empty() {
            return;
        }
        let Ok(fmt) = self.gfx.text_format(font, DWRITE_FONT_WEIGHT_BOLD, false) else { return };
        let Ok(layout) = self.gfx.layout(text, &fmt) else { return };
        let mut lines = [DWRITE_LINE_METRICS::default(); 1];
        let mut count = 0;
        unsafe {
            let _ = layout.GetLineMetrics(Some(&mut lines), &mut count);
            self.rt.DrawTextLayout(v(x, y - lines[0].baseline), &layout, self.brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
        }
    }

    fn ruler(&self, s: &Stroke) {
        let k = self.scale;
        let rt = self.rt;
        let (dx, dy) = (s.x2 - s.x1, s.y2 - s.y1);
        let len = (dx * dx + dy * dy).sqrt();
        if len < 4.0 * k {
            return;
        }
        let angle = dy.atan2(dx);
        let (px, py) = (-angle.sin(), angle.cos());
        let (cos, sin) = (angle.cos(), angle.sin());
        let tick = (s.size + 3.0).max(5.0) * k;
        let width = s.size * k;
        let label = format!(" {} px ", len.round() as i64);
        let Ok(fmt) = self.gfx.text_format(10.0 * k, DWRITE_FONT_WEIGHT_BOLD, true) else { return };
        let (tw, _) = self.gfx.measure(&label, &fmt);
        let (mx, my) = ((s.x1 + s.x2) / 2.0, (s.y1 + s.y2) / 2.0);
        let gap = tw / 2.0 + 2.0 * k;
        let split = len > tw + 20.0 * k;
        let style = &self.gfx.round;
        unsafe {
            if split {
                rt.DrawLine(v(s.x1, s.y1), v(mx - gap * cos, my - gap * sin), self.brush, width, style);
                rt.DrawLine(v(mx + gap * cos, my + gap * sin), v(s.x2, s.y2), self.brush, width, style);
            } else {
                rt.DrawLine(v(s.x1, s.y1), v(s.x2, s.y2), self.brush, width, style);
            }
            for (x, y) in [(s.x1, s.y1), (s.x2, s.y2)] {
                rt.DrawLine(v(x - px * tick, y - py * tick), v(x + px * tick, y + py * tick), self.brush, width, style);
            }
            if split {
                let mut old = Matrix3x2::default();
                rt.GetTransform(&mut old);
                let rot = Matrix3x2::rotation_around(angle.to_degrees(), v(mx, my));
                rt.SetTransform(&(rot * old));
                let h = 20.0 * k;
                rt.DrawText(
                    &wide(&label),
                    &fmt,
                    &rect(mx - tw, my - h / 2.0, tw * 2.0, h),
                    self.brush,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
                rt.SetTransform(&old);
            }
        }
    }
}

/// Index of the topmost stroke within the eraser radius of (ex, ey).
pub fn hit_test(strokes: &[Stroke], ex: f32, ey: f32, erase_size: f32, scale: f32) -> Option<usize> {
    let r = erase_size * 12.0 * scale;
    let r2 = r * r;
    let seg2 = |px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32| {
        let (sdx, sdy) = (bx - ax, by - ay);
        let l2 = sdx * sdx + sdy * sdy;
        if l2 == 0.0 {
            return (px - ax).powi(2) + (py - ay).powi(2);
        }
        let t = (((px - ax) * sdx + (py - ay) * sdy) / l2).clamp(0.0, 1.0);
        (ax + t * sdx - px).powi(2) + (ay + t * sdy - py).powi(2)
    };
    strokes.iter().rposition(|s| match s.tool {
        Tool::Pencil | Tool::Highlighter => {
            s.points.first().is_some_and(|p| (p.x - ex).powi(2) + (p.y - ey).powi(2) < r2)
                || s.points.windows(2).any(|p| seg2(ex, ey, p[0].x, p[0].y, p[1].x, p[1].y) < r2)
        }
        Tool::Line | Tool::Arrow | Tool::Ruler => seg2(ex, ey, s.x1, s.y1, s.x2, s.y2) < r2,
        Tool::Rect | Tool::Blur => {
            let (l, t, w, h) = s.bounds();
            let (rr, b) = (l + w, t + h);
            seg2(ex, ey, l, t, rr, t) < r2
                || seg2(ex, ey, rr, t, rr, b) < r2
                || seg2(ex, ey, rr, b, l, b) < r2
                || seg2(ex, ey, l, b, l, t) < r2
        }
        Tool::Circle => {
            let erx = (s.x2 - s.x1).abs().max(1.0);
            let ery = (s.y2 - s.y1).abs().max(1.0);
            let norm = (((ex - s.x1) / erx).powi(2) + ((ey - s.y1) / ery).powi(2)).sqrt();
            (norm - 1.0).abs() < r / erx.max(ery)
        }
        Tool::Text => (ex - s.x1).powi(2) + (ey - s.y1).powi(2) < r2 * 4.0,
        Tool::Step => {
            let sr = (s.size * 1.5 + 9.0).max(10.0) * scale;
            (ex - s.x1).powi(2) + (ey - s.y1).powi(2) < sr * sr * 1.5
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(x: f32) -> Stroke {
        Stroke { tool: Tool::Step, ..Stroke::new(Tool::Step, 0, 3.0, x, 0.0) }
    }

    #[test]
    fn steps_renumber_after_removal() {
        let mut s = vec![step(0.0), Stroke::new(Tool::Line, 0, 3.0, 0.0, 0.0), step(10.0), step(20.0)];
        assert_eq!(reindex_steps(&mut s), 4);
        s.remove(0);
        assert_eq!(reindex_steps(&mut s), 3);
        let numbers: Vec<u32> = s.iter().filter(|s| s.tool == Tool::Step).map(|s| s.step).collect();
        assert_eq!(numbers, vec![1, 2]);
    }

    #[test]
    fn eraser_hits_topmost_line() {
        let mut a = Stroke::new(Tool::Line, 0, 3.0, 0.0, 0.0);
        a.x2 = 100.0;
        let b = a.clone();
        assert_eq!(hit_test(&[a, b], 50.0, 5.0, 2.0, 1.0), Some(1));
        assert_eq!(hit_test(&[], 50.0, 5.0, 2.0, 1.0), None);
    }

    #[test]
    fn eraser_hits_between_sparse_freehand_samples() {
        let mut s = Stroke::new(Tool::Pencil, 0, 3.0, 0.0, 0.0);
        s.points = vec![Pt { x: 0.0, y: 0.0, w: 3.0 }, Pt { x: 100.0, y: 0.0, w: 3.0 }];
        assert_eq!(hit_test(&[s], 50.0, 0.0, 1.0, 1.0), Some(0));
    }
}
