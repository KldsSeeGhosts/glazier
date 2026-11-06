//! Shared Direct2D, DirectWrite and WIC factories plus the Tabler icon font.

use std::cell::RefCell;
use windows::Win32::Foundation::{E_INVALIDARG, FALSE};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;
use windows::core::*;

static ICON_FONT: &[u8] = include_bytes!("../assets/tabler-icons.ttf");

pub struct Gfx {
    pub d2d: ID2D1Factory1,
    pub dwrite: IDWriteFactory5,
    pub wic: IWICImagingFactory,
    icons: IDWriteFontCollection1,
    icon_family: HSTRING,
    hud_family: HSTRING,
    theme: RefCell<Option<crate::theme_tokens::ThemeTokens>>,
    theme_font: RefCell<Option<(HSTRING, Option<IDWriteFontCollection1>)>>,
    pub round: ID2D1StrokeStyle,
    pub square: ID2D1StrokeStyle,
    _loader: IDWriteInMemoryFontFileLoader,
    formats: RefCell<Vec<(u32, i32, bool, IDWriteTextFormat)>>,
    layouts: RefCell<Vec<(String, IDWriteTextFormat, IDWriteTextLayout)>>,
}

pub const UI_FONT: &str = "Segoe UI";
/// The Windows 11 system face, used by the editor toolbar when installed.
const HUD_FONT: &str = "Segoe UI Variable Text";

impl Gfx {
    pub fn new() -> Result<Self> {
        unsafe {
            let d2d: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory5 = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;

            let loader = dwrite.CreateInMemoryFontFileLoader()?;
            dwrite.RegisterFontFileLoader(&loader)?;
            let file = loader.CreateInMemoryFontFileReference(
                &dwrite,
                ICON_FONT.as_ptr().cast(),
                ICON_FONT.len() as u32,
                None::<&IUnknown>,
            )?;
            let builder: IDWriteFontSetBuilder1 = dwrite.CreateFontSetBuilder()?;
            builder.AddFontFile(&file)?;
            let set = builder.CreateFontSet()?;
            let icons = dwrite.CreateFontCollectionFromFontSet(&set)?;
            let names = icons.GetFontFamily(0)?.GetFamilyNames()?;
            let len = names.GetStringLength(0)? as usize;
            let mut buf = vec![0u16; len + 1];
            names.GetString(0, &mut buf)?;
            let icon_family = HSTRING::from_wide(&buf[..len]);

            let mut system = None;
            dwrite.GetSystemFontCollection(false, &mut system, false)?;
            let (mut index, mut exists) = (0, FALSE);
            if let Some(system) = &system {
                system.FindFamilyName(&HSTRING::from(HUD_FONT), &mut index, &mut exists)?;
            }
            let hud_family = HSTRING::from(if exists.as_bool() { HUD_FONT } else { UI_FONT });

            let stroke = |cap: D2D1_CAP_STYLE| {
                ID2D1Factory::CreateStrokeStyle(
                    &d2d,
                    &D2D1_STROKE_STYLE_PROPERTIES {
                        startCap: cap,
                        endCap: cap,
                        dashCap: cap,
                        lineJoin: D2D1_LINE_JOIN_ROUND,
                        miterLimit: 10.0,
                        dashStyle: D2D1_DASH_STYLE_SOLID,
                        dashOffset: 0.0,
                    },
                    None,
                )
            };
            let round = stroke(D2D1_CAP_STYLE_ROUND)?;
            let square = stroke(D2D1_CAP_STYLE_SQUARE)?;

            Ok(Self {
                d2d,
                dwrite,
                wic,
                icons,
                icon_family,
                hud_family,
                theme: RefCell::new(None),
                theme_font: RefCell::new(None),
                round,
                square,
                _loader: loader,
                formats: RefCell::new(Vec::new()),
                layouts: RefCell::new(Vec::new()),
            })
        }
    }

    /// Refresh only UI appearance; annotation text retains its original font.
    pub fn reload_theme(&self, path: &str) {
        let mut theme = crate::theme_tokens::ThemeTokens::load(path);
        let mut font = None;
        if let Some(tokens) = &theme {
            let family = HSTRING::from(&tokens.font.family);
            let collection = if let Some(path) = &tokens.font.file {
                let result = unsafe {
                    (|| -> Result<IDWriteFontCollection1> {
                        let file = self.dwrite.CreateFontFileReference(&HSTRING::from(path), None)?;
                        let builder: IDWriteFontSetBuilder1 = self.dwrite.CreateFontSetBuilder()?;
                        builder.AddFontFile(&file)?;
                        let collection = self.dwrite.CreateFontCollectionFromFontSet(&builder.CreateFontSet()?)?;
                        let (mut index, mut found) = (0, FALSE);
                        collection.FindFamilyName(&family, &mut index, &mut found)?;
                        if !found.as_bool() {
                            return Err(Error::from(E_INVALIDARG));
                        }
                        Ok(collection)
                    })()
                };
                match result {
                    Ok(collection) => Some(collection),
                    Err(_) => {
                        theme = None;
                        None
                    }
                }
            } else {
                None
            };
            if theme.is_some() {
                font = Some((family, collection));
            }
        }
        *self.theme.borrow_mut() = theme;
        *self.theme_font.borrow_mut() = font;
        self.clear_text_cache();
    }

    pub fn theme(&self) -> Option<crate::theme_tokens::ThemeTokens> {
        self.theme.borrow().clone()
    }

    pub fn text_format(&self, size: f32, weight: DWRITE_FONT_WEIGHT, centered: bool) -> Result<IDWriteTextFormat> {
        if let Some((_, _, _, format)) =
            self.formats.borrow().iter().find(|(s, w, c, _)| *s == size.to_bits() && *w == weight.0 && *c == centered)
        {
            return Ok(format.clone());
        }
        unsafe {
            let f = self.dwrite.CreateTextFormat(
                &HSTRING::from(UI_FONT),
                None,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!(""),
            )?;
            f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            if centered {
                f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
                f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            }
            let mut formats = self.formats.borrow_mut();
            if formats.len() == 32 {
                formats.remove(0);
            }
            formats.push((size.to_bits(), weight.0, centered, f.clone()));
            Ok(f)
        }
    }

    /// Toolbar text in the system face. `centered` also centers vertically.
    pub fn hud_format(&self, size: f32, weight: DWRITE_FONT_WEIGHT, centered: bool) -> Result<IDWriteTextFormat> {
        unsafe {
            let theme_font = self.theme_font.borrow();
            let (family, collection) = match theme_font.as_ref() {
                Some((family, collection)) => (family, collection.as_ref().map(|c| &**c)),
                None => (&self.hud_family, None),
            };
            let f = self.dwrite.CreateTextFormat(
                family,
                collection,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!(""),
            )?;
            f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            if centered {
                f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            }
            Ok(f)
        }
    }

    pub fn icon_format(&self, size: f32) -> Result<IDWriteTextFormat> {
        unsafe {
            let f = self.dwrite.CreateTextFormat(
                &self.icon_family,
                &self.icons,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!(""),
            )?;
            f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            Ok(f)
        }
    }

    pub fn layout(&self, text: &str, format: &IDWriteTextFormat) -> Result<IDWriteTextLayout> {
        if let Some((_, _, layout)) = self.layouts.borrow().iter().find(|(s, f, _)| s == text && f == format) {
            return Ok(layout.clone());
        }
        let wide: Vec<u16> = text.encode_utf16().collect();
        let layout = unsafe { self.dwrite.CreateTextLayout(&wide, format, 100_000.0, 100_000.0)? };
        let mut layouts = self.layouts.borrow_mut();
        if layouts.len() == 64 {
            layouts.remove(0);
        }
        layouts.push((text.to_owned(), format.clone(), layout.clone()));
        Ok(layout)
    }

    pub fn clear_text_cache(&self) {
        self.layouts.borrow_mut().clear();
        self.formats.borrow_mut().clear();
    }

    /// Width of `text` and the distance from the layout top to its first baseline.
    pub fn measure(&self, text: &str, format: &IDWriteTextFormat) -> (f32, f32) {
        let Ok(layout) = self.layout(text, format) else {
            return (0.0, 0.0);
        };
        unsafe {
            let mut m = DWRITE_TEXT_METRICS::default();
            let _ = layout.GetMetrics(&mut m);
            let mut lines = [DWRITE_LINE_METRICS::default(); 1];
            let mut count = 0;
            let _ = layout.GetLineMetrics(Some(&mut lines), &mut count);
            (m.widthIncludingTrailingWhitespace, lines[0].baseline)
        }
    }
}

impl Drop for Gfx {
    fn drop(&mut self) {
        unsafe {
            let _ = self.dwrite.UnregisterFontFileLoader(&self._loader);
        }
    }
}

pub fn rect(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h }
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Tabler glyphs used by the Noctalia toolbar.
pub fn icon(name: &str) -> &'static str {
    match name {
        "arrow-back-up" => "\u{eb77}",
        "arrow-forward-up" => "\u{eb78}",
        "arrow-up-right" => "\u{ea24}",
        "capture" => "\u{ec3c}",
        "check" => "\u{ea5e}",
        "chevron-down" => "\u{ea5f}",
        "circle" => "\u{ea6b}",
        "copy" => "\u{ea7a}",
        "corner-up-left" => "\u{ea82}",
        "corner-up-right" => "\u{ea83}",
        "device-floppy" => "\u{eb62}",
        "dots" => "\u{ea95}",
        "download" => "\u{ea96}",
        "eye-off" => "\u{ecf0}",
        "folder" => "\u{eaad}",
        "highlight" => "\u{ef3f}",
        "microphone" => "\u{eaf0}",
        "microphone-off" => "\u{ed16}",
        "minus" => "\u{eaf2}",
        "number-123" => "\u{f554}",
        "pencil" => "\u{eb04}",
        "plus" => "\u{eb0b}",
        "player-pause-filled" => "\u{f690}",
        "player-play-filled" => "\u{f691}",
        "player-record-filled" => "\u{f692}",
        "player-stop-filled" => "\u{f695}",
        "refresh" => "\u{eb13}",
        "ruler-2" => "\u{eed2}",
        "slash" => "\u{f4f9}",
        "square" => "\u{eb2c}",
        "text-size" => "\u{f2b1}",
        "trash" => "\u{eb41}",
        "volume" => "\u{eb51}",
        "volume-off" => "\u{f1c3}",
        "x" => "\u{eb55}",
        "zoom-in" => "\u{eb56}",
        "zoom-out" => "\u{eb57}",
        _ => "\u{ea05}",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ui_font_and_invalid_file_fallback() {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
        }
        {
            let gfx = Gfx::new().unwrap();
            let dir = std::env::temp_dir().join(format!("glazier-font-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let font = dir.join("icons.ttf");
            let json = dir.join("theme.json");
            std::fs::write(&font, ICON_FONT).unwrap();
            let mut data: serde_json::Value = serde_json::from_str(crate::theme_tokens::tests::VALID).unwrap();
            data["font"]["family"] = gfx.icon_family.to_string_lossy().into();
            data["font"]["file"] = font.to_string_lossy().to_string().into();
            std::fs::write(&json, serde_json::to_vec(&data).unwrap()).unwrap();
            gfx.reload_theme(json.to_str().unwrap());
            assert!(gfx.theme().is_some());
            assert!(gfx.hud_format(12.0, DWRITE_FONT_WEIGHT_NORMAL, false).is_ok());
            data["colours"]["primary"] = "#abcdef".into();
            std::fs::write(&json, serde_json::to_vec(&data).unwrap()).unwrap();
            gfx.reload_theme(json.to_str().unwrap());
            assert_eq!(gfx.theme().unwrap().colours.primary, "#abcdef");
            data["font"]["file"] = dir.join("missing.ttf").to_string_lossy().to_string().into();
            std::fs::write(&json, serde_json::to_vec(&data).unwrap()).unwrap();
            gfx.reload_theme(json.to_str().unwrap());
            assert!(gfx.theme().is_none());
            assert!(gfx.hud_format(12.0, DWRITE_FONT_WEIGHT_NORMAL, false).is_ok());
        }
        unsafe {
            CoUninitialize();
        }
    }
}
