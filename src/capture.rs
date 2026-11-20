//! Screen capture through Windows.Graphics.Capture (GPU-only path, no CPU copies).

use windows::Foundation::TimeSpan;
use windows::Graphics::Capture::*;
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
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

pub struct Capturer {
    _item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    d3d: IDirect3DDevice,
    pub width: u32,
    pub height: u32,
}

impl Capturer {
    pub fn new(gpu: &Gpu, monitor: &Monitor) -> Res<Capturer> {
        unsafe {
            let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().ctx("capture interop")?;
            let item: GraphicsCaptureItem = interop
                .CreateForMonitor(HMONITOR(monitor.handle as _))
                .ctx("CreateForMonitor")?;
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
            Ok(Capturer { _item: item, pool, session, d3d, width: size.Width as u32, height: size.Height as u32 })
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
            let bx = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: sd.Width.min(dd.Width),
                bottom: sd.Height.min(dd.Height),
                back: 1,
            };
            unsafe { gpu.ctx.CopySubresourceRegion(dst, 0, 0, 0, 0, &src, 0, Some(&bx)) };
            let cs = frame.ContentSize().unwrap_or(SizeInt32 { Width: self.width as i32, Height: self.height as i32 });
            if cs.Width as u32 != self.width || cs.Height as u32 != self.height {
                // Resolution changed mid-recording. Keep the pool in sync; the output
                // keeps its original size and shows the overlapping region.
                let _ = self.pool.Recreate(&self.d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 3, cs);
                self.width = cs.Width as u32;
                self.height = cs.Height as u32;
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
