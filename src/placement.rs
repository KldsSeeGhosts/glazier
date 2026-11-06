//! Places floating controls around a selection, inside a monitor's work area.

use crate::capture::IRect;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub x: f32,
    pub y: f32,
    /// The surface overlaps the selection because neither side had room.
    pub inside: bool,
}

/// Clamps one axis to `[low, high]`, using the work area's hard edge when the
/// margin-preferred range is too small, and the low edge when the surface is
/// larger than the work area.
fn axis(value: f32, low: f32, high: f32, hard_low: f32, hard_high: f32) -> f32 {
    if high >= low {
        value.clamp(low, high)
    } else if hard_high >= hard_low {
        value.clamp(hard_low, hard_high)
    } else {
        hard_low
    }
}

/// Keeps a `w` by `h` surface inside `work`, preferring `margin` on each edge.
pub fn clamp(work: IRect, x: f32, y: f32, w: f32, h: f32, margin: f32) -> (f32, f32) {
    let (left, top) = (work.x as f32, work.y as f32);
    let (right, bottom) = (left + work.w as f32, top + work.h as f32);
    (
        axis(x, left + margin, right - margin - w, left, right - w),
        axis(y, top + margin, bottom - margin - h, top, bottom - h),
    )
}

/// Places a `w` by `h` surface around `region` in `work`.
///
/// Below the selection when it fits, then above, and otherwise just inside the
/// selection's bottom edge. The result is clamped to the work area so a
/// toolbar stays visible next to a full-monitor or multi-monitor selection.
pub fn controls(work: IRect, region: IRect, w: f32, h: f32, gap: f32, margin: f32) -> Placement {
    let region_left = region.x as f32;
    let region_top = region.y as f32;
    let region_right = region_left + region.w as f32;
    let region_bottom = region_top + region.h as f32;
    let (left, top) = (work.x as f32, work.y as f32);
    let (right, bottom) = (left + work.w as f32, top + work.h as f32);
    let x = axis(
        region_left + (region_right - region_left) / 2.0 - w / 2.0,
        left + margin,
        right - margin - w,
        left,
        right - w,
    );
    let below = region_bottom + gap;
    let above = region_top - gap - h;
    let (y, inside) = if below + h <= bottom - margin {
        (below, false)
    } else if above >= top + margin {
        (above, false)
    } else {
        (region_bottom - gap - h, true)
    };
    // A selection can extend beyond the work area (for example into a top or
    // bottom taskbar), so every branch needs the final clamp.
    let y = axis(y, top + margin, bottom - margin - h, top, bottom - h);
    Placement { x, y, inside }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_inside(p: Placement, work: IRect, w: f32, h: f32) {
        assert!(p.x >= work.x as f32, "left edge outside work area: {p:?}");
        assert!(p.y >= work.y as f32, "top edge outside work area: {p:?}");
        assert!(p.x + w <= (work.x + work.w) as f32 + 0.01, "right edge outside work area: {p:?}");
        assert!(p.y + h <= (work.y + work.h) as f32 + 0.01, "bottom edge outside work area: {p:?}");
    }

    #[test]
    fn prefers_below_then_above() {
        let work = IRect { x: 0, y: 0, w: 1920, h: 1080 };
        let below = controls(work, IRect { x: 300, y: 200, w: 500, h: 300 }, 620.0, 44.0, 10.0, 6.0);
        assert!(!below.inside);
        assert_eq!((below.x, below.y), (240.0, 510.0));

        let above = controls(work, IRect { x: 300, y: 700, w: 500, h: 350 }, 620.0, 44.0, 10.0, 6.0);
        assert!(!above.inside);
        assert_eq!((above.x, above.y), (240.0, 646.0));
        assert_inside(above, work, 620.0, 44.0);
    }

    #[test]
    fn full_monitor_selection_goes_inside_above_the_work_area_bottom() {
        let work = IRect { x: 0, y: 0, w: 1920, h: 1040 };
        let p = controls(work, IRect { x: 0, y: 0, w: 1920, h: 1080 }, 620.0, 44.0, 10.0, 6.0);
        assert!(p.inside);
        assert_eq!((p.x, p.y), (650.0, 990.0));
        assert_inside(p, work, 620.0, 44.0);
    }

    #[test]
    fn below_placement_stays_inside_a_work_area_below_the_selection_top() {
        // A top taskbar starts at y=48 while the selection starts at y=30.
        let work = IRect { x: 0, y: 48, w: 1920, h: 992 };
        let p = controls(work, IRect { x: 100, y: 30, w: 200, h: 5 }, 100.0, 44.0, 10.0, 6.0);
        assert!(!p.inside);
        assert_eq!(p.y, 54.0, "below branch must be clamped to the work area top");
        assert_inside(p, work, 100.0, 44.0);
    }

    #[test]
    fn above_placement_stays_inside_a_work_area_above_the_selection_bottom() {
        // The selection runs five pixels past the bottom of the work area.
        let work = IRect { x: 0, y: 0, w: 1920, h: 1040 };
        let p = controls(work, IRect { x: 100, y: 1045, w: 200, h: 5 }, 100.0, 44.0, 10.0, 6.0);
        assert!(!p.inside);
        assert_eq!(p.y, 990.0, "above branch must be clamped to the work area bottom");
        assert_inside(p, work, 100.0, 44.0);
    }

    #[test]
    fn clamps_offset_and_negative_work_areas() {
        let work = IRect { x: -1920, y: 48, w: 1920, h: 1032 };
        let p = controls(work, work, 1600.0, 66.0, 12.0, 9.0);
        assert_eq!((p.x, p.y), (-1760.0, 1002.0));
        assert_inside(p, work, 1600.0, 66.0);
    }

    #[test]
    fn mixed_dpi_sizes_stay_on_screen() {
        for k in [1.0, 1.25, 1.5, 1.75, 2.0] {
            let work = IRect { x: 2560, y: 0, w: (2560.0 * k) as i32, h: (1400.0 * k) as i32 };
            let region = IRect { x: work.x, y: work.y, w: work.w, h: work.h };
            let (w, h) = (620.0 * k, 44.0 * k);
            let p = controls(work, region, w, h, 10.0 * k, 6.0 * k);
            assert!(p.inside, "a full selection has no outside room at {k}x");
            assert_inside(p, work, w, h);
        }
    }

    #[test]
    fn spanning_selection_uses_the_work_area_of_its_monitor() {
        let work = IRect { x: 0, y: 0, w: 1920, h: 1040 };
        let p = controls(work, IRect { x: -500, y: 300, w: 3000, h: 300 }, 700.0, 44.0, 10.0, 6.0);
        assert!(!p.inside);
        assert_eq!((p.x, p.y), (650.0, 610.0));
        assert_inside(p, work, 700.0, 44.0);

        // The selection extends past the bottom of the work area.
        let tall = IRect { x: -300, y: -200, w: 2500, h: 1600 };
        let p = controls(work, tall, 700.0, 44.0, 10.0, 6.0);
        assert!(p.inside);
        assert_inside(p, work, 700.0, 44.0);
    }

    #[test]
    fn oversized_surface_starts_at_the_work_area_edge() {
        let work = IRect { x: 100, y: 100, w: 200, h: 200 };
        let p = controls(work, work, 300.0, 300.0, 10.0, 6.0);
        assert_eq!((p.x, p.y), (100.0, 100.0));
    }
}
