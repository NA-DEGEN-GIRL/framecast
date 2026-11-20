#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture;
mod d2d;
mod export;
mod gpu;
mod input;
mod json;
mod media;
mod project;
mod recorder;
mod render;
mod util;

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
    if args.get(1).map(String::as_str) == Some("--export") {
        let dir = std::path::PathBuf::from(args.get(2).expect("project dir"));
        let p = project::Project::open(&dir).expect("open project");
        let out = dir.join("export_test.mp4");
        let t = std::time::Instant::now();
        let r = export::run(&p, &out, &Default::default(), &Default::default());
        println!("{:?} in {:.2}s -> {}", r, t.elapsed().as_secs_f64(), out.display());
        return;
    }
    if args.get(1).map(String::as_str) == Some("--record") {
        let secs: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3.0);
        let mon = capture::monitors().into_iter().next().expect("no monitor");
        let rec = recorder::start(recorder::RecordOptions { monitor: mon, fps: 60, all_keys: false }).expect("start");
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
    }
}
