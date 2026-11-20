//! Audio: WASAPI capture (system loopback / microphone) into float WAV files,
//! offline mixing for export, and a small WASAPI player for editor preview.
//!
//! Capture is timestamped with QPC (same clock as video) so silence gaps, which
//! loopback simply doesn't deliver, are padded and audio stays in sync.

use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;

use crate::log;
use crate::clock::RecClock;
use crate::util::{Ctx, Res};

/// Sample rate used for mixing and export.
pub const MIX_RATE: u32 = 48_000;

#[derive(Clone, Copy, PartialEq)]
pub enum Source {
    System,
    Mic,
}

impl Source {
    pub fn file(self) -> &'static str {
        match self {
            Source::System => "system.wav",
            Source::Mic => "mic.wav",
        }
    }
}

struct WavWriter {
    f: BufWriter<std::fs::File>,
    frames: u64,
}

impl WavWriter {
    fn create(path: &Path, rate: u32) -> Res<WavWriter> {
        let mut f = BufWriter::new(std::fs::File::create(path).ctx("create wav")?);
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
        h.extend_from_slice(&2u16.to_le_bytes());
        h.extend_from_slice(&rate.to_le_bytes());
        h.extend_from_slice(&(rate * 8).to_le_bytes());
        h.extend_from_slice(&8u16.to_le_bytes());
        h.extend_from_slice(&32u16.to_le_bytes());
        h.extend_from_slice(b"data\0\0\0\0");
        f.write_all(&h).ctx("wav header")?;
        Ok(WavWriter { f, frames: 0 })
    }

    fn write(&mut self, stereo: &[f32]) {
        let mut buf = Vec::with_capacity(stereo.len() * 4);
        for s in stereo {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        let _ = self.f.write_all(&buf);
        self.frames += (stereo.len() / 2) as u64;
    }

    fn silence(&mut self, frames: u64) {
        let zeros = vec![0f32; 2 * 1024];
        let mut left = frames;
        while left > 0 {
            let n = left.min(1024);
            self.write(&zeros[..(n * 2) as usize]);
            left -= n;
        }
    }

    fn finish(mut self) -> Res<()> {
        let data = (self.frames * 8).min(u32::MAX as u64 - 64) as u32;
        self.f.flush().ctx("flush wav")?;
        let mut f = self.f.into_inner().map_err(|e| e.to_string())?;
        f.seek(SeekFrom::Start(4)).ctx("seek")?;
        f.write_all(&(36 + data).to_le_bytes()).ctx("riff size")?;
        f.seek(SeekFrom::Start(40)).ctx("seek")?;
        f.write_all(&data.to_le_bytes()).ctx("data size")?;
        Ok(())
    }
}

/// Decoded stereo float audio.
#[derive(Clone)]
pub struct Clip {
    pub rate: u32,
    pub samples: Vec<f32>, // interleaved L R
}

impl Clip {
    fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    /// Linear-interpolated stereo sample at time t (seconds).
    #[inline]
    pub fn at(&self, t: f64) -> (f32, f32) {
        let pos = t * self.rate as f64;
        if pos < 0.0 {
            return (0.0, 0.0);
        }
        let i = pos.floor() as usize;
        if i + 1 >= self.frames() {
            return (0.0, 0.0);
        }
        let f = (pos - i as f64) as f32;
        let s = &self.samples[i * 2..i * 2 + 4];
        (s[0] + (s[2] - s[0]) * f, s[1] + (s[3] - s[1]) * f)
    }
}

/// Reads the float/PCM16 WAV files we write (and common simple WAVs).
pub fn read_wav(path: &Path) -> Res<Clip> {
    let mut b = Vec::new();
    std::fs::File::open(path).ctx("open wav")?.read_to_end(&mut b).ctx("read wav")?;
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err("not a WAV file".into());
    }
    let (mut fmt, mut ch, mut rate, mut bits) = (0u16, 0u16, 0u32, 0u16);
    let mut i = 12;
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let len = u32::from_le_bytes(b[i + 4..i + 8].try_into().unwrap()) as usize;
        let body = &b[i + 8..(i + 8 + len).min(b.len())];
        if id == b"fmt " && body.len() >= 16 {
            fmt = u16::from_le_bytes([body[0], body[1]]);
            ch = u16::from_le_bytes([body[2], body[3]]);
            rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            bits = u16::from_le_bytes([body[14], body[15]]);
        } else if id == b"data" {
            if ch == 0 || rate == 0 {
                return Err("wav: missing fmt".into());
            }
            let mut samples = Vec::with_capacity(body.len() / (bits as usize / 8).max(1) / ch as usize * 2);
            let step = (bits as usize / 8) * ch as usize;
            for fr in body.chunks_exact(step) {
                let get = |c: usize| -> f32 {
                    let o = c * (bits as usize / 8);
                    match (fmt, bits) {
                        (3, 32) => f32::from_le_bytes(fr[o..o + 4].try_into().unwrap()),
                        (1, 16) => i16::from_le_bytes([fr[o], fr[o + 1]]) as f32 / 32768.0,
                        _ => 0.0,
                    }
                };
                let l = get(0);
                let r = if ch > 1 { get(1) } else { l };
                samples.push(l);
                samples.push(r);
            }
            return Ok(Clip { rate, samples });
        }
        i += 8 + len + (len & 1);
    }
    Err("wav: no data".into())
}

pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<Res<()>>>,
}

impl Capture {
    /// Starts capturing `src` into `dir/<src>.wav`, placed on the recording clock.
    pub fn start(src: Source, dir: &Path, clock: Arc<RecClock>) -> Capture {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let path = dir.join(src.file());
        let thread = std::thread::Builder::new()
            .name("audio-capture".into())
            .spawn(move || {
                let r = capture_thread(src, &path, &clock, &s2);
                if let Err(e) = &r {
                    log!("audio capture ({}) failed: {e}", src.file());
                    let _ = std::fs::remove_file(&path);
                }
                r
            })
            .ok();
        Capture { stop, thread }
    }

    pub fn finish(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct MixFormat {
    channels: usize,
    rate: u32,
    float: bool,
    bits: u16,
}

unsafe fn mix_format(client: &IAudioClient) -> Res<(*mut WAVEFORMATEX, MixFormat)> {
    unsafe {
        let p = client.GetMixFormat().ctx("GetMixFormat")?;
        let w = &*p;
        let float = match w.wFormatTag as u32 {
            3 => true,
            0xFFFE => {
                let ext = &*(p as *const WAVEFORMATEXTENSIBLE);
                ext.SubFormat.data1 == 3
            }
            _ => false,
        };
        Ok((p, MixFormat { channels: w.nChannels as usize, rate: w.nSamplesPerSec, float, bits: w.wBitsPerSample }))
    }
}

fn device(flow: EDataFlow) -> Res<IMMDevice> {
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ctx("device enumerator")?;
        en.GetDefaultAudioEndpoint(flow, eConsole).ctx("no default audio device")
    }
}

/// True if a default device of the given kind exists (used to grey out the mic toggle).
pub fn has_device(src: Source) -> bool {
    device(if src == Source::Mic { eCapture } else { eRender }).is_ok()
}

fn capture_thread(src: Source, path: &Path, clock: &RecClock, stop: &AtomicBool) -> Res<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let dev = device(if src == Source::System { eRender } else { eCapture })?;
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).ctx("Activate")?;
        let (fmtp, fmt) = mix_format(&client)?;
        let flags = if src == Source::System { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
        let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, fmtp, None);
        CoTaskMemFree(Some(fmtp as _));
        r.ctx("IAudioClient::Initialize")?;
        let cap: IAudioCaptureClient = client.GetService().ctx("capture client")?;
        let mut wav = WavWriter::create(path, fmt.rate)?;
        client.Start().ctx("Start")?;
        let rate = fmt.rate as f64;
        let mut stereo = Vec::new();
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(8));
            loop {
                let n = cap.GetNextPacketSize().unwrap_or(0);
                if n == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut bflags = 0u32;
                let mut qpc = 0u64;
                if cap.GetBuffer(&mut data, &mut frames, &mut bflags, None, Some(&mut qpc)).is_err() {
                    break;
                }
                stereo.clear();
                let silent = bflags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let bytes = (fmt.bits / 8) as usize;
                for f in 0..frames as usize {
                    let sample = |c: usize| -> f32 {
                        if silent {
                            return 0.0;
                        }
                        let c = c.min(fmt.channels - 1);
                        let p = data.add((f * fmt.channels + c) * bytes);
                        match (fmt.float, fmt.bits) {
                            (true, 32) => *(p as *const f32),
                            (false, 16) => *(p as *const i16) as f32 / 32768.0,
                            (false, 32) => *(p as *const i32) as f32 / 2147483648.0,
                            _ => 0.0,
                        }
                    };
                    stereo.push(sample(0));
                    stereo.push(sample(1));
                }
                let _ = cap.ReleaseBuffer(frames);
                // Place the packet where the clock says it belongs; drop it while paused.
                let Some(active) = clock.active(qpc as i64) else { continue };
                let expected = (active as f64 / 1e7 * rate).round() as i64;
                let mut skip = 0usize;
                if expected < 0 {
                    skip = (-expected) as usize; // captured before t=0
                } else {
                    let have = wav.frames as i64;
                    if expected > have + (rate * 0.02) as i64 {
                        wav.silence((expected - have) as u64);
                    } else if have > expected + (rate * 0.1) as i64 {
                        skip = (have - expected) as usize; // drifted ahead: resync
                    }
                }
                if skip < frames as usize {
                    wav.write(&stereo[skip * 2..]);
                }
            }
        }
        let _ = client.Stop();
        // Pad up to the stop time so the file covers the whole recording.
        let end = (clock.now() as f64 / 1e7 * rate) as i64;
        if end > wav.frames as i64 {
            wav.silence((end - wav.frames as i64) as u64);
        }
        wav.finish()
    }
}

/// Loaded audio tracks of a project.
#[derive(Clone, Default)]
pub struct Tracks {
    pub system: Option<Arc<Clip>>,
    pub mic: Option<Arc<Clip>>,
}

impl Tracks {
    pub fn load(dir: &Path) -> Tracks {
        let load = |s: Source| -> Option<Arc<Clip>> {
            let p: PathBuf = dir.join(s.file());
            if !p.exists() {
                return None;
            }
            match read_wav(&p) {
                Ok(c) => Some(Arc::new(c)),
                Err(e) => {
                    log!("couldn't read {}: {e}", p.display());
                    None
                }
            }
        };
        Tracks { system: load(Source::System), mic: load(Source::Mic) }
    }

    pub fn is_empty(&self) -> bool {
        self.system.is_none() && self.mic.is_none()
    }

    #[inline]
    pub fn mix(&self, t: f64, sys_vol: f32, mic_vol: f32) -> (f32, f32) {
        let mut l = 0.0;
        let mut r = 0.0;
        if let Some(c) = &self.system {
            let s = c.at(t);
            l += s.0 * sys_vol;
            r += s.1 * sys_vol;
        }
        if let Some(c) = &self.mic {
            let s = c.at(t);
            // Mics are usually mono-ish; center them.
            let m = (s.0 + s.1) * 0.5 * mic_vol;
            l += m;
            r += m;
        }
        (soft_clip(l), soft_clip(r))
    }

    /// 16-bit stereo PCM at MIX_RATE covering sample frames [n0, n1) after `t0`.
    pub fn render_pcm16(&self, t0: f64, n0: u64, n1: u64, sys_vol: f32, mic_vol: f32) -> Vec<i16> {
        let mut out = Vec::with_capacity(((n1 - n0) * 2) as usize);
        for n in n0..n1 {
            let t = t0 + n as f64 / MIX_RATE as f64;
            let (l, r) = self.mix(t, sys_vol, mic_vol);
            out.push((l * 32767.0) as i16);
            out.push((r * 32767.0) as i16);
        }
        out
    }
}

#[inline]
fn soft_clip(x: f32) -> f32 {
    if x.abs() <= 0.9 { x } else { x.signum() * (0.9 + 0.1 * ((x.abs() - 0.9) / 0.1).tanh()) }
}

/// Editor preview playback through the default output device.
pub struct Player {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

pub struct PlayParams {
    pub tracks: Tracks,
    pub start: f64,
    pub end: f64,
    pub volumes: Arc<Mutex<(f32, f32)>>,
}

impl Player {
    pub fn start(p: PlayParams) -> Player {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let thread = std::thread::Builder::new()
            .name("audio-play".into())
            .spawn(move || {
                if let Err(e) = play_thread(p, &s2) {
                    log!("audio playback failed: {e}");
                }
            })
            .ok();
        Player { stop, thread }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn play_thread(p: PlayParams, stop: &AtomicBool) -> Res<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let dev = device(eRender)?;
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).ctx("Activate")?;
        let (fmtp, fmt) = mix_format(&client)?;
        let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, fmtp, None);
        CoTaskMemFree(Some(fmtp as _));
        r.ctx("Initialize")?;
        if !(fmt.float && fmt.bits == 32) {
            return Err("unsupported output format".into());
        }
        let render: IAudioRenderClient = client.GetService().ctx("render client")?;
        let size = client.GetBufferSize().ctx("buffer size")?;
        let written = AtomicU64::new(0);
        let fill = |frames: u32| -> Res<()> {
            let buf = render.GetBuffer(frames).ctx("GetBuffer")? as *mut f32;
            let (sv, mv) = *p.volumes.lock().unwrap();
            let base = written.load(Ordering::Relaxed);
            for f in 0..frames as usize {
                let t = p.start + (base + f as u64) as f64 / fmt.rate as f64;
                let (l, r) = if t < p.end { p.tracks.mix(t, sv, mv) } else { (0.0, 0.0) };
                for c in 0..fmt.channels {
                    *buf.add(f * fmt.channels + c) = if c % 2 == 0 { l } else { r };
                }
            }
            written.fetch_add(frames as u64, Ordering::Relaxed);
            render.ReleaseBuffer(frames, 0).ctx("ReleaseBuffer")
        };
        fill(size)?;
        client.Start().ctx("Start")?;
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(10));
            let pad = client.GetCurrentPadding().unwrap_or(size);
            if pad < size {
                fill(size - pad)?;
            }
        }
        let _ = client.Stop();
        Ok(())
    }
}
