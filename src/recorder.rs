//! Recording session: capture thread (GPU copy -> hardware H.264) + input log.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

use crate::capture::{Capturer, Target};
use crate::clock::RecClock;
use crate::gpu::Gpu;
use crate::input::{CursorSampler, InputLogger};
use crate::log;
use crate::media::{Encoder, EncoderConfig};
use crate::project::{CursorSample, Recording};
use crate::util::{Ctx, Res, now_hns};

#[derive(Clone)]
pub struct RecordOptions {
    pub target: Target,
    pub fps: u32,
    pub all_keys: bool,
    pub system_audio: bool,
    pub mic: bool,
}

pub struct ActiveRecording {
    pub stop: Arc<AtomicBool>,
    pub clock: Arc<RecClock>,
    thread: Option<JoinHandle<Res<PathBuf>>>,
}

impl ActiveRecording {
    /// Recorded length so far (pauses excluded).
    pub fn elapsed(&self) -> f64 {
        self.clock.now() as f64 / 1e7
    }
    pub fn is_paused(&self) -> bool {
        self.clock.is_paused()
    }
    pub fn toggle_pause(&self) {
        self.clock.toggle();
    }
    pub fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
    pub fn join(mut self) -> Res<PathBuf> {
        self.request_stop();
        match self.thread.take() {
            Some(t) => t.join().map_err(|_| "recorder thread panicked".to_string())?,
            None => Err("not running".into()),
        }
    }
}

pub fn start(opts: RecordOptions) -> Res<ActiveRecording> {
    let dir = crate::util::app_dir().join(crate::util::stamp());
    std::fs::create_dir_all(&dir).ctx("create project folder")?;
    let stop = Arc::new(AtomicBool::new(false));
    let frames = Arc::new(AtomicU64::new(0));
    let (tx, rx) = std::sync::mpsc::channel::<Res<Arc<RecClock>>>();
    let stop2 = stop.clone();
    let frames2 = frames.clone();
    let thread = std::thread::Builder::new()
        .name("recorder".into())
        .spawn(move || record_thread(opts, dir, stop2, frames2, tx))
        .ctx("spawn recorder")?;
    // Wait until capture actually started so the UI timer matches the video.
    let clock = rx.recv().map_err(|_| "recorder failed to start".to_string())??;
    Ok(ActiveRecording { stop, clock, thread: Some(thread) })
}

fn record_thread(
    opts: RecordOptions,
    dir: PathBuf,
    stop: Arc<AtomicBool>,
    frames: Arc<AtomicU64>,
    started: std::sync::mpsc::Sender<Res<Arc<RecClock>>>,
) -> Res<PathBuf> {
    let setup = (|| -> Res<_> {
        let gpu = Gpu::new()?;
        let cap = Capturer::new(&gpu, &opts.target)?;
        // H.264 needs even dimensions.
        let w = cap.width & !1;
        let h = cap.height & !1;
        let px_rate = w as f64 * h as f64 * opts.fps as f64;
        let bitrate = (px_rate * 0.14).clamp(8e6, 90e6) as u32;
        let enc = Encoder::new(&gpu, &dir.join("raw.mp4"), &EncoderConfig { width: w, height: h, fps: opts.fps, bitrate, audio: false })?;
        let latest = gpu.texture(w, h, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET, false)?;
        Ok((gpu, cap, enc, latest, w, h))
    })();
    let (gpu, mut cap, mut enc, latest, w, h) = match setup {
        Ok(v) => v,
        Err(e) => {
            let _ = started.send(Err(e.clone()));
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
    };
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }

    let mut origin = cap.page_origin().unwrap_or_else(|| opts.target.origin());
    let follow = matches!(opts.target, Target::Window(_));
    let mut dpi = (96u32, 96u32);
    unsafe {
        let _ = GetDpiForMonitor(HMONITOR(opts.target.monitor_handle() as _), MDT_EFFECTIVE_DPI, &mut dpi.0, &mut dpi.1);
    }
    // Where the recorded pixels were on screen over time (moves with a recorded window).
    let mut origins: Vec<(f64, i32, i32)> = Vec::with_capacity(60 * 60 * 10);
    let sampler = CursorSampler::new();
    let mut cursor: Vec<CursorSample> = Vec::with_capacity(60 * 60 * 10);

    let clock = Arc::new(RecClock::new(now_hns()));
    // Hooks log screen coordinates; they're mapped into the video after recording.
    let input = InputLogger::start(clock.clone(), (0, 0), opts.all_keys, stop.clone());
    let mut audio = Vec::new();
    if opts.system_audio {
        audio.push(crate::audio::Capture::start(crate::audio::Source::System, &dir, clock.clone()));
    }
    if opts.mic {
        audio.push(crate::audio::Capture::start(crate::audio::Source::Mic, &dir, clock.clone()));
    }
    let _ = started.send(Ok(clock.clone()));
    log!("recording {}x{} @{}fps -> {}", w, h, opts.fps, dir.display());

    let period = 10_000_000 / opts.fps as i64;
    let mut have_frame = false;
    let mut idx: i64 = 0;
    let mut last_written: i64 = -1;
    let mut result = Ok(());
    while !stop.load(Ordering::SeqCst) {
        if clock.is_paused() {
            // Keep draining the capture pool so the first frame after resuming is fresh.
            if cap.copy_latest(&gpu, &latest) {
                have_frame = true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }
        // Schedule on the active timeline: resuming continues right where we paused.
        let due = idx * period;
        let now = clock.now();
        if now < due {
            let wait = due - now;
            if wait > 15_000 {
                std::thread::sleep(std::time::Duration::from_micros(((wait - 10_000) / 10) as u64));
            } else {
                std::hint::spin_loop();
            }
            continue;
        }
        // If we fell behind (machine busy), skip ahead instead of bursting frames.
        let behind = (now - due) / period;
        if behind > 0 {
            idx += behind;
        }
        if cap.copy_latest(&gpu, &latest) {
            have_frame = true;
        }
        let (cx, cy, kind) = sampler.sample();
        let t = (idx * period) as f64 / 1e7;
        if let Some(o) = cap.page_origin() {
            origin = o;
        } else if follow {
            origin = opts.target.origin();
        }
        origins.push((t, origin.0, origin.1));
        cursor.push(CursorSample {
            t,
            x: (cx - origin.0) as f32,
            y: (cy - origin.1) as f32,
            kind: kind as i32,
        });
        if have_frame {
            let tex = enc.next_texture();
            unsafe { gpu.ctx.CopyResource(&tex, &latest) };
            if last_written < 0 && idx > 0 {
                // First frame arrives a bit late: stretch it back to t=0 so timing stays aligned.
                if let Err(e) = enc.submit(&tex, 0, idx * period + period) {
                    result = Err(e);
                    break;
                }
            } else if let Err(e) = enc.submit(&tex, idx * period, period) {
                result = Err(e);
                break;
            }
            last_written = idx;
            frames.fetch_add(1, Ordering::Relaxed);
        }
        idx += 1;
    }
    let duration = (idx * period) as f64 / 1e7;
    let log = input.finish();
    for a in audio {
        a.finish();
    }
    drop(cap);
    let fin = enc.finish();
    result?;
    fin?;
    if last_written < 0 {
        return Err("no frames captured".into());
    }
    // Map clicks into video coordinates and drop the ones outside the recorded area.
    let clicks = log
        .clicks
        .into_iter()
        .filter_map(|mut c| {
            let i = origins.partition_point(|o| o.0 <= c.t).saturating_sub(1);
            let (_, ox, oy) = origins.get(i).copied().unwrap_or((0.0, origin.0, origin.1));
            c.x -= ox as f32;
            c.y -= oy as f32;
            let inside = c.x >= 0.0 && c.y >= 0.0 && c.x < w as f32 && c.y < h as f32;
            (inside || !c.down).then_some(c)
        })
        .collect();
    let rec = Recording {
        source: opts.target.label(),
        cuts: clock.cuts(),
        width: w,
        height: h,
        fps: opts.fps,
        duration,
        dpi_scale: dpi.0 as f32 / 96.0,
        cursor,
        clicks,
        keys: log.keys,
    };
    rec.save(&dir)?;
    log!("saved {} frames, {:.1}s", frames.load(Ordering::Relaxed), duration);
    Ok(dir)
}
