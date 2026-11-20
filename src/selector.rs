//! Full-screen picker shown before recording a window or an area: a frozen
//! snapshot of the display, dimmed, with the hovered window / dragged rectangle
//! highlighted (like the Snipping Tool).

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_SHIFT, GetKeyState};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::browser::PageFinder;
use crate::capture::{self, Capturer, Monitor, Target, WindowInfo};
use crate::d2d::{D2d, rgba};
use crate::gpu::Gpu;
use crate::ui::theme::*;
use crate::ui::{Align, Rect, Ui};
use crate::util::Res;
use crate::window::{self, CursorShape, Kind, Surface};

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Window,
    /// Like `Window`, but only browsers, and only their web page is recorded.
    Browser,
    Area,
}

pub enum Outcome {
    Pending,
    Cancel,
    Done(Target),
}

pub struct Selector {
    surface: Surface,
    snapshot: Option<ID2D1Bitmap1>,
    monitor: Monitor,
    mode: Mode,
    /// Windows on this display, front-most first, in overlay (display-relative) pixels.
    windows: Vec<(WindowInfo, Rect)>,
    /// Browser mode: each window's web page (same order), if it has one.
    pages: Vec<Option<(RECT, Rect)>>,
    drag: Option<(f32, f32)>,
    ui: Ui,
    t0: std::time::Instant,
}

/// Grabs one frame of the display so the picker can show it frozen.
fn snapshot(gpu: &Gpu, d2d: &D2d, m: &Monitor) -> Res<ID2D1Bitmap1> {
    let mut cap = Capturer::new(gpu, &Target::Display(m.clone()))?;
    let tex = gpu.texture(cap.width, cap.height, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET, false)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    while !cap.copy_latest(gpu, &tex) {
        if std::time::Instant::now() > deadline {
            return Err("display snapshot timed out".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    d2d.source_for(&tex)
}

impl Selector {
    pub fn new(gpu: &Gpu, d2d: &D2d, monitor: &Monitor, mode: Mode) -> Res<Selector> {
        let snap = snapshot(gpu, d2d, monitor).ok();
        let r = monitor.rect;
        let (mw, mh) = (monitor.width() as f32, monitor.height() as f32);
        // Screen rect -> overlay rect, clipped to this display.
        let local = |s: &RECT| {
            let x0 = ((s.left - r.left) as f32).max(0.0);
            let y0 = ((s.top - r.top) as f32).max(0.0);
            let x1 = ((s.right - r.left) as f32).min(mw);
            let y1 = ((s.bottom - r.top) as f32).min(mh);
            (x1 - x0 > 40.0 && y1 - y0 > 40.0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
        };
        let windows: Vec<(WindowInfo, Rect)> = if mode == Mode::Area {
            Vec::new()
        } else {
            capture::windows().into_iter().filter_map(|w| local(&w.rect).map(|lr| (w, lr))).collect()
        };
        let pages = if mode == Mode::Browser {
            windows.iter().map(|(w, _)| PageFinder::new(w.hwnd).rect().and_then(|p| local(&p).map(|lr| (p, lr)))).collect()
        } else {
            Vec::new()
        };
        let surface = Surface::create(gpu, "Framecast picker", Kind::Overlay, r.left, r.top, r.right - r.left, r.bottom - r.top)?;
        surface.show(SW_SHOW);
        unsafe {
            let _ = SetForegroundWindow(surface.hwnd);
        }
        Ok(Selector {
            surface,
            snapshot: snap,
            monitor: monitor.clone(),
            mode,
            windows,
            pages,
            drag: None,
            ui: Ui::new(),
            t0: std::time::Instant::now(),
        })
    }

    fn hovered_window(&self, x: f32, y: f32) -> Option<usize> {
        self.windows.iter().position(|(_, r)| r.contains(x, y))
    }

    fn area_rect(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        let (mw, mh) = (self.monitor.width() as f32, self.monitor.height() as f32);
        let mut w = (x1 - x0).abs();
        let mut h = (y1 - y0).abs();
        if unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0 {
            // Shift: lock to 16:9, the most common video shape.
            h = w * 9.0 / 16.0;
            if h > (y1 - y0).abs().max(1.0) * 4.0 {
                h = (y1 - y0).abs();
                w = h * 16.0 / 9.0;
            }
        }
        let x = if x1 < x0 { x0 - w } else { x0 };
        let y = if y1 < y0 { y0 - h } else { y0 };
        let x = x.clamp(0.0, mw);
        let y = y.clamp(0.0, mh);
        Rect::new(x, y, w.min(mw - x), h.min(mh - y))
    }

    pub fn frame(&mut self, d2d: &D2d) -> Res<Outcome> {
        let inp = window::take_input(self.surface.hwnd);
        if inp.resized {
            self.surface.handle_resize();
        }
        self.ui.begin_frame(inp, self.surface.dpi_scale(), self.t0.elapsed().as_secs_f64());
        if self.ui.key(VK_ESCAPE.0) || self.ui.inp.close {
            return Ok(Outcome::Cancel);
        }
        let (mx, my) = (self.ui.inp.mx, self.ui.inp.my);
        let mut outcome = Outcome::Pending;

        // Interaction. The flag says whether clicking would pick it.
        let selection: Option<(Rect, String, bool)> = match self.mode {
            Mode::Window => {
                let hov = self.hovered_window(mx, my);
                if self.ui.inp.released {
                    if let Some(i) = hov {
                        outcome = Outcome::Done(Target::Window(self.windows[i].0.clone()));
                    }
                }
                hov.map(|i| {
                    let (w, r) = &self.windows[i];
                    (*r, format!("{}   {}×{}", w.title, w.rect.right - w.rect.left, w.rect.bottom - w.rect.top), true)
                })
            }
            Mode::Browser => {
                let hov = self.hovered_window(mx, my);
                if self.ui.inp.released {
                    if let Some(i) = hov.filter(|&i| self.pages[i].is_some()) {
                        outcome = Outcome::Done(Target::Browser(self.windows[i].0.clone()));
                    }
                }
                hov.map(|i| {
                    let (w, r) = &self.windows[i];
                    match &self.pages[i] {
                        Some((p, lr)) => (*lr, format!("{}   ·   page {}×{}", w.title, p.right - p.left, p.bottom - p.top), true),
                        None => (*r, "Not a browser: no web page found in this window".to_string(), false),
                    }
                })
            }
            Mode::Area => {
                if self.ui.inp.pressed {
                    self.drag = Some((mx, my));
                }
                let sel = self.drag.map(|(x0, y0)| self.area_rect(x0, y0, mx, my));
                if self.ui.inp.released {
                    if let Some(r) = sel.filter(|r| r.w >= 64.0 && r.h >= 64.0) {
                        let (x, y) = (r.x.round() as i32, r.y.round() as i32);
                        // Even size: H.264 requirement.
                        let (w, h) = ((r.w.round() as i32) & !1, (r.h.round() as i32) & !1);
                        outcome = Outcome::Done(Target::Area(self.monitor.clone(), RECT { left: x, top: y, right: x + w, bottom: y + h }));
                    }
                    self.drag = None;
                }
                sel.map(|r| (r, format!("{} × {}", (r.w.round() as i32) & !1, (r.h.round() as i32) & !1), true))
            }
        };

        // Drawing.
        self.surface.begin(d2d)?;
        let ui = &mut self.ui;
        let full = Rect::new(0.0, 0.0, self.surface.w as f32, self.surface.h as f32);
        unsafe {
            d2d.dc.Clear(Some(&rgba(0x000000, 1.0)));
            if let Some(b) = &self.snapshot {
                d2d.dc.DrawBitmap(b, Some(&full.d2()), 1.0, D2D1_INTERPOLATION_MODE_LINEAR, None, None);
            }
        }
        ui.fill(d2d, full, 0.0, rgba(0x000000, 0.55));
        if let Some((r, label, ok)) = &selection {
            let color = if *ok { ACCENT } else { FAINT };
            if let Some(b) = self.snapshot.as_ref().filter(|_| *ok) {
                unsafe { d2d.dc.DrawBitmap(b, Some(&r.d2()), 1.0, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, Some(&r.d2()), None) };
            }
            ui.stroke(d2d, r.inset(-ui.px(2.0)), 0.0, rgba(color, 1.0), ui.px(3.0));
            let tw = ui.measure(d2d, label, 13.0, DWRITE_FONT_WEIGHT_SEMI_BOLD).min(ui.px(520.0)) + ui.px(24.0);
            let th = ui.px(30.0);
            let ly = if r.y - th - ui.px(10.0) > 0.0 { r.y - th - ui.px(10.0) } else { r.y + ui.px(10.0) };
            let lr = Rect::new(r.x.max(ui.px(8.0)), ly, tw, th);
            ui.fill(d2d, lr, ui.px(8.0), rgba(color, 1.0));
            ui.text(d2d, lr.inset_xy(ui.px(12.0), 0.0), label, 13.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, rgba(0xffffff, 1.0), Align::Left);
        } else if self.mode == Mode::Area && ui.inp.inside {
            ui.line(d2d, mx, 0.0, mx, full.h, rgba(0xffffff, 0.35), 1.0);
            ui.line(d2d, 0.0, my, full.w, my, rgba(0xffffff, 0.35), 1.0);
        }

        // Instructions.
        let hint = match self.mode {
            Mode::Window => "Click a window to record it   ·   Esc to cancel",
            Mode::Browser => "Click a browser window: only the web page is recorded, no tabs or address bar   ·   Esc to cancel",
            Mode::Area => "Drag to select an area   ·   Hold Shift for 16:9   ·   Esc to cancel",
        };
        let hw = ui.measure(d2d, hint, 14.0, DWRITE_FONT_WEIGHT_SEMI_BOLD) + ui.px(40.0);
        let hr = Rect::new(full.cx() - hw * 0.5, ui.px(28.0), hw, ui.px(44.0));
        ui.fill(d2d, hr, ui.px(22.0), rgba(0x15171D, 0.94));
        ui.stroke(d2d, hr, ui.px(22.0), rgba(0xffffff, 0.12), 1.0);
        ui.text(d2d, hr, hint, 14.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, rgba(TEXT, 1.0), Align::Center);
        let empty = match self.mode {
            Mode::Window => self.windows.is_empty().then_some("No windows on this display"),
            Mode::Browser => self.pages.iter().all(Option::is_none).then_some("No browser windows on this display"),
            Mode::Area => None,
        };
        if let Some(msg) = empty {
            let r = Rect::new(full.cx() - ui.px(200.0), full.cy() - ui.px(20.0), ui.px(400.0), ui.px(40.0));
            ui.text(d2d, r, msg, 16.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, rgba(TEXT, 1.0), Align::Center);
        }
        let pickable = selection.as_ref().is_some_and(|s| s.2);
        ui.cursor = if self.mode != Mode::Area && pickable { CursorShape::Hand } else { CursorShape::Cross };
        ui.end_frame();
        window::set_cursor(self.surface.hwnd, ui.cursor);
        self.surface.end(d2d)?;
        Ok(outcome)
    }
}
