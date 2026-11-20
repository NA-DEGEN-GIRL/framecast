#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// A misspelled/unimported constant in a `match` silently becomes a catch-all binding.
#![deny(unreachable_patterns)]

mod app;
mod audio;
mod capture;
mod clock;
mod d2d;
mod export;
mod gif;
mod gpu;
mod icon;
mod image;
mod input;
mod json;
mod media;
mod project;
mod recorder;
mod render;
mod selector;
mod ui;
mod util;
mod window;

use util::Res;

fn init() -> Res<()> {
    unsafe {
        use windows::Win32::System::Com::*;
        use windows::Win32::UI::HiDpi::*;
        // Physical pixels everywhere: capture, hooks and UI must agree on coordinates.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    util::init_log();
    media::startup()
}

fn main() {
    if let Err(e) = init() {
        log!("init failed: {e}");
        std::process::exit(1);
    }
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--make-icon") {
        let out = args.get(2).map_or("assets/framecast.ico", String::as_str);
        if let Err(e) = icon::make(std::path::Path::new(out)) {
            log!("icon failed: {e}");
            std::process::exit(2);
        }
        return;
    }
    if args.get(1).map(String::as_str) == Some("--export") {
        let dir = std::path::PathBuf::from(args.get(2).expect("project dir"));
        let p = project::Project::open(&dir).expect("open project");
        let out = dir.join(if p.is_gif() { "export_test.gif" } else { "export_test.mp4" });
        let t = std::time::Instant::now();
        let r = if p.is_gif() {
            export::run_gif(&p, &out, &Default::default(), &Default::default())
        } else {
            export::run(&p, &out, &Default::default(), &Default::default())
        };
        println!("{:?} in {:.2}s -> {}", r, t.elapsed().as_secs_f64(), out.display());
        return;
    }
    if args.get(1).map(String::as_str) == Some("--record") {
        let secs: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3.0);
        let mon = capture::monitors().into_iter().next().expect("no monitor");
        let rec = recorder::start(recorder::RecordOptions { target: capture::Target::Display(mon), fps: 60, all_keys: false, system_audio: true, mic: false }).expect("start");
        while rec.elapsed() < secs && !rec.stop_requested() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        match rec.join() {
            Ok(dir) => println!("{}", dir.display()),
            Err(e) => {
                log!("recording failed: {e}");
                std::process::exit(2);
            }
        }
        return;
    }
    match app::App::new() {
        Ok(mut a) => {
            if args.get(1).map(String::as_str) == Some("--open") {
                if let Some(d) = args.get(2) {
                    a.open_project(d.into());
                }
            }
            a.run()
        }
        Err(e) => {
            log!("startup failed: {e}");
            let msg = util::wide(&format!("Framecast couldn't start:
{e}"));
            let title = util::wide("Framecast");
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
                    None,
                    windows::core::PCWSTR(msg.as_ptr()),
                    windows::core::PCWSTR(title.as_ptr()),
                    windows::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
                );
            }
        }
    }
}
