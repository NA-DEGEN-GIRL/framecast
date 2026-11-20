//! Compositor: turns a raw screen frame into the styled output frame.
//! Pass 1 (pixel shader): gradient background, soft shadow, rounded content with
//! camera zoom/pan and zoom motion blur. Pass 2 (Direct2D): cursor, click effects,
//! keystroke badges.

use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::PCSTR;
use windows_numerics::{Matrix3x2, Vector2};

use crate::d2d::{D2d, ellipse, rect, rgba, rrect};
use crate::gpu::Gpu;
use crate::project::{BACKGROUNDS, Motion, Project};
use crate::util::{Ctx, Res};

const SHADER: &str = r#"
cbuffer P : register(b0) {
    float4 canvas;   // w, h, 1/w, 1/h
    float4 rect;     // content x, y, w, h (px)
    float4 bg0;
    float4 bg1;
    float4 shape;    // radius, shadow strength, shadow blur, shadow y offset
    float4 cam;      // center x, center y, scale, blur samples
    float4 camPrev;  // previous center x, y, scale
};
Texture2D src : register(t0);
SamplerState smp : register(s0);

struct VOut { float4 pos : SV_Position; };

VOut vs(uint id : SV_VertexID) {
    VOut o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.pos = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return o;
}

float sdRoundRect(float2 p, float2 b, float r) {
    float2 q = abs(p) - b + r;
    return length(max(q, 0)) + min(max(q.x, q.y), 0) - r;
}

float hash(float2 p) { return frac(sin(dot(p, float2(12.9898, 78.233))) * 43758.5453); }

float4 ps(VOut i) : SV_Target {
    float2 p = i.pos.xy;
    float2 uvc = p * canvas.zw;
    float g = saturate(uvc.x * 0.55 + uvc.y * 0.45);
    float3 col = lerp(bg0.rgb, bg1.rgb, g);
    col += 0.05 * saturate(1 - length(uvc - float2(0.25, 0.15)) * 1.4);

    float2 hb = rect.zw * 0.5;
    float2 c = rect.xy + hb;
    float r = shape.x;

    // Soft drop shadow (analytic falloff of the rounded-rect distance).
    float sd = sdRoundRect(p - c - float2(0, shape.w), hb, r);
    float sh = shape.y * 0.6 * pow(saturate(1 - sd / shape.z), 2.4);
    col *= 1 - sh;

    // Screen content with camera transform. Derivatives are taken outside the
    // branch so mip selection stays correct (sharp text when scaled down).
    float2 q = (p - rect.xy) / rect.zw;
    float2 uv = cam.xy + (q - 0.5) / cam.z;
    float2 dx = ddx(uv), dy = ddy(uv);
    float d = sdRoundRect(p - c, hb, r);
    float cov = saturate(0.5 - d);
    if (cov > 0) {
        float3 s = 0;
        int n = (int)cam.w;
        if (n <= 1) {
            s = src.SampleGrad(smp, uv, dx, dy).rgb;
        } else {
            float2 uvp = camPrev.xy + (q - 0.5) / camPrev.z;
            float2 dv = uv - uvp;
            [loop] for (int k = 0; k < n; k++) {
                float f = (float)k / (n - 1) - 0.5;
                s += src.SampleGrad(smp, uv + dv * f, dx, dy).rgb;
            }
            s /= n;
        }
        // Hairline highlight on the edge reads well on dark and light content.
        float edge = saturate(1 - abs(d + 0.75)) * 0.10;
        col = lerp(col, s + edge, cov);
    }
    col += (hash(p) - 0.5) / 255.0; // dither: no banding in gradients
    return float4(col, 1);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Constants {
    canvas: [f32; 4],
    rect: [f32; 4],
    bg0: [f32; 4],
    bg1: [f32; 4],
    shape: [f32; 4],
    cam: [f32; 4],
    cam_prev: [f32; 4],
}

fn compile(entry: &str, target: &str) -> Res<Vec<u8>> {
    unsafe {
        let mut code = None;
        let mut err = None;
        let e = format!("{entry}\0");
        let t = format!("{target}\0");
        let r = D3DCompile(
            SHADER.as_ptr() as _,
            SHADER.len(),
            PCSTR(c"compositor.hlsl".as_ptr() as _),
            None,
            None,
            PCSTR(e.as_ptr()),
            PCSTR(t.as_ptr()),
            windows::Win32::Graphics::Direct3D::Fxc::D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut err),
        );
        if let Err(e) = r {
            let msg = err
                .map(|b: ID3DBlob| String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())).to_string())
                .unwrap_or_default();
            return Err(format!("shader compile failed: {e} {msg}"));
        }
        let b = code.ok_or("no shader blob")?;
        Ok(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec())
    }
}

/// Where the screen recording sits inside the output canvas.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub radius: f32,
}

pub fn layout(p: &Project, cw: f32, ch: f32) -> Layout {
    let pad = p.style.padding * cw.min(ch);
    let (aw, ah) = ((cw - 2.0 * pad).max(16.0), (ch - 2.0 * pad).max(16.0));
    let src_aspect = p.rec.width as f32 / p.rec.height as f32;
    let (w, h) = if aw / ah > src_aspect { (ah * src_aspect, ah) } else { (aw, aw / src_aspect) };
    let radius = if p.style.padding <= 0.001 { 0.0 } else { p.style.radius * w.min(h) };
    Layout { x: (cw - w) * 0.5, y: (ch - h) * 0.5, w, h, radius }
}

struct CursorShapes {
    arrow: ID2D1PathGeometry1,
    hand: ID2D1PathGeometry1,
    ibeam: ID2D1PathGeometry1,
}

fn cursor_shapes(d2d: &D2d) -> Res<CursorShapes> {
    // Shapes in a 24px-tall box with the hotspot at (0,0) (arrow/hand tip) or center (I-beam).
    let arrow = d2d.polygon(&[(0.0, 0.0), (0.0, 17.5), (4.2, 13.6), (7.0, 20.0), (9.8, 18.8), (7.1, 12.6), (12.6, 12.6)])?;
    let finger = d2d.rounded(rect(-1.6, 0.0, 1.9, 12.0), 1.75)?;
    let palm = d2d.rounded(rect(-5.5, 8.0, 9.5, 20.5), 4.2)?;
    let k1 = d2d.rounded(rect(1.6, 6.5, 4.9, 12.0), 1.6)?;
    let k2 = d2d.rounded(rect(4.6, 7.2, 7.9, 12.5), 1.6)?;
    let k3 = d2d.rounded(rect(7.4, 8.4, 10.4, 13.2), 1.5)?;
    let thumb = d2d.polygon(&[(-5.4, 13.0), (-8.6, 9.6), (-9.6, 10.8), (-5.4, 17.5)])?;
    let mut hand = d2d.union(&finger, &palm)?;
    for g in [&k1, &k2, &k3] {
        hand = d2d.union(&windows::core::Interface::cast(&hand).ctx("geom")?, g)?;
    }
    let hand = d2d.union(&windows::core::Interface::cast(&hand).ctx("geom")?, &windows::core::Interface::cast(&thumb).ctx("geom")?)?;
    let ibeam = d2d.polygon(&[
        (-4.0, -10.0),
        (4.0, -10.0),
        (4.0, -8.6),
        (1.0, -8.6),
        (1.0, 8.6),
        (4.0, 8.6),
        (4.0, 10.0),
        (-4.0, 10.0),
        (-4.0, 8.6),
        (-1.0, 8.6),
        (-1.0, -8.6),
        (-4.0, -8.6),
    ])?;
    Ok(CursorShapes { arrow, hand, ibeam })
}

pub struct Compositor {
    gpu: Gpu,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    cb: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    pub out: ID3D11Texture2D,
    out_rtv: ID3D11RenderTargetView,
    out_target: ID2D1Bitmap1,
    pub width: u32,
    pub height: u32,
    shapes: CursorShapes,
    key_font: Option<(f32, IDWriteTextFormat)>,
}

impl Compositor {
    pub fn new(gpu: &Gpu, d2d: &D2d, width: u32, height: u32) -> Res<Compositor> {
        unsafe {
            let vsb = compile("vs", "vs_5_0")?;
            let psb = compile("ps", "ps_5_0")?;
            let mut vs = None;
            gpu.device.CreateVertexShader(&vsb, None, Some(&mut vs)).ctx("CreateVertexShader")?;
            let mut ps = None;
            gpu.device.CreatePixelShader(&psb, None, Some(&mut ps)).ctx("CreatePixelShader")?;
            let bd = D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<Constants>() as u32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            let mut cb = None;
            gpu.device.CreateBuffer(&bd, None, Some(&mut cb)).ctx("CreateBuffer")?;
            let sd = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            gpu.device.CreateSamplerState(&sd, Some(&mut sampler)).ctx("CreateSamplerState")?;
            let (out, out_rtv, out_target) = Self::make_target(gpu, d2d, width, height)?;
            Ok(Compositor {
                gpu: gpu.clone(),
                vs: vs.ok_or("vs")?,
                ps: ps.ok_or("ps")?,
                cb: cb.ok_or("cb")?,
                sampler: sampler.ok_or("sampler")?,
                out,
                out_rtv,
                out_target,
                width,
                height,
                shapes: cursor_shapes(d2d)?,
                key_font: None,
            })
        }
    }

    fn make_target(gpu: &Gpu, d2d: &D2d, w: u32, h: u32) -> Res<(ID3D11Texture2D, ID3D11RenderTargetView, ID2D1Bitmap1)> {
        let t = gpu.texture(w, h, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE, false)?;
        let rtv = gpu.rtv(&t)?;
        let bmp = d2d.target_for(&t)?;
        Ok((t, rtv, bmp))
    }

    pub fn resize(&mut self, d2d: &D2d, w: u32, h: u32) -> Res<()> {
        if w == self.width && h == self.height {
            return Ok(());
        }
        let (t, r, b) = Self::make_target(&self.gpu, d2d, w, h)?;
        self.out = t;
        self.out_rtv = r;
        self.out_target = b;
        self.width = w;
        self.height = h;
        self.key_font = None;
        Ok(())
    }

    /// Renders output frame for time `t` (seconds, recording time) into `self.out`.
    pub fn render(&mut self, d2d: &D2d, src: Option<&ID3D11ShaderResourceView>, p: &Project, motion: &Motion, t: f64, fps: f64) -> Res<()> {
        let (cw, ch) = (self.width as f32, self.height as f32);
        let lay = layout(p, cw, ch);
        let fr = motion.at(&p.rec, t);
        // Shutter of ~1/3 frame: enough to smooth fast zooms without smearing text.
        let prev = motion.at(&p.rec, (t - 0.33 / fps).max(0.0));
        let (b0, b1) = (BACKGROUNDS[p.style.background].1, BACKGROUNDS[p.style.background].2);
        let col = |h: u32| [((h >> 16) & 255) as f32 / 255.0, ((h >> 8) & 255) as f32 / 255.0, (h & 255) as f32 / 255.0, 1.0];
        let blur_samples = if p.style.motion_blur && fr.cam_speed > 0.15 { 8.0 } else { 1.0 };
        let k = Constants {
            canvas: [cw, ch, 1.0 / cw, 1.0 / ch],
            rect: [lay.x, lay.y, lay.w, lay.h],
            bg0: col(b0),
            bg1: col(b1),
            shape: [lay.radius, if p.style.padding > 0.001 { p.style.shadow } else { 0.0 }, 0.035 * cw.min(ch) + 1.0, 0.01 * ch],
            cam: [fr.cam_x, fr.cam_y, fr.cam_scale, blur_samples],
            cam_prev: [prev.cam_x, prev.cam_y, prev.cam_scale, 0.0],
        };
        unsafe {
            let ctx = &self.gpu.ctx;
            ctx.UpdateSubresource(&self.cb, 0, None, &k as *const _ as _, 0, 0);
            ctx.OMSetRenderTargets(Some(&[Some(self.out_rtv.clone())]), None);
            let vp = D3D11_VIEWPORT { Width: cw, Height: ch, MaxDepth: 1.0, ..Default::default() };
            ctx.RSSetViewports(Some(&[vp]));
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.IASetInputLayout(None);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps, None);
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.PSSetShaderResources(0, Some(&[src.cloned()]));
            ctx.Draw(3, 0);
            ctx.PSSetShaderResources(0, Some(&[None]));
            ctx.OMSetRenderTargets(None, None);
        }
        self.overlays(d2d, p, &lay, &fr, t)
    }

    fn overlays(&mut self, d2d: &D2d, p: &Project, lay: &Layout, fr: &crate::project::Frame, t: f64) -> Res<()> {
        let rec = &p.rec;
        let (sw, sh) = (rec.width as f32, rec.height as f32);
        let to_canvas = |x: f32, y: f32| {
            let qx = (x / sw - fr.cam_x) * fr.cam_scale + 0.5;
            let qy = (y / sh - fr.cam_y) * fr.cam_scale + 0.5;
            (lay.x + qx * lay.w, lay.y + qy * lay.h)
        };
        let px_scale = lay.w / sw * fr.cam_scale; // canvas px per source px
        let dc = &d2d.dc;
        unsafe {
            dc.SetTarget(&self.out_target);
            dc.BeginDraw();
            dc.SetTransform(&Matrix3x2::identity());
            dc.PushAxisAlignedClip(&rect(lay.x, lay.y, lay.x + lay.w, lay.y + lay.h), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);

            // Click effects.
            if p.style.click_effect > 0 {
                for c in rec.clicks.iter().filter(|c| c.down && c.t <= t && t - c.t < 0.6) {
                    let a = ((t - c.t) / 0.6) as f32;
                    let (x, y) = to_canvas(c.x, c.y);
                    let base = 22.0 * rec.dpi_scale * p.style.cursor_size.max(1.0) * px_scale;
                    let ease = 1.0 - (1.0 - a).powi(3);
                    if p.style.click_effect == 1 {
                        let r = base * (0.3 + 1.1 * ease);
                        dc.FillEllipse(&ellipse(x, y, r), d2d.brush(rgba(0xffffff, 0.22 * (1.0 - a))));
                        dc.DrawEllipse(&ellipse(x, y, r), d2d.brush(rgba(0xffffff, 0.85 * (1.0 - a))), (2.5 * px_scale).max(1.5), None);
                    } else {
                        let r = base * (0.6 + 0.4 * ease);
                        dc.FillEllipse(&ellipse(x, y, r), d2d.brush(rgba(0xFFD34E, 0.45 * (1.0 - a))));
                    }
                }
            }

            // Cursor (redrawn as vector art: stays crisp at any zoom).
            if fr.cursor_alpha > 0.01 {
                let (x, y) = to_canvas(fr.cursor_x, fr.cursor_y);
                let press = 1.0 - 0.15 * fr.pressed;
                let scale = rec.dpi_scale * p.style.cursor_size * px_scale * press;
                let geom = match fr.cursor_kind {
                    1 => &self.shapes.ibeam,
                    2 => &self.shapes.hand,
                    _ => &self.shapes.arrow,
                };
                let alpha = fr.cursor_alpha;
                // Shadow.
                dc.SetTransform(&(Matrix3x2::scale(scale, scale) * Matrix3x2::translation(x + 0.8 * scale, y + 1.6 * scale)));
                dc.FillGeometry(geom, d2d.brush(rgba(0x000000, 0.22 * alpha)), None);
                dc.SetTransform(&(Matrix3x2::scale(scale, scale) * Matrix3x2::translation(x, y)));
                dc.DrawGeometry(geom, d2d.brush(rgba(0xffffff, alpha)), 2.6, None);
                dc.FillGeometry(geom, d2d.brush(rgba(0x111111, alpha)), None);
                dc.SetTransform(&Matrix3x2::identity());
            }
            dc.PopAxisAlignedClip();

            // Keystroke badge (shortcuts), bottom center.
            if p.style.show_keys {
                if let Some(k) = rec.keys.iter().rev().find(|k| k.t <= t && t - k.t < 1.6) {
                    let age = (t - k.t) as f32;
                    let alpha = (age / 0.12).min(1.0) * ((1.6 - age) / 0.25).clamp(0.0, 1.0);
                    let size = (self.height.min(self.width) as f32 * 0.032).max(12.0);
                    if self.key_font.as_ref().is_none_or(|f| f.0 != size) {
                        self.key_font = Some((size, d2d.text_format(size, DWRITE_FONT_WEIGHT_SEMI_BOLD)?));
                    }
                    let fmt = &self.key_font.as_ref().unwrap().1;
                    let layout = d2d.text_layout(&k.label, fmt, self.width as f32, size * 2.0)?;
                    let mut m = DWRITE_TEXT_METRICS::default();
                    layout.GetMetrics(&mut m).ctx("metrics")?;
                    let (pw, ph) = (m.width + size * 1.4, m.height + size * 0.7);
                    let rise = (1.0 - (age / 0.18).min(1.0)) * size * 0.4;
                    let cx = self.width as f32 * 0.5;
                    let by = self.height as f32 - size * 1.6 + rise;
                    let r = rect(cx - pw * 0.5, by - ph, cx + pw * 0.5, by);
                    dc.FillRoundedRectangle(&rrect(r, ph * 0.3), d2d.brush(rgba(0x15151A, 0.82 * alpha)));
                    dc.DrawRoundedRectangle(&rrect(r, ph * 0.3), d2d.brush(rgba(0xffffff, 0.14 * alpha)), 1.0, None);
                    dc.DrawTextLayout(
                        Vector2 { X: r.left + size * 0.7, Y: r.top + size * 0.35 },
                        &layout,
                        d2d.brush(rgba(0xffffff, alpha)),
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                    );
                }
            }
            dc.EndDraw(None, None).ctx("EndDraw")?;
            dc.SetTarget(None);
        }
        Ok(())
    }
}
