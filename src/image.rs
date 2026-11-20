//! Image files via the Windows Imaging Component (PNG, JPEG, BMP, GIF, TIFF,
//! plus WebP/HEIC when the OS codecs are installed), and the "choose image" dialog.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{GENERIC_READ, HWND};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::*;
use windows::core::{Interface, PCWSTR};

use crate::util::{Ctx, Res, wide};

pub struct Image {
    pub width: u32,
    pub height: u32,
    /// 32-bit BGRA rows, tightly packed.
    pub pixels: Vec<u8>,
}

/// Decodes the first frame of an image, scaled down to fit `max_side` if larger.
/// `premultiplied` selects PBGRA (what Direct2D wants) instead of straight BGRA.
pub fn load(path: &Path, max_side: u32, premultiplied: bool) -> Res<Image> {
    unsafe {
        // Works from any thread (export runs on its own).
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let f: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ctx("WIC")?;
        let w = wide(&path.to_string_lossy());
        let dec = f
            .CreateDecoderFromFilename(PCWSTR(w.as_ptr()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)
            .ctx("Unsupported or unreadable image")?;
        let frame = dec.GetFrame(0).ctx("image frame")?;
        let (mut iw, mut ih) = (0u32, 0u32);
        frame.GetSize(&mut iw, &mut ih).ctx("image size")?;
        if iw == 0 || ih == 0 {
            return Err("empty image".into());
        }
        let k = (max_side as f64 / iw.max(ih) as f64).min(1.0);
        let (tw, th) = (((iw as f64 * k).round() as u32).max(1), ((ih as f64 * k).round() as u32).max(1));
        let src: IWICBitmapSource = if (tw, th) != (iw, ih) {
            let sc = f.CreateBitmapScaler().ctx("scaler")?;
            sc.Initialize(&frame, tw, th, WICBitmapInterpolationModeHighQualityCubic).ctx("scale")?;
            sc.cast().ctx("cast")?
        } else {
            frame.cast().ctx("cast")?
        };
        let conv = f.CreateFormatConverter().ctx("converter")?;
        let fmt = if premultiplied { GUID_WICPixelFormat32bppPBGRA } else { GUID_WICPixelFormat32bppBGRA };
        conv.Initialize(&src, &fmt, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom).ctx("convert")?;
        let mut pixels = vec![0u8; (tw * th * 4) as usize];
        conv.CopyPixels(std::ptr::null(), tw * 4, &mut pixels).ctx("decode")?;
        Ok(Image { width: tw, height: th, pixels })
    }
}

/// Shows the Open dialog for images. Blocks; call it from a worker thread.
pub fn pick(owner: isize) -> Option<PathBuf> {
    unsafe {
        // The common item dialog wants a single-threaded apartment.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let (n1, s1) = (wide("Images"), wide("*.png;*.jpg;*.jpeg;*.bmp;*.gif;*.tif;*.tiff;*.webp;*.heic"));
        let (n2, s2) = (wide("All files"), wide("*.*"));
        let specs = [
            COMDLG_FILTERSPEC { pszName: PCWSTR(n1.as_ptr()), pszSpec: PCWSTR(s1.as_ptr()) },
            COMDLG_FILTERSPEC { pszName: PCWSTR(n2.as_ptr()), pszSpec: PCWSTR(s2.as_ptr()) },
        ];
        let _ = dlg.SetFileTypes(&specs);
        let title = wide("Choose a background image");
        let _ = dlg.SetTitle(PCWSTR(title.as_ptr()));
        dlg.Show(Some(HWND(owner as _))).ok()?; // Err = cancelled
        let item = dlg.GetResult().ok()?;
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as _));
        s.map(PathBuf::from)
    }
}

/// Copies a chosen image into the project folder (projects stay self-contained)
/// and returns the stored file name. Unique names keep undo history valid.
pub fn import_into(dir: &Path, src: &Path) -> Res<String> {
    // Validate before copying.
    load(src, 64, false)?;
    let ext = src.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_else(|| "png".into());
    let name = format!("background-{}.{}", crate::util::stamp(), ext);
    std::fs::copy(src, dir.join(&name)).ctx("copy image")?;
    Ok(name)
}
