//! "Identify displays": a big number badge in the corner of every monitor for a
//! few seconds (like Windows Settings > Display > Identify), so the Display
//! choice on the home screen can be matched to a physical screen.
//! The numbers match the home screen's list; clicking a badge selects that display.

use std::time::{Duration, Instant};

use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE;

use crate::capture::Monitor;
use crate::d2d::{D2d, rgba};
use crate::gpu::Gpu;
use crate::ui::theme::*;
use crate::ui::{Align, Rect, Ui};
use crate::util::Res;
use crate::window::{self, CursorShape, Input, Kind, Surface};

const SHOW_FOR: Duration = Duration::from_secs(3);
/// Badge size and distance from the work-area corner, in DIPs.
const BADGE_W: f32 = 200.0;
const BADGE_H: f32 = 184.0;
const MARGIN: f32 = 40.0;

struct Badge {
    surface: Surface,
    ui: Ui,
    index: usize,
    label: String,
}

pub struct Identify {
    badges: Vec<Badge>,
    until: Instant,
    /// Selection the badges were last drawn with (None = needs a redraw).
    drawn: Option<usize>,
}

fn monitor_scale(m: &Monitor) -> f32 {
    let (mut x, mut y) = (96u32, 96u32);
    unsafe {
        if GetDpiForMonitor(HMONITOR(m.handle as _), MDT_EFFECTIVE_DPI, &mut x, &mut y).is_err() {
            x = 96;
        }
    }
    x.max(96) as f32 / 96.0
}

impl Identify {
    pub fn show(gpu: &Gpu, monitors: &[Monitor]) -> Res<Identify> {
        let mut badges = Vec::new();
        for (index, m) in monitors.iter().enumerate() {
            let s = monitor_scale(m);
            let (w, h, margin) = ((BADGE_W * s) as i32, (BADGE_H * s) as i32, (MARGIN * s) as i32);
            // Bottom-left of the work area, like Windows' own identify numbers.
            let x = m.work.left + margin;
            let y = (m.work.bottom - h - margin).max(m.work.top);
            let surface = Surface::create(gpu, "Framecast display number", Kind::Floating, x, y, w, h)?;
            surface.show(SW_SHOWNOACTIVATE);
            let label = format!("{}×{}{}", m.width(), m.height(), if m.primary { "  ·  main" } else { "" });
            badges.push(Badge { surface, ui: Ui::new(), index, label });
        }
        Ok(Identify { badges, until: Instant::now() + SHOW_FOR, drawn: None })
    }

    pub fn expired(&self) -> bool {
        Instant::now() >= self.until
    }

    /// Handles badge input and redraws when needed. Returns the display index
    /// of a clicked badge.
    pub fn frame(&mut self, d2d: &D2d, selected: usize, time: f64) -> Res<Option<usize>> {
        let mut clicked = None;
        let mut redraw = self.drawn != Some(selected);
        for b in &mut self.badges {
            let inp = window::take_input(b.surface.hwnd);
            if inp.resized {
                b.surface.handle_resize();
                redraw = true;
            }
            if inp.released && inp.inside {
                clicked = Some(b.index);
            }
            window::set_cursor(b.surface.hwnd, CursorShape::Hand);
        }
        if let Some(i) = clicked {
            // Keep the badges up a moment so the new selection is visible.
            self.until = self.until.max(Instant::now() + Duration::from_millis(1500));
            if i != selected {
                redraw = true;
            }
        }
        let selected = clicked.unwrap_or(selected);
        if redraw {
            for b in &mut self.badges {
                b.draw(d2d, b.index == selected, time)?;
            }
            self.drawn = Some(selected);
        }
        Ok(clicked)
    }
}

impl Badge {
    fn draw(&mut self, d2d: &D2d, selected: bool, time: f64) -> Res<()> {
        let s = self.surface.dpi_scale();
        self.ui.begin_frame(Input::default(), s, time);
        self.surface.begin(d2d)?;
        let ui = &mut self.ui;
        let full = Rect::new(0.0, 0.0, self.surface.w as f32, self.surface.h as f32);
        let (bg, fg, sub) = if selected { (ACCENT, 0xFFFFFF, 0xE6E0FF) } else { (PANEL, TEXT, MUTED) };
        unsafe { d2d.dc.Clear(Some(&rgba(bg, 1.0))) };
        if !selected {
            ui.stroke(d2d, full.inset(0.5), 0.0, rgba(BORDER, 1.0), 1.0);
        }
        let body = full.inset_xy(ui.px(12.0), ui.px(10.0));
        let (num, rest) = body.cut_top(ui.px(116.0));
        let (res, rest) = rest.cut_top(ui.px(22.0));
        ui.text(d2d, num, &(self.index + 1).to_string(), 92.0, DWRITE_FONT_WEIGHT_BOLD, rgba(fg, 1.0), Align::Center);
        ui.text(d2d, res, &self.label, 13.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, rgba(fg, 1.0), Align::Center);
        let hint = if selected { "Selected for recording" } else { "Click to select" };
        ui.text(d2d, rest, hint, 11.0, DWRITE_FONT_WEIGHT_NORMAL, rgba(sub, 1.0), Align::Center);
        ui.end_frame();
        self.surface.end(d2d)
    }
}
