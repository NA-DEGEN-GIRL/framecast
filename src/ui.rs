//! Tiny immediate-mode widget toolkit drawn with Direct2D.

use std::collections::HashMap;

use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows_numerics::Vector2;

use crate::d2d::{D2d, ellipse, rect, rgba, rrect};
use crate::window::{CursorShape, Input};

pub mod theme {
    pub const BG: u32 = 0x0E1014;
    pub const PANEL: u32 = 0x161920;
    pub const PANEL2: u32 = 0x1E222B;
    pub const HOVER: u32 = 0x272C37;
    pub const BORDER: u32 = 0x2A2F3B;
    pub const TEXT: u32 = 0xE9EBF1;
    pub const MUTED: u32 = 0x8C93A6;
    pub const FAINT: u32 = 0x5A6172;
    pub const ACCENT: u32 = 0x7C5CFF;
    pub const ACCENT_HI: u32 = 0x9278FF;
    pub const RED: u32 = 0xFF4D5E;
    pub const RED_HI: u32 = 0xFF6B79;
    pub const GREEN: u32 = 0x3DDC97;
}
use theme::*;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w: w.max(0.0), h: h.max(0.0) }
    }
    pub fn r(&self) -> f32 {
        self.x + self.w
    }
    pub fn b(&self) -> f32 {
        self.y + self.h
    }
    pub fn cx(&self) -> f32 {
        self.x + self.w * 0.5
    }
    pub fn cy(&self) -> f32 {
        self.y + self.h * 0.5
    }
    pub fn inset(&self, d: f32) -> Rect {
        Rect::new(self.x + d, self.y + d, self.w - 2.0 * d, self.h - 2.0 * d)
    }
    pub fn inset_xy(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w - 2.0 * dx, self.h - 2.0 * dy)
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.r() && y >= self.y && y < self.b()
    }
    /// Cuts `h` from the top; returns (top, rest).
    pub fn cut_top(&self, h: f32) -> (Rect, Rect) {
        let h = h.min(self.h);
        (Rect::new(self.x, self.y, self.w, h), Rect::new(self.x, self.y + h, self.w, self.h - h))
    }
    pub fn cut_bottom(&self, h: f32) -> (Rect, Rect) {
        let h = h.min(self.h);
        (Rect::new(self.x, self.b() - h, self.w, h), Rect::new(self.x, self.y, self.w, self.h - h))
    }
    pub fn cut_left(&self, w: f32) -> (Rect, Rect) {
        let w = w.min(self.w);
        (Rect::new(self.x, self.y, w, self.h), Rect::new(self.x + w, self.y, self.w - w, self.h))
    }
    pub fn cut_right(&self, w: f32) -> (Rect, Rect) {
        let w = w.min(self.w);
        (Rect::new(self.r() - w, self.y, w, self.h), Rect::new(self.x, self.y, self.w - w, self.h))
    }
    pub fn d2(&self) -> windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F {
        rect(self.x, self.y, self.r(), self.b())
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, PartialEq)]
pub enum ButtonKind {
    Primary,
    Danger,
    Secondary,
    Ghost,
}

pub fn id(s: &str) -> u64 {
    // FNV-1a: stable widget ids from labels.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub fn idx(s: &str, i: usize) -> u64 {
    id(s) ^ (i as u64).wrapping_mul(0x9E3779B97F4A7C15)
}

pub struct Ui {
    pub inp: Input,
    pub s: f32, // DPI scale
    pub active: Option<u64>,
    pub hot: Option<u64>,
    /// Set when a widget starts changing a value (used for undo snapshots).
    pub edit_started: bool,
    pub cursor: CursorShape,
    fonts: HashMap<(u32, i32), IDWriteTextFormat>,
    pub time: f64,
    drag_origin: (f32, f32),
    drag_value: f32,
}

impl Ui {
    pub fn new() -> Ui {
        Ui {
            inp: Input::default(),
            s: 1.0,
            active: None,
            hot: None,
            edit_started: false,
            cursor: CursorShape::Arrow,
            fonts: HashMap::new(),
            time: 0.0,
            drag_origin: (0.0, 0.0),
            drag_value: 0.0,
        }
    }

    pub fn begin_frame(&mut self, inp: Input, scale: f32, time: f64) {
        if (scale - self.s).abs() > 0.01 {
            self.fonts.clear();
        }
        self.inp = inp;
        self.s = scale;
        self.edit_started = false;
        self.cursor = CursorShape::Arrow;
        self.hot = None;
        self.time = time;
    }

    pub fn end_frame(&mut self) {
        if !self.inp.down && !self.inp.pressed {
            self.active = None;
        }
    }

    pub fn px(&self, v: f32) -> f32 {
        (v * self.s).round()
    }

    pub fn hover(&self, r: Rect) -> bool {
        self.inp.inside && r.contains(self.inp.mx, self.inp.my) && (self.active.is_none() || !self.inp.down)
    }

    /// Generic press/drag/click behavior. Returns (hovered, held, clicked).
    pub fn behave(&mut self, wid: u64, r: Rect) -> (bool, bool, bool) {
        let over = self.inp.inside && r.contains(self.inp.mx, self.inp.my);
        if over && self.active.is_none_or(|a| a == wid) {
            self.hot = Some(wid);
        }
        if over && self.inp.pressed && self.active.is_none() {
            self.active = Some(wid);
            self.drag_origin = (self.inp.mx, self.inp.my);
        }
        let held = self.active == Some(wid) && (self.inp.down || self.inp.pressed);
        let clicked = self.active == Some(wid) && self.inp.released && over;
        (over && self.active.is_none_or(|a| a == wid), held, clicked)
    }

    pub fn drag_delta(&self) -> (f32, f32) {
        (self.inp.mx - self.drag_origin.0, self.inp.my - self.drag_origin.1)
    }

    fn font(&mut self, d2d: &D2d, size: f32, weight: DWRITE_FONT_WEIGHT) -> IDWriteTextFormat {
        let px = (size * self.s * 10.0).round() as u32;
        self.fonts
            .entry((px, weight.0))
            .or_insert_with(|| d2d.text_format(px as f32 / 10.0, weight).expect("font"))
            .clone()
    }

    pub fn measure(&mut self, d2d: &D2d, text: &str, size: f32, weight: DWRITE_FONT_WEIGHT) -> f32 {
        let f = self.font(d2d, size, weight);
        d2d.text_layout(text, &f, 10000.0, 1000.0)
            .ok()
            .map(|l| unsafe {
                let mut m = DWRITE_TEXT_METRICS::default();
                let _ = l.GetMetrics(&mut m);
                m.widthIncludingTrailingWhitespace
            })
            .unwrap_or(0.0)
    }

    pub fn text(&mut self, d2d: &D2d, r: Rect, text: &str, size: f32, weight: DWRITE_FONT_WEIGHT, color: D2D1_COLOR_F, align: Align) {
        let f = self.font(d2d, size, weight);
        let Ok(l) = d2d.text_layout(text, &f, r.w.max(1.0), r.h.max(1.0)) else { return };
        unsafe {
            let _ = l.SetTextAlignment(match align {
                Align::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                Align::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
            });
            let _ = l.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            let _ = l.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
            let trim = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
            let _ = l.SetTrimming(&trim, None);
            d2d.dc.DrawTextLayout(Vector2 { X: r.x, Y: r.y }, &l, d2d.brush(color), D2D1_DRAW_TEXT_OPTIONS_CLIP);
        }
    }

    /// Multi-line wrapped text (help hints).
    pub fn para(&mut self, d2d: &D2d, r: Rect, text: &str, size: f32, color: D2D1_COLOR_F) {
        let f = self.font(d2d, size, DWRITE_FONT_WEIGHT_NORMAL);
        let Ok(l) = d2d.text_layout(text, &f, r.w.max(1.0), r.h.max(1.0)) else { return };
        unsafe { d2d.dc.DrawTextLayout(Vector2 { X: r.x, Y: r.y }, &l, d2d.brush(color), D2D1_DRAW_TEXT_OPTIONS_CLIP) };
    }

    pub fn fill(&self, d2d: &D2d, r: Rect, radius: f32, c: D2D1_COLOR_F) {
        unsafe {
            if radius > 0.0 {
                d2d.dc.FillRoundedRectangle(&rrect(r.d2(), radius), d2d.brush(c));
            } else {
                d2d.dc.FillRectangle(&r.d2(), d2d.brush(c));
            }
        }
    }

    pub fn stroke(&self, d2d: &D2d, r: Rect, radius: f32, c: D2D1_COLOR_F, width: f32) {
        let h = width * 0.5;
        unsafe { d2d.dc.DrawRoundedRectangle(&rrect(r.inset(h).d2(), radius), d2d.brush(c), width, None) };
    }

    pub fn circle(&self, d2d: &D2d, x: f32, y: f32, rad: f32, c: D2D1_COLOR_F) {
        unsafe { d2d.dc.FillEllipse(&ellipse(x, y, rad), d2d.brush(c)) };
    }

    pub fn line(&self, d2d: &D2d, x0: f32, y0: f32, x1: f32, y1: f32, c: D2D1_COLOR_F, w: f32) {
        unsafe { d2d.dc.DrawLine(Vector2 { X: x0, Y: y0 }, Vector2 { X: x1, Y: y1 }, d2d.brush(c), w, None) };
    }

    pub fn button(&mut self, d2d: &D2d, wid: u64, r: Rect, label: &str, kind: ButtonKind) -> bool {
        let (hov, held, clicked) = self.behave(wid, r);
        if hov {
            self.cursor = CursorShape::Hand;
        }
        let (bg, bg_hi, fg) = match kind {
            ButtonKind::Primary => (ACCENT, ACCENT_HI, 0xffffff),
            ButtonKind::Danger => (RED, RED_HI, 0xffffff),
            ButtonKind::Secondary => (PANEL2, HOVER, TEXT),
            ButtonKind::Ghost => (PANEL, PANEL2, TEXT),
        };
        let alpha = if kind == ButtonKind::Ghost && !hov { 0.0 } else { 1.0 };
        let c = if held { bg } else if hov { bg_hi } else { bg };
        let r2 = if held { r.inset(self.s * 0.5) } else { r };
        self.fill(d2d, r2, self.px(8.0), rgba(c, alpha));
        if kind == ButtonKind::Secondary {
            self.stroke(d2d, r2, self.px(8.0), rgba(BORDER, 1.0), 1.0);
        }
        self.text(d2d, r2, label, 13.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, rgba(fg, 1.0), Align::Center);
        clicked
    }

    pub fn toggle(&mut self, d2d: &D2d, wid: u64, r: Rect, label: &str, v: &mut bool) -> bool {
        let (hov, _, clicked) = self.behave(wid, r);
        if hov {
            self.cursor = CursorShape::Hand;
        }
        let (tw, th) = (self.px(34.0), self.px(20.0));
        let track = Rect::new(r.r() - tw, r.cy() - th * 0.5, tw, th);
        self.text(d2d, Rect::new(r.x, r.y, r.w - tw - self.px(8.0), r.h), label, 13.0, DWRITE_FONT_WEIGHT_NORMAL, rgba(TEXT, 1.0), Align::Left);
        if clicked {
            self.edit_started = true;
            *v = !*v;
        }
        let on = *v;
        self.fill(d2d, track, th * 0.5, rgba(if on { ACCENT } else { if hov { HOVER } else { PANEL2 } }, 1.0));
        if !on {
            self.stroke(d2d, track, th * 0.5, rgba(BORDER, 1.0), 1.0);
        }
        let k = th * 0.5 - self.px(3.0);
        let kx = if on { track.r() - th * 0.5 } else { track.x + th * 0.5 };
        self.circle(d2d, kx, track.cy(), k, rgba(0xffffff, if on { 1.0 } else { 0.8 }));
        clicked
    }

    /// Labeled slider. Shows value with `fmt`. Returns true while changing.
    pub fn slider(&mut self, d2d: &D2d, wid: u64, r: Rect, label: &str, v: &mut f32, lo: f32, hi: f32, fmt: &dyn Fn(f32) -> String) -> bool {
        let (top, bottom) = r.cut_top(r.h * 0.5);
        self.text(d2d, top, label, 12.5, DWRITE_FONT_WEIGHT_NORMAL, rgba(MUTED, 1.0), Align::Left);
        self.text(d2d, top, &fmt(*v), 12.5, DWRITE_FONT_WEIGHT_SEMI_BOLD, rgba(TEXT, 1.0), Align::Right);
        let track = Rect::new(bottom.x, bottom.cy() - self.px(2.0), bottom.w, self.px(4.0));
        let hit = bottom.inset_xy(-self.px(4.0), 0.0);
        let was_active = self.active == Some(wid);
        let (hov, held, _) = self.behave(wid, hit);
        if hov || held {
            self.cursor = CursorShape::Hand;
        }
        let mut changed = false;
        if held {
            if !was_active {
                self.edit_started = true;
                self.drag_value = *v;
            }
            let f = ((self.inp.mx - track.x) / track.w).clamp(0.0, 1.0);
            let nv = lo + (hi - lo) * f;
            if (nv - *v).abs() > f32::EPSILON {
                *v = nv;
                changed = true;
            }
        }
        let f = ((*v - lo) / (hi - lo)).clamp(0.0, 1.0);
        self.fill(d2d, track, self.px(2.0), rgba(PANEL2, 1.0));
        self.fill(d2d, Rect::new(track.x, track.y, track.w * f, track.h), self.px(2.0), rgba(ACCENT, 1.0));
        let kr = if held || hov { self.px(8.0) } else { self.px(7.0) };
        self.circle(d2d, track.x + track.w * f, track.cy(), kr, rgba(0xffffff, 1.0));
        changed
    }

    pub fn segmented(&mut self, d2d: &D2d, wid: u64, r: Rect, opts: &[&str], v: &mut usize) -> bool {
        self.fill(d2d, r, self.px(8.0), rgba(PANEL2, 1.0));
        let n = opts.len().max(1) as f32;
        let w = (r.w - self.px(4.0)) / n;
        let mut changed = false;
        for (i, o) in opts.iter().enumerate() {
            let cell = Rect::new(r.x + self.px(2.0) + w * i as f32, r.y + self.px(2.0), w, r.h - self.px(4.0));
            let (hov, _, clicked) = self.behave(wid ^ (i as u64 + 1).wrapping_mul(0x9E37), cell);
            if hov {
                self.cursor = CursorShape::Hand;
            }
            if clicked && *v != i {
                self.edit_started = true;
                *v = i;
                changed = true;
            }
            let sel = *v == i;
            if sel {
                self.fill(d2d, cell, self.px(6.0), rgba(ACCENT, 1.0));
            } else if hov {
                self.fill(d2d, cell, self.px(6.0), rgba(HOVER, 1.0));
            }
            self.text(
                d2d,
                cell,
                o,
                12.0,
                if sel { DWRITE_FONT_WEIGHT_SEMI_BOLD } else { DWRITE_FONT_WEIGHT_NORMAL },
                rgba(if sel { 0xffffff } else { MUTED }, 1.0),
                Align::Center,
            );
        }
        changed
    }

    pub fn section(&mut self, d2d: &D2d, r: Rect, title: &str) {
        self.text(d2d, r, &title.to_uppercase(), 11.0, DWRITE_FONT_WEIGHT_BOLD, rgba(FAINT, 1.0), Align::Left);
    }

    pub fn push_clip(&self, d2d: &D2d, r: Rect) {
        unsafe { d2d.dc.PushAxisAlignedClip(&r.d2(), D2D1_ANTIALIAS_MODE_ALIASED) };
    }

    pub fn pop_clip(&self, d2d: &D2d) {
        unsafe { d2d.dc.PopAxisAlignedClip() };
    }

    pub fn key(&self, vk: u16) -> bool {
        self.inp.keys.iter().any(|k| k.0 == vk && !k.1)
    }

    pub fn ctrl_key(&self, vk: u16, shift: bool) -> bool {
        self.inp.keys.iter().any(|k| k.0 == vk && k.1 && k.2 == shift)
    }
}
