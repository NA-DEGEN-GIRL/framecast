//! Global mouse / keyboard logging during a recording (low-level hooks), plus the
//! stop hotkey. Hooks run on their own thread with a message pump.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::clock::RecClock;
use crate::util::now_hns;

#[derive(Clone, Copy, Debug)]
pub struct Click {
    pub t: f64,
    pub x: f32,
    pub y: f32,
    pub button: u8, // 0 left, 1 right, 2 middle
    pub down: bool,
}

#[derive(Clone, Debug)]
pub struct Key {
    pub t: f64,
    pub label: String,
}

#[derive(Default)]
pub struct Log {
    pub clicks: Vec<Click>,
    pub keys: Vec<Key>,
}

struct HookState {
    clock: Arc<RecClock>,
    origin: (i32, i32),
    log: Log,
    all_keys: bool,
}

static STATE: Mutex<Option<HookState>> = Mutex::new(None);
static STOP_FLAG: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);
static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);

/// Hotkey that stops a recording from anywhere: Ctrl + Shift + F9.
pub const STOP_HOTKEY_LABEL: &str = "Ctrl + Shift + F9";
const HOTKEY_ID: i32 = 0xF9;
/// Hotkey that pauses / resumes: Ctrl + Shift + F10.
pub const PAUSE_HOTKEY_LABEL: &str = "Ctrl + Shift + F10";
const PAUSE_HOTKEY_ID: i32 = 0xFA;

unsafe extern "system" fn mouse_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 {
            let info = &*(lp.0 as *const MSLLHOOKSTRUCT);
            let (button, down) = match wp.0 as u32 {
                WM_LBUTTONDOWN => (0, true),
                WM_LBUTTONUP => (0, false),
                WM_RBUTTONDOWN => (1, true),
                WM_RBUTTONUP => (1, false),
                WM_MBUTTONDOWN => (2, true),
                WM_MBUTTONUP => (2, false),
                _ => (255, false),
            };
            // Clicks on our own recording bar are controls, not part of the demo.
            let ours = || {
                let mut pid = 0u32;
                GetWindowThreadProcessId(WindowFromPoint(info.pt), Some(&mut pid));
                pid == std::process::id()
            };
            if button != 255 && !ours() {
                if let Ok(mut g) = STATE.lock() {
                    if let Some(s) = g.as_mut() {
                        // Clicks while paused aren't part of the video.
                        let Some(t) = s.clock.active(now_hns()) else { return CallNextHookEx(None, code, wp, lp) };
                        let t = t as f64 / 1e7;
                        let (ox, oy) = s.origin;
                        s.log.clicks.push(Click {
                            t,
                            x: (info.pt.x - ox) as f32,
                            y: (info.pt.y - oy) as f32,
                            button,
                            down,
                        });
                    }
                }
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

fn is_modifier(vk: u32) -> bool {
    matches!(
        VIRTUAL_KEY(vk as u16),
        VK_SHIFT | VK_LSHIFT | VK_RSHIFT | VK_CONTROL | VK_LCONTROL | VK_RCONTROL | VK_MENU | VK_LMENU | VK_RMENU | VK_LWIN | VK_RWIN
    )
}

fn key_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000 != 0 }
}

fn key_name(info: &KBDLLHOOKSTRUCT) -> String {
    let vk = VIRTUAL_KEY(info.vkCode as u16);
    let fixed = match vk {
        VK_RETURN => "Enter",
        VK_ESCAPE => "Esc",
        VK_TAB => "Tab",
        VK_BACK => "Backspace",
        VK_DELETE => "Delete",
        VK_SPACE => "Space",
        VK_LEFT => "←",
        VK_RIGHT => "→",
        VK_UP => "↑",
        VK_DOWN => "↓",
        VK_HOME => "Home",
        VK_END => "End",
        VK_PRIOR => "PgUp",
        VK_NEXT => "PgDn",
        _ => "",
    };
    if !fixed.is_empty() {
        return fixed.to_string();
    }
    if (VK_F1.0..=VK_F24.0).contains(&vk.0) {
        return format!("F{}", vk.0 - VK_F1.0 + 1);
    }
    let mut lp = (info.scanCode as i32) << 16;
    if info.flags.0 & LLKHF_EXTENDED.0 != 0 {
        lp |= 1 << 24;
    }
    let mut buf = [0u16; 64];
    let n = unsafe { GetKeyNameTextW(lp, &mut buf) };
    if n > 0 {
        let s = String::from_utf16_lossy(&buf[..n as usize]);
        if s.chars().count() == 1 { s.to_uppercase() } else { s }
    } else {
        format!("#{}", vk.0)
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 && matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN) {
            let info = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            if !is_modifier(info.vkCode) {
                let ctrl = key_down(VK_CONTROL);
                let alt = key_down(VK_MENU);
                let shift = key_down(VK_SHIFT);
                let win = key_down(VK_LWIN) || key_down(VK_RWIN);
                let vk = VIRTUAL_KEY(info.vkCode as u16);
                let special = matches!(vk, VK_RETURN | VK_ESCAPE | VK_TAB | VK_DELETE)
                    || (VK_F1.0..=VK_F24.0).contains(&vk.0);
                // Our own stop / pause hotkeys shouldn't show up as badges.
                let own_hotkey = ctrl && shift && !alt && matches!(vk, VK_F9 | VK_F10);
                if let Ok(mut g) = STATE.lock() {
                    if let Some(s) = g.as_mut() {
                        // Privacy: by default only shortcuts are logged, never plain typing.
                        if !own_hotkey && (ctrl || alt || win || special || s.all_keys) {
                            let mut parts = Vec::new();
                            if ctrl {
                                parts.push("Ctrl".to_string());
                            }
                            if alt {
                                parts.push("Alt".to_string());
                            }
                            if shift && (ctrl || alt || win || special) {
                                parts.push("Shift".to_string());
                            }
                            if win {
                                parts.push("Win".to_string());
                            }
                            parts.push(key_name(info));
                            if let Some(t) = s.clock.active(now_hns()) {
                                s.log.keys.push(Key { t: t as f64 / 1e7, label: parts.join(" + ") });
                            }
                        }
                    }
                }
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

pub struct InputLogger {
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InputLogger {
    /// `origin`: top-left of the recorded monitor in virtual-screen pixels.
    pub fn start(clock: Arc<RecClock>, origin: (i32, i32), all_keys: bool, stop: Arc<AtomicBool>) -> InputLogger {
        *STATE.lock().unwrap() = Some(HookState { clock, origin, log: Log::default(), all_keys });
        *STOP_FLAG.lock().unwrap() = Some(stop);
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || unsafe {
            HOOK_THREAD.store(windows::Win32::System::Threading::GetCurrentThreadId(), Ordering::SeqCst);
            let m = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0).ok();
            let k = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), None, 0).ok();
            let hk = RegisterHotKey(None, HOTKEY_ID, MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, VK_F9.0 as u32).is_ok();
            let hk_pause = RegisterHotKey(None, PAUSE_HOTKEY_ID, MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, VK_F10.0 as u32).is_ok();
            let _ = tx.send(());
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                if msg.message == WM_HOTKEY && msg.wParam.0 as i32 == HOTKEY_ID {
                    if let Some(f) = STOP_FLAG.lock().unwrap().as_ref() {
                        f.store(true, Ordering::SeqCst);
                    }
                }
                if msg.message == WM_HOTKEY && msg.wParam.0 as i32 == PAUSE_HOTKEY_ID {
                    if let Some(s) = STATE.lock().unwrap().as_ref() {
                        s.clock.toggle();
                    }
                }
            }
            if hk {
                let _ = UnregisterHotKey(None, HOTKEY_ID);
            }
            if hk_pause {
                let _ = UnregisterHotKey(None, PAUSE_HOTKEY_ID);
            }
            if let Some(m) = m {
                let _ = UnhookWindowsHookEx(m);
            }
            if let Some(k) = k {
                let _ = UnhookWindowsHookEx(k);
            }
        });
        let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
        InputLogger { thread: Some(thread) }
    }

    pub fn finish(mut self) -> Log {
        self.shutdown();
        STATE.lock().unwrap().take().map(|s| s.log).unwrap_or_default()
    }

    fn shutdown(&mut self) {
        if let Some(t) = self.thread.take() {
            unsafe {
                let _ = PostThreadMessageW(HOOK_THREAD.load(Ordering::SeqCst), WM_QUIT, WPARAM(0), LPARAM(0));
            }
            let _ = t.join();
        }
        *STOP_FLAG.lock().unwrap() = None;
    }
}

impl Drop for InputLogger {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Cursor shapes we know how to redraw crisply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorKind {
    Hidden = -1,
    Arrow = 0,
    IBeam = 1,
    Hand = 2,
    Wait = 3,
    Cross = 4,
    SizeAll = 5,
    SizeWE = 6,
    SizeNS = 7,
    SizeNWSE = 8,
    SizeNESW = 9,
    No = 10,
}


pub struct CursorSampler {
    known: Vec<(isize, CursorKind)>,
}

impl CursorSampler {
    pub fn new() -> CursorSampler {
        use CursorKind::*;
        let ids = [
            (IDC_ARROW, Arrow),
            (IDC_IBEAM, IBeam),
            (IDC_HAND, Hand),
            (IDC_WAIT, Wait),
            (IDC_APPSTARTING, Wait),
            (IDC_CROSS, Cross),
            (IDC_SIZEALL, SizeAll),
            (IDC_SIZEWE, SizeWE),
            (IDC_SIZENS, SizeNS),
            (IDC_SIZENWSE, SizeNWSE),
            (IDC_SIZENESW, SizeNESW),
            (IDC_NO, No),
        ];
        let known = ids
            .iter()
            .filter_map(|(id, k)| unsafe { LoadCursorW(None, *id).ok().map(|h| (h.0 as isize, *k)) })
            .collect();
        CursorSampler { known }
    }

    /// Current cursor position (virtual-screen pixels) and shape.
    pub fn sample(&self) -> (i32, i32, CursorKind) {
        let mut ci = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
        unsafe {
            if GetCursorInfo(&mut ci).is_err() {
                return (0, 0, CursorKind::Hidden);
            }
        }
        if ci.flags.0 & CURSOR_SHOWING.0 == 0 {
            return (ci.ptScreenPos.x, ci.ptScreenPos.y, CursorKind::Hidden);
        }
        let h = ci.hCursor.0 as isize;
        let kind = self.known.iter().find(|(k, _)| *k == h).map(|(_, c)| *c).unwrap_or(CursorKind::Arrow);
        (ci.ptScreenPos.x, ci.ptScreenPos.y, kind)
    }
}
