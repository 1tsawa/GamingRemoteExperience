//! Exécution de la sonde des encodeurs sur un adaptateur réel (voir [`crate::probe`]).
//!
//! Combinaisons testées sur l'adaptateur : chaque vendeur de MFT matériel énuméré × H.264/HEVC/AV1
//! × 4:2:0 8/10 bits ; NVENC direct × H.264/HEVC/AV1 × 4:4:4 8/10 bits (adaptateur NVIDIA) ;
//! OpenH264 × H.264 4:2:0 8 bits (si installé). Chaque test crée l'encodeur, encode deux images
//! 256x256 et valide le flux (NAL/OBU, IDR, profil, sous-échantillonnage, profondeur).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Instant;

use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter1, IDXGIDevice, DXGI_ADAPTER_FLAG_SOFTWARE};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::core::Interface;

use crate::encoder::{enum_hw_encoders, mf_startup, MfEncoder};
use crate::error::{Result, VideoError, WinCtx};
use crate::facade::{upload_texture, EncoderKind};
use crate::nvenc::NvencEncoder;
use crate::probe::ProbeResult;
use crate::soft::SoftEncoder;
use crate::streaminfo::check_stream;
use crate::{gpu, openh264, Codec, EncodedFrame, EncoderConfig, PixelFormat, Vendor};

/// Taille des images de test (acceptée par NVENC, AMF, QSV et OpenH264).
pub const PROBE_SIZE: u32 = 256;

/// Identité d'un adaptateur (clé d'invalidation du cache de la sonde).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterId {
    pub index: u32,
    /// LUID (stable jusqu'au redémarrage seulement : ne pas l'utiliser comme clé de cache).
    pub luid: i64,
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub subsys_id: u32,
    pub revision: u32,
    /// Version du pilote (UMD), `CheckInterfaceSupport(IDXGIDevice)`.
    pub driver_version: u64,
}

impl AdapterId {
    /// Même GPU physique (même modèle, pilote, nom) : Windows peut énumérer un GPU plusieurs fois
    /// sous des LUID différents (pilotes d'écran indirect, ex. écrans virtuels).
    pub fn same_hardware(&self, o: &AdapterId) -> bool {
        (self.vendor_id, self.device_id, self.subsys_id, self.revision, self.driver_version, &self.name)
            == (o.vendor_id, o.device_id, o.subsys_id, o.revision, o.driver_version, &o.name)
    }
    pub fn vendor(&self) -> Vendor {
        Vendor::from_pci(self.vendor_id)
    }
    /// Version du pilote au format Windows « a.b.c.d ».
    pub fn driver_label(&self) -> String {
        let v = self.driver_version;
        format!("{}.{}.{}.{}", v >> 48, (v >> 32) & 0xFFFF, (v >> 16) & 0xFFFF, v & 0xFFFF)
    }
}

fn describe(index: u32, a: &IDXGIAdapter1) -> Result<(AdapterId, bool)> {
    let d = unsafe { a.GetDesc1() }.ctx("IDXGIAdapter1::GetDesc1")?;
    let len = d.Description.iter().position(|&c| c == 0).unwrap_or(d.Description.len());
    let driver_version = unsafe { a.CheckInterfaceSupport(&IDXGIDevice::IID) }.map(|v| v as u64).unwrap_or(0);
    let id = AdapterId {
        index,
        luid: ((d.AdapterLuid.HighPart as i64) << 32) | d.AdapterLuid.LowPart as i64,
        name: String::from_utf16_lossy(&d.Description[..len]),
        vendor_id: d.VendorId,
        device_id: d.DeviceId,
        subsys_id: d.SubSysId,
        revision: d.Revision,
        driver_version,
    };
    Ok((id, d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0))
}

/// Adaptateurs matériels (sans WARP / « Microsoft Basic Render Driver »).
pub fn hardware_adapters() -> Vec<AdapterId> {
    let Ok(f) = gpu::factory() else { return Vec::new() };
    let mut v = Vec::new();
    let mut i = 0;
    while let Ok(a) = unsafe { f.EnumAdapters1(i) } {
        if let Ok((id, soft)) = describe(i, &a) {
            if !soft && id.vendor_id != 0x1414 && !v.iter().any(|x: &AdapterId| x.luid == id.luid) {
                v.push(id);
            }
        }
        i += 1;
    }
    v
}

/// Identité de l'adaptateur `index` (`None` : adaptateur préféré, NVIDIA d'abord).
pub fn adapter_id(index: Option<u32>) -> Result<AdapterId> {
    let (i, a) = match index {
        Some(i) => (i, unsafe { gpu::factory()?.EnumAdapters1(i) }.ctx("EnumAdapters1")?),
        None => {
            let a = gpu::preferred_adapter()?;
            let luid = unsafe { a.GetDesc1() }.ctx("GetDesc1")?.AdapterLuid;
            let i = hardware_adapters()
                .iter()
                .find(|x| x.luid == ((luid.HighPart as i64) << 32) | luid.LowPart as i64)
                .map(|x| x.index)
                .unwrap_or(0);
            (i, a)
        }
    };
    Ok(describe(i, &a)?.0)
}

/// Mire BGRA (dégradés + damier) décalée de `shift` pixels : contenu non trivial à encoder.
fn pattern(shift: u32) -> Vec<u8> {
    let n = PROBE_SIZE;
    let mut v = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let xs = (x + shift) % n;
            let check = if ((xs / 16) + (y / 16)).is_multiple_of(2) { 40 } else { 0 };
            v.extend_from_slice(&[(xs as u8).wrapping_add(check), (y as u8).wrapping_add(check), ((xs ^ y) as u8), 255]);
        }
    }
    v
}

enum Enc {
    Mf(MfEncoder),
    Nv(NvencEncoder),
    Soft(SoftEncoder),
}

impl Enc {
    fn encode(&mut self, device: &ID3D11Device, shift: u32) -> Result<EncodedFrame> {
        let px = pattern(shift);
        let t = upload_texture(device, DXGI_FORMAT_B8G8R8A8_UNORM, &px, PROBE_SIZE, PROBE_SIZE, PROBE_SIZE * 4)?;
        match self {
            Enc::Mf(e) => e.encode_texture(&t, PROBE_SIZE, PROBE_SIZE),
            Enc::Nv(e) => e.encode_texture(&t),
            Enc::Soft(e) => e.encode_texture(&t, PROBE_SIZE, PROBE_SIZE),
        }
    }
}

/// Teste une combinaison : création, deux images, validation. `Err((raison, passager))`.
fn run_one(device: &ID3D11Device, luid: Option<i64>, kind: EncoderKind, mf_index: usize, cfg: EncoderConfig) -> (std::result::Result<(), (String, bool)>, u32, u32) {
    let t0 = Instant::now();
    let codec = cfg.codec;
    let format = cfg.format;
    let made: Result<Enc> = match kind {
        EncoderKind::Mf(_) => {
            // Énumération fraîche : un IMFActivate ne sert qu'une fois.
            let mfts = enum_hw_encoders(codec, luid).unwrap_or_default();
            match mfts.get(mf_index) {
                Some((act, name, ven)) => MfEncoder::with_activate(device, cfg, act, name.clone(), *ven).map(Enc::Mf),
                None => Err(VideoError::Other("MFT disparu de l'énumération".into())),
            }
        }
        EncoderKind::Nvenc => NvencEncoder::new(device, cfg).map(Enc::Nv),
        EncoderKind::OpenH264 => SoftEncoder::new(device, &cfg).map(Enc::Soft),
    };
    let transient = |e: &VideoError| matches!(e, VideoError::Timeout(_)) || {
        let s = e.to_string().to_ascii_uppercase();
        s.contains("OUT_OF_MEMORY") || s.contains("BUSY") || s.contains("DEVICE_REMOVED")
    };
    let mut enc = match made {
        Ok(e) => e,
        Err(e) => {
            let tr = transient(&e);
            return (Err((format!("création : {e}"), tr)), t0.elapsed().as_millis() as u32, 0);
        }
    };
    let mut data = Vec::new();
    let f0 = match enc.encode(device, 0) {
        Ok(f) => f,
        Err(e) => {
            let tr = transient(&e);
            return (Err((format!("1re image : {e}"), tr)), t0.elapsed().as_millis() as u32, 0);
        }
    };
    let init_ms = t0.elapsed().as_millis() as u32;
    data.extend_from_slice(&f0.data);
    let t1 = Instant::now();
    match enc.encode(device, 8) {
        Ok(f) => data.extend_from_slice(&f.data),
        Err(e) => {
            let tr = transient(&e);
            return (Err((format!("2e image : {e}"), tr)), init_ms, 0);
        }
    }
    let frame_ms = t1.elapsed().as_millis() as u32;
    // La première image doit être une IDR complète ; le flux entier doit avoir le bon profil.
    let first = if f0.data.is_empty() { &data[..] } else { &f0.data[..] };
    let r = check_stream(codec, format.is_444(), format.is_10bit(), first).map(|_| ()).map_err(|e| (format!("flux invalide : {e}"), false));
    (r, init_ms, frame_ms)
}

/// Sonde complète de l'adaptateur `index` (quelques secondes). N'installe rien : voir
/// [`crate::probe::install`].
pub fn probe_adapter(index: u32) -> Result<(AdapterId, Vec<ProbeResult>)> {
    gpu::init_com();
    mf_startup()?;
    let adapter = unsafe { gpu::factory()?.EnumAdapters1(index) }.ctx("EnumAdapters1")?;
    let (id, _) = describe(index, &adapter)?;
    let (device, _ctx) = gpu::create_device_for_adapter(&adapter)?;
    let luid = Some(id.luid);
    let mut combos: Vec<(EncoderKind, usize, String, Codec, PixelFormat)> = Vec::new();
    for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
        let mfts = enum_hw_encoders(codec, luid).unwrap_or_default();
        let mut seen: Vec<Vendor> = Vec::new();
        // Vendeur de l'adaptateur d'abord (même ordre que plan_routes).
        let mut order: Vec<usize> = (0..mfts.len()).filter(|&i| mfts[i].2 == id.vendor()).collect();
        order.extend((0..mfts.len()).filter(|&i| mfts[i].2 != id.vendor()));
        for i in order {
            let (_, name, ven) = &mfts[i];
            if seen.contains(ven) {
                continue;
            }
            seen.push(*ven);
            for f in [PixelFormat::Yuv420, PixelFormat::Yuv420P10] {
                combos.push((EncoderKind::Mf(*ven), i, name.clone(), codec, f));
            }
        }
    }
    if id.vendor() == Vendor::Nvidia {
        // HEVC seulement, comme `nvenc::yuv444_caps` : en H.264 4:4:4, NVENC encode mais ne
        // respecte pas le débit demandé (mesuré : ~180 Mb/s pour 15) ; AV1 4:4:4 non supporté.
        for codec in [Codec::Hevc] {
            for f in [PixelFormat::Yuv444, PixelFormat::Yuv444P10] {
                combos.push((EncoderKind::Nvenc, 0, "NVENC direct".into(), codec, f));
            }
        }
    }
    if let Ok(v) = openh264::availability() {
        combos.push((EncoderKind::OpenH264, 0, v, Codec::H264, PixelFormat::Yuv420));
    }
    let mut out = Vec::new();
    for (kind, i, name, codec, format) in combos {
        let cfg = EncoderConfig { codec, width: PROBE_SIZE, height: PROBE_SIZE, fps: 60, bitrate_kbps: 2000, format, ..Default::default() };
        let (r, init_ms, frame_ms) = catch_unwind(AssertUnwindSafe(|| run_one(&device, luid, kind, i, cfg)))
            .unwrap_or_else(|_| (Err(("panique pendant le test".into(), false)), 0, 0));
        let (ok, reason, transient) = match r {
            Ok(()) => (true, String::new(), false),
            Err((m, t)) => (false, m, t),
        };
        out.push(ProbeResult { kind, name, codec, format, ok, reason, init_ms, frame_ms, transient });
    }
    Ok((id, out))
}
