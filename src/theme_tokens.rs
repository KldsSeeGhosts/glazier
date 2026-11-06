//! Portable appearance tokens supplied by an external shell.

use serde::Deserialize;
use std::io::Read;

const MAX_THEME_BYTES: u64 = 64 * 1024;

// Reject UNC, device/namespace and drive-relative paths before touching the
// filesystem. Also reject mapped network drives, even though they have a
// drive-letter prefix.
fn local_path(path: &str) -> bool {
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows::core::HSTRING;

    let bytes = path.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || !matches!(bytes[2], b'\\' | b'/')
        || path[3..].chars().any(|c| c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        || path[3..].split(['\\', '/']).any(reserved_device_name)
    {
        return false;
    }
    let root = HSTRING::from(format!("{}:\\", bytes[0] as char));
    local_drive_type(unsafe { GetDriveTypeW(&root) })
}

fn local_drive_type(drive_type: u32) -> bool {
    use windows::Win32::System::WindowsProgramming::{DRIVE_CDROM, DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOVABLE};
    matches!(drive_type, DRIVE_FIXED | DRIVE_REMOVABLE | DRIVE_CDROM | DRIVE_RAMDISK)
}

fn reserved_device_name(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or("").trim_end_matches(' ').to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$") {
        return true;
    }
    ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix)
            .is_some_and(|n| matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "\u{00b9}" | "\u{00b2}" | "\u{00b3}"))
    })
}

fn read_bounded(reader: impl Read) -> Option<String> {
    let mut bytes = Vec::new();
    reader.take(MAX_THEME_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_THEME_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Colours {
    pub surface: String,
    pub surface_container_low: String,
    pub surface_container: String,
    pub surface_container_high: String,
    pub surface_container_highest: String,
    pub on_surface: String,
    pub on_surface_variant: String,
    pub primary: String,
    pub on_primary: String,
    pub primary_container: String,
    pub secondary_container: String,
    pub outline: String,
    pub error: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Font {
    pub family: String,
    pub file: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Radii {
    pub small: f32,
    pub normal: f32,
    pub large: f32,
    pub full: f32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ThemeTokens {
    pub version: u32,
    pub mode: String,
    pub colours: Colours,
    pub font: Font,
    pub radii: Radii,
}

impl ThemeTokens {
    pub fn parse(text: &str) -> Option<Self> {
        let tokens: Self = serde_json::from_str(text).ok()?;
        let c = &tokens.colours;
        let colours = [
            &c.surface,
            &c.surface_container_low,
            &c.surface_container,
            &c.surface_container_high,
            &c.surface_container_highest,
            &c.on_surface,
            &c.on_surface_variant,
            &c.primary,
            &c.on_primary,
            &c.primary_container,
            &c.secondary_container,
            &c.outline,
            &c.error,
        ];
        let radii = [tokens.radii.small, tokens.radii.normal, tokens.radii.large];
        if tokens.version != 1
            || !matches!(tokens.mode.as_str(), "light" | "dark")
            || colours.iter().any(|c| parse_colour(c).is_none())
            || tokens.font.family.trim().is_empty()
            || tokens.font.file.as_ref().is_some_and(|p| !local_path(p))
            || radii.iter().any(|r| !r.is_finite() || *r < 0.0 || *r > 4096.0)
            || !tokens.radii.full.is_finite()
            || tokens.radii.full < 0.0
            || tokens.radii.full > 2_147_483_647.0
        {
            return None;
        }
        Some(tokens)
    }

    pub fn load(path: &str) -> Option<Self> {
        if !local_path(path) {
            return None;
        }
        let file = std::fs::File::open(path).ok()?;
        if file.metadata().ok()?.len() > MAX_THEME_BYTES {
            return None;
        }
        Self::parse(&read_bounded(file)?)
    }

    /// Resolve semantic roles into the editor's existing HUD fields.
    pub fn hud(&self) -> crate::theme::Hud {
        use crate::theme::{Hud, rgb, with_alpha};
        let colour = |s: &str| rgb(parse_colour(s).expect("validated theme colour"));
        let c = &self.colours;
        let surface = colour(&c.surface_container);
        let foreground = colour(&c.on_surface);
        let variant = colour(&c.on_surface_variant);
        let primary = colour(&c.primary);
        let outline = colour(&c.outline);
        let error = colour(&c.error);
        Hud {
            tint: with_alpha(surface, 0.88),
            solid: surface,
            edge: with_alpha(outline, 0.6),
            highlight: with_alpha(foreground, 0.07),
            fg: foreground,
            fg2: variant,
            hover: with_alpha(colour(&c.surface_container_highest), 0.8),
            divider: with_alpha(outline, 0.5),
            selected: colour(&c.secondary_container),
            on_selected: foreground,
            action: primary,
            on_action: colour(&c.on_primary),
            track: colour(&c.surface_container_high),
            thumb: primary,
            on_thumb: colour(&c.on_primary),
            danger: error,
            danger_fill: with_alpha(error, 0.15),
            shadow: if self.mode == "light" { 0.55 } else { 1.0 },
            radius_small: self.radii.small,
            radius_large: self.radii.large,
            radius_tooltip: self.radii.small,
        }
    }

    pub fn palette(&self) -> crate::theme::Palette {
        use crate::theme::{Palette, rgb};
        let colour = |s: &str| rgb(parse_colour(s).expect("validated theme colour"));
        let c = &self.colours;
        Palette {
            surface: colour(&c.surface_container),
            surface_variant: colour(&c.surface_container_high),
            on_surface: colour(&c.on_surface),
            on_surface_variant: colour(&c.on_surface_variant),
            primary: colour(&c.primary),
            on_primary: colour(&c.on_primary),
            primary_container: colour(&c.primary_container),
            secondary: colour(&c.secondary_container),
            hover: colour(&c.surface_container_highest),
            on_hover: colour(&c.on_surface),
            outline: colour(&c.outline),
            error: colour(&c.error),
            record: colour(&c.error),
            radius_small: self.radii.small,
            radius_normal: self.radii.normal,
            radius_large: self.radii.large,
        }
    }
}

pub fn parse_colour(s: &str) -> Option<u32> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const VALID: &str = r##"{
        "version": 1, "mode": "dark",
        "colours": {
            "surface": "#010203", "surfaceContainerLow": "#020304",
            "surfaceContainer": "#030405", "surfaceContainerHigh": "#040506",
            "surfaceContainerHighest": "#050607", "onSurface": "#f0f1f2",
            "onSurfaceVariant": "#e0e1e2", "primary": "#123456",
            "onPrimary": "#654321", "primaryContainer": "#112233",
            "secondaryContainer": "#334455", "outline": "#556677", "error": "#ff0000"
        },
        "font": { "family": "Test", "file": null },
        "radii": { "small": 8, "normal": 12, "large": 16, "full": 2147483647 }
    }"##;

    #[test]
    fn parses_schema_and_colours() {
        let theme = ThemeTokens::parse(VALID).unwrap();
        assert_eq!(parse_colour(&theme.colours.primary), Some(0x123456));
        assert_eq!(parse_colour("abcdef"), Some(0xabcdef));
        assert_eq!(theme.radii.normal, 12.0);
        let hud = theme.hud();
        assert_eq!(hud.action, crate::theme::rgb(0x123456));
        assert_eq!(hud.on_action, crate::theme::rgb(0x654321));
        assert_eq!(hud.solid, crate::theme::rgb(0x030405));
    }

    #[test]
    fn invalid_tokens_fall_back() {
        assert!(ThemeTokens::load("").is_none());
        assert!(ThemeTokens::load("relative.json").is_none());
        assert!(ThemeTokens::parse(&VALID.replace("\"version\": 1", "\"version\": 2")).is_none());
        assert!(ThemeTokens::parse(&VALID.replace("#123456", "bad")).is_none());
        assert!(ThemeTokens::parse(&VALID.replace("\"dark\"", "\"other\"")).is_none());
        assert!(ThemeTokens::parse(&VALID.replace("\"small\": 8", "\"small\": -1")).is_none());
    }

    #[test]
    fn rejects_network_device_and_nonlocal_paths() {
        use windows::Win32::System::WindowsProgramming::{DRIVE_FIXED, DRIVE_REMOTE, DRIVE_UNKNOWN};
        assert!(local_drive_type(DRIVE_FIXED));
        assert!(!local_drive_type(DRIVE_REMOTE));
        assert!(!local_drive_type(DRIVE_UNKNOWN));
        for path in [
            r"\\server\share\font.ttf",
            r"//server/share/font.ttf",
            r"\\?\C:\font.ttf",
            r"\\?\UNC\server\share\font.ttf",
            r"\\.\C:\font.ttf",
            r"\??\C:\font.ttf",
            r"C:font.ttf",
            r"\font.ttf",
            r"C:\NUL.ttf",
            r"C:\COM1\font.ttf",
            r"C:\theme.json:stream",
        ] {
            assert!(!local_path(path), "accepted {path}");
            let mut data: serde_json::Value = serde_json::from_str(VALID).unwrap();
            data["font"]["file"] = path.into();
            assert!(ThemeTokens::parse(&data.to_string()).is_none(), "accepted font {path}");
        }
        assert!(local_path(&std::env::temp_dir().join("theme.json").to_string_lossy()));
    }

    #[test]
    fn limits_file_size_and_reads_after_growth() {
        let dir = std::env::temp_dir().join(format!("glazier-theme-size-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("theme.json");
        let mut text = VALID.to_string();
        text.extend(std::iter::repeat_n(' ', MAX_THEME_BYTES as usize - text.len()));
        std::fs::write(&path, &text).unwrap();
        assert!(ThemeTokens::load(path.to_str().unwrap()).is_some());
        text.push(' ');
        std::fs::write(&path, &text).unwrap();
        assert!(ThemeTokens::load(path.to_str().unwrap()).is_none());
        // A file that grows after metadata was checked is still bounded.
        let count = std::cell::Cell::new(0);
        struct Growing<'a>(&'a std::cell::Cell<usize>);
        impl Read for Growing<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                buf.fill(b' ');
                self.0.set(self.0.get() + buf.len());
                Ok(buf.len())
            }
        }
        assert!(read_bounded(Growing(&count)).is_none());
        assert_eq!(count.get(), MAX_THEME_BYTES as usize + 1);
    }
}
