//! Screen capture through Windows.Graphics.Capture (GPU-only path, no CPU copies).

use windows::Foundation::TimeSpan;
use windows::Graphics::Capture::*;
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, Interface};

use crate::gpu::Gpu;
use crate::util::{Ctx, Res};

#[derive(Clone, Debug)]
pub struct Monitor {
    pub handle: isize,
    pub rect: RECT,
    pub primary: bool,
}

impl Monitor {
    pub fn width(&self) -> i32 {
        self.rect.right - self.rect.left
    }
    pub fn height(&self) -> i32 {
        self.rect.bottom - self.rect.top
    }
}

pub fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        unsafe {
            let list = &mut *(data.0 as *mut Vec<Monitor>);
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            if GetMonitorInfoW(h, &mut info.monitorInfo as *mut _ as *mut MONITORINFO).as_bool() {
                list.push(Monitor {
                    handle: h.0 as isize,
                    rect: info.monitorInfo.rcMonitor,
                    primary: info.monitorInfo.dwFlags & windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY != 0,
                });
            }
        }
        BOOL(1)
    }
    let mut list: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut list as *mut _ as isize));
    }
    list.sort_by_key(|m| (!m.primary, m.rect.left, m.rect.top));
    list
}

/// A top-level window that can be recorded.
#[derive(Clone, Debug)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub title: String,
    /// Visible bounds (no drop shadow), virtual-screen pixels.
    pub rect: RECT,
}

/// Visible bounds of a window without its invisible resize border / shadow.
pub fn window_bounds(hwnd: isize) -> Option<RECT> {
    let h = HWND(hwnd as _);
    unsafe {
        if !IsWindow(Some(h)).as_bool() {
            return None;
        }
        let mut r = RECT::default();
        if DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut _ as _, std::mem::size_of::<RECT>() as u32).is_err() {
            let _ = GetWindowRect(h, &mut r);
        }
        Some(r)
    }
}

/// Recordable windows, front-most first.
pub fn windows() -> Vec<WindowInfo> {
    unsafe extern "system" fn cb(h: HWND, data: LPARAM) -> BOOL {
        unsafe {
            let list = &mut *(data.0 as *mut Vec<WindowInfo>);
            if !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
                return BOOL(1);
            }
            let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
            if ex & WS_EX_TOOLWINDOW.0 != 0 {
                return BOOL(1);
            }
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut _ as _, 4);
            if cloaked != 0 {
                return BOOL(1);
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, Some(&mut pid));
            if pid == std::process::id() {
                return BOOL(1);
            }
            let mut buf = [0u16; 256];
            let n = GetWindowTextW(h, &mut buf);
            if n <= 0 {
                return BOOL(1);
            }
            let Some(rect) = window_bounds(h.0 as isize) else { return BOOL(1) };
            if rect.right - rect.left < 80 || rect.bottom - rect.top < 60 {
                return BOOL(1);
            }
            list.push(WindowInfo { hwnd: h.0 as isize, title: String::from_utf16_lossy(&buf[..n as usize]), rect });
        }
        BOOL(1)
    }
    let mut list: Vec<WindowInfo> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

/// What to record.
#[derive(Clone, Debug)]
pub enum Target {
    Display(Monitor),
    /// A single window, captured even when covered by other windows.
    Window(WindowInfo),
    /// A rectangle of a display, relative to the display's top-left.
    Area(Monitor, RECT),
}

impl Target {
    pub fn monitor_handle(&self) -> isize {
        match self {
            Target::Display(m) | Target::Area(m, _) => m.handle,
            Target::Window(w) => unsafe { MonitorFromWindow(HWND(w.hwnd as _), MONITOR_DEFAULTTONEAREST).0 as isize },
        }
    }

    /// Top-left of the recorded pixels in virtual-screen coordinates (follows window moves).
    pub fn origin(&self) -> (i32, i32) {
        match self {
            Target::Display(m) => (m.rect.left, m.rect.top),
            Target::Area(m, r) => (m.rect.left + r.left, m.rect.top + r.top),
            Target::Window(w) => window_bounds(w.hwnd).map_or((w.rect.left, w.rect.top), |r| (r.left, r.top)),
        }
    }

    /// Screen rectangle of what is recorded (for placing the countdown).
    pub fn screen_rect(&self) -> RECT {
        let (x, y) = self.origin();
        match self {
            Target::Display(m) => m.rect,
            Target::Area(_, r) => RECT { left: x, top: y, right: x + r.right - r.left, bottom: y + r.bottom - r.top },
            Target::Window(w) => window_bounds(w.hwnd).unwrap_or(w.rect),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Target::Display(m) => format!("Display {}×{}", m.width(), m.height()),
            Target::Area(_, r) => format!("Area {}×{}", r.right - r.left, r.bottom - r.top),
            Target::Window(w) => w.title.clone(),
        }
    }
}

pub struct Capturer {
    _item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    d3d: IDirect3DDevice,
    pub width: u32,
    pub height: u32,
    /// Sub-rectangle of the source frame that becomes the video (area mode).
    crop: Option<RECT>,
    last_size: (u32, u32),
}

impl Capturer {
    pub fn new(gpu: &Gpu, target: &Target) -> Res<Capturer> {
        unsafe {
            let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().ctx("capture interop")?;
            let item: GraphicsCaptureItem = match target {
                Target::Display(m) | Target::Area(m, _) => interop.CreateForMonitor(HMONITOR(m.handle as _)).ctx("CreateForMonitor")?,
                Target::Window(w) => interop.CreateForWindow(HWND(w.hwnd as _)).ctx("This window can't be captured")?,
            };
            let dxgi: IDXGIDevice = gpu.device.cast().ctx("IDXGIDevice")?;
            let d3d: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi).ctx("WinRT device")?.cast().ctx("cast")?;
            let size = item.Size().ctx("item size")?;
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 3, size)
                .ctx("frame pool")?;
            let session = pool.CreateCaptureSession(&item).ctx("session")?;
            // We draw our own, nicer cursor later, so keep the OS cursor out of the pixels.
            let _ = session.SetIsCursorCaptureEnabled(false);
            // Yellow capture border: hide it when the OS allows (Windows 11).
            if let Ok(op) = GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless) {
                let _ = op.join();
            }
            let _ = session.SetIsBorderRequired(false);
            // Allow frame delivery faster than the default 60 Hz cap on newer builds.
            let _ = session.SetMinUpdateInterval(TimeSpan { Duration: 10_000 });
            session.StartCapture().ctx("StartCapture")?;
            let crop = match target {
                Target::Area(_, r) => Some(*r),
                _ => None,
            };
            let (width, height) = match crop {
                Some(r) => ((r.right - r.left) as u32, (r.bottom - r.top) as u32),
                None => (size.Width as u32, size.Height as u32),
            };
            let last_size = (size.Width as u32, size.Height as u32);
            Ok(Capturer { _item: item, pool, session, d3d, width, height, crop, last_size })
        }
    }

    /// Copies the most recent frame (if any arrived) into `dst`. Returns true when updated.
    pub fn copy_latest(&mut self, gpu: &Gpu, dst: &ID3D11Texture2D) -> bool {
        let mut last = None;
        while let Ok(f) = self.pool.TryGetNextFrame() {
            if let Some(old) = last.replace(f) {
                let _ = old.Close();
            }
        }
        let Some(frame) = last else { return false };
        let ok = (|| -> Res<()> {
            let surface = frame.Surface().ctx("surface")?;
            let access: IDirect3DDxgiInterfaceAccess = surface.cast().ctx("dxgi access")?;
            let src: ID3D11Texture2D = unsafe { access.GetInterface() }.ctx("texture")?;
            let sd = crate::gpu::tex_desc(&src);
            let dd = crate::gpu::tex_desc(dst);
            let cs = frame.ContentSize().unwrap_or(SizeInt32 { Width: sd.Width as i32, Height: sd.Height as i32 });
            let (cw, ch) = (cs.Width.max(0) as u32, cs.Height.max(0) as u32);
            let (x0, y0) = self.crop.map_or((0, 0), |r| (r.left.max(0) as u32, r.top.max(0) as u32));
            let right = (x0 + dd.Width).min(cw).min(sd.Width);
            let bottom = (y0 + dd.Height).min(ch).min(sd.Height);
            if right > x0 && bottom > y0 {
                if right - x0 < dd.Width || bottom - y0 < dd.Height {
                    // A shrunken window no longer fills the video: clear what it doesn't cover.
                    if let Ok(rtv) = gpu.rtv(dst) {
                        unsafe { gpu.ctx.ClearRenderTargetView(&rtv, &[0.0, 0.0, 0.0, 1.0]) };
                    }
                }
                let bx = D3D11_BOX { left: x0, top: y0, front: 0, right, bottom, back: 1 };
                unsafe { gpu.ctx.CopySubresourceRegion(dst, 0, 0, 0, 0, &src, 0, Some(&bx)) };
            }
            if (cw, ch) != self.last_size && cw > 0 && ch > 0 {
                // Source resized (window resize / display change): keep the pool in sync.
                // The video keeps its original size.
                let _ = self.pool.Recreate(&self.d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 3, cs);
                self.last_size = (cw, ch);
            }
            Ok(())
        })()
        .is_ok();
        let _ = frame.Close();
        ok
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
