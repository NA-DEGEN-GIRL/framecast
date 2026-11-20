//! Export: decode raw recording -> composite -> hardware H.264 MP4. Runs on its own
//! thread with its own GPU device so the editor stays responsive.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::d2d::D2d;
use crate::gpu::Gpu;
use crate::media::{Decoder, Encoder, EncoderConfig};
use crate::project::{Motion, Project};
use crate::render::Compositor;
use crate::util::{Ctx, Res, secs_to_hns};

pub struct ExportJob {
    pub progress: Arc<AtomicU32>, // 0..=10000
    pub cancel: Arc<AtomicBool>,
    pub out: PathBuf,
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
    let mut out = base.join(format!("{name}.mp4"));
    let mut i = 2;
    while out.exists() {
        out = base.join(format!("{name} ({i}).mp4"));
        i += 1;
    }
    out
}

pub fn start(p: Project) -> ExportJob {
    let progress = Arc::new(AtomicU32::new(0));
    let cancel = Arc::new(AtomicBool::new(false));
    let out = output_path(&p);
    let (pr, ca, o) = (progress.clone(), cancel.clone(), out.clone());
    let thread = std::thread::Builder::new()
        .name("export".into())
        .spawn(move || {
            let r = run(&p, &o, &pr, &ca);
            if r.is_err() {
                let _ = std::fs::remove_file(&o);
            }
            r.map(|_| o)
        })
        .ok();
    ExportJob { progress, cancel, out, thread }
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
    let mut enc = Encoder::new(&gpu, out, &EncoderConfig { width: w, height: h, fps, bitrate })?;
    let (t0, t1) = (p.trim_start, p.trim_end.min(p.rec.duration));
    let frames = ((t1 - t0) * fps as f64).floor().max(1.0) as u64;
    let period = 10_000_000 / fps as i64;
    dec.seek(secs_to_hns(t0))?;
    for i in 0..frames {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let t = t0 + i as f64 / fps as f64;
        dec.frame_at(&gpu, secs_to_hns(t))?;
        comp.render(&d2d, dec.frame_srv.as_ref(), p, &motion, t, fps as f64)?;
        let tex = enc.next_texture();
        unsafe { gpu.ctx.CopyResource(&tex, &comp.out) };
        enc.submit(&tex, i as i64 * period, period)?;
        progress.store(((i + 1) * 10000 / frames) as u32, Ordering::Relaxed);
    }
    enc.finish().ctx("finish export")
}
