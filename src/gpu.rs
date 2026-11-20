//! Direct3D 11 device creation and small texture helpers.

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use crate::util::{Ctx, Res};

#[derive(Clone)]
pub struct Gpu {
    pub device: ID3D11Device,
    pub ctx: ID3D11DeviceContext,
}

impl Gpu {
    pub fn new() -> Res<Gpu> {
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut device = None;
        let mut ctx = None;
        let mut try_create = |driver| unsafe {
            D3D11CreateDevice(
                None,
                driver,
                HMODULE::default(),
                flags,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
        };
        if try_create(D3D_DRIVER_TYPE_HARDWARE).is_err() {
            // WARP keeps the app usable on machines without a usable GPU driver.
            try_create(D3D_DRIVER_TYPE_WARP).ctx("D3D11CreateDevice")?;
        }
        let device: ID3D11Device = device.ok_or("no device")?;
        let ctx: ID3D11DeviceContext = ctx.ok_or("no context")?;
        // Media Foundation and Direct2D touch the device from their own threads.
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            unsafe {
                let _ = mt.SetMultithreadProtected(true);
            }
        }
        Ok(Gpu { device, ctx })
    }

    pub fn texture(&self, w: u32, h: u32, format: DXGI_FORMAT, bind: D3D11_BIND_FLAG, mips: bool) -> Res<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: if mips { 0 } else { 1 },
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: if mips { D3D11_RESOURCE_MISC_GENERATE_MIPS.0 as u32 } else { 0 },
        };
        let mut t = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut t)) }.ctx("CreateTexture2D")?;
        t.ok_or_else(|| "CreateTexture2D returned null".into())
    }

    pub fn srv(&self, tex: &ID3D11Texture2D) -> Res<ID3D11ShaderResourceView> {
        let mut v = None;
        unsafe { self.device.CreateShaderResourceView(tex, None, Some(&mut v)) }.ctx("CreateSRV")?;
        v.ok_or_else(|| "null SRV".into())
    }

    pub fn rtv(&self, tex: &ID3D11Texture2D) -> Res<ID3D11RenderTargetView> {
        let mut v = None;
        unsafe { self.device.CreateRenderTargetView(tex, None, Some(&mut v)) }.ctx("CreateRTV")?;
        v.ok_or_else(|| "null RTV".into())
    }
}

pub fn tex_desc(t: &ID3D11Texture2D) -> D3D11_TEXTURE2D_DESC {
    let mut d = D3D11_TEXTURE2D_DESC::default();
    unsafe { t.GetDesc(&mut d) };
    d
}
