//! [`VideoDecoder`] : façade commune aux voies de décodage (Media Foundation, FFmpeg D3D11VA),
//! chaîne de repli et sondage des décodeurs réellement utilisables sur le GPU.

use std::sync::Mutex;

use grx_video::{Codec, EncodedFrame, PixelFormat};
use windows::core::{Interface, GUID};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_AYUV, DXGI_FORMAT_NV12, DXGI_FORMAT_P010};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;

use crate::chain::{self, Attempt, ChainError, DecoderKind, DecoderPref, DisabledDecoders};
use crate::decoder::{sync_device, DecodedFrame, DecoderStats, FrameKeep, MfDecoder};
use crate::error::{RenderError, Result};
use crate::ffmpeg::FfDecoder;
use crate::gpu;
use crate::soft_decoder::SoftDecoder;

enum Inner {
    Mf(MfDecoder),
    Ff { dec: FfDecoder, device: ID3D11Device, stats: DecoderStats, query: Option<ID3D11Query> },
    Soft { dec: SoftDecoder, query: Option<ID3D11Query> },
}

/// Décodeur vidéo matériel zéro copie (sortie : texture NV12 du device donné).
pub struct VideoDecoder {
    inner: Inner,
    kind: DecoderKind,
    codec: Codec,
    next_pts: u64,
    hdr: bool,
}

// SAFETY : voir MfDecoder / FfDecoder (device protégé multithread, usage via &mut self).
unsafe impl Send for VideoDecoder {}

impl VideoDecoder {
    /// Décodeur Media Foundation avec son propre device D3D11 (adaptateur NVIDIA préféré).
    /// Pour afficher les images avec un [`crate::PresenterHandle`], utiliser [`VideoDecoder::with_device`].
    pub fn new(codec: Codec) -> Result<Self> {
        let device = gpu::create_device()?;
        Self::with_device(&device, codec)
    }

    /// Décodeur Media Foundation sur le device D3D11 donné (doit avoir
    /// `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` et la protection multithread ; voir [`crate::create_device`]).
    pub fn with_device(device: &ID3D11Device, codec: Codec) -> Result<Self> {
        Self::with_kind(device, codec, DecoderKind::MediaFoundation)
    }

    /// Décodeur de la voie `kind`.
    pub fn with_kind(device: &ID3D11Device, codec: Codec, kind: DecoderKind) -> Result<Self> {
        let inner = match kind {
            DecoderKind::MediaFoundation => Inner::Mf(MfDecoder::with_device(device, codec)?),
            DecoderKind::FfmpegD3d11va => {
                if !gpu_supports(device, codec) {
                    return Err(RenderError::Other(format!(
                        "le GPU n'expose pas de profil D3D11 {} 8 bits (NV12)",
                        chain::codec_key(codec)
                    )));
                }
                Inner::Ff {
                    dec: FfDecoder::new(device, codec)?,
                    device: device.clone(),
                    stats: DecoderStats::default(),
                    query: None,
                }
            }
            DecoderKind::OpenH264 => {
                if codec != Codec::H264 {
                    return Err(RenderError::Other("OpenH264 ne décode que le H.264".into()));
                }
                Inner::Soft { dec: SoftDecoder::openh264(device)?, query: None }
            }
            DecoderKind::FfmpegSoftware => Inner::Soft { dec: SoftDecoder::ffmpeg(device, codec)?, query: None },
        };
        Ok(VideoDecoder { inner, kind, codec, next_pts: 0, hdr: false })
    }

    /// Premier décodeur utilisable parmi les voies de `pref` (Auto : matériel puis logiciel).
    pub fn create_best_pref(
        device: &ID3D11Device,
        codec: Codec,
        disabled: &DisabledDecoders,
        pref: DecoderPref,
    ) -> std::result::Result<(Self, Vec<Attempt>), ChainError> {
        let vendor = crate::gpu::vendor_id(device);
        chain::create_with_pref_vendor(codec, disabled, pref, vendor, |k| {
            Self::with_kind(device, codec, k).map_err(|e| short_error(&e))
        })
        .map(|(d, _, a)| (d, a))
    }

    /// Flux HDR10 annoncé par le host (`GRX-CDC`) : les images sont marquées PQ/BT.2020.
    pub fn set_hdr(&mut self, hdr: bool) {
        self.hdr = hdr;
    }

    /// Premier décodeur utilisable de la chaîne MF -> FFmpeg D3D11VA ; renvoie aussi les échecs.
    pub fn create_best(
        device: &ID3D11Device,
        codec: Codec,
        disabled: &DisabledDecoders,
    ) -> std::result::Result<(Self, Vec<Attempt>), ChainError> {
        chain::create_with(codec, disabled, |k| Self::with_kind(device, codec, k).map_err(|e| short_error(&e)))
            .map(|(d, _, a)| (d, a))
    }

    /// Voie utilisée.
    pub fn kind(&self) -> DecoderKind {
        self.kind
    }

    /// Nom du MFT ou du décodeur FFmpeg utilisé.
    pub fn backend_name(&self) -> &str {
        match &self.inner {
            Inner::Mf(d) => d.backend_name(),
            Inner::Ff { dec, .. } => dec.name(),
            Inner::Soft { dec, .. } => dec.name(),
        }
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Device D3D11 sur lequel vivent les textures décodées.
    pub fn device(&self) -> &ID3D11Device {
        match &self.inner {
            Inner::Mf(d) => d.device(),
            Inner::Ff { device, .. } => device,
            Inner::Soft { dec, .. } => &dec.device,
        }
    }

    /// Taille visible du dernier flux décodé (0x0 avant la première image).
    pub fn size(&self) -> (u32, u32) {
        match &self.inner {
            Inner::Mf(d) => d.size(),
            Inner::Ff { dec, .. } => dec.size(),
            Inner::Soft { dec, .. } => dec.size(),
        }
    }

    pub fn stats(&self) -> DecoderStats {
        match &self.inner {
            Inner::Mf(d) => d.stats(),
            Inner::Ff { stats, .. } => *stats,
            Inner::Soft { dec, .. } => dec.stats,
        }
    }

    /// Format du flux annoncé par le host : la voie Media Foundation choisit la sortie du MFT
    /// correspondante (aucune conversion par image dans le MFT). Sans effet pour les autres voies.
    pub fn set_format_hint(&mut self, f: PixelFormat) {
        if let Inner::Mf(d) = &mut self.inner {
            d.set_format_hint(f);
        }
    }

    /// Format de sortie du décodeur (« NV12 »…) pour le diagnostic ; `None` hors Media Foundation.
    pub fn output_label(&self) -> Option<&'static str> {
        match &self.inner {
            Inner::Mf(d) => Some(d.output_label()),
            _ => None,
        }
    }

    /// `true` tant qu'un IDR n'a pas été reçu (au démarrage et après [`VideoDecoder::reset`]) :
    /// les paquets non-IDR sont ignorés. Le client doit alors demander un IDR au host.
    pub fn needs_keyframe(&self) -> bool {
        match &self.inner {
            Inner::Mf(d) => d.needs_keyframe(),
            Inner::Ff { dec, .. } => dec.needs_keyframe(),
            Inner::Soft { dec, .. } => dec.needs_keyframe(),
        }
    }

    /// Décode une access unit (Annex-B, ou OBU pour AV1). Horodatages synthétiques ; utiliser
    /// [`VideoDecoder::decode_pts`] / [`VideoDecoder::decode_frame`] pour les conserver.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<DecodedFrame>> {
        let pts = self.next_pts;
        self.next_pts += 16_667;
        self.decode_pts(data, pts)
    }

    /// Comme [`VideoDecoder::decode`] avec un `EncodedFrame` (conserve `pts`).
    pub fn decode_frame(&mut self, f: &EncodedFrame) -> Result<Option<DecodedFrame>> {
        self.decode_pts(&f.data, f.pts)
    }

    pub fn decode_pts(&mut self, data: &[u8], pts_us: u64) -> Result<Option<DecodedFrame>> {
        let hdr = self.hdr;
        self.decode_inner(data, pts_us).map(|o| {
            o.map(|mut f| {
                f.hdr |= hdr;
                f
            })
        })
    }

    fn decode_inner(&mut self, data: &[u8], pts_us: u64) -> Result<Option<DecodedFrame>> {
        match &mut self.inner {
            Inner::Mf(d) => d.decode_pts(data, pts_us),
            Inner::Soft { dec, .. } => dec.decode_pts(data, pts_us),
            Inner::Ff { dec, stats, .. } => {
                stats.packets_in += 1;
                let out = dec.decode(data, &mut stats.skipped_waiting_idr)?;
                Ok(out.map(|(texture, subresource, width, height, keep)| {
                    stats.frames_out += 1;
                    DecodedFrame { texture, subresource, width, height, pts: pts_us, hdr: false, _keep: FrameKeep::Ff(keep) }
                }))
            }
        }
    }

    /// Bloque jusqu'à ce que le GPU ait fini les commandes soumises (mesure du temps de décodage réel).
    pub fn sync_gpu(&mut self) -> Result<()> {
        match &mut self.inner {
            Inner::Mf(d) => d.sync_gpu(),
            Inner::Ff { device, query, .. } => sync_device(device, query),
            Inner::Soft { dec, query } => sync_device(&dec.device, query),
        }
    }

    /// Vide le décodeur ; il attend ensuite un IDR (comme après `reset()`).
    pub fn flush(&mut self) -> Result<Option<DecodedFrame>> {
        match &mut self.inner {
            Inner::Mf(d) => d.flush(),
            Inner::Ff { dec, .. } => {
                dec.reset();
                Ok(None)
            }
            Inner::Soft { dec, .. } => {
                dec.reset();
                Ok(None)
            }
        }
    }

    /// Repart de zéro après une perte : ignore les paquets jusqu'au prochain IDR.
    pub fn reset(&mut self) -> Result<()> {
        match &mut self.inner {
            Inner::Mf(d) => d.reset(),
            Inner::Ff { dec, .. } => {
                dec.reset();
                Ok(())
            }
            Inner::Soft { dec, .. } => {
                dec.reset();
                Ok(())
            }
        }
    }
}

/// Message d'erreur court pour le résumé de la chaîne de repli.
fn short_error(e: &RenderError) -> String {
    match e {
        RenderError::NoDecoder(c) => match c {
            Codec::Hevc => "aucun MFT HEVC (extension « HEVC Video Extensions » absente)".into(),
            Codec::Av1 => "aucun MFT AV1 (extension « AV1 Video Extension » absente)".into(),
            Codec::H264 => "aucun MFT H.264".into(),
        },
        e => e.to_string(),
    }
}

fn decoder_profile(codec: Codec) -> windows::core::GUID {
    match codec {
        Codec::H264 => D3D11_DECODER_PROFILE_H264_VLD_NOFGT,
        Codec::Hevc => D3D11_DECODER_PROFILE_HEVC_VLD_MAIN,
        Codec::Av1 => D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0,
    }
}

/// Le GPU sait-il décoder ce codec (profil 8 bits 4:2:0, sortie NV12) via D3D11 Video ?
pub fn gpu_supports(device: &ID3D11Device, codec: Codec) -> bool {
    gpu_supports_format(device, codec, PixelFormat::Yuv420)
}

/// DXVA HEVC RExt Main 4:4:4 8 bits (dxva.h, `DXVA_ModeHEVC_VLD_Main444`).
const HEVC_VLD_MAIN444: GUID = GUID::from_u128(0x4008018f_f537_4b36_98cf_61af8a2c1a33);

fn profile_for(codec: Codec, f: PixelFormat) -> Option<(GUID, DXGI_FORMAT)> {
    Some(match (codec, f) {
        (_, PixelFormat::Yuv420) => (decoder_profile(codec), DXGI_FORMAT_NV12),
        (Codec::Hevc, PixelFormat::Yuv420P10) => (D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10, DXGI_FORMAT_P010),
        (Codec::Av1, PixelFormat::Yuv420P10) => (D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0, DXGI_FORMAT_P010),
        (Codec::Hevc, PixelFormat::Yuv444) => (HEVC_VLD_MAIN444, DXGI_FORMAT_AYUV),
        _ => return None,
    })
}

/// Le GPU expose-t-il un profil D3D11 pour ce codec dans ce format (4:2:0 8/10 bits, HEVC 4:4:4) ?
pub fn gpu_supports_format(device: &ID3D11Device, codec: Codec, f: PixelFormat) -> bool {
    let Some((want, fmt)) = profile_for(codec, f) else { return false };
    let Ok(vdev) = device.cast::<ID3D11VideoDevice>() else { return false };
    let n = unsafe { vdev.GetVideoDecoderProfileCount() };
    (0..n).any(|i| {
        unsafe { vdev.GetVideoDecoderProfile(i) }.is_ok_and(|g| g == want)
            && unsafe { vdev.CheckVideoDecoderFormat(&want, fmt) }.is_ok_and(|b| b.as_bool())
    })
}

/// Formats annoncés pour une voie : matériel = profils D3D11 du GPU (le 4:4:4 HEVC sort en AYUV
/// du décodeur Microsoft, vérifié sur RTX 4070 Ti) ; OpenH264 = 4:2:0 8 bits ; FFmpeg logiciel =
/// tout (4:2:0/4:4:4, 8/10 bits).
pub fn kind_formats(device: &ID3D11Device, codec: Codec, kind: DecoderKind) -> Vec<PixelFormat> {
    match kind {
        DecoderKind::OpenH264 => vec![PixelFormat::Yuv420],
        DecoderKind::FfmpegSoftware => PixelFormat::ALL.to_vec(),
        // FFmpeg 8.1 D3D11VA : le HEVC 4:4:4 retombe en logiciel (mesuré, RTX 4070 Ti) -> 4:2:0 seulement.
        DecoderKind::FfmpegD3d11va => [PixelFormat::Yuv420, PixelFormat::Yuv420P10]
            .into_iter()
            .filter(|&f| gpu_supports_format(device, codec, f))
            .collect(),
        _ => [PixelFormat::Yuv420, PixelFormat::Yuv420P10, PixelFormat::Yuv444]
            .into_iter()
            .filter(|&f| gpu_supports_format(device, codec, f))
            .collect(),
    }
}

/// Résultat du sondage pour un codec.
#[derive(Clone, Debug)]
pub struct ProbeEntry {
    pub codec: Codec,
    /// Voie retenue (`None` : codec indisponible).
    pub kind: Option<DecoderKind>,
    /// Nom du décodeur retenu.
    pub backend: String,
    /// Échecs (voies précédentes, ou toutes si `kind` est `None`).
    pub attempts: Vec<Attempt>,
    /// Durée du sondage de ce codec (ms).
    pub probe_ms: f64,
    /// Voies utilisables et leurs formats (matérielle retenue, puis logicielles disponibles).
    pub caps: Vec<(DecoderKind, Vec<PixelFormat>)>,
}

impl ProbeEntry {
    pub fn error(&self) -> Option<ChainError> {
        self.kind.is_none().then(|| ChainError { codec: self.codec, attempts: self.attempts.clone() })
    }
}

type ProbeCache = Mutex<Vec<((i64, DisabledDecoders), Vec<ProbeEntry>)>>;
static PROBE_CACHE: ProbeCache = Mutex::new(Vec::new());

fn adapter_luid(device: &ID3D11Device) -> i64 {
    (|| -> windows::core::Result<i64> {
        let d: IDXGIDevice = device.cast()?;
        let a = unsafe { d.GetAdapter() }?;
        let desc = unsafe { a.GetDesc() }?;
        Ok(((desc.AdapterLuid.HighPart as i64) << 32) | desc.AdapterLuid.LowPart as i64)
    })()
    .unwrap_or(0)
}

/// Sonde HEVC, H.264 et AV1 en créant réellement un décodeur sur `device` (chaîne MF -> FFmpeg),
/// après vérification du profil D3D11 du GPU. Résultat mis en cache par adaptateur.
/// Travaille sur un thread dédié (COM MTA) : appelable depuis le thread de la fenêtre.
pub fn probe_decoders(device: &ID3D11Device, disabled: &DisabledDecoders) -> Vec<ProbeEntry> {
    let key = (adapter_luid(device), disabled.clone());
    if let Some((_, v)) = PROBE_CACHE.lock().unwrap().iter().find(|(k, _)| *k == key) {
        return v.clone();
    }
    let (dev, dis) = (device.clone(), disabled.clone());
    struct SendDev(ID3D11Device);
    // SAFETY : device protégé multithread (create_device).
    unsafe impl Send for SendDev {}
    let sd = SendDev(dev);
    let res = std::thread::spawn(move || {
        let sd = sd;
        let dev = &sd.0;
        [Codec::Hevc, Codec::H264, Codec::Av1]
            .into_iter()
            .map(|codec| {
                let t0 = std::time::Instant::now();
                let mut e = ProbeEntry { codec, kind: None, backend: String::new(), attempts: Vec::new(), probe_ms: 0.0, caps: Vec::new() };
                if !gpu_supports(dev, codec) {
                    let msg = format!("le GPU n'expose pas de profil D3D11 {} 8 bits", chain::codec_key(codec));
                    e.attempts = DecoderKind::CHAIN.iter().map(|&kind| Attempt { kind, error: msg.clone() }).collect();
                } else {
                    match VideoDecoder::create_best(dev, codec, &dis) {
                        Ok((d, a)) => {
                            e.kind = Some(d.kind());
                            e.backend = d.backend_name().to_string();
                            e.attempts = a;
                            e.caps.push((d.kind(), kind_formats(dev, codec, d.kind())));
                        }
                        Err(err) => e.attempts = err.attempts,
                    }
                }
                // Voies logicielles (secours, ou choix « Logiciel ») : création réelle.
                for kind in DecoderKind::SOFTWARE_CHAIN {
                    if dis.is_disabled(kind, codec) || (kind == DecoderKind::OpenH264 && codec != Codec::H264) {
                        continue;
                    }
                    match VideoDecoder::with_kind(dev, codec, kind) {
                        Ok(d) => {
                            if e.kind.is_none() {
                                e.kind = Some(kind);
                                e.backend = d.backend_name().to_string();
                            }
                            e.caps.push((kind, kind_formats(dev, codec, kind)));
                        }
                        Err(err) => e.attempts.push(Attempt { kind, error: short_error(&err) }),
                    }
                }
                e.probe_ms = t0.elapsed().as_secs_f64() * 1000.0;
                e
            })
            .collect::<Vec<_>>()
    })
    .join()
    .unwrap_or_default();
    PROBE_CACHE.lock().unwrap().push((key, res.clone()));
    res
}
