//! Export: decode raw recording -> composite -> hardware H.264 MP4. Runs on its own
//! thread with its own GPU device so the editor stays responsive.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::d2d::D2d;
use crate::gpu::Gpu;
use crate::media::{Decoder, Encoder, EncoderConfig};
use crate::project::{Motion, Project, Timeline};
use crate::render::Compositor;
use crate::util::{Ctx, Res, secs_to_hns};

pub struct ExportJob {
    pub progress: Arc<AtomicU32>, // 0..=10000
    pub cancel: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<Res<PathBuf>>>,
}

impl ExportJob {
    pub fn fraction(&self) -> f32 {
        self.progress.load(Ordering::Relaxed) as f32 / 10000.0
    }
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
    pub fn join(mut self) -> Res<PathBuf> {
        match self.thread.take() {
            Some(t) => t.join().map_err(|_| "export thread panicked".to_string())?,
            None => Err("not running".into()),
        }
    }
}

pub fn output_path(p: &Project) -> PathBuf {
    let name = p.dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(crate::util::stamp);
    let base = crate::util::app_dir().join("Exports");
    let _ = std::fs::create_dir_all(&base);
    let ext = if p.is_gif() { "gif" } else { "mp4" };
    let mut out = base.join(format!("{name}.{ext}"));
    let mut i = 2;
    while out.exists() {
        out = base.join(format!("{name} ({i}).{ext}"));
        i += 1;
    }
    out
}

pub fn start(p: Project) -> ExportJob {
    let progress = Arc::new(AtomicU32::new(0));
    let cancel = Arc::new(AtomicBool::new(false));
    let o = output_path(&p);
    let (pr, ca) = (progress.clone(), cancel.clone());
    let thread = std::thread::Builder::new()
        .name("export".into())
        .spawn(move || {
            let r = if p.is_gif() { run_gif(&p, &o, &pr, &ca) } else { run(&p, &o, &pr, &ca) };
            if r.is_err() {
                let _ = std::fs::remove_file(&o);
            }
            r.map(|_| o)
        })
        .ok();
    ExportJob { progress, cancel, thread }
}

pub fn run(p: &Project, out: &std::path::Path, progress: &AtomicU32, cancel: &AtomicBool) -> Res<()> {
    let gpu = Gpu::new()?;
    let d2d = D2d::new(&gpu)?;
    let (w, h) = p.canvas_size();
    let mut comp = Compositor::new(&gpu, &d2d, w, h)?;
    let mut dec = Decoder::open(&gpu, &p.dir.join("raw.mp4"))?;
    let motion = Motion::build(p);
    let fps = p.rec.fps.clamp(24, 60);
    let bitrate = ((w * h) as f64 * fps as f64 * 0.11).clamp(6e6, 60e6) as u32;
    let tracks = crate::audio::Tracks::load(&p.dir);
    let audio = !tracks.is_empty() && (p.style.sys_volume > 0.0 || p.style.mic_volume > 0.0);
    let mut enc = Encoder::new(&gpu, out, &EncoderConfig { width: w, height: h, fps, bitrate, audio })?;
    let rate = crate::audio::MIX_RATE as u64;
    let tl = Timeline::new(p);
    let frames = (tl.duration * fps as f64).floor().max(1.0) as u64;
    let period = 10_000_000 / fps as i64;
    dec.seek(secs_to_hns(tl.to_rec(0.0)))?;
    for i in 0..frames {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let t = tl.to_rec(i as f64 / fps as f64);
        dec.frame_at(&gpu, secs_to_hns(t))?;
        comp.render(&d2d, dec.frame_srv.as_ref(), p, &motion, t, fps as f64)?;
        let tex = enc.next_texture();
        unsafe { gpu.ctx.CopyResource(&tex, &comp.out) };
        enc.submit(&tex, i as i64 * period, period)?;
        if audio {
            // Audio for exactly this frame's time span keeps both streams interleaved.
            let (n0, n1) = (i * rate / fps as u64, (i + 1) * rate / fps as u64);
            let pcm = render_pcm16(&tracks, &tl, n0, n1, p.style.sys_volume, p.style.mic_volume);
            enc.submit_audio(&pcm, (n0 * 10_000_000 / rate) as i64)?;
        }
        progress.store(((i + 1) * 10000 / frames) as u32, Ordering::Relaxed);
    }
    enc.finish().ctx("finish export")
}

/// GIF export: render each frame at GIF size, read it back from the GPU, encode.
pub fn run_gif(p: &Project, out: &std::path::Path, progress: &AtomicU32, cancel: &AtomicBool) -> Res<()> {
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;

    let gpu = Gpu::new()?;
    let d2d = D2d::new(&gpu)?;
    let (w, h) = p.gif_size();
    let mut comp = Compositor::new(&gpu, &d2d, w, h)?;
    let mut dec = Decoder::open(&gpu, &p.dir.join("raw.mp4"))?;
    let motion = Motion::build(p);
    let fps = crate::project::GIF_FPS[p.style.gif_fps].1;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging = None;
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut staging)) }.ctx("staging texture")?;
    let staging = staging.ok_or("no staging texture")?;
    let mut gif = crate::gif::GifWriter::create(out, w, h)?;
    let tl = Timeline::new(p);
    let frames = (tl.duration * fps as f64).floor().max(1.0) as u64;
    let cs = |i: u64| (i * 100 + fps as u64 / 2) / fps as u64; // centiseconds, rounded
    dec.seek(secs_to_hns(tl.to_rec(0.0)))?;
    for i in 0..frames {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let t = tl.to_rec(i as f64 / fps as f64);
        dec.frame_at(&gpu, secs_to_hns(t))?;
        comp.render(&d2d, dec.frame_srv.as_ref(), p, &motion, t, fps as f64)?;
        unsafe {
            gpu.ctx.CopyResource(&staging, &comp.out);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            gpu.ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m)).ctx("Map")?;
            let pitch = m.RowPitch as usize;
            let bytes = std::slice::from_raw_parts(m.pData as *const u8, pitch * h as usize);
            let r = gif.add_frame(bytes, pitch, (cs(i + 1) - cs(i)) as u32);
            gpu.ctx.Unmap(&staging, 0);
            r?;
        }
        progress.store(((i + 1) * 10000 / frames) as u32, Ordering::Relaxed);
    }
    gif.finish()
}

/// 16-bit stereo PCM for output sample frames [n0, n1), following the edited
/// timeline. Audio fades over a few milliseconds at each cut so joins don't click.
fn render_pcm16(tracks: &crate::audio::Tracks, tl: &Timeline, n0: u64, n1: u64, sys: f32, mic: f32) -> Vec<i16> {
    let rate = crate::audio::MIX_RATE as f64;
    let mut out = Vec::with_capacity(((n1 - n0) * 2) as usize);
    for n in n0..n1 {
        let (t, edge) = tl.to_rec_edge(n as f64 / rate);
        let g = (edge / 0.006).min(1.0) as f32;
        let (l, r) = tracks.mix(t, sys, mic);
        out.push((l * g * 32767.0) as i16);
        out.push((r * g * 32767.0) as i16);
    }
    out
}
