//! Création du device D3D11 (adaptateur NVIDIA préféré) avec support vidéo.

use crate::error::{RenderError, Result, WinCtx};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT, COINIT_MULTITHREADED};

const VENDOR_NVIDIA: u32 = 0x10DE;

thread_local! {
    static UI_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Marque le thread courant comme thread de fenêtre (winit y exige un apartment STA).
pub(crate) fn mark_ui_thread() {
    UI_THREAD.with(|c| c.set(true));
}

/// Initialise COM pour le thread courant (MTA, ou STA sur le thread de fenêtre) ; ignore
/// « déjà initialisé ».
pub(crate) fn init_com() {
    let ui = UI_THREAD.with(|c| c.get());
    unsafe {
        let _ = CoInitializeEx(None, if ui { COINIT(2) } else { COINIT_MULTITHREADED });
    }
}

pub(crate) fn factory() -> Result<IDXGIFactory2> {
    unsafe { CreateDXGIFactory1::<IDXGIFactory2>().ctx("CreateDXGIFactory1") }
}

fn preferred_adapter() -> Result<IDXGIAdapter1> {
    let f = factory()?;
    let mut fallback = None;
    let mut i = 0;
    while let Ok(a) = unsafe { f.EnumAdapters1(i) } {
        i += 1;
        let d = unsafe { a.GetDesc1() }.ctx("GetDesc1")?;
        if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue;
        }
        if d.VendorId == VENDOR_NVIDIA {
            return Ok(a);
        }
        if fallback.is_none() {
            fallback = Some(a);
        }
    }
    fallback.ok_or_else(|| RenderError::Other("no hardware DXGI adapter".into()))
}

/// Crée un device D3D11 (BGRA + VIDEO_SUPPORT, protection multithread) sur l'adaptateur
/// NVIDIA s'il existe. C'est le device partagé entre décodeur et présentateur.
///
/// N'initialise pas COM : winit exige un thread STA (OleInitialize) pour la boucle de fenêtre.
pub fn create_device() -> Result<ID3D11Device> {
    let adapter = preferred_adapter()?;
    let mut device: Option<ID3D11Device> = None;
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .ctx("D3D11CreateDevice")?;
    }
    let device = device.ok_or_else(|| RenderError::Other("D3D11CreateDevice: no device".into()))?;
    unsafe {
        let mt: ID3D11Multithread = device.cast().ctx("ID3D11Multithread")?;
        let _ = mt.SetMultithreadProtected(true);
    }
    Ok(device)
}
