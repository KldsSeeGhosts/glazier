//! OLE copy drag with real files, PNG, DIB and the shell drag-image helper.

use crate::{export, gfx::Gfx};
use std::path::Path;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Ole::*;
use windows::Win32::System::SystemServices::*;
use windows::Win32::UI::Shell::*;
use windows::core::*;

#[implement(IDropSource)]
struct Source;

impl IDropSource_Impl for Source_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            DRAGDROP_S_CANCEL
        } else if !keys.contains(MK_LBUTTON) {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

unsafe fn set_bytes(data: &IDataObject, format: u16, bytes: &[u8]) -> Result<()> {
    unsafe {
        let fmt = FORMATETC {
            cfFormat: format,
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
            ..Default::default()
        };
        let mut medium = STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: export::global_copy(&[bytes])? },
            ..Default::default()
        };
        if let Err(e) = data.SetData(&fmt, &medium, true) {
            ReleaseStgMedium(&mut medium);
            return Err(e);
        }
        Ok(())
    }
}

fn file_drop(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    let header = DROPFILES { pFiles: size_of::<DROPFILES>() as u32, fWide: TRUE, ..Default::default() };
    let mut bytes =
        unsafe { std::slice::from_raw_parts(&header as *const _ as *const u8, size_of::<DROPFILES>()).to_vec() };
    for c in path.as_os_str().encode_wide().chain([0, 0]) {
        bytes.extend_from_slice(&c.to_le_bytes());
    }
    bytes
}

pub fn start(gfx: &Gfx, path: &Path, preview: &export::Image) -> Result<()> {
    let path = std::path::absolute(path).map_err(|e| Error::new(E_FAIL, e.to_string()))?;
    let video = path.extension().is_some_and(|s| s.eq_ignore_ascii_case("mp4"));
    unsafe {
        // The shell object supports auxiliary formats that the drag-image helper
        // sets, and duplicates media correctly when a target requests them.
        let data: IDataObject = SHCreateDataObject(None, None, None::<&IDataObject>)?;
        set_bytes(&data, CF_HDROP.0, &file_drop(&path))?;
        if !video {
            let png = std::fs::read(&path).map_err(|e| Error::new(E_FAIL, e.to_string()))?;
            let image = export::decode_png_bytes(gfx, &png)?;
            set_bytes(&data, RegisterClipboardFormatW(w!("PNG")) as u16, &png)?;
            set_bytes(&data, CF_DIB.0, &export::dib_bytes(&image))?;
        }
        set_bytes(
            &data,
            RegisterClipboardFormatW(w!("Preferred DropEffect")) as u16,
            &DROPEFFECT_COPY.0.to_le_bytes(),
        )?;
        let helper: Result<IDragSourceHelper> = CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER);
        if let Ok(helper) = helper {
            let bitmap = bitmap(preview)?;
            let info = SHDRAGIMAGE {
                sizeDragImage: SIZE { cx: preview.width as i32, cy: preview.height as i32 },
                ptOffset: POINT { x: preview.width as i32 / 2, y: preview.height as i32 / 2 },
                hbmpDragImage: bitmap,
                crColorKey: COLORREF(0xffffffff),
            };
            if helper.InitializeFromBitmap(&info, &data).is_err() {
                let _ = DeleteObject(bitmap.into());
            }
            // On success the helper owns the bitmap.
        }
        let source: IDropSource = Source.into();
        let mut effect = DROPEFFECT_NONE;
        let result = DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect);
        result.ok()
    }
}

pub fn copy_file(path: &Path) -> Result<()> {
    unsafe {
        let data: IDataObject = SHCreateDataObject(None, None, None::<&IDataObject>)?;
        set_bytes(&data, CF_HDROP.0, &file_drop(path))?;
        set_bytes(
            &data,
            RegisterClipboardFormatW(w!("Preferred DropEffect")) as u16,
            &DROPEFFECT_COPY.0.to_le_bytes(),
        )?;
        OleSetClipboard(&data)?;
        OleFlushClipboard()
    }
}

pub fn bitmap(image: &export::Image) -> Result<HBITMAP> {
    unsafe {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: image.width as i32,
                biHeight: -(image.height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
        std::ptr::copy_nonoverlapping(image.pixels.as_ptr(), bits.cast(), image.pixels.len());
        Ok(bitmap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdrop_is_unicode_and_double_terminated() {
        let bytes = file_drop(Path::new("C:\\shots\\日本.png"));
        let header = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<DROPFILES>()) };
        assert_eq!(header.pFiles as usize, size_of::<DROPFILES>());
        assert!(header.fWide.as_bool());
        assert_eq!(&bytes[bytes.len() - 4..], &[0; 4]);
    }

    #[test]
    fn shell_data_object_enumerates_and_copies_our_media() {
        unsafe {
            OleInitialize(None).unwrap();
            {
                let data: IDataObject = SHCreateDataObject(None, None, None::<&IDataObject>).unwrap();
                for format in [CF_HDROP.0, CF_DIB.0, RegisterClipboardFormatW(w!("PNG")) as u16] {
                    set_bytes(&data, format, b"test payload").unwrap();
                    let fmt = FORMATETC {
                        cfFormat: format,
                        dwAspect: DVASPECT_CONTENT.0,
                        lindex: -1,
                        tymed: TYMED_HGLOBAL.0 as u32,
                        ..Default::default()
                    };
                    data.QueryGetData(&fmt).ok().unwrap();
                    let mut medium = data.GetData(&fmt).unwrap();
                    let ptr = windows::Win32::System::Memory::GlobalLock(medium.u.hGlobal);
                    assert_eq!(std::slice::from_raw_parts(ptr.cast::<u8>(), 12), b"test payload");
                    let _ = windows::Win32::System::Memory::GlobalUnlock(medium.u.hGlobal);
                    ReleaseStgMedium(&mut medium);
                }
                let formats = data.EnumFormatEtc(DATADIR_GET.0 as u32).unwrap();
                let mut count = 0;
                let mut fmt = [FORMATETC::default()];
                while formats.Next(&mut fmt, None) == S_OK {
                    count += 1;
                }
                assert!(count >= 3);
            }
            OleUninitialize();
        }
    }
}
