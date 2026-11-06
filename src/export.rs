//! Flattening, PNG encoding, clipboard and file output.

use crate::config::Config;
use crate::gfx::Gfx;
use crate::strokes::{Painter, Stroke};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::Storage::FileSystem::MoveFileW;
use windows::Win32::System::Com::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::CF_DIB;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Shell::*;
use windows::core::*;

pub struct Image {
    pub width: u32,
    pub height: u32,
    /// Top-down BGRA, opaque.
    pub pixels: Vec<u8>,
}

fn image_buffer(width: u32, height: u32) -> Result<Vec<u8>> {
    let len = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| Error::new(E_OUTOFMEMORY, "Image exceeds WIC's buffer size limit"))? as usize;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(len).map_err(|_| Error::from(E_OUTOFMEMORY))?;
    pixels.resize(len, 0);
    Ok(pixels)
}

pub fn bitmap_props() -> D2D1_BITMAP_PROPERTIES {
    D2D1_BITMAP_PROPERTIES {
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
        dpiX: 96.0,
        dpiY: 96.0,
    }
}

pub fn target_props() -> D2D1_RENDER_TARGET_PROPERTIES {
    D2D1_RENDER_TARGET_PROPERTIES {
        r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
        pixelFormat: bitmap_props().pixelFormat,
        dpiX: 96.0,
        dpiY: 96.0,
        usage: D2D1_RENDER_TARGET_USAGE_NONE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
    }
}

/// Renders strokes over `base` exactly as the editor shows them.
pub fn flatten(gfx: &Gfx, base: &Image, strokes: &[Stroke], scale: f32, pixel: Option<&[u8]>) -> Result<Image> {
    let (w, h) = (base.width, base.height);
    unsafe {
        let target = gfx.wic.CreateBitmap(w, h, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad)?;
        let rt = gfx.d2d.CreateWicBitmapRenderTarget(&target, &target_props())?;
        rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        let size = D2D_SIZE_U { width: w, height: h };
        let base_bmp = rt.CreateBitmap(size, Some(base.pixels.as_ptr().cast()), w * 4, &bitmap_props())?;
        let pixel_bmp = match pixel {
            Some(p) => Some(rt.CreateBitmap(size, Some(p.as_ptr().cast()), w * 4, &bitmap_props())?),
            None => None,
        };
        let brush = rt.CreateSolidColorBrush(&crate::theme::WHITE, None)?;
        rt.BeginDraw();
        rt.DrawBitmap(&base_bmp, None, 1.0, D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR, None);
        let painter = Painter { gfx, rt: &rt, brush: &brush, pixel: pixel_bmp.as_ref(), scale, clip: None };
        for s in strokes {
            painter.draw(s);
        }
        rt.EndDraw(None, None)?;
        let mut pixels = image_buffer(w, h)?;
        target.CopyPixels(std::ptr::null(), w * 4, &mut pixels)?;
        Ok(Image { width: w, height: h, pixels })
    }
}

/// High-quality resize, used for the magnifier export.
pub fn resize(gfx: &Gfx, img: &Image, width: u32, height: u32) -> Result<Image> {
    unsafe {
        let mut pixels = image_buffer(width, height)?;
        let src = source(gfx, img)?;
        let scaler = gfx.wic.CreateBitmapScaler()?;
        scaler.Initialize(&src, width, height, WICBitmapInterpolationModeHighQualityCubic)?;
        scaler.CopyPixels(std::ptr::null(), width * 4, &mut pixels)?;
        Ok(Image { width, height, pixels })
    }
}

unsafe fn source(gfx: &Gfx, img: &Image) -> Result<IWICBitmap> {
    unsafe {
        gfx.wic.CreateBitmapFromMemory(img.width, img.height, &GUID_WICPixelFormat32bppBGRA, img.width * 4, &img.pixels)
    }
}

pub fn encode_png(gfx: &Gfx, img: &Image) -> Result<Vec<u8>> {
    unsafe {
        let src = source(gfx, img)?;
        let converter = gfx.wic.CreateFormatConverter()?;
        converter.Initialize(
            &src,
            &GUID_WICPixelFormat24bppBGR,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let stream = SHCreateMemStream(None).ok_or_else(|| Error::from(E_OUTOFMEMORY))?;
        let encoder = gfx.wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut frame = None;
        let mut options = None;
        encoder.CreateNewFrame(&mut frame, &mut options)?;
        let frame = frame.ok_or_else(|| Error::from(E_FAIL))?;
        frame.Initialize(options.as_ref())?;
        frame.SetSize(img.width, img.height)?;
        let mut format = GUID_WICPixelFormat24bppBGR;
        frame.SetPixelFormat(&mut format)?;
        frame.WriteSource(&converter, std::ptr::null())?;
        frame.Commit()?;
        encoder.Commit()?;

        let mut stat = STATSTG::default();
        stream.Stat(&mut stat, STATFLAG_NONAME)?;
        stream.Seek(0, STREAM_SEEK_SET, None)?;
        let mut data = vec![0u8; stat.cbSize as usize];
        let mut read = 0u32;
        stream.Read(data.as_mut_ptr().cast(), data.len() as u32, Some(&mut read)).ok()?;
        data.truncate(read as usize);
        Ok(data)
    }
}

pub(crate) unsafe fn global_copy(parts: &[&[u8]]) -> Result<HGLOBAL> {
    unsafe {
        let len: usize = parts.iter().map(|p| p.len()).sum();
        let mem = GlobalAlloc(GMEM_MOVEABLE, len)?;
        let dst = GlobalLock(mem) as *mut u8;
        if dst.is_null() {
            let _ = GlobalFree(Some(mem));
            return Err(Error::from(E_OUTOFMEMORY));
        }
        let mut off = 0;
        for p in parts {
            std::ptr::copy_nonoverlapping(p.as_ptr(), dst.add(off), p.len());
            off += p.len();
        }
        let _ = GlobalUnlock(mem);
        Ok(mem)
    }
}

pub fn decode_png(gfx: &Gfx, path: &Path) -> Result<Image> {
    let png = std::fs::read(path).map_err(|e| Error::new(E_FAIL, e.to_string()))?;
    decode_png_bytes(gfx, &png)
}

pub fn decode_png_bytes(gfx: &Gfx, png: &[u8]) -> Result<Image> {
    unsafe {
        let stream = gfx.wic.CreateStream()?;
        stream.InitializeFromMemory(png)?;
        let decoder = gfx.wic.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnLoad)?;
        let frame = decoder.GetFrame(0)?;
        let (mut width, mut height) = (0, 0);
        frame.GetSize(&mut width, &mut height)?;
        let converter = gfx.wic.CreateFormatConverter()?;
        converter.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let mut pixels = image_buffer(width, height)?;
        converter.CopyPixels(std::ptr::null(), width * 4, &mut pixels)?;
        Ok(Image { width, height, pixels })
    }
}

pub fn dib_bytes(img: &Image) -> Vec<u8> {
    let header = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: img.width as i32,
        biHeight: img.height as i32,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: img.width * img.height * 4,
        ..Default::default()
    };
    let mut data =
        unsafe { std::slice::from_raw_parts(&header as *const _ as *const u8, size_of::<BITMAPINFOHEADER>()).to_vec() };
    for row in img.pixels.chunks_exact(img.width as usize * 4).rev() {
        data.extend_from_slice(row);
    }
    data
}

struct ClipboardMemory(HGLOBAL);

impl ClipboardMemory {
    unsafe fn give(self, format: u32) -> Result<()> {
        unsafe {
            SetClipboardData(format, Some(HANDLE(self.0.0)))?;
        }
        std::mem::forget(self);
        Ok(())
    }
}

impl Drop for ClipboardMemory {
    fn drop(&mut self) {
        unsafe {
            let _ = GlobalFree(Some(self.0));
        }
    }
}

/// Places the image on the clipboard as PNG and as a DIB for older apps.
pub fn copy_to_clipboard(owner: HWND, img: &Image, png: &[u8]) -> Result<()> {
    let header = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: img.width as i32,
        biHeight: img.height as i32,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: img.width * img.height * 4,
        ..Default::default()
    };
    let header_bytes =
        unsafe { std::slice::from_raw_parts(&header as *const _ as *const u8, size_of::<BITMAPINFOHEADER>()) };
    let stride = (img.width * 4) as usize;
    unsafe {
        let png_format = RegisterClipboardFormatW(w!("PNG"));
        if png_format == 0 {
            return Err(Error::from_thread());
        }
        // Prepare both allocations before clearing the existing clipboard.
        let png_mem = ClipboardMemory(global_copy(&[png])?);
        let parts: Vec<_> = std::iter::once(header_bytes).chain(img.pixels.chunks_exact(stride).rev()).collect();
        let dib_mem = ClipboardMemory(global_copy(&parts)?);
        let mut opened = OpenClipboard(Some(owner));
        for _ in 0..10 {
            if opened.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
            opened = OpenClipboard(Some(owner));
        }
        opened?;
        let result = (|| {
            EmptyClipboard()?;
            png_mem.give(png_format)?;
            dib_mem.give(CF_DIB.0 as u32)?;
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let path = p.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(p.0 as *const _));
        path
    }
}

pub fn expand(path: &str) -> PathBuf {
    let mut out = String::new();
    let mut rest = path.trim();
    if let Some(tail) = rest.strip_prefix('~') {
        out.push_str(&std::env::var("USERPROFILE").unwrap_or_default());
        rest = tail;
    }
    let parts: Vec<&str> = rest.split('%').collect();
    out.push_str(parts[0]);
    let mut i = 1;
    while i < parts.len() {
        let name = parts[i];
        if i + 1 == parts.len() {
            out.push('%');
            out.push_str(name);
            break;
        }
        match std::env::var(name) {
            Ok(v) if !name.is_empty() => out.push_str(&v),
            _ => out.push_str(&format!("%{name}%")),
        }
        out.push_str(parts[i + 1]);
        i += 2;
    }
    PathBuf::from(out)
}

/// Folder for saved annotations and whether it may be created.
pub fn save_dir(cfg: &Config) -> Option<(PathBuf, bool)> {
    if !cfg.save_directory.trim().is_empty() {
        return Some((expand(&cfg.save_directory), true));
    }
    [known_folder(&FOLDERID_Screenshots), known_folder(&FOLDERID_Pictures)]
        .into_iter()
        .flatten()
        .find(|p| p.is_dir())
        .map(|p| (p, false))
}

pub fn file_stem(format: &str) -> String {
    let t = unsafe { GetLocalTime() };
    let fmt = if format.trim().is_empty() { "annotate-%Y-%m-%d_%H-%M-%S" } else { format.trim() };
    let stem = fmt
        .replace("%Y", &format!("{:04}", t.wYear))
        .replace("%m", &format!("{:02}", t.wMonth))
        .replace("%d", &format!("{:02}", t.wDay))
        .replace("%H", &format!("{:02}", t.wHour))
        .replace("%M", &format!("{:02}", t.wMinute))
        .replace("%S", &format!("{:02}", t.wSecond));
    let clean: String = stem.chars().map(|c| if "\\/:*?\"<>|\r\n\0".contains(c) { '_' } else { c }).collect();
    let clean = clean.trim().trim_end_matches('.').to_string();
    if clean.is_empty() { file_stem("") } else { clean }
}

/// Writes atomically without replacing an existing file.
pub fn save_png(cfg: &Config, png: &[u8]) -> std::result::Result<PathBuf, String> {
    let (dir, create) = save_dir(cfg).ok_or("No Screenshots or Pictures folder was found")?;
    if create {
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let stem = file_stem(&cfg.filename_format);
    let mut dest = dir.join(format!("{stem}.png"));
    let mut n = 2;
    while dest.exists() {
        dest = dir.join(format!("{stem} ({n}).png"));
        n += 1;
    }
    static TEMP_ID: AtomicU64 = AtomicU64::new(0);
    let tmp =
        dir.join(format!(".{stem}.{}-{}.glazier-tmp", std::process::id(), TEMP_ID.fetch_add(1, Ordering::Relaxed)));
    write_then_rename(&tmp, &dest, png).map_err(|e| format!("{}: {e}", dest.display()))?;
    Ok(dest)
}

fn write_then_rename(tmp: &Path, dest: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(tmp)?;
    let result = file.write_all(data);
    drop(file);
    result
        .and_then(|_| unsafe {
            // std::fs::rename replaces an existing destination on Windows.
            MoveFileW(&HSTRING::from(tmp.as_os_str()), &HSTRING::from(dest.as_os_str()))
                .map_err(|e| std::io::Error::from_raw_os_error((e.code().0 as u32 & 0xffff) as i32))
        })
        .inspect_err(|_| {
            let _ = std::fs::remove_file(tmp);
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_are_safe() {
        let s = file_stem("shot %Y/%m:%d");
        assert!(!s.contains('/') && !s.contains(':'));
        assert!(file_stem("").starts_with("annotate-"));
        assert!(file_stem("...").starts_with("annotate-"));
    }

    #[test]
    fn expands_variables() {
        let profile = std::env::var("USERPROFILE").unwrap();
        assert_eq!(expand("%USERPROFILE%\\shots"), PathBuf::from(profile).join("shots"));
        assert_eq!(expand("100%"), PathBuf::from("100%"));
    }

    #[test]
    fn oversized_resize_is_rejected_before_allocation() {
        assert_eq!(image_buffer(20, 10).unwrap().len(), 800);
        assert!(image_buffer(u32::MAX, 2).is_err());
    }

    #[test]
    fn save_does_not_replace_existing_file() {
        let dir = std::env::temp_dir().join(format!("glazier-save-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (tmp, dest) = (dir.join("new.tmp"), dir.join("existing.png"));
        std::fs::write(&dest, b"original").unwrap();
        assert!(write_then_rename(&tmp, &dest, b"replacement").is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"original");
        assert!(!tmp.exists());
        std::fs::remove_file(&dest).unwrap();
        write_then_rename(&tmp, &dest, b"new").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        std::fs::remove_file(&dest).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn annotations_flatten_and_png_preserves_pixels() {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
            {
                let gfx = Gfx::new().unwrap();
                let base = Image { width: 16, height: 16, pixels: vec![255; 16 * 16 * 4] };
                let mut line = Stroke::new(crate::strokes::Tool::Line, 0xff0000, 3.0, 2.0, 8.0);
                line.x2 = 14.0;
                let image = flatten(&gfx, &base, &[line], 1.0, None).unwrap();
                assert_eq!(&image.pixels[(8 * 16 + 8) * 4..(8 * 16 + 9) * 4], &[0, 0, 255, 255]);
                assert_eq!(&image.pixels[..4], &[255; 4]);
                let png = encode_png(&gfx, &image).unwrap();
                let decoded = decode_png_bytes(&gfx, &png).unwrap();
                assert_eq!((decoded.width, decoded.height), (image.width, image.height));
                assert_eq!(decoded.pixels, image.pixels);
            }
            CoUninitialize();
        }
    }
}
