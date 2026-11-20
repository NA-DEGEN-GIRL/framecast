//! App icon, drawn with the same Direct2D code as the UI and written as a
//! multi-size .ico (`framecast --make-icon assets/framecast.ico`). The build
//! script embeds that file into the exe; this only runs when redesigning it.
//!
//! Design: a violet→pink squircle (the app's accent colors) with white
//! "focus" corner brackets around a cursor — recording + auto zoom.

use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows_numerics::{Matrix3x2, Vector2};

use crate::d2d::{D2d, rect, rgba, rrect};
use crate::gpu::Gpu;
use crate::util::{Ctx, Res, wide};

const SIZES: [u32; 9] = [16, 20, 24, 32, 40, 48, 64, 128, 256];

fn draw(d2d: &D2d, s: f32) -> Res<()> {
    let dc = &d2d.dc;
    unsafe {
        dc.Clear(Some(&rgba(0, 0.0)));
        // Background squircle with a diagonal gradient.
        let m = if s <= 24.0 { 0.0 } else { s * 0.04 };
        let bg = rect(m, m, s - m, s - m);
        let stops = [
            D2D1_GRADIENT_STOP { position: 0.0, color: rgba(0x6E48FF, 1.0) },
            D2D1_GRADIENT_STOP { position: 0.55, color: rgba(0xB14FD8, 1.0) },
            D2D1_GRADIENT_STOP { position: 1.0, color: rgba(0xFF5F8F, 1.0) },
        ];
        let coll = ID2D1RenderTarget::CreateGradientStopCollection(dc, &stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP).ctx("stops")?;
        let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: Vector2 { X: 0.0, Y: 0.0 }, endPoint: Vector2 { X: s, Y: s } };
        let grad = dc.CreateLinearGradientBrush(&props, None, &coll).ctx("gradient")?;
        dc.FillRoundedRectangle(&rrect(bg, s * 0.23), &grad);

        // Focus brackets (skipped at tiny sizes where they'd turn to mush).
        let white = rgba(0xffffff, 1.0);
        if s >= 32.0 {
            let (a, b) = (s * 0.21, s * 0.79);
            let t = (s * 0.06).max(2.0);
            let l = s * 0.15;
            for (x, y, dx, dy) in [(a, a, 1.0, 1.0), (b, a, -1.0, 1.0), (a, b, 1.0, -1.0), (b, b, -1.0, -1.0)] {
                let h = rect(x.min(x + dx * l), y.min(y + dy * t), x.max(x + dx * l), y.max(y + dy * t));
                let v = rect(x.min(x + dx * t), y.min(y + dy * l), x.max(x + dx * t), y.max(y + dy * l));
                dc.FillRoundedRectangle(&rrect(h, t * 0.5), d2d.brush(white));
                dc.FillRoundedRectangle(&rrect(v, t * 0.5), d2d.brush(white));
            }
        }

        // Cursor (same arrow the recorder draws), with a soft shadow.
        let arrow = d2d.polygon(&[(0.0, 0.0), (0.0, 17.5), (4.2, 13.6), (7.0, 20.0), (9.8, 18.8), (7.1, 12.6), (12.6, 12.6)])?;
        let (k, tx, ty) = if s >= 32.0 { (s * 0.43 / 20.0, s * 0.42, s * 0.33) } else { (s * 0.66 / 20.0, s * 0.32, s * 0.15) };
        let sh = (s * 0.012).max(0.5);
        dc.SetTransform(&(Matrix3x2::scale(k, k) * Matrix3x2::translation(tx + sh, ty + sh * 1.5)));
        dc.FillGeometry(&arrow, d2d.brush(rgba(0x2A0F4F, 0.22)), None);
        dc.SetTransform(&(Matrix3x2::scale(k, k) * Matrix3x2::translation(tx, ty)));
        dc.FillGeometry(&arrow, d2d.brush(white), None);
        dc.DrawGeometry(&arrow, d2d.brush(rgba(0x2A0F4F, 0.55)), (1.0 / k).min(1.6), None);
        dc.SetTransform(&Matrix3x2::identity());
    }
    Ok(())
}

/// Renders one size and returns straight-alpha BGRA rows (top-down).
fn render(gpu: &Gpu, d2d: &D2d, s: u32) -> Res<Vec<u8>> {
    let tex = gpu.texture(s, s, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE, false)?;
    let target = d2d.target_for(&tex)?;
    unsafe {
        d2d.dc.SetTarget(&target);
        d2d.dc.BeginDraw();
        draw(d2d, s as f32)?;
        d2d.dc.EndDraw(None, None).ctx("EndDraw")?;
        d2d.dc.SetTarget(None);
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: s,
        Height: s,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        ..Default::default()
    };
    let mut st = None;
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut st)) }.ctx("staging")?;
    let st = st.ok_or("staging")?;
    let mut out = vec![0u8; (s * s * 4) as usize];
    unsafe {
        gpu.ctx.CopyResource(&st, &tex);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        gpu.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).ctx("Map")?;
        for y in 0..s as usize {
            let src = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize), s as usize * 4);
            out[y * s as usize * 4..(y + 1) * s as usize * 4].copy_from_slice(src);
        }
        gpu.ctx.Unmap(&st, 0);
    }
    // Direct2D renders premultiplied alpha; icons want straight alpha.
    for p in out.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    Ok(out)
}

/// 32-bit DIB icon image (for small sizes).
fn dib(px: &[u8], s: u32) -> Vec<u8> {
    let mut v = Vec::new();
    for x in [40u32, s, s * 2] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&32u16.to_le_bytes());
    v.extend_from_slice(&[0u8; 24]);
    for y in (0..s as usize).rev() {
        v.extend_from_slice(&px[y * s as usize * 4..(y + 1) * s as usize * 4]);
    }
    let mask_row = (s as usize).div_ceil(32) * 4;
    v.extend(std::iter::repeat_n(0u8, mask_row * s as usize));
    v
}

/// PNG via the Windows Imaging Component (for 128/256, keeps the exe small).
fn png(px: &[u8], s: u32, tmp: &std::path::Path) -> Res<Vec<u8>> {
    unsafe {
        let f: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ctx("WIC")?;
        let stream = f.CreateStream().ctx("stream")?;
        let w = wide(&tmp.to_string_lossy());
        stream.InitializeFromFilename(windows::core::PCWSTR(w.as_ptr()), windows::Win32::Foundation::GENERIC_WRITE.0).ctx("png file")?;
        let enc = f.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null()).ctx("png encoder")?;
        enc.Initialize(&stream, WICBitmapEncoderNoCache).ctx("init")?;
        let mut frame = None;
        let mut opts = None;
        enc.CreateNewFrame(&mut frame, &mut opts).ctx("frame")?;
        let frame = frame.ok_or("frame")?;
        frame.Initialize(opts.as_ref()).ctx("frame init")?;
        frame.SetSize(s, s).ctx("size")?;
        let mut fmt = GUID_WICPixelFormat32bppBGRA;
        frame.SetPixelFormat(&mut fmt).ctx("format")?;
        frame.WritePixels(s, s * 4, px).ctx("pixels")?;
        frame.Commit().ctx("commit")?;
        enc.Commit().ctx("commit")?;
    }
    let bytes = std::fs::read(tmp).ctx("read png")?;
    let _ = std::fs::remove_file(tmp);
    Ok(bytes)
}

pub fn make(out: &std::path::Path) -> Res<()> {
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).ctx("create icon folder")?;
    }
    let gpu = Gpu::new()?;
    let d2d = D2d::new(&gpu)?;
    let mut images = Vec::new();
    for &s in &SIZES {
        let px = render(&gpu, &d2d, s)?;
        let data = if s >= 128 { png(&px, s, &out.with_extension(format!("{s}.png")))? } else { dib(&px, s) };
        images.push((s, data));
    }
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]);
    ico.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (s, data) in &images {
        let b = if *s >= 256 { 0 } else { *s as u8 };
        ico.extend_from_slice(&[b, b, 0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&32u16.to_le_bytes());
        ico.extend_from_slice(&(data.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in &images {
        ico.extend_from_slice(data);
    }
    std::fs::write(out, ico).ctx("write ico")?;
    // Also a large preview PNG for docs / store pages.
    let big = render(&gpu, &d2d, 512)?;
    let preview = png(&big, 512, &out.with_extension("png"))?;
    std::fs::write(out.with_extension("png"), preview).ctx("write png")?;
    Ok(())
}
