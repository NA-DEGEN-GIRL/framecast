//! Direct2D / DirectWrite on top of our D3D11 device (shared GPU, no copies).

use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISurface};
use windows::core::Interface;
use windows_numerics::{Matrix3x2, Vector2};

use crate::gpu::Gpu;
use crate::util::{Ctx, Res, wide};

pub struct D2d {
    pub factory: ID2D1Factory1,
    pub dc: ID2D1DeviceContext,
    pub dwrite: IDWriteFactory,
    brush: ID2D1SolidColorBrush,
}

pub fn rgba(hex: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a,
    }
}

pub fn rect(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: l, top: t, right: r, bottom: b }
}

pub fn rrect(r: D2D_RECT_F, rad: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT { rect: r, radiusX: rad, radiusY: rad }
}

pub fn ellipse(x: f32, y: f32, r: f32) -> D2D1_ELLIPSE {
    D2D1_ELLIPSE { point: Vector2 { X: x, Y: y }, radiusX: r, radiusY: r }
}

pub fn pt(x: f32, y: f32) -> Vector2 {
    Vector2 { X: x, Y: y }
}

impl D2d {
    pub fn new(gpu: &Gpu) -> Res<D2d> {
        unsafe {
            let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).ctx("D2D1CreateFactory")?;
            let dxgi: IDXGIDevice = gpu.device.cast().ctx("IDXGIDevice")?;
            let dev = factory.CreateDevice(&dxgi).ctx("D2D CreateDevice")?;
            let dc = dev.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE).ctx("CreateDeviceContext")?;
            dc.SetUnitMode(D2D1_UNIT_MODE_PIXELS);
            dc.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).ctx("DWriteCreateFactory")?;
            let brush = dc.CreateSolidColorBrush(&rgba(0xffffff, 1.0), None).ctx("brush")?;
            Ok(D2d { factory, dc, dwrite, brush })
        }
    }

    fn bitmap(&self, tex: &ID3D11Texture2D, opts: D2D1_BITMAP_OPTIONS) -> Res<ID2D1Bitmap1> {
        unsafe {
            let surface: IDXGISurface = tex.cast().ctx("IDXGISurface")?;
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: opts,
                colorContext: std::mem::ManuallyDrop::new(None),
            };
            self.dc.CreateBitmapFromDxgiSurface(&surface, Some(&props)).ctx("CreateBitmapFromDxgiSurface")
        }
    }

    /// A bitmap D2D can render into.
    pub fn target_for(&self, tex: &ID3D11Texture2D) -> Res<ID2D1Bitmap1> {
        self.bitmap(tex, D2D1_BITMAP_OPTIONS_TARGET)
    }

    /// A bitmap D2D can draw from (texture needs SHADER_RESOURCE binding).
    pub fn source_for(&self, tex: &ID3D11Texture2D) -> Res<ID2D1Bitmap1> {
        self.bitmap(tex, D2D1_BITMAP_OPTIONS_NONE)
    }

    /// Shared brush recolored on each use: cheap and avoids brush churn.
    pub fn brush(&self, c: D2D1_COLOR_F) -> &ID2D1SolidColorBrush {
        unsafe { self.brush.SetColor(&c) };
        &self.brush
    }

    pub fn text_format(&self, size: f32, weight: DWRITE_FONT_WEIGHT) -> Res<IDWriteTextFormat> {
        unsafe {
            let fam = wide("Segoe UI Variable Text");
            let fam = if self.has_font("Segoe UI Variable Text") { fam } else { wide("Segoe UI") };
            let loc = wide("en-us");
            self.dwrite
                .CreateTextFormat(
                    windows::core::PCWSTR(fam.as_ptr()),
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    windows::core::PCWSTR(loc.as_ptr()),
                )
                .ctx("CreateTextFormat")
        }
    }

    fn has_font(&self, name: &str) -> bool {
        unsafe {
            let mut col = None;
            if self.dwrite.GetSystemFontCollection(&mut col, false).is_err() {
                return false;
            }
            let Some(col) = col else { return false };
            let w = wide(name);
            let mut idx = 0;
            let mut exists = windows::core::BOOL(0);
            col.FindFamilyName(windows::core::PCWSTR(w.as_ptr()), &mut idx, &mut exists).is_ok() && exists.as_bool()
        }
    }

    pub fn text_layout(&self, s: &str, fmt: &IDWriteTextFormat, max_w: f32, max_h: f32) -> Res<IDWriteTextLayout> {
        let w: Vec<u16> = s.encode_utf16().collect();
        unsafe { self.dwrite.CreateTextLayout(&w, fmt, max_w, max_h) }.ctx("CreateTextLayout")
    }

    pub fn polygon(&self, pts: &[(f32, f32)]) -> Res<ID2D1PathGeometry1> {
        unsafe {
            let g = self.factory.CreatePathGeometry().ctx("path")?;
            let sink = g.Open().ctx("open")?;
            sink.BeginFigure(pt(pts[0].0, pts[0].1), D2D1_FIGURE_BEGIN_FILLED);
            let rest: Vec<Vector2> = pts[1..].iter().map(|p| pt(p.0, p.1)).collect();
            sink.AddLines(&rest);
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            sink.Close().ctx("close")?;
            Ok(g)
        }
    }

    pub fn union(&self, a: &ID2D1Geometry, b: &ID2D1Geometry) -> Res<ID2D1PathGeometry1> {
        unsafe {
            let g = self.factory.CreatePathGeometry().ctx("path")?;
            let sink = g.Open().ctx("open")?;
            a.CombineWithGeometry(b, D2D1_COMBINE_MODE_UNION, None, 0.05, &sink).ctx("combine")?;
            sink.Close().ctx("close")?;
            Ok(g)
        }
    }

    pub fn rounded(&self, r: D2D_RECT_F, rad: f32) -> Res<ID2D1Geometry> {
        unsafe { self.factory.CreateRoundedRectangleGeometry(&rrect(r, rad)) }.ctx("rrect geom").and_then(|g| g.cast().ctx("cast"))
    }

    pub fn set_transform(&self, m: Matrix3x2) {
        unsafe { self.dc.SetTransform(&m) };
    }
}
