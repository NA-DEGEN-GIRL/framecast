//! Win32 windows rendered with Direct2D through a flip-model swap chain.
//! The window procedure only records input; all drawing happens in the app loop.

use std::cell::RefCell;
use std::collections::HashMap;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap1;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, PCWSTR};

use crate::d2d::D2d;
use crate::gpu::Gpu;
use crate::util::{Ctx, Res, wide};

#[derive(Default, Clone)]
pub struct Input {
    pub mx: f32,
    pub my: f32,
    pub inside: bool,
    pub down: bool,
    pub pressed: bool,
    pub released: bool,
    pub double: bool,
    pub wheel: f32,
    pub keys: Vec<(u16, bool, bool)>, // vk, ctrl, shift
    pub resized: bool,
    pub close: bool,
    pub any: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Arrow,
    Hand,
    SizeWE,
}

struct WinState {
    input: Input,
    cursor: CursorShape,
    /// Pixel width (from the right edge) that is *not* a drag handle; for popup bars.
    drag_except_right: Option<i32>,
}

thread_local! {
    static WINDOWS: RefCell<HashMap<isize, WinState>> = RefCell::new(HashMap::new());
}

fn with_state<R>(hwnd: HWND, f: impl FnOnce(&mut WinState) -> R) -> Option<R> {
    WINDOWS.with(|w| w.try_borrow_mut().ok()?.get_mut(&(hwnd.0 as isize)).map(f))
}

pub fn take_input(hwnd: HWND) -> Input {
    with_state(hwnd, |s| {
        let mut out = s.input.clone();
        // Edge-triggered fields reset each frame; level fields persist.
        s.input.pressed = false;
        s.input.released = false;
        s.input.double = false;
        s.input.wheel = 0.0;
        s.input.keys.clear();
        s.input.resized = false;
        s.input.close = false;
        s.input.any = false;
        out.down = out.down || out.pressed;
        out
    })
    .unwrap_or_default()
}

pub fn set_cursor(hwnd: HWND, c: CursorShape) {
    with_state(hwnd, |s| s.cursor = c);
}

fn lparam_xy(lp: LPARAM) -> (f32, f32) {
    let x = (lp.0 & 0xffff) as i16 as f32;
    let y = ((lp.0 >> 16) & 0xffff) as i16 as f32;
    (x, y)
}

/// Win32 calls that can re-enter the window procedure; run after the state borrow ends.
enum After {
    Default,
    Ret(LRESULT),
    TrackLeave,
    Capture,
    Release,
    Cursor(CursorShape),
    HitTest(i32),
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let after = with_state(hwnd, |s| {
            let i = &mut s.input;
            i.any = true;
            match msg {
                WM_MOUSEMOVE => {
                    (i.mx, i.my) = lparam_xy(lp);
                    if !i.inside {
                        i.inside = true;
                        return After::TrackLeave;
                    }
                    After::Default
                }
                WM_MOUSELEAVE => {
                    // While dragging (captured) the pointer may leave; keep tracking it.
                    i.inside = i.down;
                    After::Default
                }
                WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                    (i.mx, i.my) = lparam_xy(lp);
                    i.inside = true;
                    i.down = true;
                    i.pressed = true;
                    i.double |= msg == WM_LBUTTONDBLCLK;
                    After::Capture
                }
                WM_LBUTTONUP => {
                    (i.mx, i.my) = lparam_xy(lp);
                    i.down = false;
                    i.released = true;
                    After::Release
                }
                WM_MOUSEWHEEL => {
                    i.wheel += ((wp.0 >> 16) & 0xffff) as i16 as f32 / 120.0;
                    After::Ret(LRESULT(0))
                }
                WM_KEYDOWN | WM_SYSKEYDOWN => {
                    let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
                    let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
                    i.keys.push((wp.0 as u16, ctrl, shift));
                    if msg == WM_SYSKEYDOWN { After::Default } else { After::Ret(LRESULT(0)) }
                }
                WM_SIZE => {
                    i.resized = true;
                    After::Default
                }
                WM_CLOSE => {
                    i.close = true;
                    After::Ret(LRESULT(0))
                }
                WM_SETCURSOR if (lp.0 & 0xffff) as u32 == HTCLIENT => After::Cursor(s.cursor),
                WM_NCHITTEST => match s.drag_except_right {
                    Some(px) => After::HitTest(px),
                    None => After::Default,
                },
                _ => {
                    i.any = false;
                    After::Default
                }
            }
        })
        .unwrap_or(After::Default);

        match after {
            After::Ret(r) => return r,
            After::TrackLeave => {
                let mut t = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut t);
            }
            After::Capture => {
                SetCapture(hwnd);
                return LRESULT(0);
            }
            After::Release => {
                let _ = ReleaseCapture();
                return LRESULT(0);
            }
            After::Cursor(c) => {
                let id = match c {
                    CursorShape::Arrow => IDC_ARROW,
                    CursorShape::Hand => IDC_HAND,
                    CursorShape::SizeWE => IDC_SIZEWE,
                };
                if let Ok(c) = LoadCursorW(None, id) {
                    SetCursor(Some(c));
                }
                return LRESULT(1);
            }
            After::HitTest(ex) => {
                let r = DefWindowProcW(hwnd, msg, wp, lp);
                if r.0 as u32 != HTCLIENT {
                    return r;
                }
                let mut pt = POINT { x: (lp.0 & 0xffff) as i16 as i32, y: ((lp.0 >> 16) & 0xffff) as i16 as i32 };
                let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                return LRESULT(if pt.x < rc.right - ex { HTCAPTION as isize } else { HTCLIENT as isize });
            }
            After::Default => {}
        }
        match msg {
            WM_DPICHANGED => {
                let r = &*(lp.0 as *const RECT);
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let _ = windows::Win32::Graphics::Gdi::ValidateRect(Some(hwnd), None);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// Lives in the Controls namespace of the bindings; defined here to avoid that feature.
const WM_MOUSELEAVE: u32 = 0x02A3;

const CLASS: &str = "FramecastWindow";

fn register_class() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let name = wide(CLASS);
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let icon = LoadIconW(Some(hinst.into()), PCWSTR(1 as _)).ok();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS | CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: icon.unwrap_or_default(),
            lpszClassName: PCWSTR(name.as_ptr()),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        std::mem::forget(name);
    });
}

pub struct Surface {
    pub hwnd: HWND,
    swap: IDXGISwapChain1,
    target: Option<ID2D1Bitmap1>,
    pub w: u32,
    pub h: u32,
}

pub enum Kind {
    Main,
    /// Small always-on-top bar, invisible to screen capture.
    Floating,
}

impl Surface {
    pub fn create(gpu: &Gpu, title: &str, kind: Kind, x: i32, y: i32, w: i32, h: i32) -> Res<Surface> {
        register_class();
        unsafe {
            let cls = wide(CLASS);
            let t = wide(title);
            let (ex, style) = match kind {
                Kind::Main => (WINDOW_EX_STYLE(0), WS_OVERLAPPEDWINDOW),
                Kind::Floating => (WS_EX_TOPMOST | WS_EX_TOOLWINDOW, WS_POPUP),
            };
            let hwnd = CreateWindowExW(ex, PCWSTR(cls.as_ptr()), PCWSTR(t.as_ptr()), style, x, y, w, h, None, None, None, None)
                .ctx("CreateWindowExW")?;
            WINDOWS.with(|m| {
                m.borrow_mut().insert(
                    hwnd.0 as isize,
                    WinState {
                        input: Input::default(),
                        cursor: CursorShape::Arrow,
                        drag_except_right: None,
                    },
                )
            });
            let dark: i32 = 1;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4);
            if let Kind::Floating = kind {
                let round = DWMWCP_ROUND;
                let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &round as *const _ as _, 4);
                // Keep our own controls out of the recording.
                if std::env::var_os("FRAMECAST_SHOW_CONTROLS_IN_CAPTURE").is_none() {
                    let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
                }
            }
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let (cw, chh) = ((rc.right - rc.left).max(1) as u32, (rc.bottom - rc.top).max(1) as u32);
            let dxgi: IDXGIDevice = gpu.device.cast().ctx("IDXGIDevice")?;
            let adapter = dxgi.GetAdapter().ctx("adapter")?;
            let factory: IDXGIFactory2 = adapter.GetParent().ctx("factory")?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: cw,
                Height: chh,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                Scaling: DXGI_SCALING_NONE,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                ..Default::default()
            };
            let swap = factory.CreateSwapChainForHwnd(&gpu.device, hwnd, &desc, None, None).ctx("CreateSwapChainForHwnd")?;
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
            Ok(Surface { hwnd, swap, target: None, w: cw, h: chh })
        }
    }

    pub fn set_drag_except_right(&self, px: i32) {
        with_state(self.hwnd, |s| s.drag_except_right = Some(px));
    }

    pub fn dpi_scale(&self) -> f32 {
        unsafe { GetDpiForWindow(self.hwnd).max(96) as f32 / 96.0 }
    }

    pub fn show(&self, cmd: SHOW_WINDOW_CMD) {
        unsafe {
            let _ = ShowWindow(self.hwnd, cmd);
        }
    }

    pub fn visible(&self) -> bool {
        unsafe { IsWindowVisible(self.hwnd).as_bool() && !IsIconic(self.hwnd).as_bool() }
    }

    pub fn handle_resize(&mut self) {
        unsafe {
            let mut rc = RECT::default();
            let _ = GetClientRect(self.hwnd, &mut rc);
            let (w, h) = ((rc.right - rc.left).max(1) as u32, (rc.bottom - rc.top).max(1) as u32);
            if w != self.w || h != self.h {
                self.target = None;
                if self.swap.ResizeBuffers(0, w, h, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0)).is_ok() {
                    self.w = w;
                    self.h = h;
                }
            }
        }
    }

    pub fn begin(&mut self, d2d: &D2d) -> Res<()> {
        if self.target.is_none() {
            unsafe {
                let tex: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D = self.swap.GetBuffer(0).ctx("GetBuffer")?;
                self.target = Some(d2d.swap_target_for(&tex)?);
            }
        }
        unsafe {
            if let Some(t) = &self.target {
                d2d.dc.SetTarget(t);
            }
            d2d.dc.BeginDraw();
        }
        Ok(())
    }

    pub fn end(&mut self, d2d: &D2d) -> Res<()> {
        unsafe {
            let r = d2d.dc.EndDraw(None, None);
            d2d.dc.SetTarget(None);
            if r.is_err() {
                self.target = None; // recreate next frame (device hiccup / resize)
            }
            let _ = self.swap.Present(1, DXGI_PRESENT(0));
        }
        Ok(())
    }

}

impl Drop for Surface {
    fn drop(&mut self) {
        self.target = None;
        WINDOWS.with(|m| m.borrow_mut().remove(&(self.hwnd.0 as isize)));
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Pumps pending messages. Returns false when the app should quit.
pub fn pump(wait_ms: Option<u32>) -> bool {
    unsafe {
        if let Some(ms) = wait_ms {
            MsgWaitForMultipleObjectsEx(None, ms, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                return false;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    true
}
