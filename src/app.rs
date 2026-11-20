//! Application screens: Home (start a recording / open recent), the floating
//! countdown + recording bar, and the Editor (preview, timeline, styling, export).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows_numerics::Matrix3x2;

use crate::capture::{self, Monitor, Target};
use crate::d2d::{D2d, rgba};
use crate::export::{self, ExportJob};
use crate::gpu::Gpu;
use crate::input::{PAUSE_HOTKEY_LABEL, STOP_HOTKEY_LABEL};
use crate::log;
use crate::media::Decoder;
use crate::project::{self, ASPECTS, BACKGROUNDS, CLICK_EFFECTS, FORMATS, GIF_FPS, GIF_SIZES, Motion, Project, RESOLUTIONS, Style, ZoomSeg};
use crate::recorder::{self, ActiveRecording, RecordOptions};
use crate::render::Compositor;
use crate::selector::{self, Outcome, Selector};
use crate::ui::theme::*;
use crate::ui::{Align, ButtonKind, Rect, Ui, id, idx};
use crate::util::{Res, fmt_time, secs_to_hns, wide};
use crate::window::{self, CursorShape, Kind, Surface};

const W_NORMAL: DWRITE_FONT_WEIGHT = DWRITE_FONT_WEIGHT_NORMAL;
const W_SEMI: DWRITE_FONT_WEIGHT = DWRITE_FONT_WEIGHT_SEMI_BOLD;
const W_BOLD: DWRITE_FONT_WEIGHT = DWRITE_FONT_WEIGHT_BOLD;

enum Screen {
    Home,
    Selecting { sel: Box<Selector>, opts: RecordOptions },
    Countdown { until: Instant, opts: RecordOptions },
    Recording(ActiveRecording),
    Editor(Box<Editor>),
}

pub struct App {
    gpu: Gpu,
    d2d: D2d,
    main: Surface,
    ui: Ui,
    screen: Screen,
    monitors: Vec<Monitor>,
    mon_sel: usize,
    /// 0 full screen, 1 window, 2 area
    mode_sel: usize,
    fps_sel: usize,
    countdown: bool,
    all_keys: bool,
    sys_audio: bool,
    mic: bool,
    has_mic: bool,
    recents: Vec<(PathBuf, String, f64, String)>,
    toast: Option<(String, Instant, bool)>,
    bar: Option<Surface>,
    bar_ui: Ui,
    t0: Instant,
    home_scroll: f32,
}

pub fn open_in_explorer(path: &std::path::Path, select: bool) {
    let arg = if select { format!("/select,\"{}\"", path.display()) } else { format!("\"{}\"", path.display()) };
    let (exe, a) = (wide("explorer.exe"), wide(&arg));
    unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            windows::core::PCWSTR::null(),
            windows::core::PCWSTR(exe.as_ptr()),
            windows::core::PCWSTR(a.as_ptr()),
            windows::core::PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn pretty_name(folder: &str) -> String {
    // 2026-11-20_19-40-27 -> 2026-11-20  19:40
    let parts: Vec<&str> = folder.split('_').collect();
    if parts.len() == 2 && parts[1].len() >= 5 {
        format!("{}  {}", parts[0], parts[1][..5].replace('-', ":"))
    } else {
        folder.to_string()
    }
}

impl App {
    pub fn new() -> Res<App> {
        let t = Instant::now();
        let gpu = Gpu::new()?;
        let d2d = D2d::new(&gpu)?;
        let s = unsafe { windows::Win32::UI::HiDpi::GetDpiForSystem().max(96) as f32 / 96.0 };
        let main = Surface::create(&gpu, "Framecast", Kind::Main, CW_USEDEFAULT, CW_USEDEFAULT, (1360.0 * s) as i32, (860.0 * s) as i32)?;
        main.show(SW_SHOWNORMAL);
        log!("startup {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);
        let monitors = capture::monitors();
        Ok(App {
            gpu,
            d2d,
            main,
            ui: Ui::new(),
            screen: Screen::Home,
            monitors,
            mon_sel: 0,
            mode_sel: 0,
            fps_sel: 1,
            countdown: true,
            all_keys: false,
            sys_audio: true,
            mic: false,
            has_mic: crate::audio::has_device(crate::audio::Source::Mic),
            recents: project::recent_projects(),
            toast: None,
            bar: None,
            bar_ui: Ui::new(),
            t0: Instant::now(),
            home_scroll: 0.0,
        })
    }

    pub fn open_project(&mut self, dir: PathBuf) {
        let _ = self.apply(Action::Open(dir));
    }

    pub fn run(&mut self) {
        loop {
            let busy = match self.frame() {
                Ok(b) => b,
                Err(e) => {
                    log!("frame error: {e}");
                    self.toast(&e, true);
                    true
                }
            };
            if !window::pump(if busy { None } else { Some(250) }) {
                break;
            }
        }
    }

    fn toast(&mut self, msg: &str, error: bool) {
        self.toast = Some((msg.to_string(), Instant::now() + Duration::from_secs(if error { 8 } else { 4 }), error));
    }

    /// One UI frame. Returns true if something is animating (keep rendering).
    fn frame(&mut self) -> Res<bool> {
        let mut busy = false;
        busy |= self.select_frame()?;
        // Floating bar (countdown / recording).
        busy |= self.bar_frame()?;

        let inp = window::take_input(self.main.hwnd);
        if inp.resized {
            self.main.handle_resize();
        }
        if inp.close {
            self.on_close();
        }
        if !self.main.visible() {
            return Ok(busy || self.bar.is_some());
        }
        let time = self.t0.elapsed().as_secs_f64();
        let had_input = inp.any;
        if (inp.pressed || inp.released) && std::env::var_os("FRAMECAST_DEBUG_INPUT").is_some() {
            log!("input pressed={} released={} down={} inside={} at {},{}", inp.pressed, inp.released, inp.down, inp.inside, inp.mx, inp.my);
        }
        self.ui.begin_frame(inp, self.main.dpi_scale(), time);

        // Editor renders its preview frame before the UI pass (shares the D2D context).
        if let Screen::Editor(ed) = &mut self.screen {
            busy |= ed.update(&self.gpu, &self.d2d, &mut self.ui)?;
        }

        self.main.begin(&self.d2d)?;
        unsafe { self.d2d.dc.Clear(Some(&rgba(BG, 1.0))) };
        let full = Rect::new(0.0, 0.0, self.main.w as f32, self.main.h as f32);
        let mut action = None;
        match &mut self.screen {
            Screen::Home => action = self.home(full),
            Screen::Editor(ed) => {
                action = ed.draw(&mut self.ui, &self.d2d, full);
                busy |= ed.playing || ed.export.is_some();
            }
            _ => {}
        }
        self.draw_toast(full);
        self.ui.end_frame();
        window::set_cursor(self.main.hwnd, self.ui.cursor);
        self.main.end(&self.d2d)?;
        if let Some(a) = action {
            self.apply(a)?;
            busy = true;
        }
        busy |= self.ui.active.is_some() || had_input || self.toast.is_some();
        Ok(busy)
    }

    fn on_close(&mut self) {
        match &mut self.screen {
            Screen::Editor(ed) => {
                if let Some(job) = &ed.export {
                    job.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                let _ = ed.p.save();
            }
            Screen::Recording(r) => r.request_stop(),
            _ => {}
        }
        unsafe { PostQuitMessage(0) };
    }

    fn apply(&mut self, a: Action) -> Res<()> {
        match a {
            Action::StartRecording => {
                let Some(mon) = self.monitors.get(self.mon_sel).cloned() else {
                    self.toast("No display found", true);
                    return Ok(());
                };
                let opts = RecordOptions {
                    target: Target::Display(mon.clone()),
                    fps: [30, 60][self.fps_sel],
                    all_keys: self.all_keys,
                    system_audio: self.sys_audio,
                    mic: self.mic && self.has_mic,
                };
                self.main.show(SW_HIDE);
                // Give the hidden main window a moment to disappear from the screen.
                std::thread::sleep(Duration::from_millis(250));
                if self.mode_sel == 0 {
                    self.target_chosen(opts)?;
                } else {
                    let mode = if self.mode_sel == 1 { selector::Mode::Window } else { selector::Mode::Area };
                    match Selector::new(&self.gpu, &self.d2d, &mon, mode) {
                        Ok(sel) => self.screen = Screen::Selecting { sel: Box::new(sel), opts },
                        Err(e) => {
                            self.main.show(SW_SHOWNORMAL);
                            self.toast(&format!("Couldn't open the picker: {e}"), true);
                        }
                    }
                }
            }
            Action::Open(dir) => match Editor::open(&self.gpu, &self.d2d, &dir) {
                Ok(ed) => self.screen = Screen::Editor(Box::new(ed)),
                Err(e) => self.toast(&format!("Couldn't open recording: {e}"), true),
            },
            Action::Home => {
                if let Screen::Editor(ed) = &self.screen {
                    let _ = ed.p.save();
                }
                self.recents = project::recent_projects();
                self.screen = Screen::Home;
            }
            Action::OpenFolder(p) => open_in_explorer(&p, false),
            Action::Reveal(p) => open_in_explorer(&p, true),
        }
        Ok(())
    }

    /// Countdown (if enabled) and then recording, for an already chosen target.
    fn target_chosen(&mut self, opts: RecordOptions) -> Res<()> {
        if !self.countdown {
            return self.begin_recording(opts);
        }
        let s = self.main.dpi_scale();
        let sz = (180.0 * s) as i32;
        let r = opts.target.screen_rect();
        let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
        let bar = Surface::create(&self.gpu, "Framecast countdown", Kind::Floating, cx - sz / 2, cy - sz / 2, sz, sz)?;
        bar.show(SW_SHOWNOACTIVATE);
        self.bar = Some(bar);
        self.screen = Screen::Countdown { until: Instant::now() + Duration::from_secs(3), opts };
        Ok(())
    }

    fn select_frame(&mut self) -> Res<bool> {
        let Screen::Selecting { sel, .. } = &mut self.screen else { return Ok(false) };
        match sel.frame(&self.d2d)? {
            Outcome::Pending => {}
            Outcome::Cancel => {
                self.screen = Screen::Home;
                self.main.show(SW_SHOWNORMAL);
            }
            Outcome::Done(target) => {
                let Screen::Selecting { mut opts, .. } = std::mem::replace(&mut self.screen, Screen::Home) else { unreachable!() };
                opts.target = target;
                self.target_chosen(opts)?;
            }
        }
        Ok(true)
    }

    fn begin_recording(&mut self, opts: RecordOptions) -> Res<()> {
        let tr = opts.target.screen_rect();
        let mon = self
            .monitors
            .iter()
            .find(|m| m.handle == opts.target.monitor_handle())
            .map_or(tr, |m| m.rect);
        match recorder::start(opts) {
            Ok(rec) => {
                let s = self.main.dpi_scale();
                let (w, h) = ((300.0 * s) as i32, (52.0 * s) as i32);
                let gap = (12.0 * s) as i32;
                // Just above what's being recorded (or inside its top edge if there's no room).
                let x = ((tr.left + tr.right) / 2 - w / 2).clamp(mon.left, (mon.right - w).max(mon.left));
                let y = if tr.top - h - gap >= mon.top { tr.top - h - gap } else { tr.top.max(mon.top) + gap };
                match self.bar.as_mut() {
                    Some(bar) => unsafe {
                        let _ = SetWindowPos(bar.hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
                        bar.handle_resize();
                    },
                    None => {
                        let bar = Surface::create(&self.gpu, "Framecast recording", Kind::Floating, x, y, w, h)?;
                        bar.show(SW_SHOWNOACTIVATE);
                        self.bar = Some(bar);
                    }
                }
                if let Some(bar) = &self.bar {
                    bar.set_drag_except_right((150.0 * s) as i32);
                }
                self.screen = Screen::Recording(rec);
            }
            Err(e) => {
                self.bar = None;
                self.main.show(SW_SHOWNORMAL);
                self.screen = Screen::Home;
                self.toast(&format!("Recording failed to start: {e}"), true);
            }
        }
        Ok(())
    }

    fn finish_recording(&mut self) -> Res<()> {
        let Screen::Recording(rec) = std::mem::replace(&mut self.screen, Screen::Home) else { return Ok(()) };
        self.bar = None;
        let result = rec.join();
        self.main.show(SW_SHOWNORMAL);
        unsafe {
            let _ = SetForegroundWindow(self.main.hwnd);
        }
        self.recents = project::recent_projects();
        match result {
            Ok(dir) => self.apply(Action::Open(dir))?,
            Err(e) => self.toast(&format!("Recording failed: {e}"), true),
        }
        Ok(())
    }

    fn bar_frame(&mut self) -> Res<bool> {
        // State transitions first.
        if let Screen::Countdown { until, opts } = &self.screen {
            if Instant::now() >= *until {
                let opts = opts.clone();
                self.begin_recording(opts)?;
            }
        }
        if let Screen::Recording(rec) = &self.screen {
            if rec.stop_requested() || rec.is_finished() {
                self.finish_recording()?;
                return Ok(true);
            }
        }
        let Some(bar) = self.bar.as_mut() else { return Ok(false) };
        let inp = window::take_input(bar.hwnd);
        if (inp.pressed || inp.released) && std::env::var_os("FRAMECAST_DEBUG_INPUT").is_some() {
            log!("bar input pressed={} released={} down={} inside={} at {},{}", inp.pressed, inp.released, inp.down, inp.inside, inp.mx, inp.my);
        }
        if inp.resized {
            bar.handle_resize();
        }
        let s = bar.dpi_scale();
        self.bar_ui.begin_frame(inp, s, self.t0.elapsed().as_secs_f64());
        bar.begin(&self.d2d)?;
        let d2d = &self.d2d;
        let ui = &mut self.bar_ui;
        let full = Rect::new(0.0, 0.0, bar.w as f32, bar.h as f32);
        unsafe { d2d.dc.Clear(Some(&rgba(0x101216, 1.0))) };
        let mut stop = false;
        let mut cancel = false;
        match &self.screen {
            Screen::Countdown { until, .. } => {
                let left = until.saturating_duration_since(Instant::now()).as_secs_f64();
                let n = left.ceil().max(1.0) as u32;
                let frac = (left.fract()) as f32;
                ui.stroke(d2d, full.inset(ui.px(10.0)), full.w * 0.5, rgba(ACCENT, 0.25 + 0.5 * frac), ui.px(3.0));
                ui.text(d2d, full.inset_xy(0.0, -ui.px(10.0)), &n.to_string(), 72.0, W_BOLD, rgba(TEXT, 1.0), Align::Center);
                let (hint, _) = full.cut_bottom(ui.px(44.0));
                ui.text(d2d, hint, "Esc to cancel", 11.0, W_NORMAL, rgba(MUTED, 1.0), Align::Center);
                if ui.key(VK_ESCAPE.0) {
                    cancel = true;
                }
            }
            Screen::Recording(rec) => {
                let paused = rec.is_paused();
                let (right, left) = full.cut_right(ui.px(150.0));
                if paused {
                    // Steady amber "pause" glyph instead of the pulsing red dot.
                    let (x, y) = (left.x + ui.px(20.0), left.cy());
                    ui.fill(d2d, Rect::new(x, y - ui.px(6.0), ui.px(3.5), ui.px(12.0)), 1.0, rgba(AMBER, 1.0));
                    ui.fill(d2d, Rect::new(x + ui.px(6.5), y - ui.px(6.0), ui.px(3.5), ui.px(12.0)), 1.0, rgba(AMBER, 1.0));
                } else {
                    let pulse = 0.55 + 0.45 * ((ui.time * 3.0).sin() * 0.5 + 0.5) as f32;
                    ui.circle(d2d, left.x + ui.px(24.0), left.cy(), ui.px(6.0), rgba(RED, pulse));
                }
                let tr = Rect::new(left.x + ui.px(40.0), left.y, left.w - ui.px(40.0), left.h);
                let label = if paused { format!("Paused  {}", fmt_time(rec.elapsed())) } else { fmt_time(rec.elapsed()) };
                ui.text(d2d, tr, &label, 16.0, W_SEMI, rgba(if paused { AMBER } else { TEXT }, 1.0), Align::Left);
                let row = right.inset_xy(ui.px(8.0), ui.px(9.0));
                let (stop_r, pause_r) = row.cut_right(ui.px(84.0));
                let pause_r = Rect::new(pause_r.x, pause_r.y, pause_r.w - ui.px(6.0), pause_r.h);
                if ui.button(d2d, id("pause"), pause_r, if paused { "▶" } else { "❚❚" }, ButtonKind::Secondary) {
                    rec.toggle_pause();
                }
                if ui.button(d2d, id("stop"), stop_r, "■  Stop", ButtonKind::Danger) {
                    stop = true;
                }
            }
            _ => {}
        }
        ui.end_frame();
        window::set_cursor(bar.hwnd, ui.cursor);
        bar.end(&self.d2d)?;
        if stop {
            if let Screen::Recording(rec) = &self.screen {
                rec.request_stop();
            }
        }
        if cancel {
            self.bar = None;
            self.screen = Screen::Home;
            self.main.show(SW_SHOWNORMAL);
        }
        // Recording bar only needs ~10 fps; the countdown animates smoothly.
        if matches!(self.screen, Screen::Recording(_)) {
            std::thread::sleep(Duration::from_millis(60));
        }
        Ok(true)
    }

    fn draw_toast(&mut self, full: Rect) {
        let Some((msg, until, err)) = &self.toast else { return };
        if Instant::now() > *until {
            self.toast = None;
            return;
        }
        let (msg, err) = (msg.clone(), *err);
        let ui = &mut self.ui;
        let w = (ui.measure(&self.d2d, &msg, 13.0, W_SEMI) + ui.px(40.0)).min(full.w - ui.px(40.0));
        let r = Rect::new(full.cx() - w * 0.5, full.b() - ui.px(72.0), w, ui.px(40.0));
        ui.fill(&self.d2d, r, ui.px(10.0), rgba(if err { 0x3A1A20 } else { 0x1D2A24 }, 0.97));
        ui.stroke(&self.d2d, r, ui.px(10.0), rgba(if err { RED } else { GREEN }, 0.6), 1.0);
        ui.text(&self.d2d, r.inset_xy(ui.px(16.0), 0.0), &msg, 13.0, W_SEMI, rgba(TEXT, 1.0), Align::Center);
    }

    fn home(&mut self, full: Rect) -> Option<Action> {
        let d2d = &self.d2d;
        let ui = &mut self.ui;
        let s = ui.s;
        let mut action = None;
        let body = full.inset_xy(ui.px(40.0), ui.px(32.0));
        let (head, body) = body.cut_top(ui.px(88.0));
        let (t1, t2) = head.cut_top(ui.px(44.0));
        ui.text(d2d, t1, "Framecast", 28.0, W_BOLD, rgba(TEXT, 1.0), Align::Left);
        ui.text(d2d, t2.cut_top(ui.px(24.0)).0, "Record your screen, get a polished demo: auto zoom, smooth cursor, beautiful frame.", 14.0, W_NORMAL, rgba(MUTED, 1.0), Align::Left);

        let gap = ui.px(24.0);
        let lw = (body.w * 0.5).clamp(ui.px(380.0), ui.px(560.0)).min(body.w);
        let (left, right) = body.cut_left(lw);
        let right = Rect::new(right.x + gap, right.y, right.w - gap, right.h);

        // New recording card.
        let card_h = ui.px(600.0).min(left.h);
        let card = Rect::new(left.x, left.y, left.w, card_h);
        ui.fill(d2d, card, ui.px(14.0), rgba(PANEL, 1.0));
        ui.stroke(d2d, card, ui.px(14.0), rgba(BORDER, 1.0), 1.0);
        let c = card.inset(ui.px(24.0));
        let (r, c) = c.cut_top(ui.px(28.0));
        ui.text(d2d, r, "New recording", 18.0, W_SEMI, rgba(TEXT, 1.0), Align::Left);
        let (_, c) = c.cut_top(ui.px(16.0));
        let (r, c) = c.cut_top(ui.px(20.0));
        ui.section(d2d, r, "Record");
        let (r, c) = c.cut_top(ui.px(38.0));
        ui.segmented(d2d, id("mode"), r, &["Full screen", "Window", "Area"], &mut self.mode_sel);
        let (_, c) = c.cut_top(ui.px(16.0));
        let (r, c) = c.cut_top(ui.px(20.0));
        ui.section(d2d, r, "Display");
        let (r, c) = c.cut_top(ui.px(38.0));
        let names: Vec<String> = self
            .monitors
            .iter()
            .enumerate()
            .map(|(i, m)| format!("{}{}  ·  {}×{}", i + 1, if m.primary { " (main)" } else { "" }, m.width(), m.height()))
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        if refs.is_empty() {
            ui.text(d2d, r, "No displays detected", 13.0, W_NORMAL, rgba(RED, 1.0), Align::Left);
        } else {
            ui.segmented(d2d, id("monitor"), r, &refs, &mut self.mon_sel);
        }
        let (_, c) = c.cut_top(ui.px(16.0));
        let (r, c) = c.cut_top(ui.px(20.0));
        ui.section(d2d, r, "Frame rate");
        let (r, c) = c.cut_top(ui.px(38.0));
        ui.segmented(d2d, id("fps"), r, &["30 fps", "60 fps (smoothest)"], &mut self.fps_sel);
        let (_, c) = c.cut_top(ui.px(14.0));
        let (r, c) = c.cut_top(ui.px(36.0));
        ui.toggle(d2d, id("countdown"), r, "3-second countdown", &mut self.countdown);
        let (r, c) = c.cut_top(ui.px(36.0));
        ui.toggle(d2d, id("allkeys"), r, "Record all keys (default: shortcuts only)", &mut self.all_keys);
        let (r, c) = c.cut_top(ui.px(36.0));
        ui.toggle(d2d, id("sysaudio"), r, "Record system audio", &mut self.sys_audio);
        let (r, c) = c.cut_top(ui.px(36.0));
        if self.has_mic {
            ui.toggle(d2d, id("mic"), r, "Record microphone", &mut self.mic);
        } else {
            ui.text(d2d, r, "No microphone found", 13.0, W_NORMAL, rgba(FAINT, 1.0), Align::Left);
        }
        let (_, c) = c.cut_top(ui.px(16.0));
        let (r, c) = c.cut_top(ui.px(52.0));
        let label = ["●   Start recording", "●   Choose window…", "●   Select area…"][self.mode_sel.min(2)];
        if ui.button(d2d, id("record"), r, label, ButtonKind::Danger) {
            action = Some(Action::StartRecording);
        }
        let (_, c) = c.cut_top(ui.px(14.0));
        ui.para(
            d2d,
            c,
            &format!(
                "Pause / resume with {PAUSE_HOTKEY_LABEL}, stop with {STOP_HOTKEY_LABEL} or the floating bar (it never shows up in the video). \
                 Cursor, clicks and shortcuts are stored separately, so you can restyle them afterwards."
            ),
            12.0,
            rgba(MUTED, 1.0),
        );

        // Recent recordings.
        let card = Rect::new(right.x, right.y, right.w, right.h);
        if card.w > ui.px(200.0) {
            ui.fill(d2d, card, ui.px(14.0), rgba(PANEL, 1.0));
            ui.stroke(d2d, card, ui.px(14.0), rgba(BORDER, 1.0), 1.0);
            let c = card.inset(ui.px(24.0));
            let (r, c) = c.cut_top(ui.px(28.0));
            ui.text(d2d, r, "Recent recordings", 18.0, W_SEMI, rgba(TEXT, 1.0), Align::Left);
            let (btn, _) = r.cut_right(ui.px(110.0));
            if ui.button(d2d, id("openfolder"), btn, "Open folder", ButtonKind::Secondary) {
                action = Some(Action::OpenFolder(crate::util::app_dir()));
            }
            let (_, list) = c.cut_top(ui.px(14.0));
            if self.recents.is_empty() {
                ui.text(d2d, list.cut_top(ui.px(40.0)).0, "No recordings yet. Your first one will show up here.", 13.0, W_NORMAL, rgba(FAINT, 1.0), Align::Left);
            }
            let row_h = ui.px(56.0);
            let total = self.recents.len() as f32 * (row_h + ui.px(6.0));
            if ui.hover(list) && ui.inp.wheel != 0.0 {
                self.home_scroll -= ui.inp.wheel * row_h;
            }
            self.home_scroll = self.home_scroll.clamp(0.0, (total - list.h).max(0.0));
            ui.push_clip(d2d, list);
            for (i, (dir, name, dur, source)) in self.recents.iter().enumerate() {
                let r = Rect::new(list.x, list.y + i as f32 * (row_h + ui.px(6.0)) - self.home_scroll, list.w, row_h);
                if r.b() < list.y || r.y > list.b() {
                    continue;
                }
                let (hov, _, clicked) = ui.behave(idx("recent", i), r);
                if hov {
                    ui.cursor = CursorShape::Hand;
                }
                ui.fill(d2d, r, ui.px(10.0), rgba(if hov { HOVER } else { PANEL2 }, 1.0));
                let inner = r.inset_xy(ui.px(16.0), 0.0);
                ui.circle(d2d, inner.x + ui.px(8.0), inner.cy(), ui.px(8.0), rgba(ACCENT, 0.9));
                let tr = Rect::new(inner.x + ui.px(28.0), inner.y, inner.w - ui.px(28.0), inner.h);
                let (top, bottom) = tr.inset_xy(0.0, ui.px(8.0)).cut_top(ui.px(22.0));
                ui.text(d2d, top, &pretty_name(name), 14.0, W_SEMI, rgba(TEXT, 1.0), Align::Left);
                let src = if source.is_empty() { "Recording" } else { source.as_str() };
                ui.text(d2d, Rect::new(bottom.x, bottom.y, bottom.w - ui.px(130.0), bottom.h), src, 12.0, W_NORMAL, rgba(MUTED, 1.0), Align::Left);
                ui.text(d2d, tr, &format!("{}   Open →", fmt_time(*dur)), 12.5, W_NORMAL, rgba(if hov { TEXT } else { MUTED }, 1.0), Align::Right);
                if clicked {
                    action = Some(Action::Open(dir.clone()));
                }
            }
            ui.pop_clip(d2d);
        }
        let _ = s;
        action
    }
}

enum Action {
    StartRecording,
    Open(PathBuf),
    Home,
    OpenFolder(PathBuf),
    Reveal(PathBuf),
}

#[derive(Clone, PartialEq)]
struct Snap {
    style: Style,
    zooms: Vec<ZoomSeg>,
    trim: (f64, f64),
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    None,
    Scrub,
    ZoomMove { i: usize, grab: f64, len: f64 },
    ZoomStart(usize),
    ZoomEnd(usize),
    TrimStart,
    TrimEnd,
}

struct Editor {
    p: Project,
    motion: Motion,
    motion_key: Snap,
    dec: Decoder,
    comp: Compositor,
    preview: Option<ID2D1Bitmap1>,
    t: f64,
    playing: bool,
    last_tick: Instant,
    rendered: Option<(f64, Snap)>,
    sel: Option<usize>,
    drag: Drag,
    undo: Vec<Snap>,
    redo: Vec<Snap>,
    saved: Snap,
    save_at: Option<Instant>,
    export: Option<ExportJob>,
    export_result: Option<Res<PathBuf>>,
    side_scroll: f32,
    title: String,
    tracks: crate::audio::Tracks,
    player: Option<crate::audio::Player>,
    volumes: std::sync::Arc<std::sync::Mutex<(f32, f32)>>,
}

impl Editor {
    fn open(gpu: &Gpu, d2d: &D2d, dir: &std::path::Path) -> Res<Editor> {
        let p = Project::open(dir)?;
        let dec = Decoder::open(gpu, &dir.join("raw.mp4"))?;
        let (w, h) = p.canvas_size();
        let comp = Compositor::new(gpu, d2d, w, h)?;
        let motion = Motion::build(&p);
        let snap = Snap { style: p.style.clone(), zooms: p.zooms.clone(), trim: (p.trim_start, p.trim_end) };
        let mut title = pretty_name(&dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default());
        if !p.rec.source.is_empty() {
            title = format!("{title}   ·   {}", p.rec.source);
        }
        let t = p.trim_start;
        let tracks = crate::audio::Tracks::load(dir);
        let volumes = std::sync::Arc::new(std::sync::Mutex::new((p.style.sys_volume, p.style.mic_volume)));
        Ok(Editor {
            tracks,
            player: None,
            volumes,
            p,
            motion,
            motion_key: snap.clone(),
            dec,
            comp,
            preview: None,
            t,
            playing: false,
            last_tick: Instant::now(),
            rendered: None,
            sel: None,
            drag: Drag::None,
            undo: Vec::new(),
            redo: Vec::new(),
            saved: snap,
            save_at: None,
            export: None,
            export_result: None,
            side_scroll: 0.0,
            title,
        })
    }

    fn snap(&self) -> Snap {
        Snap { style: self.p.style.clone(), zooms: self.p.zooms.clone(), trim: (self.p.trim_start, self.p.trim_end) }
    }

    fn restore(&mut self, s: Snap) {
        self.p.style = s.style;
        self.p.zooms = s.zooms;
        (self.p.trim_start, self.p.trim_end) = s.trim;
        if self.sel.is_some_and(|i| i >= self.p.zooms.len()) {
            self.sel = None;
        }
    }

    fn push_undo(&mut self, before: Snap) {
        if self.undo.last() != Some(&before) {
            self.undo.push(before);
            if self.undo.len() > 200 {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
    }

    /// Advances playback and renders the preview frame if anything changed.
    fn update(&mut self, gpu: &Gpu, d2d: &D2d, ui: &mut Ui) -> Res<bool> {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f64().min(0.1);
        self.last_tick = now;

        // Keyboard shortcuts.
        if ui.key(VK_SPACE.0) {
            self.toggle_play();
        }
        if ui.key(VK_DELETE.0) || ui.key(VK_BACK.0) {
            if let Some(i) = self.sel.take() {
                let before = self.snap();
                self.p.zooms.remove(i);
                self.push_undo(before);
            }
        }
        let step = 1.0 / self.p.rec.fps.max(1) as f64;
        if ui.key(VK_LEFT.0) {
            self.playing = false;
            self.t = (self.t - step).max(0.0);
        }
        if ui.key(VK_RIGHT.0) {
            self.playing = false;
            self.t = (self.t + step).min(self.p.rec.duration);
        }
        if ui.ctrl_key(b'Z' as u16, false) {
            if let Some(s) = self.undo.pop() {
                let cur = self.snap();
                self.redo.push(cur);
                self.restore(s);
            }
        }
        if ui.ctrl_key(b'Y' as u16, false) || ui.ctrl_key(b'Z' as u16, true) {
            if let Some(s) = self.redo.pop() {
                let cur = self.snap();
                self.undo.push(cur);
                self.restore(s);
            }
        }

        if self.playing {
            self.t += dt;
            if self.t >= self.p.trim_end {
                self.t = self.p.trim_end;
                self.playing = false;
            }
        }

        // Preview audio follows the play state.
        *self.volumes.lock().unwrap() = (self.p.style.sys_volume, self.p.style.mic_volume);
        if self.playing && self.player.is_none() && !self.tracks.is_empty() {
            self.player = Some(crate::audio::Player::start(crate::audio::PlayParams {
                tracks: self.tracks.clone(),
                start: self.t,
                end: self.p.trim_end,
                volumes: self.volumes.clone(),
            }));
        } else if !self.playing {
            self.player = None;
        }

        let snap = self.snap();
        if snap.style.smoothing != self.motion_key.style.smoothing
            || snap.style.hide_idle != self.motion_key.style.hide_idle
            || snap.style.zoom_speed != self.motion_key.style.zoom_speed
            || snap.zooms != self.motion_key.zooms
        {
            self.motion = Motion::build(&self.p);
        }
        self.motion_key = snap.clone();

        let (w, h) = self.p.canvas_size();
        if (w, h) != (self.comp.width, self.comp.height) {
            self.comp.resize(d2d, w, h)?;
            self.preview = None;
            self.rendered = None;
        }
        if self.preview.is_none() {
            self.preview = Some(d2d.source_for(&self.comp.out)?);
        }
        let need = self.rendered.as_ref().is_none_or(|(t, s)| *t != self.t || *s != snap);
        if need {
            self.dec.frame_at(gpu, secs_to_hns(self.t))?;
            self.comp.render(d2d, self.dec.frame_srv.as_ref(), &self.p, &self.motion, self.t, 60.0)?;
            self.rendered = Some((self.t, snap.clone()));
        }

        // Autosave a moment after the last change.
        if snap != self.saved {
            self.save_at.get_or_insert(now + Duration::from_millis(800));
            if self.save_at.is_some_and(|t| now >= t) {
                if let Err(e) = self.p.save() {
                    log!("save failed: {e}");
                }
                self.saved = snap;
                self.save_at = None;
            }
        }

        // Export finished?
        if self.export.as_ref().is_some_and(|j| j.is_finished()) {
            let job = self.export.take().unwrap();
            self.export_result = Some(job.join());
        }
        Ok(need || self.save_at.is_some())
    }

    fn toggle_play(&mut self) {
        self.playing = !self.playing;
        if self.playing && self.t >= self.p.trim_end - 0.01 {
            self.t = self.p.trim_start;
        }
        self.last_tick = Instant::now();
    }

    fn draw(&mut self, ui: &mut Ui, d2d: &D2d, full: Rect) -> Option<Action> {
        let before = self.snap();
        let mut action = None;
        let (top, rest) = full.cut_top(ui.px(56.0));
        let (bottom, rest) = rest.cut_bottom(ui.px(214.0));
        let (side, center) = rest.cut_right(ui.px(316.0));

        // Top bar.
        ui.fill(d2d, top, 0.0, rgba(PANEL, 1.0));
        ui.line(d2d, top.x, top.b() - 0.5, top.r(), top.b() - 0.5, rgba(BORDER, 1.0), 1.0);
        let tb = top.inset_xy(ui.px(12.0), ui.px(10.0));
        let (back, tb) = tb.cut_left(ui.px(96.0));
        if ui.button(d2d, id("back"), back, "←  Home", ButtonKind::Ghost) {
            action = Some(Action::Home);
        }
        let (exp, tb) = tb.cut_right(ui.px(150.0));
        self.export_ui(ui, d2d, exp, tb, &mut action);
        let title = Rect::new(tb.x + ui.px(12.0), tb.y, tb.w * 0.5, tb.h);
        ui.text(d2d, title, &self.title, 15.0, W_SEMI, rgba(TEXT, 1.0), Align::Left);

        self.preview_ui(ui, d2d, center);
        self.sidebar(ui, d2d, side);
        self.timeline(ui, d2d, bottom);

        if ui.edit_started {
            self.push_undo(before);
        }
        action
    }

    fn export_ui(&mut self, ui: &mut Ui, d2d: &D2d, btn: Rect, area: Rect, action: &mut Option<Action>) {
        if let Some(job) = &self.export {
            let f = job.fraction();
            if ui.button(d2d, id("cancel_export"), btn, "Cancel", ButtonKind::Secondary) {
                job.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            let bar = Rect::new(btn.x - ui.px(260.0), btn.cy() - ui.px(4.0), ui.px(240.0), ui.px(8.0));
            ui.fill(d2d, bar, ui.px(4.0), rgba(PANEL2, 1.0));
            ui.fill(d2d, Rect::new(bar.x, bar.y, bar.w * f, bar.h), ui.px(4.0), rgba(ACCENT, 1.0));
            let label = Rect::new(bar.x - ui.px(130.0), btn.y, ui.px(120.0), btn.h);
            ui.text(d2d, label, &format!("Exporting {:.0}%", f * 100.0), 12.5, W_SEMI, rgba(TEXT, 1.0), Align::Right);
            return;
        }
        let label = if self.p.is_gif() { "Export GIF" } else { "Export MP4" };
        if ui.button(d2d, id("export"), btn, label, ButtonKind::Primary) {
            let _ = self.p.save();
            self.playing = false;
            self.export_result = None;
            self.export = Some(export::start(self.p.clone()));
        }
        match &self.export_result {
            Some(Ok(path)) => {
                let (show, rest) = Rect::new(area.x, area.y, area.w - ui.px(12.0), area.h).cut_right(ui.px(130.0));
                if ui.button(d2d, id("reveal"), show, "Show in folder", ButtonKind::Secondary) {
                    *action = Some(Action::Reveal(path.clone()));
                }
                let name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                ui.text(d2d, Rect::new(rest.x, rest.y, rest.w - ui.px(12.0), rest.h), &format!("✓ Saved {name}"), 12.5, W_SEMI, rgba(GREEN, 1.0), Align::Right);
            }
            Some(Err(e)) => {
                let msg = if e == "cancelled" { "Export cancelled".to_string() } else { format!("Export failed: {e}") };
                let r = Rect::new(area.x, area.y, area.w - ui.px(12.0), area.h);
                ui.text(d2d, r, &msg, 12.5, W_SEMI, rgba(if e == "cancelled" { MUTED } else { RED }, 1.0), Align::Right);
            }
            None => {}
        }
    }

    fn preview_ui(&mut self, ui: &mut Ui, d2d: &D2d, area: Rect) {
        let a = area.inset(ui.px(24.0));
        let (cw, ch) = (self.comp.width as f32, self.comp.height as f32);
        let k = (a.w / cw).min(a.h / ch);
        let r = Rect::new(a.cx() - cw * k * 0.5, a.cy() - ch * k * 0.5, cw * k, ch * k);
        if let Some(bmp) = &self.preview {
            unsafe {
                d2d.dc.DrawBitmap(bmp, Some(&r.d2()), 1.0, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, None, None);
            }
        }
        ui.stroke(d2d, r.inset(-1.0), 0.0, rgba(BORDER, 1.0), 1.0);
        let (hov, _, clicked) = ui.behave(id("preview"), r);
        if clicked {
            self.toggle_play();
        }
        if hov && !self.playing {
            ui.cursor = CursorShape::Hand;
            let rad = ui.px(30.0);
            ui.circle(d2d, r.cx(), r.cy(), rad, rgba(0x000000, 0.45));
            if let Ok(g) = d2d.polygon(&[(-0.35, -0.5), (0.55, 0.0), (-0.35, 0.5)]) {
                unsafe {
                    d2d.dc.SetTransform(&(Matrix3x2::scale(rad, rad) * Matrix3x2::translation(r.cx() + rad * 0.05, r.cy())));
                    d2d.dc.FillGeometry(&g, d2d.brush(rgba(0xffffff, 0.95)), None);
                    d2d.dc.SetTransform(&Matrix3x2::identity());
                }
            }
        }
    }

    fn sidebar(&mut self, ui: &mut Ui, d2d: &D2d, area: Rect) {
        ui.fill(d2d, area, 0.0, rgba(PANEL, 1.0));
        ui.line(d2d, area.x + 0.5, area.y, area.x + 0.5, area.b(), rgba(BORDER, 1.0), 1.0);
        let view = area.inset_xy(ui.px(20.0), ui.px(8.0));
        if ui.hover(area) && ui.inp.wheel != 0.0 {
            self.side_scroll -= ui.inp.wheel * ui.px(60.0);
        }
        ui.push_clip(d2d, area);
        let mut y = view.y - self.side_scroll;
        let x = view.x;
        let w = view.w;
        let row = |y: &mut f32, h: f32| {
            let r = Rect::new(x, *y, w, h);
            *y += h;
            r
        };
        let gap = ui.px(10.0);
        let st = &mut self.p.style;
        let pct = |v: f32| format!("{:.0}%", v * 100.0);

        y += ui.px(10.0);
        ui.section(d2d, row(&mut y, ui.px(24.0)), "Export");
        ui.segmented(d2d, id("format"), row(&mut y, ui.px(32.0)), FORMATS, &mut st.format);
        y += gap;
        if st.format == 1 {
            let names: Vec<&str> = GIF_SIZES.iter().map(|a| a.0).collect();
            ui.segmented(d2d, id("gifsize"), row(&mut y, ui.px(32.0)), &names, &mut st.gif_size);
            y += gap * 0.6;
            let names: Vec<&str> = GIF_FPS.iter().map(|a| a.0).collect();
            ui.segmented(d2d, id("giffps"), row(&mut y, ui.px(32.0)), &names, &mut st.gif_fps);
            y += gap * 0.4;
            ui.text(d2d, row(&mut y, ui.px(18.0)), "Silent, loops forever. Smaller size/fps = smaller file.", 11.5, W_NORMAL, rgba(FAINT, 1.0), Align::Left);
        } else {
            let names: Vec<&str> = RESOLUTIONS.iter().map(|a| a.0).collect();
            ui.segmented(d2d, id("res"), row(&mut y, ui.px(32.0)), &names, &mut st.resolution);
        }
        y += ui.px(22.0);

        ui.section(d2d, row(&mut y, ui.px(24.0)), "Background");
        let cols = 4;
        let sw = (w - gap * (cols - 1) as f32) / cols as f32;
        let sh = ui.px(38.0);
        for (i, (name, c0, c1)) in BACKGROUNDS.iter().enumerate() {
            let (cx, cy) = (i % cols, i / cols);
            let r = Rect::new(x + cx as f32 * (sw + gap), y + cy as f32 * (sh + gap), sw, sh);
            let (hov, _, clicked) = ui.behave(idx("bg", i), r);
            if hov {
                ui.cursor = CursorShape::Hand;
            }
            if clicked && st.background != i {
                ui.edit_started = true;
                st.background = i;
            }
            unsafe {
                let stops = [
                    D2D1_GRADIENT_STOP { position: 0.0, color: rgba(*c0, 1.0) },
                    D2D1_GRADIENT_STOP { position: 1.0, color: rgba(*c1, 1.0) },
                ];
                if let Ok(coll) = ID2D1RenderTarget::CreateGradientStopCollection(&d2d.dc, &stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP) {
                    let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                        startPoint: windows_numerics::Vector2 { X: r.x, Y: r.y },
                        endPoint: windows_numerics::Vector2 { X: r.r(), Y: r.b() },
                    };
                    if let Ok(b) = d2d.dc.CreateLinearGradientBrush(&props, None, &coll) {
                        d2d.dc.FillRoundedRectangle(&crate::d2d::rrect(r.d2(), ui.px(8.0)), &b);
                    }
                }
            }
            if st.background == i {
                ui.stroke(d2d, r.inset(-ui.px(3.0)), ui.px(10.0), rgba(0xffffff, 0.95), ui.px(2.0));
            } else if hov {
                ui.stroke(d2d, r, ui.px(8.0), rgba(0xffffff, 0.4), 1.0);
                ui.text(d2d, r, name, 11.0, W_SEMI, rgba(0xffffff, 0.9), Align::Center);
            }
        }
        y += (BACKGROUNDS.len().div_ceil(cols)) as f32 * (sh + gap) + ui.px(8.0);

        ui.section(d2d, row(&mut y, ui.px(24.0)), "Frame");
        ui.slider(d2d, id("padding"), row(&mut y, ui.px(40.0)), "Padding", &mut st.padding, 0.0, 0.2, &pct);
        y += gap * 0.5;
        ui.slider(d2d, id("radius"), row(&mut y, ui.px(40.0)), "Corner radius", &mut st.radius, 0.0, 0.06, &|v| format!("{:.0}", v / 0.06 * 100.0));
        y += gap * 0.5;
        ui.slider(d2d, id("shadow"), row(&mut y, ui.px(40.0)), "Shadow", &mut st.shadow, 0.0, 1.0, &pct);
        y += gap;
        ui.text(d2d, row(&mut y, ui.px(20.0)), "Aspect ratio", 12.5, W_NORMAL, rgba(MUTED, 1.0), Align::Left);
        let names: Vec<&str> = ASPECTS.iter().map(|a| a.0).collect();
        ui.segmented(d2d, id("aspect"), row(&mut y, ui.px(32.0)), &names, &mut st.aspect);
        y += ui.px(22.0);

        ui.section(d2d, row(&mut y, ui.px(24.0)), "Cursor");
        ui.slider(d2d, id("csize"), row(&mut y, ui.px(40.0)), "Size", &mut st.cursor_size, 0.5, 3.5, &|v| format!("{v:.1}×"));
        y += gap * 0.5;
        ui.slider(d2d, id("smooth"), row(&mut y, ui.px(40.0)), "Movement smoothing", &mut st.smoothing, 0.0, 1.0, &pct);
        y += gap * 0.5;
        ui.toggle(d2d, id("hideidle"), row(&mut y, ui.px(34.0)), "Hide when idle", &mut st.hide_idle);
        y += gap * 0.5;
        ui.text(d2d, row(&mut y, ui.px(20.0)), "Click effect", 12.5, W_NORMAL, rgba(MUTED, 1.0), Align::Left);
        ui.segmented(d2d, id("click"), row(&mut y, ui.px(32.0)), CLICK_EFFECTS, &mut st.click_effect);
        y += ui.px(22.0);

        ui.section(d2d, row(&mut y, ui.px(24.0)), "Zoom");
        ui.slider(d2d, id("zspeed"), row(&mut y, ui.px(40.0)), "Zoom speed", &mut st.zoom_speed, 0.0, 1.0, &|v| {
            match v {
                v if v < 0.33 => "Gentle",
                v if v < 0.66 => "Balanced",
                _ => "Snappy",
            }
            .to_string()
        });
        y += gap * 0.5;
        ui.toggle(d2d, id("mblur"), row(&mut y, ui.px(34.0)), "Motion blur", &mut st.motion_blur);
        y += gap * 0.5;
        let r = row(&mut y, ui.px(36.0));
        if ui.button(d2d, id("autozoom"), r, "Auto-zoom on clicks", ButtonKind::Secondary) {
            ui.edit_started = true;
            let lvl = self.sel.and_then(|i| self.p.zooms.get(i)).map_or(2.0, |z| z.scale);
            self.p.zooms = project::auto_zooms(&self.p.rec, lvl);
            self.sel = None;
        }
        y += gap * 0.5;
        let n_clicks = self.p.rec.clicks.iter().filter(|c| c.down).count();
        ui.text(
            d2d,
            row(&mut y, ui.px(18.0)),
            &format!("{} clicks recorded · {} zoom{}", n_clicks, self.p.zooms.len(), if self.p.zooms.len() == 1 { "" } else { "s" }),
            11.5,
            W_NORMAL,
            rgba(FAINT, 1.0),
            Align::Left,
        );
        if let Some(i) = self.sel.filter(|i| *i < self.p.zooms.len()) {
            y += gap;
            let card = Rect::new(x - ui.px(10.0), y, w + ui.px(20.0), ui.px(122.0));
            ui.fill(d2d, card, ui.px(10.0), rgba(PANEL2, 1.0));
            ui.stroke(d2d, card, ui.px(10.0), rgba(ACCENT, 0.6), 1.0);
            y += ui.px(10.0);
            ui.text(d2d, row(&mut y, ui.px(20.0)), "Selected zoom", 12.5, W_SEMI, rgba(TEXT, 1.0), Align::Left);
            let z = &mut self.p.zooms[i];
            ui.slider(d2d, id("zlevel"), row(&mut y, ui.px(40.0)), "Zoom level", &mut z.scale, 1.2, 4.0, &|v| format!("{v:.1}×"));
            y += gap * 0.5;
            let r = row(&mut y, ui.px(32.0));
            if ui.button(d2d, id("zdel"), r, "Delete zoom", ButtonKind::Secondary) {
                ui.edit_started = true;
                self.p.zooms.remove(i);
                self.sel = None;
            }
            y += ui.px(14.0);
        }
        y += ui.px(22.0);

        if !self.tracks.is_empty() {
            ui.section(d2d, row(&mut y, ui.px(24.0)), "Audio");
            let st = &mut self.p.style;
            let vol = |v: f32| if v <= 0.001 { "Muted".to_string() } else { format!("{:.0}%", v * 100.0) };
            if self.tracks.system.is_some() {
                ui.slider(d2d, id("sysvol"), row(&mut y, ui.px(40.0)), "System audio", &mut st.sys_volume, 0.0, 2.0, &vol);
                y += gap * 0.5;
            }
            if self.tracks.mic.is_some() {
                ui.slider(d2d, id("micvol"), row(&mut y, ui.px(40.0)), "Microphone", &mut st.mic_volume, 0.0, 2.0, &vol);
                y += gap * 0.5;
            }
            y += ui.px(16.0);
        }

        ui.section(d2d, row(&mut y, ui.px(24.0)), "Keystrokes");
        let st = &mut self.p.style;
        ui.toggle(d2d, id("keys"), row(&mut y, ui.px(34.0)), "Show shortcut badges", &mut st.show_keys);
        ui.text(d2d, row(&mut y, ui.px(18.0)), &format!("{} shortcuts recorded", self.p.rec.keys.len()), 11.5, W_NORMAL, rgba(FAINT, 1.0), Align::Left);
        y += ui.px(16.0);
        let r = row(&mut y, ui.px(32.0));
        if ui.button(d2d, id("reset"), r, "Reset style", ButtonKind::Ghost) {
            ui.edit_started = true;
            self.p.style = Style::default();
        }
        y += ui.px(20.0);
        ui.pop_clip(d2d);

        let content_h = y + self.side_scroll - view.y;
        self.side_scroll = self.side_scroll.clamp(0.0, (content_h - view.h).max(0.0));
    }

    fn timeline(&mut self, ui: &mut Ui, d2d: &D2d, area: Rect) {
        ui.fill(d2d, area, 0.0, rgba(PANEL, 1.0));
        ui.line(d2d, area.x, area.y + 0.5, area.r(), area.y + 0.5, rgba(BORDER, 1.0), 1.0);
        let inner = area.inset_xy(ui.px(20.0), ui.px(10.0));
        let (transport, rest) = inner.cut_top(ui.px(44.0));
        let dur = self.p.rec.duration.max(0.01);

        // Transport row.
        let pb = Rect::new(transport.x, transport.cy() - ui.px(17.0), ui.px(34.0), ui.px(34.0));
        let (hov, _, clicked) = ui.behave(id("play"), pb);
        if hov {
            ui.cursor = CursorShape::Hand;
        }
        ui.circle(d2d, pb.cx(), pb.cy(), pb.w * 0.5, rgba(if hov { ACCENT_HI } else { ACCENT }, 1.0));
        unsafe {
            let k = pb.w * 0.5;
            if self.playing {
                let bw = k * 0.22;
                d2d.dc.FillRectangle(&crate::d2d::rect(pb.cx() - bw * 1.6, pb.cy() - k * 0.4, pb.cx() - bw * 0.6, pb.cy() + k * 0.4), d2d.brush(rgba(0xffffff, 1.0)));
                d2d.dc.FillRectangle(&crate::d2d::rect(pb.cx() + bw * 0.6, pb.cy() - k * 0.4, pb.cx() + bw * 1.6, pb.cy() + k * 0.4), d2d.brush(rgba(0xffffff, 1.0)));
            } else if let Ok(g) = d2d.polygon(&[(-0.3, -0.42), (0.48, 0.0), (-0.3, 0.42)]) {
                d2d.dc.SetTransform(&(Matrix3x2::scale(k, k) * Matrix3x2::translation(pb.cx() + k * 0.06, pb.cy())));
                d2d.dc.FillGeometry(&g, d2d.brush(rgba(0xffffff, 1.0)), None);
                d2d.dc.SetTransform(&Matrix3x2::identity());
            }
        }
        if clicked {
            self.toggle_play();
        }
        let tr = Rect::new(pb.r() + ui.px(14.0), transport.y, ui.px(220.0), transport.h);
        ui.text(d2d, tr, &format!("{}  /  {}", fmt_time(self.t), fmt_time(dur)), 14.0, W_SEMI, rgba(TEXT, 1.0), Align::Left);
        let hint = Rect::new(tr.r(), transport.y, transport.r() - tr.r(), transport.h);
        ui.text(
            d2d,
            hint,
            &format!(
                "Clip {}   ·   Space play/pause   ·   Double-click zoom track to add   ·   Del removes   ·   Ctrl+Z undo",
                fmt_time(self.p.trim_end - self.p.trim_start)
            ),
            11.5,
            W_NORMAL,
            rgba(FAINT, 1.0),
            Align::Right,
        );

        let (ruler, rest) = rest.cut_top(ui.px(26.0));
        let (_, rest) = rest.cut_top(ui.px(4.0));
        let (clip, rest) = rest.cut_top(ui.px(46.0));
        let (_, rest) = rest.cut_top(ui.px(8.0));
        let (ztrack, _) = rest.cut_top(ui.px(46.0));
        let x_of = |t: f64| ruler.x + (t / dur) as f32 * ruler.w;
        let t_of = |x: f32| (((x - ruler.x) / ruler.w) as f64 * dur).clamp(0.0, dur);

        // Ruler with adaptive tick spacing.
        let min_px = ui.px(70.0);
        let step = [0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0]
            .into_iter()
            .find(|s| (*s / dur) as f32 * ruler.w >= min_px)
            .unwrap_or(600.0);
        let mut tt = 0.0;
        while tt <= dur + 1e-6 {
            let x = x_of(tt);
            ui.line(d2d, x, ruler.b() - ui.px(6.0), x, ruler.b(), rgba(FAINT, 1.0), 1.0);
            let label = if step < 1.0 { format!("{tt:.1}s") } else { fmt_time(tt).trim_end_matches(".0").to_string() };
            ui.text(d2d, Rect::new(x + ui.px(4.0), ruler.y, ui.px(60.0), ruler.h - ui.px(6.0)), &label, 11.0, W_NORMAL, rgba(FAINT, 1.0), Align::Left);
            tt += step;
        }

        // Clip track (trim).
        let handle_w = ui.px(10.0);
        let (a, b) = (x_of(self.p.trim_start), x_of(self.p.trim_end));
        ui.fill(d2d, clip, ui.px(8.0), rgba(0x12141A, 1.0));
        let body = Rect::new(a, clip.y, b - a, clip.h);
        ui.fill(d2d, body, ui.px(8.0), rgba(0x2B2F55, 1.0));
        ui.stroke(d2d, body, ui.px(8.0), rgba(ACCENT, 0.9), ui.px(1.5));
        ui.text(d2d, body.inset_xy(ui.px(14.0), 0.0), "Recording", 12.0, W_SEMI, rgba(TEXT, 0.85), Align::Left);
        for c in self.p.rec.clicks.iter().filter(|c| c.down) {
            ui.circle(d2d, x_of(c.t), clip.b() - ui.px(9.0), ui.px(3.0), rgba(0xffffff, 0.8));
        }
        // Where the recording was paused: a seam the viewer won't see, but useful to know.
        for &c in &self.p.rec.cuts {
            let x = x_of(c);
            ui.line(d2d, x, clip.y + ui.px(4.0), x, clip.b() - ui.px(4.0), rgba(AMBER, 0.9), ui.px(2.0));
            ui.circle(d2d, x, clip.y + ui.px(4.0), ui.px(3.5), rgba(AMBER, 1.0));
        }
        for k in &self.p.rec.keys {
            let x = x_of(k.t);
            ui.line(d2d, x, clip.y + ui.px(8.0), x, clip.y + ui.px(16.0), rgba(0xFFD34E, 0.9), ui.px(2.0));
        }
        let h_start = Rect::new(a, clip.y, handle_w, clip.h);
        let h_end = Rect::new(b - handle_w, clip.y, handle_w, clip.h);
        for (hr, wid, kind) in [(h_start, id("trim_s"), Drag::TrimStart), (h_end, id("trim_e"), Drag::TrimEnd)] {
            let (hov, held, _) = ui.behave(wid, hr);
            if hov || held {
                ui.cursor = CursorShape::SizeWE;
            }
            if held && self.drag == Drag::None {
                self.drag = kind;
                ui.edit_started = true;
                self.playing = false;
            }
            ui.fill(d2d, Rect::new(hr.cx() - ui.px(2.0), hr.y + ui.px(12.0), ui.px(4.0), hr.h - ui.px(24.0)), ui.px(2.0), rgba(0xffffff, if hov || held { 1.0 } else { 0.7 }));
        }

        // Zoom track.
        ui.fill(d2d, ztrack, ui.px(8.0), rgba(0x12141A, 1.0));
        if self.p.zooms.is_empty() {
            ui.text(d2d, ztrack, "No zooms yet — double-click here to add one, or use “Auto-zoom on clicks”", 12.0, W_NORMAL, rgba(FAINT, 1.0), Align::Center);
        }
        let edge = ui.px(8.0);
        let mut hovered_any = false;
        for i in 0..self.p.zooms.len() {
            let z = self.p.zooms[i];
            let r = Rect::new(x_of(z.start), ztrack.y + ui.px(3.0), (x_of(z.end) - x_of(z.start)).max(ui.px(6.0)), ztrack.h - ui.px(6.0));
            let sel = self.sel == Some(i);
            let left = Rect::new(r.x - edge * 0.5, r.y, edge, r.h);
            let right = Rect::new(r.r() - edge * 0.5, r.y, edge, r.h);
            let (hl, held_l, _) = ui.behave(idx("zl", i), left);
            let (hr, held_r, _) = ui.behave(idx("zr", i), right);
            let (hb, held_b, clicked) = ui.behave(idx("zb", i), r);
            hovered_any |= hl || hr || hb;
            if hl || hr || held_l || held_r {
                ui.cursor = CursorShape::SizeWE;
            } else if hb {
                ui.cursor = CursorShape::Hand;
            }
            if self.drag == Drag::None {
                if held_l {
                    self.drag = Drag::ZoomStart(i);
                    self.sel = Some(i);
                    ui.edit_started = true;
                } else if held_r {
                    self.drag = Drag::ZoomEnd(i);
                    self.sel = Some(i);
                    ui.edit_started = true;
                } else if held_b && ui.drag_delta().0.abs() > ui.px(3.0) {
                    self.drag = Drag::ZoomMove { i, grab: t_of(ui.inp.mx - ui.drag_delta().0) - z.start, len: z.end - z.start };
                    self.sel = Some(i);
                    ui.edit_started = true;
                }
            }
            if clicked {
                self.sel = Some(i);
            }
            let c = if sel { ACCENT_HI } else { ACCENT };
            ui.fill(d2d, r, ui.px(7.0), rgba(c, if sel { 1.0 } else if hb { 0.85 } else { 0.7 }));
            if sel {
                ui.stroke(d2d, r, ui.px(7.0), rgba(0xffffff, 0.95), ui.px(2.0));
            }
            if r.w > ui.px(60.0) {
                ui.text(d2d, r.inset_xy(ui.px(10.0), 0.0), &format!("Zoom {:.1}×", z.scale), 12.0, W_SEMI, rgba(0xffffff, 1.0), Align::Left);
            }
            for e in [left, right] {
                ui.fill(d2d, Rect::new(e.cx() - ui.px(1.5), r.y + ui.px(10.0), ui.px(3.0), r.h - ui.px(20.0)), ui.px(1.5), rgba(0xffffff, if sel { 0.9 } else { 0.5 }));
            }
        }
        if ui.inp.double && ztrack.contains(ui.inp.mx, ui.inp.my) && !hovered_any {
            let tc = t_of(ui.inp.mx);
            let mut s = (tc - 1.0).max(0.0);
            let mut e = (tc + 1.5).min(dur);
            // Don't overlap neighbours.
            for z in &self.p.zooms {
                if z.end <= tc {
                    s = s.max(z.end);
                }
                if z.start >= tc {
                    e = e.min(z.start);
                }
            }
            if e - s > 0.3 {
                ui.edit_started = true;
                self.p.zooms.push(ZoomSeg { start: s, end: e, scale: 2.0 });
                self.p.zooms.sort_by(|a, b| a.start.total_cmp(&b.start));
                self.sel = self.p.zooms.iter().position(|z| z.start == s);
            }
        }

        // Scrubbing on the ruler / clip / empty zoom track.
        let scrub_area = Rect::new(ruler.x, ruler.y, ruler.w, clip.b() - ruler.y);
        let (_, held, _) = ui.behave(id("scrub"), scrub_area);
        if held && self.drag == Drag::None {
            self.drag = Drag::Scrub;
            self.playing = false;
        }
        let (_, zheld, zclick) = ui.behave(id("ztrack_bg"), ztrack);
        let over_zoom = self.p.zooms.iter().any(|z| (z.start..=z.end).contains(&t_of(ui.inp.mx)));
        if zclick && !hovered_any && !over_zoom {
            self.sel = None;
        }
        let _ = zheld;

        // Apply drags.
        if ui.inp.down || ui.inp.pressed {
            let mt = t_of(ui.inp.mx);
            let min_len = 0.3;
            match self.drag {
                Drag::Scrub => self.t = mt,
                Drag::TrimStart => {
                    self.p.trim_start = mt.min(self.p.trim_end - 0.5).max(0.0);
                    self.t = self.p.trim_start;
                }
                Drag::TrimEnd => {
                    self.p.trim_end = mt.max(self.p.trim_start + 0.5).min(dur);
                    self.t = self.p.trim_end;
                }
                Drag::ZoomStart(i) if i < self.p.zooms.len() => {
                    let lo = if i > 0 { self.p.zooms[i - 1].end } else { 0.0 };
                    self.p.zooms[i].start = mt.clamp(lo, self.p.zooms[i].end - min_len);
                }
                Drag::ZoomEnd(i) if i < self.p.zooms.len() => {
                    let hi = self.p.zooms.get(i + 1).map_or(dur, |z| z.start);
                    self.p.zooms[i].end = mt.clamp(self.p.zooms[i].start + min_len, hi);
                }
                Drag::ZoomMove { i, grab, len } if i < self.p.zooms.len() => {
                    let lo = if i > 0 { self.p.zooms[i - 1].end } else { 0.0 };
                    let hi = self.p.zooms.get(i + 1).map_or(dur, |z| z.start);
                    let s = (mt - grab).clamp(lo, (hi - len).max(lo));
                    self.p.zooms[i].start = s;
                    self.p.zooms[i].end = (s + len).min(hi);
                }
                _ => {}
            }
        } else {
            self.drag = Drag::None;
        }

        // Playhead.
        let px = x_of(self.t);
        ui.line(d2d, px, ruler.y + ui.px(4.0), px, ztrack.b(), rgba(0xffffff, 0.95), ui.px(2.0));
        ui.circle(d2d, px, ruler.y + ui.px(6.0), ui.px(5.0), rgba(0xffffff, 1.0));
    }
}
