//! Colors from the Caelestia Material 3 scheme the Linux annotator used.

use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;

pub const fn rgb(v: u32) -> D2D1_COLOR_F {
    rgba(v, 1.0)
}

pub const fn rgba(v: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: ((v >> 16) & 0xff) as f32 / 255.0, g: ((v >> 8) & 0xff) as f32 / 255.0, b: (v & 0xff) as f32 / 255.0, a }
}

pub const fn with_alpha(c: D2D1_COLOR_F, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { a, ..c }
}

pub const PRIMARY: D2D1_COLOR_F = rgb(0xc2c1ff);
pub const ON_PRIMARY: D2D1_COLOR_F = rgb(0x2a2a60);
pub const PRIMARY_CONTAINER: D2D1_COLOR_F = rgb(0x7171ac);
pub const SECONDARY: D2D1_COLOR_F = rgb(0xc6c4e0);
pub const ERROR: D2D1_COLOR_F = rgb(0xffb4ab);
pub const SURFACE: D2D1_COLOR_F = rgb(0x201f23);
pub const SURFACE_VARIANT: D2D1_COLOR_F = rgb(0x2a292e);
pub const ON_SURFACE: D2D1_COLOR_F = rgb(0xe5e1e7);
pub const ON_SURFACE_VARIANT: D2D1_COLOR_F = rgb(0xc8c5d1);
pub const HOVER: D2D1_COLOR_F = rgb(0x45455c);
pub const ON_HOVER: D2D1_COLOR_F = rgb(0xb4b2ce);
pub const CAPSULE_BORDER: D2D1_COLOR_F = rgb(0x21215f);
/// Saturated red for the record button and live indicator.
pub const RECORD: D2D1_COLOR_F = rgb(0xff5449);
pub const WHITE: D2D1_COLOR_F = rgb(0xffffff);
pub const BLACK: D2D1_COLOR_F = rgb(0x000000);

#[derive(Clone, Copy)]
pub struct Palette {
    pub surface: D2D1_COLOR_F,
    pub surface_variant: D2D1_COLOR_F,
    pub on_surface: D2D1_COLOR_F,
    pub on_surface_variant: D2D1_COLOR_F,
    pub primary: D2D1_COLOR_F,
    pub on_primary: D2D1_COLOR_F,
    pub primary_container: D2D1_COLOR_F,
    pub secondary: D2D1_COLOR_F,
    pub hover: D2D1_COLOR_F,
    pub on_hover: D2D1_COLOR_F,
    pub outline: D2D1_COLOR_F,
    pub error: D2D1_COLOR_F,
    pub record: D2D1_COLOR_F,
    pub radius_small: f32,
    pub radius_normal: f32,
    pub radius_large: f32,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            surface: SURFACE,
            surface_variant: SURFACE_VARIANT,
            on_surface: ON_SURFACE,
            on_surface_variant: ON_SURFACE_VARIANT,
            primary: PRIMARY,
            on_primary: ON_PRIMARY,
            primary_container: PRIMARY_CONTAINER,
            secondary: SECONDARY,
            hover: HOVER,
            on_hover: ON_HOVER,
            outline: CAPSULE_BORDER,
            error: ERROR,
            record: RECORD,
            radius_small: 8.0,
            radius_normal: 9.0,
            radius_large: 20.0,
        }
    }
}

/// Colors for the editor toolbar's translucent surfaces.
#[derive(Clone, Copy)]
pub struct Hud {
    /// Tint laid over the blurred capture.
    pub tint: D2D1_COLOR_F,
    /// Opaque tint, used where a ring must cut into the surface.
    pub solid: D2D1_COLOR_F,
    pub edge: D2D1_COLOR_F,
    pub highlight: D2D1_COLOR_F,
    pub fg: D2D1_COLOR_F,
    pub fg2: D2D1_COLOR_F,
    pub hover: D2D1_COLOR_F,
    pub divider: D2D1_COLOR_F,
    pub selected: D2D1_COLOR_F,
    pub on_selected: D2D1_COLOR_F,
    pub action: D2D1_COLOR_F,
    pub on_action: D2D1_COLOR_F,
    pub track: D2D1_COLOR_F,
    pub thumb: D2D1_COLOR_F,
    pub on_thumb: D2D1_COLOR_F,
    pub danger: D2D1_COLOR_F,
    pub danger_fill: D2D1_COLOR_F,
    pub shadow: f32,
    pub radius_small: f32,
    pub radius_large: f32,
    pub radius_tooltip: f32,
}

pub const HUD_DARK: Hud = Hud {
    tint: rgba(0x26262a, 0.7),
    solid: rgb(0x262629),
    edge: rgba(0xffffff, 0.11),
    highlight: rgba(0xffffff, 0.07),
    fg: rgb(0xf2f2f5),
    fg2: rgba(0xebebf5, 0.55),
    hover: rgba(0xffffff, 0.08),
    divider: rgba(0xffffff, 0.12),
    selected: rgba(0xc2c1ff, 0.26),
    on_selected: rgb(0xdedcff),
    action: PRIMARY,
    on_action: rgb(0x23234f),
    track: rgba(0xffffff, 0.07),
    thumb: rgba(0xffffff, 0.17),
    on_thumb: rgb(0xf2f2f5),
    danger: rgb(0xff9a90),
    danger_fill: rgba(0xff6e64, 0.15),
    shadow: 1.0,
    radius_small: 8.0,
    radius_large: 14.0,
    radius_tooltip: 7.0,
};

pub const HUD_LIGHT: Hud = Hud {
    tint: rgba(0xfafafc, 0.74),
    solid: rgb(0xfafafc),
    edge: rgba(0x000000, 0.09),
    highlight: rgba(0xffffff, 0.8),
    fg: rgb(0x1d1d1f),
    fg2: rgba(0x1d1d1f, 0.5),
    hover: rgba(0x000000, 0.05),
    divider: rgba(0x000000, 0.1),
    selected: rgba(0x5e5ce6, 0.14),
    on_selected: rgb(0x4644cc),
    action: rgb(0x5e5ce6),
    on_action: WHITE,
    track: rgba(0x000000, 0.055),
    thumb: WHITE,
    on_thumb: rgb(0x1d1d1f),
    danger: rgb(0xd4382d),
    danger_fill: rgba(0xd4382d, 0.1),
    shadow: 0.55,
    radius_small: 8.0,
    radius_large: 14.0,
    radius_tooltip: 7.0,
};

/// Annotation palette, in the order the toolbar popover shows it.
pub const DRAW_COLORS: [u32; 9] = [0xFF4444, 0xFF8C00, 0xFFD700, 0x44FF88, 0x44AAFF, 0xCC44FF, 0xFF44CC, 0xFFFFFF, 0x000000];

/// Line sizes offered by the popover: (size, label).
pub const DRAW_SIZES: [(f32, &str); 3] = [(2.0, "S"), (4.0, "M"), (7.0, "L")];

/// Resolves the `toolbar_theme` setting; "auto" follows the Windows app theme.
pub fn hud(setting: &str) -> &'static Hud {
    use windows::Win32::System::Registry::*;
    use windows::core::w;
    let light = match setting.trim().to_ascii_lowercase().as_str() {
        "light" => true,
        "dark" => false,
        _ => {
            let mut value = 0u32;
            let mut len = 4u32;
            let found = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
                    w!("AppsUseLightTheme"),
                    RRF_RT_REG_DWORD,
                    None,
                    Some((&mut value as *mut u32).cast()),
                    Some(&mut len),
                )
            };
            found == windows::Win32::Foundation::ERROR_SUCCESS && value != 0
        }
    };
    if light { &HUD_LIGHT } else { &HUD_DARK }
}

pub fn parse_hex(s: &str) -> Option<u32> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    u32::from_str_radix(s, 16).ok()
}
