//! Création de devices D3D11 et énumération des adaptateurs.

use crate::error::{Result, WinCtx};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

pub const VENDOR_NVIDIA: u32 = 0x10DE;

#[derive(Debug, Clone)]
pub struct AdapterInfo {
    pub index: u32,
    pub name: String,
    pub vendor_id: u32,
    pub is_software: bool,
}

/// Initialise COM (MTA) pour le thread courant ; ignore « déjà initialisé ».
pub(crate) fn init_com() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

pub(crate) fn factory() -> Result<IDXGIFactory1> {
    unsafe { CreateDXGIFactory1::<IDXGIFactory1>().ctx("CreateDXGIFactory1") }
}

pub(crate) fn adapter_desc(a: &IDXGIAdapter1) -> Result<AdapterInfo> {
    let d = unsafe { a.GetDesc1() }.ctx("IDXGIAdapter1::GetDesc1")?;
    let len = d.Description.iter().position(|&c| c == 0).unwrap_or(d.Description.len());
    Ok(AdapterInfo {
        index: 0,
        name: String::from_utf16_lossy(&d.Description[..len]),
        vendor_id: d.VendorId,
        is_software: (d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0,
    })
}

/// Liste les adaptateurs DXGI.
pub fn list_adapters() -> Result<Vec<AdapterInfo>> {
    let f = factory()?;
    let mut v = Vec::new();
    let mut i = 0;
    while let Ok(a) = unsafe { f.EnumAdapters1(i) } {
        let mut info = adapter_desc(&a)?;
        info.index = i;
        v.push(info);
        i += 1;
    }
    Ok(v)
}

/// Crée un device D3D11 sur l'adaptateur donné, avec support vidéo (VideoProcessor / MFT)
/// et protection multithread activée (exigée par Media Foundation).
pub fn create_device_for_adapter(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device: Option<ID3D11Device> = None;
    let mut ctx: Option<ID3D11DeviceContext> = None;
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )
        .ctx("D3D11CreateDevice")?;
    }
    let device = device.ok_or_else(|| crate::VideoError::Other("D3D11CreateDevice: no device".into()))?;
    let ctx = ctx.ok_or_else(|| crate::VideoError::Other("D3D11CreateDevice: no context".into()))?;
    unsafe {
        let mt: ID3D11Multithread = device.cast().ctx("ID3D11Multithread")?;
        let _ = mt.SetMultithreadProtected(true);
    }
    Ok((device, ctx))
}

/// Adaptateur matériel préféré : NVIDIA non logiciel, sinon premier matériel.
pub(crate) fn preferred_adapter() -> Result<IDXGIAdapter1> {
    let f = factory()?;
    let mut fallback = None;
    let mut i = 0;
    while let Ok(a) = unsafe { f.EnumAdapters1(i) } {
        i += 1;
        let info = adapter_desc(&a)?;
        if info.is_software {
            continue;
        }
        if info.vendor_id == VENDOR_NVIDIA {
            return Ok(a);
        }
        if fallback.is_none() {
            fallback = Some(a);
        }
    }
    fallback.ok_or(crate::VideoError::NoOutput)
}
