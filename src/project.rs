//! Project data: what was recorded (cursor, clicks, keys) and how it should look
//! (style + zoom segments). Also the deterministic motion model: camera and cursor
//! paths are simulated once with springs and then sampled at any time, so scrubbing
//! and export always agree frame-for-frame.

use std::path::Path;

use crate::input::{Click, Key};
use crate::json::Json;
use crate::util::{Ctx, Res};

#[derive(Clone, Copy, Debug)]
pub struct CursorSample {
    pub t: f64,
    pub x: f32,
    pub y: f32,
    pub kind: i32,
}

#[derive(Clone)]
pub struct Recording {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub duration: f64,
    pub dpi_scale: f32,
    pub cursor: Vec<CursorSample>,
    pub clicks: Vec<Click>,
    pub keys: Vec<Key>,
}

impl Recording {
    pub fn save(&self, dir: &Path) -> Res<()> {
        let mut cur = Vec::with_capacity(self.cursor.len() * 4);
        for c in &self.cursor {
            cur.push(Json::Num((c.t * 1000.0).round() / 1000.0));
            cur.push(Json::Num(c.x as f64));
            cur.push(Json::Num(c.y as f64));
            cur.push(Json::Num(c.kind as f64));
        }
        let mut clicks = Vec::new();
        for c in &self.clicks {
            clicks.push(Json::Arr(vec![
                Json::Num((c.t * 1000.0).round() / 1000.0),
                Json::Num(c.x as f64),
                Json::Num(c.y as f64),
                Json::Num(c.button as f64),
                Json::Bool(c.down),
            ]));
        }
        let mut keys = Vec::new();
        for k in &self.keys {
            let mut o = Json::obj();
            o.set("t", (k.t * 1000.0).round() / 1000.0).set("label", k.label.as_str());
            keys.push(o);
        }
        let mut j = Json::obj();
        j.set("version", 1)
            .set("width", self.width)
            .set("height", self.height)
            .set("fps", self.fps)
            .set("duration", self.duration)
            .set("dpi_scale", self.dpi_scale)
            .set("cursor", cur)
            .set("clicks", clicks)
            .set("keys", keys);
        std::fs::write(dir.join("events.json"), j.to_string()).ctx("write events.json")
    }

    pub fn load(dir: &Path) -> Res<Recording> {
        let s = std::fs::read_to_string(dir.join("events.json")).ctx("read events.json")?;
        let j = Json::parse(&s)?;
        let flat: Vec<f64> = j.arr("cursor").iter().filter_map(|v| v.as_f64()).collect();
        let cursor = flat
            .chunks_exact(4)
            .map(|c| CursorSample { t: c[0], x: c[1] as f32, y: c[2] as f32, kind: c[3] as i32 })
            .collect();
        let clicks = j
            .arr("clicks")
            .iter()
            .filter_map(|c| match c {
                Json::Arr(a) if a.len() >= 5 => Some(Click {
                    t: a[0].as_f64()?,
                    x: a[1].as_f64()? as f32,
                    y: a[2].as_f64()? as f32,
                    button: a[3].as_f64()? as u8,
                    down: a[4] == Json::Bool(true),
                }),
                _ => None,
            })
            .collect();
        let keys = j
            .arr("keys")
            .iter()
            .map(|k| Key { t: k.f64_or("t", 0.0), label: k.str_or("label", "").to_string() })
            .collect();
        Ok(Recording {
            width: j.f64_or("width", 1920.0) as u32,
            height: j.f64_or("height", 1080.0) as u32,
            fps: j.f64_or("fps", 60.0) as u32,
            duration: j.f64_or("duration", 0.0),
            dpi_scale: j.f32_or("dpi_scale", 1.0),
            cursor,
            clicks,
            keys,
        })
    }

    /// Raw (unsmoothed) cursor position at time t, in source pixels.
    pub fn cursor_at(&self, t: f64) -> (f32, f32, i32) {
        let c = &self.cursor;
        if c.is_empty() {
            return (self.width as f32 / 2.0, self.height as f32 / 2.0, -1);
        }
        let i = c.partition_point(|s| s.t <= t);
        if i == 0 {
            return (c[0].x, c[0].y, c[0].kind);
        }
        if i >= c.len() {
            let l = c[c.len() - 1];
            return (l.x, l.y, l.kind);
        }
        let (a, b) = (c[i - 1], c[i]);
        let f = ((t - a.t) / (b.t - a.t).max(1e-6)) as f32;
        (a.x + (b.x - a.x) * f, a.y + (b.y - a.y) * f, a.kind)
    }
}

/// Background presets: (name, top-left color, bottom-right color).
pub const BACKGROUNDS: &[(&str, u32, u32)] = &[
    ("Dusk", 0x6E48AA, 0xF0638E),
    ("Ocean", 0x1CB5E0, 0x1A2A6C),
    ("Mint", 0x43E97B, 0x38A1D6),
    ("Peach", 0xFFB88C, 0xDE6262),
    ("Aurora", 0x00C9A7, 0x845EC2),
    ("Sunset", 0xF7971E, 0xC0392B),
    ("Slate", 0x434343, 0x101010),
    ("Paper", 0xF5F7FA, 0xC3CFE2),
];

pub const ASPECTS: &[(&str, f32)] = &[("Source", 0.0), ("16:9", 16.0 / 9.0), ("4:3", 4.0 / 3.0), ("1:1", 1.0), ("9:16", 9.0 / 16.0)];
pub const RESOLUTIONS: &[(&str, u32)] = &[("720p", 720), ("1080p", 1080), ("1440p", 1440), ("4K", 2160)];
pub const CLICK_EFFECTS: &[&str] = &["None", "Ripple", "Spotlight"];

#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    pub background: usize,
    pub padding: f32,      // fraction of the shorter canvas side
    pub radius: f32,       // corner radius, fraction of the shorter content side
    pub shadow: f32,       // 0..1
    pub aspect: usize,     // index into ASPECTS
    pub resolution: usize, // index into RESOLUTIONS
    pub cursor_size: f32,  // multiplier over the recorded size
    pub smoothing: f32,    // 0 = raw, 1 = very floaty
    pub hide_idle: bool,
    pub click_effect: usize,
    pub show_keys: bool,
    pub zoom_speed: f32, // 0 slow .. 1 fast
    pub motion_blur: bool,
    pub sys_volume: f32,
    pub mic_volume: f32,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            background: 0,
            padding: 0.06,
            radius: 0.018,
            shadow: 0.6,
            aspect: 1,
            resolution: 1,
            cursor_size: 1.6,
            smoothing: 0.55,
            hide_idle: true,
            click_effect: 1,
            show_keys: true,
            zoom_speed: 0.5,
            motion_blur: true,
            sys_volume: 1.0,
            mic_volume: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ZoomSeg {
    pub start: f64,
    pub end: f64,
    pub scale: f32,
}

#[derive(Clone)]
pub struct Project {
    pub dir: std::path::PathBuf,
    pub rec: Recording,
    pub style: Style,
    pub zooms: Vec<ZoomSeg>,
    pub trim_start: f64,
    pub trim_end: f64,
}

impl Project {
    pub fn open(dir: &Path) -> Res<Project> {
        let rec = Recording::load(dir)?;
        let mut p = Project {
            dir: dir.to_path_buf(),
            trim_start: 0.0,
            trim_end: rec.duration,
            zooms: Vec::new(),
            style: Style::default(),
            rec,
        };
        match std::fs::read_to_string(dir.join("project.json")).ok().and_then(|s| Json::parse(&s).ok()) {
            Some(j) => p.apply_json(&j),
            None => p.zooms = auto_zooms(&p.rec, 2.0),
        }
        Ok(p)
    }

    fn apply_json(&mut self, j: &Json) {
        let d = Style::default();
        if let Some(s) = j.get("style") {
            let idx = |k: &str, def: usize, n: usize| (s.f64_or(k, def as f64) as usize).min(n - 1);
            self.style = Style {
                background: idx("background", d.background, BACKGROUNDS.len()),
                padding: s.f32_or("padding", d.padding).clamp(0.0, 0.25),
                radius: s.f32_or("radius", d.radius).clamp(0.0, 0.1),
                shadow: s.f32_or("shadow", d.shadow).clamp(0.0, 1.0),
                aspect: idx("aspect", d.aspect, ASPECTS.len()),
                resolution: idx("resolution", d.resolution, RESOLUTIONS.len()),
                cursor_size: s.f32_or("cursor_size", d.cursor_size).clamp(0.5, 4.0),
                smoothing: s.f32_or("smoothing", d.smoothing).clamp(0.0, 1.0),
                hide_idle: s.bool_or("hide_idle", d.hide_idle),
                click_effect: idx("click_effect", d.click_effect, CLICK_EFFECTS.len()),
                show_keys: s.bool_or("show_keys", d.show_keys),
                zoom_speed: s.f32_or("zoom_speed", d.zoom_speed).clamp(0.0, 1.0),
                motion_blur: s.bool_or("motion_blur", d.motion_blur),
                sys_volume: s.f32_or("sys_volume", d.sys_volume).clamp(0.0, 2.0),
                mic_volume: s.f32_or("mic_volume", d.mic_volume).clamp(0.0, 2.0),
            };
        }
        self.zooms = j
            .arr("zooms")
            .iter()
            .map(|z| ZoomSeg { start: z.f64_or("start", 0.0), end: z.f64_or("end", 0.0), scale: z.f32_or("scale", 2.0).clamp(1.1, 5.0) })
            .filter(|z| z.end > z.start)
            .collect();
        self.trim_start = j.f64_or("trim_start", 0.0).clamp(0.0, self.rec.duration);
        self.trim_end = j.f64_or("trim_end", self.rec.duration).clamp(self.trim_start, self.rec.duration);
        if self.trim_end - self.trim_start < 0.1 {
            self.trim_start = 0.0;
            self.trim_end = self.rec.duration;
        }
    }

    pub fn save(&self) -> Res<()> {
        let s = &self.style;
        let mut st = Json::obj();
        st.set("background", s.background as u32)
            .set("padding", s.padding)
            .set("radius", s.radius)
            .set("shadow", s.shadow)
            .set("aspect", s.aspect as u32)
            .set("resolution", s.resolution as u32)
            .set("cursor_size", s.cursor_size)
            .set("smoothing", s.smoothing)
            .set("hide_idle", s.hide_idle)
            .set("click_effect", s.click_effect as u32)
            .set("show_keys", s.show_keys)
            .set("zoom_speed", s.zoom_speed)
            .set("motion_blur", s.motion_blur)
            .set("sys_volume", s.sys_volume)
            .set("mic_volume", s.mic_volume);
        let zooms = self
            .zooms
            .iter()
            .map(|z| {
                let mut o = Json::obj();
                o.set("start", z.start).set("end", z.end).set("scale", z.scale);
                o
            })
            .collect::<Vec<_>>();
        let mut j = Json::obj();
        j.set("version", 1)
            .set("style", st)
            .set("zooms", zooms)
            .set("trim_start", self.trim_start)
            .set("trim_end", self.trim_end);
        std::fs::write(self.dir.join("project.json"), j.to_string()).ctx("write project.json")
    }

    /// Output canvas size for the current style.
    pub fn canvas_size(&self) -> (u32, u32) {
        let s = &self.style;
        let target_h = RESOLUTIONS[s.resolution].1 as f32;
        let aspect = match ASPECTS[s.aspect].1 {
            a if a > 0.0 => a,
            _ => self.rec.width as f32 / self.rec.height as f32,
        };
        // Portrait formats keep the long side at the chosen resolution's width.
        let (w, h) = if aspect >= 1.0 { (target_h * aspect, target_h) } else { (target_h, target_h / aspect) };
        let even = |v: f32| ((v.round() as u32) + 1) & !1;
        (even(w), even(h))
    }
}

/// Builds zoom segments around clusters of clicks (the "auto zoom" feature).
pub fn auto_zooms(rec: &Recording, scale: f32) -> Vec<ZoomSeg> {
    let mut segs: Vec<ZoomSeg> = Vec::new();
    for c in rec.clicks.iter().filter(|c| c.down && c.button == 0) {
        let s = (c.t - 0.9).max(0.0);
        let e = (c.t + 2.6).min(rec.duration);
        if let Some(last) = segs.last_mut() {
            // Merge clicks that are close in time into one continuous zoom.
            if s <= last.end + 1.2 {
                last.end = last.end.max(e);
                continue;
            }
        }
        segs.push(ZoomSeg { start: s, end: e, scale });
    }
    segs.retain(|z| z.end - z.start >= 1.0);
    segs
}

/// Simulated motion for the whole recording, sampled at a fixed rate.
pub struct Motion {
    rate: f64,
    // per step: camera center x/y (normalized 0..1), camera scale, cursor x/y (source px), cursor alpha
    cam: Vec<[f32; 3]>,
    cur: Vec<[f32; 3]>,
}

#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub cam_x: f32,
    pub cam_y: f32,
    pub cam_scale: f32,
    pub cursor_x: f32,
    pub cursor_y: f32,
    pub cursor_alpha: f32,
    pub cursor_kind: i32,
    pub pressed: f32, // 0..1 button press amount
    pub cam_speed: f32,
}

/// Implicit critically-damped spring step (stable for any dt).
fn spring(x: &mut f32, v: &mut f32, target: f32, omega: f32, dt: f32) {
    let f = 1.0 + 2.0 * dt * omega;
    let oo = omega * omega;
    let hoo = dt * oo;
    let hhoo = dt * hoo;
    let inv = 1.0 / (f + hhoo);
    let nx = (f * *x + dt * *v + hhoo * target) * inv;
    let nv = (*v + hoo * (target - *x)) * inv;
    *x = nx;
    *v = nv;
}

impl Motion {
    pub fn build(p: &Project) -> Motion {
        let rec = &p.rec;
        let s = &p.style;
        let rate = 120.0;
        let n = (rec.duration * rate).ceil() as usize + 2;
        let dt = (1.0 / rate) as f32;
        let (w, h) = (rec.width as f32, rec.height as f32);

        // Cursor: spring toward the raw path. Higher smoothing = lower stiffness.
        let omega_cursor = 60.0 * (1.0 - s.smoothing).powf(1.6) + 9.0;
        let mut cur = Vec::with_capacity(n);
        let (x0, y0, _) = rec.cursor_at(0.0);
        let (mut cx, mut cy, mut vx, mut vy) = (x0, y0, 0.0f32, 0.0f32);
        let mut last_move = 0.0f64;
        let mut alpha = 1.0f32;
        let mut prev_raw = (x0, y0);
        let mut click_i = 0;
        for i in 0..n {
            let t = i as f64 / rate;
            let (rx, ry, kind) = rec.cursor_at(t);
            if s.smoothing <= 0.01 {
                cx = rx;
                cy = ry;
            } else {
                spring(&mut cx, &mut vx, rx, omega_cursor, dt);
                spring(&mut cy, &mut vy, ry, omega_cursor, dt);
            }
            if (rx - prev_raw.0).abs() + (ry - prev_raw.1).abs() > 1.5 {
                last_move = t;
            }
            prev_raw = (rx, ry);
            while click_i < rec.clicks.len() && rec.clicks[click_i].t <= t {
                last_move = t;
                click_i += 1;
            }
            let idle = s.hide_idle && t - last_move > 2.5;
            let target_alpha = if kind < 0 || idle { 0.0 } else { 1.0 };
            let step = dt / 0.25;
            alpha = if target_alpha > alpha { (alpha + step).min(1.0) } else { (alpha - step).max(0.0) };
            cur.push([cx, cy, alpha]);
        }

        // Camera: zoom springs toward the active segment's scale and follows the
        // smoothed cursor with a dead zone so the view doesn't jitter.
        let omega_zoom = 2.5 + 5.0 * s.zoom_speed;
        let omega_pan = omega_zoom * 0.8;
        let mut cam = Vec::with_capacity(n);
        let (mut sc, mut vs) = (1.0f32, 0.0f32);
        let (mut px, mut py, mut vpx, mut vpy) = (0.5f32, 0.5f32, 0.0f32, 0.0f32);
        let (mut tx, mut ty) = (0.5f32, 0.5f32);
        let mut was_zoomed = false;
        for (i, c) in cur.iter().enumerate() {
            let t = i as f64 / rate;
            // Start zooming slightly before the segment so the zoom lands on the click.
            let lead = 0.25;
            let seg = p.zooms.iter().find(|z| t + lead >= z.start && t < z.end);
            let target_scale = seg.map_or(1.0, |z| z.scale);
            let (ux, uy) = (c[0] / w, c[1] / h);
            if let Some(z) = seg {
                if !was_zoomed {
                    // Aim at where the first click of this segment happens (or the cursor).
                    let first = rec.clicks.iter().find(|k| k.down && k.t >= z.start && k.t <= z.end);
                    (tx, ty) = first.map_or((ux, uy), |k| (k.x / w, k.y / h));
                }
                let half = 0.5 / z.scale;
                let dead = half * 0.55;
                if ux < tx - dead {
                    tx = ux + dead;
                } else if ux > tx + dead {
                    tx = ux - dead;
                }
                if uy < ty - dead {
                    ty = uy + dead;
                } else if uy > ty + dead {
                    ty = uy - dead;
                }
                tx = tx.clamp(half, 1.0 - half);
                ty = ty.clamp(half, 1.0 - half);
                was_zoomed = true;
            } else {
                was_zoomed = false;
                tx = 0.5;
                ty = 0.5;
            }
            spring(&mut sc, &mut vs, target_scale, omega_zoom, dt);
            spring(&mut px, &mut vpx, tx, omega_pan, dt);
            spring(&mut py, &mut vpy, ty, omega_pan, dt);
            cam.push([px, py, sc.max(1.0)]);
        }
        Motion { rate, cam, cur }
    }

    pub fn at(&self, rec: &Recording, t: f64) -> Frame {
        let n = self.cam.len();
        let fi = (t * self.rate).clamp(0.0, (n - 1) as f64);
        let i = (fi.floor() as usize).min(n.saturating_sub(2));
        let f = (fi - i as f64) as f32;
        let mix = |a: &[f32; 3], b: &[f32; 3], k: usize| a[k] + (b[k] - a[k]) * f;
        let (c0, c1) = (&self.cam[i], &self.cam[(i + 1).min(n - 1)]);
        let (u0, u1) = (&self.cur[i], &self.cur[(i + 1).min(n - 1)]);
        let (_, _, kind) = rec.cursor_at(t);
        let r = self.rate as f32;
        let pressed = rec
            .clicks
            .iter()
            .filter(|c| c.down && c.t <= t + 0.08 && c.t > t - 0.35)
            .map(|c| {
                let d = (t - c.t) as f32;
                if d < 0.0 { 1.0 + d / 0.08 } else { 1.0 - d / 0.35 }
            })
            .fold(0.0f32, f32::max);
        let cam_scale = mix(c0, c1, 2);
        let pan = ((c1[0] - c0[0]).powi(2) + (c1[1] - c0[1]).powi(2)).sqrt() * r * cam_scale;
        Frame {
            cam_x: mix(c0, c1, 0),
            cam_y: mix(c0, c1, 1),
            cam_scale,
            cursor_x: mix(u0, u1, 0),
            cursor_y: mix(u0, u1, 1),
            cursor_alpha: mix(u0, u1, 2),
            cursor_kind: kind,
            pressed: pressed.clamp(0.0, 1.0),
            cam_speed: pan + (c1[2] - c0[2]).abs() * r,
        }
    }
}

/// Lists recordings in the app folder, newest first.
pub fn recent_projects() -> Vec<(std::path::PathBuf, String, f64)> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(crate::util::app_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.join("events.json").exists() && p.join("raw.mp4").exists() {
                let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                let dur = std::fs::read_to_string(p.join("events.json"))
                    .ok()
                    .and_then(|s| {
                        // Cheap: duration is near the start of the file; parse all only if needed.
                        Json::parse(&s).ok().map(|j| j.f64_or("duration", 0.0))
                    })
                    .unwrap_or(0.0);
                out.push((p, name, dur));
            }
        }
    }
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out
}
