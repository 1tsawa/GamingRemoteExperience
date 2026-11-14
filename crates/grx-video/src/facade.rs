//! [`Encoder`] : façade commune aux voies d'encodage (MFT matériels NVIDIA/AMD/Intel, NVENC
//! direct pour le 4:4:4, OpenH264 logiciel), choisie par [`crate::select::plan_routes`] selon
//! `EncoderConfig::pref`, le format demandé et l'adaptateur du device de capture.

use std::sync::Mutex;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter1, IDXGIDevice};

use crate::capture::CapturedFrame;
use crate::encoder::{enum_hw_encoders, mf_startup, MfEncoder};
use crate::error::{Result, VideoError, WinCtx};
use crate::nvenc::{self, NvencEncoder};
use crate::select::{plan_routes, Route, Vendor};
use crate::shader::HdrPass;
use crate::soft::SoftEncoder;
use crate::{gpu, openh264, Codec, EncodedFrame, EncoderConfig, EncoderPref, PixelFormat};

/// Backend d'encodage effectivement utilisé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncoderBackend {
    /// MFT matériel Media Foundation (nom convivial fourni par le pilote).
    MediaFoundation { name: String, vendor: Vendor },
    /// API NVENC directe (4:4:4).
    Nvenc { name: String },
    /// OpenH264 (CPU), à la résolution réduite `width`x`height`.
    OpenH264 { name: String, width: u32, height: u32 },
}

impl EncoderBackend {
    pub fn vendor(&self) -> Vendor {
        match self {
            EncoderBackend::MediaFoundation { vendor, .. } => *vendor,
            EncoderBackend::Nvenc { .. } => Vendor::Nvidia,
            EncoderBackend::OpenH264 { .. } => Vendor::Software,
        }
    }
    pub fn kind(&self) -> EncoderKind {
        match self {
            EncoderBackend::MediaFoundation { vendor, .. } => EncoderKind::Mf(*vendor),
            EncoderBackend::Nvenc { .. } => EncoderKind::Nvenc,
            EncoderBackend::OpenH264 { .. } => EncoderKind::OpenH264,
        }
    }
    pub fn name(&self) -> &str {
        match self {
            EncoderBackend::MediaFoundation { name, .. } | EncoderBackend::Nvenc { name } | EncoderBackend::OpenH264 { name, .. } => name,
        }
    }
}

/// Famille d'encodeur (annoncée au client).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EncoderKind {
    Mf(Vendor),
    Nvenc,
    OpenH264,
}

/// Un encodeur disponible sur un adaptateur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderInfo {
    pub codec: Codec,
    pub kind: EncoderKind,
    pub name: String,
    pub formats: Vec<PixelFormat>,
}

/// LUID et vendeur de l'adaptateur d'un device.
pub(crate) fn device_adapter(device: &ID3D11Device) -> (Option<i64>, Vendor) {
    (|| -> windows::core::Result<(Option<i64>, Vendor)> {
        let d: IDXGIDevice = device.cast()?;
        let a = unsafe { d.GetAdapter() }?;
        let desc = unsafe { a.GetDesc() }?;
        let luid = ((desc.AdapterLuid.HighPart as i64) << 32) | desc.AdapterLuid.LowPart as i64;
        Ok((Some(luid), Vendor::from_pci(desc.VendorId)))
    })()
    .unwrap_or((None, Vendor::Other))
}

enum Inner {
    Mf(MfEncoder),
    Nvenc(NvencEncoder),
    Soft(SoftEncoder),
}

/// Encodeur vidéo (voie choisie à la création).
pub struct Encoder {
    inner: Inner,
    cfg: EncoderConfig,
    backend: EncoderBackend,
    device: ID3D11Device,
    hdr_pass: Option<HdrPass>,
    attempts: Vec<String>,
}

// SAFETY : voir MfEncoder / NvencEncoder / SoftEncoder (device protégé multithread, &mut self).
unsafe impl Send for Encoder {}

impl Encoder {
    /// Crée un encodeur avec son propre device D3D11 (adaptateur NVIDIA préféré).
    /// Pour encoder les textures d'un [`crate::ScreenCapturer`], utiliser plutôt [`Encoder::with_device`].
    pub fn new(cfg: EncoderConfig) -> Result<Self> {
        gpu::init_com();
        let adapter = gpu::preferred_adapter()?;
        let (device, _ctx) = gpu::create_device_for_adapter(&adapter)?;
        Self::with_device(&device, cfg)
    }

    /// Crée un encodeur sur le device D3D11 `device` (celui du capturer). Essaie les voies de
    /// [`plan_routes`] dans l'ordre ; l'erreur résume chaque échec.
    pub fn with_device(device: &ID3D11Device, cfg: EncoderConfig) -> Result<Self> {
        gpu::init_com();
        mf_startup()?;
        let (luid, adapter) = device_adapter(device);
        let mfts = if cfg.format.is_444() || cfg.pref == EncoderPref::Software {
            Vec::new()
        } else {
            enum_hw_encoders(cfg.codec, luid).unwrap_or_default()
        };
        let vendors: Vec<Vendor> = mfts.iter().map(|m| m.2).collect();
        let nv444 = cfg.format.is_444()
            && adapter == Vendor::Nvidia
            && nvenc::yuv444_caps(device).iter().any(|&(c, ten)| c == cfg.codec && (ten || !cfg.format.is_10bit()));
        let oh = openh264::availability().is_ok();
        let routes = plan_routes(cfg.pref, cfg.codec, cfg.format, adapter, &vendors, nv444, oh);
        let mut attempts = Vec::new();
        for r in &routes {
            let made = match *r {
                Route::Mf(i) => {
                    let (act, name, vendor) = &mfts[i];
                    MfEncoder::with_activate(device, cfg.clone(), act, name.clone(), *vendor)
                        .map(|e| (EncoderBackend::MediaFoundation { name: name.clone(), vendor: *vendor }, Inner::Mf(e)))
                }
                Route::Nvenc => NvencEncoder::new(device, cfg.clone())
                    .map(|e| (EncoderBackend::Nvenc { name: e.name.clone() }, Inner::Nvenc(e))),
                Route::OpenH264 => SoftEncoder::new(device, &cfg).map(|e| {
                    let (width, height) = e.output_size();
                    (EncoderBackend::OpenH264 { name: e.name.clone(), width, height }, Inner::Soft(e))
                }),
            };
            match made {
                Ok((backend, inner)) => {
                    return Ok(Encoder { inner, cfg, backend, device: device.clone(), hdr_pass: None, attempts });
                }
                Err(e) => attempts.push(format!("{r:?} : {e}")),
            }
        }
        if routes.is_empty() {
            let why = match (cfg.format.is_444(), cfg.pref) {
                (true, _) => format!("{:?} 4:4:4 : NVENC direct indisponible sur cet adaptateur ({})", cfg.codec, adapter.label()),
                (_, EncoderPref::Software) => format!("pas d'encodeur logiciel {:?} {} (OpenH264 : H.264 4:2:0 8 bits ; {})", cfg.codec, cfg.format.label(), openh264::availability().err().unwrap_or_default()),
                (_, p) if p != EncoderPref::Auto => format!("aucun encodeur {:?} {p:?} sur l'adaptateur {}", cfg.codec, adapter.label()),
                _ => return Err(VideoError::NoEncoder(cfg.codec)),
            };
            return Err(VideoError::Other(why));
        }
        Err(VideoError::Other(format!("aucun encodeur {:?} {} utilisable : {}", cfg.codec, cfg.format.label(), attempts.join(" ; "))))
    }

    /// Backend utilisé.
    pub fn backend(&self) -> &EncoderBackend {
        &self.backend
    }

    /// Configuration demandée (taille de la source ; voir [`Encoder::output_size`]).
    pub fn config(&self) -> &EncoderConfig {
        &self.cfg
    }

    /// Taille encodée (réduite pour l'encodeur logiciel).
    pub fn output_size(&self) -> (u32, u32) {
        match &self.inner {
            Inner::Soft(s) => s.output_size(),
            _ => (self.cfg.width, self.cfg.height),
        }
    }

    /// Voies essayées sans succès avant celle-ci, et réglages refusés par le pilote.
    pub fn warnings(&self) -> Vec<String> {
        let mut v = self.attempts.clone();
        if let Inner::Mf(m) = &self.inner {
            v.extend(m.warnings().iter().cloned());
        }
        v
    }

    pub fn request_keyframe(&mut self) {
        match &mut self.inner {
            Inner::Mf(e) => e.request_keyframe(),
            Inner::Nvenc(e) => e.request_keyframe(),
            Inner::Soft(e) => e.request_keyframe(),
        }
    }

    /// Compromis qualité/vitesse des MFT (0 = le plus rapide) ; sans effet sur les autres voies.
    pub fn set_quality_vs_speed(&mut self, v: u32) -> Result<()> {
        match &mut self.inner {
            Inner::Mf(e) => e.set_quality_vs_speed(v),
            _ => Ok(()),
        }
    }

    /// Change le débit cible (CBR) à chaud, en kbit/s.
    pub fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        self.cfg.bitrate_kbps = kbps;
        match &mut self.inner {
            Inner::Mf(e) => e.set_bitrate(kbps),
            Inner::Nvenc(e) => e.set_bitrate(kbps),
            Inner::Soft(e) => e.set_bitrate(kbps),
        }
    }

    /// Encode une image capturée (bloquant, quelques ms).
    pub fn encode(&mut self, frame: &CapturedFrame) -> Result<EncodedFrame> {
        self.encode_texture(&frame.texture, frame.width, frame.height)
    }

    /// Encode une texture du device de l'encodeur : BGRA (SDR), ou FP16 scRGB (capture HDR),
    /// convertie d'abord en HDR10 (flux 10 bits HDR) ou en SDR 8 bits (tone-mapping).
    pub fn encode_texture(&mut self, tex: &ID3D11Texture2D, w: u32, h: u32) -> Result<EncodedFrame> {
        let mut sd = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut sd) };
        let mut src = tex.clone();
        if sd.Format == DXGI_FORMAT_R16G16B16A16_FLOAT {
            if self.hdr_pass.is_none() {
                let to_pq = self.cfg.hdr && self.cfg.format.is_10bit();
                self.hdr_pass = Some(HdrPass::new(&self.device, sd.Width, sd.Height, to_pq, 1000.0)?);
            }
            src = self.hdr_pass.as_mut().unwrap().run(tex)?;
        }
        match &mut self.inner {
            Inner::Mf(e) => e.encode_texture(&src, w, h),
            Inner::Nvenc(e) => e.encode_texture(&src),
            Inner::Soft(e) => e.encode_texture(&src, w, h),
        }
    }

    /// Chemin lent pour les tests/outils : encode des pixels BGRA CPU (`stride` = octets par ligne).
    pub fn encode_bgra(&mut self, bgra: &[u8], width: u32, height: u32, stride: u32) -> Result<EncodedFrame> {
        let t = upload_texture(&self.device, DXGI_FORMAT_B8G8R8A8_UNORM, bgra, width, height, stride)?;
        self.encode_texture(&t, width, height)
    }
}

/// Texture D3D11 (SRV + RTV) initialisée avec des pixels CPU (tests, mire synthétique).
pub fn upload_texture(device: &ID3D11Device, format: DXGI_FORMAT, data: &[u8], width: u32, height: u32, stride: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init = D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr() as *const _, SysMemPitch: stride, SysMemSlicePitch: 0 };
    let mut t = None;
    unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut t)) }.ctx("CreateTexture2D(upload)")?;
    Ok(t.unwrap())
}

fn adapter_by_index(index: Option<u32>) -> Result<IDXGIAdapter1> {
    match index {
        Some(i) => unsafe { gpu::factory()?.EnumAdapters1(i) }.ctx("EnumAdapters1"),
        None => gpu::preferred_adapter(),
    }
}

/// Encodeurs disponibles sur l'adaptateur `index` (`None` : adaptateur préféré, NVIDIA d'abord) :
/// MFT matériels (4:2:0 8 bits ; 10 bits vérifié en créant réellement un encodeur HEVC/AV1),
/// NVENC direct (4:4:4, interrogé via `nvEncGetEncodeCaps`), OpenH264 si installé.
/// Coût : ~100 ms au premier appel (création réelle), puis cache.
pub fn encoders_for_adapter(index: Option<u32>) -> Vec<EncoderInfo> {
    static CACHE: Mutex<Vec<(Option<u32>, Vec<EncoderInfo>)>> = Mutex::new(Vec::new());
    if let Some((_, v)) = CACHE.lock().unwrap().iter().find(|(k, _)| *k == index) {
        return v.clone();
    }
    let v = probe(index).unwrap_or_default();
    CACHE.lock().unwrap().push((index, v.clone()));
    v
}

fn probe(index: Option<u32>) -> Result<Vec<EncoderInfo>> {
    gpu::init_com();
    mf_startup()?;
    let adapter = adapter_by_index(index)?;
    let (device, _ctx) = gpu::create_device_for_adapter(&adapter)?;
    let (luid, vendor) = device_adapter(&device);
    let mut out = Vec::new();
    for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
        let mfts = enum_hw_encoders(codec, luid).unwrap_or_default();
        let mut seen = Vec::new();
        for (i, (_, name, ven)) in mfts.iter().enumerate() {
            if seen.contains(ven) {
                continue;
            }
            seen.push(*ven);
            let mut formats = vec![PixelFormat::Yuv420];
            if codec != Codec::H264 {
                // 10 bits : création réelle (le MFT peut refuser P010 / Main10).
                let probe_cfg = EncoderConfig { codec, width: 640, height: 360, bitrate_kbps: 2000, format: PixelFormat::Yuv420P10, ..Default::default() };
                let fresh = enum_hw_encoders(codec, luid).unwrap_or_default();
                if let Some((act, n, v)) = fresh.get(i) {
                    if MfEncoder::with_activate(&device, probe_cfg, act, n.clone(), *v).is_ok() {
                        formats.push(PixelFormat::Yuv420P10);
                    }
                }
            }
            out.push(EncoderInfo { codec, kind: EncoderKind::Mf(*ven), name: name.clone(), formats });
        }
    }
    if vendor == Vendor::Nvidia {
        for (codec, ten) in nvenc::yuv444_caps(&device) {
            let mut formats = vec![PixelFormat::Yuv444];
            if ten {
                formats.push(PixelFormat::Yuv444P10);
            }
            out.push(EncoderInfo { codec, kind: EncoderKind::Nvenc, name: "NVENC direct".into(), formats });
        }
    }
    if let Ok(v) = openh264::availability() {
        out.push(EncoderInfo { codec: Codec::H264, kind: EncoderKind::OpenH264, name: v, formats: vec![PixelFormat::Yuv420] });
    }
    Ok(out)
}

/// Compatibilité phase 1 : `(codec, nom)` du premier encodeur matériel de l'adaptateur préféré.
pub fn available_encoders() -> Vec<(Codec, String)> {
    let mut v: Vec<(Codec, String)> = Vec::new();
    for e in encoders_for_adapter(None) {
        if matches!(e.kind, EncoderKind::Mf(_)) && !v.iter().any(|(c, _)| *c == e.codec) {
            v.push((e.codec, e.name));
        }
    }
    v
}
