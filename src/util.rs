//! Small shared helpers: clock, logging, paths, wide strings.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::UI::Shell::{FOLDERID_Videos, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::core::PCWSTR;

pub type Res<T> = Result<T, String>;

/// Converts any displayable error into our string error type.
pub trait Ctx<T> {
    fn ctx(self, what: &str) -> Res<T>;
}
impl<T, E: std::fmt::Display> Ctx<T> for Result<T, E> {
    fn ctx(self, what: &str) -> Res<T> {
        self.map_err(|e| format!("{what}: {e}"))
    }
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn pcwstr(w: &[u16]) -> PCWSTR {
    PCWSTR(w.as_ptr())
}

fn qpc_freq() -> i64 {
    static F: OnceLock<i64> = OnceLock::new();
    *F.get_or_init(|| {
        let mut f = 0i64;
        unsafe {
            let _ = QueryPerformanceFrequency(&mut f);
        }
        f.max(1)
    })
}

/// Monotonic time in 100ns units (the Media Foundation time base).
pub fn now_hns() -> i64 {
    let mut c = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
    }
    let f = qpc_freq();
    (c / f) * 10_000_000 + (c % f) * 10_000_000 / f
}

pub fn secs_to_hns(s: f64) -> i64 {
    (s * 1e7).round() as i64
}

pub fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0);
    let m = (s / 60.0).floor() as u32;
    let r = s - m as f64 * 60.0;
    format!("{m}:{r:04.1}")
}

static LOG: OnceLock<Mutex<Option<std::fs::File>>> = OnceLock::new();

pub fn app_dir() -> PathBuf {
    unsafe {
        if let Ok(p) = SHGetKnownFolderPath(&FOLDERID_Videos, KF_FLAG_DEFAULT, None) {
            let s = p.to_string().unwrap_or_default();
            windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as _));
            if !s.is_empty() {
                return PathBuf::from(s).join("Framecast");
            }
        }
    }
    std::env::temp_dir().join("Framecast")
}

pub fn init_log() {
    let dir = app_dir();
    let _ = std::fs::create_dir_all(&dir);
    let f = std::fs::File::create(dir.join("framecast.log")).ok();
    let _ = LOG.set(Mutex::new(f));
}

pub fn log_line(s: &str) {
    eprintln!("{s}");
    if let Some(m) = LOG.get() {
        if let Ok(mut g) = m.lock() {
            if let Some(f) = g.as_mut() {
                let _ = writeln!(f, "{s}");
            }
        }
    }
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::util::log_line(&format!($($t)*)) };
}

/// Local timestamp suitable for folder names: 2026-11-20_19-24-05
pub fn stamp() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}
