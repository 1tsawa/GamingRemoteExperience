//! Encodeur H.264/H.265 matériel via Media Foundation (MFT NVENC de NVIDIA).
//!
//! Entrée : textures D3D11 (zéro copie CPU) converties en NV12 par le VideoProcessor GPU.
//! Le MFT NVIDIA est *asynchrone* : on pilote sa machine à événements
//! (`METransformNeedInput` / `METransformHaveOutput`) en mode synchrone côté appelant.

use crate::capture::CapturedFrame;
use crate::convert::Converter;
use crate::error::{Result, VideoError, WinCtx};
use crate::{gpu, nal, Codec, EncodedFrame, EncoderConfig};
use std::collections::VecDeque;
use std::sync::Once;
use std::time::{Duration, Instant};
use windows::core::{Interface, GUID, PWSTR, VARIANT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Com::CoTaskMemFree;

/// Backend d'encodage effectivement utilisé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncoderBackend {
    /// MFT matériel Media Foundation (nom convivial fourni par le driver).
    MediaFoundation { name: String },
}

static MF_INIT: Once = Once::new();

fn mf_startup() -> Result<()> {
    let mut res: Result<()> = Ok(());
    MF_INIT.call_once(|| {
        res = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.ctx("MFStartup");
    });
    res
}

/// Codecs pour lesquels un encodeur MFT matériel est enregistré (H.264, HEVC, AV1), avec le nom
/// du premier (NVIDIA d'abord). Simple énumération (pas de création) ; résultat mis en cache.
/// La création peut encore échouer : l'appelant se replie alors sur H.264.
pub fn available_encoders() -> Vec<(Codec, String)> {
    static CACHE: std::sync::OnceLock<Vec<(Codec, String)>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            gpu::init_com();
            if mf_startup().is_err() {
                return Vec::new();
            }
            [Codec::H264, Codec::Hevc, Codec::Av1]
                .into_iter()
                .filter_map(|c| {
                    let mut names = enum_hw_encoders(c).ok()?.into_iter().map(|(_, n)| n).collect::<Vec<_>>();
                    names.sort_by_key(|n| !n.to_ascii_lowercase().contains("nvidia"));
                    names.into_iter().next().map(|n| (c, n))
                })
                .collect()
        })
        .clone()
}

fn enum_hw_encoders(codec: Codec) -> Result<Vec<(IMFActivate, String)>> {
    let out_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: match codec {
            Codec::H264 => MFVideoFormat_H264,
            Codec::Hevc => MFVideoFormat_HEVC,
            Codec::Av1 => MFVideoFormat_AV1,
        },
    };
    let mut pp: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0),
            None,
            Some(&out_info),
            &mut pp,
            &mut count,
        )
    }
    .ctx("MFTEnumEx")?;
    let mut cands: Vec<(IMFActivate, String)> = Vec::new();
    if pp.is_null() {
        return Ok(cands);
    }
    for i in 0..count as usize {
        // Prend possession de chaque IMFActivate (libéré au drop).
        if let Some(a) = unsafe { std::ptr::read(pp.add(i)) } {
            let mut name = String::new();
            let mut p = PWSTR::null();
            let mut len = 0u32;
            if unsafe { a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len) }.is_ok() {
                name = unsafe { p.to_string() }.unwrap_or_default();
                unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
            }
            cands.push((a, name));
        }
    }
    unsafe { CoTaskMemFree(Some(pp as *const _)) };
    Ok(cands)
}

struct RawOutput {
    data: Vec<u8>,
    clean_point: bool,
    pts_100ns: i64,
}

pub struct Encoder {
    cfg: EncoderConfig,
    backend: EncoderBackend,
    device: ID3D11Device,
    conv: Converter,
    mft: IMFTransform,
    events: IMFMediaEventGenerator,
    codec_api: ICodecAPI,
    _dxgi_mgr: IMFDXGIDeviceManager,
    need_input: u32,
    out_q: VecDeque<RawOutput>,
    provides_samples: bool,
    out_buf_size: u32,
    start: Option<Instant>,
    last_pts: i64,
    force_idr: bool,
    seq_header: Vec<u8>,
    warnings: Vec<String>,
    frames_in: u64,
}

fn set_u32(api: &ICodecAPI, key: &GUID, v: u32) -> windows::core::Result<()> {
    unsafe { api.SetValue(key, &VARIANT::from(v)) }
}

fn type_u64(t: &IMFMediaType, key: &GUID, hi: u32, lo: u32) -> Result<()> {
    unsafe { t.SetUINT64(key, ((hi as u64) << 32) | lo as u64) }.ctx("SetUINT64")
}

impl Encoder {
    /// Crée un encodeur avec son propre device D3D11 (adaptateur NVIDIA préféré).
    /// Pour encoder les textures d'un [`crate::ScreenCapturer`], utiliser plutôt [`Encoder::with_device`].
    pub fn new(cfg: EncoderConfig) -> Result<Self> {
        gpu::init_com();
        let adapter = gpu::preferred_adapter()?;
        let (device, _ctx) = gpu::create_device_for_adapter(&adapter)?;
        Self::with_device(&device, cfg)
    }

    /// Crée un encodeur sur le device D3D11 `device` (celui du capturer : les textures doivent
    /// vivre sur le même device).
    pub fn with_device(device: &ID3D11Device, cfg: EncoderConfig) -> Result<Self> {
        gpu::init_com();
        mf_startup()?;
        unsafe { timeBeginPeriod(1) };
        let (mft, name) = Self::create_mft(cfg.codec)?;

        // Débloque le mode asynchrone + low latency.
        let attrs = unsafe { mft.GetAttributes() }.ctx("IMFTransform::GetAttributes")?;
        let is_async = unsafe { attrs.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0;
        if !is_async {
            return Err(VideoError::Other(format!("MFT '{name}' is not asynchronous (unsupported)")));
        }
        unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }.ctx("MF_TRANSFORM_ASYNC_UNLOCK")?;
        let _ = unsafe { attrs.SetUINT32(&MF_LOW_LATENCY, 1) };

        // Gestionnaire de device DXGI : le MFT lit directement les textures GPU.
        let mut token = 0u32;
        let mut mgr: Option<IMFDXGIDeviceManager> = None;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut mgr) }.ctx("MFCreateDXGIDeviceManager")?;
        let mgr = mgr.ok_or_else(|| VideoError::Other("no IMFDXGIDeviceManager".into()))?;
        unsafe { mgr.ResetDevice(device, token) }.ctx("IMFDXGIDeviceManager::ResetDevice")?;
        unsafe { mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize) }
            .ctx("MFT_MESSAGE_SET_D3D_MANAGER")?;

        let codec_api: ICodecAPI = mft.cast().ctx("ICodecAPI")?;
        let events: IMFMediaEventGenerator = mft.cast().ctx("IMFMediaEventGenerator")?;

        let mut enc = Encoder {
            conv: Converter::new(device, cfg.width, cfg.height, cfg.width, cfg.height, cfg.fps)?,
            backend: EncoderBackend::MediaFoundation { name },
            device: device.clone(),
            mft,
            events,
            codec_api,
            _dxgi_mgr: mgr,
            need_input: 0,
            out_q: VecDeque::new(),
            provides_samples: true,
            out_buf_size: 0,
            start: None,
            last_pts: -1,
            force_idr: true,
            seq_header: Vec::new(),
            warnings: Vec::new(),
            frames_in: 0,
            cfg,
        };
        enc.configure()?;
        Ok(enc)
    }

    fn create_mft(codec: Codec) -> Result<(IMFTransform, String)> {
        let mut cands = enum_hw_encoders(codec)?;
        if cands.is_empty() {
            return Err(VideoError::NoEncoder(codec));
        }
        // Préfère NVIDIA.
        cands.sort_by_key(|(_, n)| !n.to_ascii_lowercase().contains("nvidia"));
        let mut last_err = None;
        for (a, n) in cands {
            match unsafe { a.ActivateObject::<IMFTransform>() } {
                Ok(t) => return Ok((t, n)),
                Err(e) => last_err = Some(e),
            }
        }
        match last_err {
            Some(e) => Err(VideoError::Win { context: "IMFActivate::ActivateObject", source: e }),
            None => Err(VideoError::NoEncoder(codec)),
        }
    }

    fn configure(&mut self) -> Result<()> {
        let cfg = self.cfg.clone();
        let api = self.codec_api.clone();
        let mut warn = |what: &str, r: windows::core::Result<()>| {
            if let Err(e) = r {
                self.warnings.push(format!("{what}: {e}"));
            }
        };
        // --- Réglages low-latency (avant SetOutputType) ---
        warn(
            "RateControlMode=CBR",
            set_u32(&api, &CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_CBR.0 as u32),
        );
        warn("MeanBitRate", set_u32(&api, &CODECAPI_AVEncCommonMeanBitRate, cfg.bitrate_kbps * 1000));
        warn("LowLatencyMode", unsafe { api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true)) });
        warn("BPictureCount=0", set_u32(&api, &CODECAPI_AVEncMPVDefaultBPictureCount, 0));
        // GOP « infini » : des IDR uniquement à la demande (request_keyframe).
        warn("GOPSize=inf", set_u32(&api, &CODECAPI_AVEncMPVGOPSize, u32::MAX));
        warn("QualityVsSpeed", set_u32(&api, &CODECAPI_AVEncCommonQualityVsSpeed, 0));
        // VBV ~ 1,5 image : lisse le débit sans ajouter de latence.
        let vbv_bits = (cfg.bitrate_kbps as u64 * 1000 * 3 / 2 / cfg.fps.max(1) as u64) as u32;
        warn("BufferSize", set_u32(&api, &CODECAPI_AVEncCommonBufferSize, vbv_bits));

        // --- Type de sortie (flux compressé) ---
        let out_t = unsafe { MFCreateMediaType() }.ctx("MFCreateMediaType")?;
        unsafe {
            out_t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ctx("MAJOR_TYPE")?;
            out_t
                .SetGUID(
                    &MF_MT_SUBTYPE,
                    &match cfg.codec {
                        Codec::H264 => MFVideoFormat_H264,
                        Codec::Hevc => MFVideoFormat_HEVC,
                        Codec::Av1 => MFVideoFormat_AV1,
                    },
                )
                .ctx("SUBTYPE")?;
            out_t.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate_kbps * 1000).ctx("AVG_BITRATE")?;
            type_u64(&out_t, &MF_MT_FRAME_SIZE, cfg.width, cfg.height)?;
            type_u64(&out_t, &MF_MT_FRAME_RATE, cfg.fps, 1)?;
            type_u64(&out_t, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            out_t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).ctx("INTERLACE")?;
            let profile = match cfg.codec {
                Codec::H264 => eAVEncH264VProfile_High.0,
                Codec::Hevc => eAVEncH265VProfile_Main_420_8.0,
                Codec::Av1 => eAVEncAV1VProfile_Main_420_8.0,
            };
            let _ = out_t.SetUINT32(&MF_MT_MPEG2_PROFILE, profile as u32);
            // Métadonnées colorimétriques (VUI) : BT.709, plage limitée — cohérent avec le VideoProcessor.
            let _ = out_t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
            let _ = out_t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
            let _ = out_t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
            let _ = out_t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
            self.mft.SetOutputType(0, &out_t, 0).ctx("SetOutputType")?;
        }

        // --- Type d'entrée : NV12 ---
        let mut set = false;
        for i in 0.. {
            let t = match unsafe { self.mft.GetInputAvailableType(0, i) } {
                Ok(t) => t,
                Err(_) => break,
            };
            let sub = unsafe { t.GetGUID(&MF_MT_SUBTYPE) }.ctx("input subtype")?;
            if sub == MFVideoFormat_NV12 {
                unsafe {
                    type_u64(&t, &MF_MT_FRAME_SIZE, cfg.width, cfg.height)?;
                    type_u64(&t, &MF_MT_FRAME_RATE, cfg.fps, 1)?;
                    type_u64(&t, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
                    let _ = t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
                    let _ = t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
                    let _ = t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
                    let _ = t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
                    let _ = t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
                    self.mft.SetInputType(0, &t, 0).ctx("SetInputType(NV12)")?;
                }
                set = true;
                break;
            }
        }
        if !set {
            return Err(VideoError::Other("MFT does not accept NV12 input".into()));
        }

        // Certains drivers ignorent les réglages faits avant SetOutputType : on les rejoue.
        let api = self.codec_api.clone();
        let _ = set_u32(&api, &CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_CBR.0 as u32);
        let _ = set_u32(&api, &CODECAPI_AVEncCommonMeanBitRate, cfg.bitrate_kbps * 1000);

        let info = unsafe { self.mft.GetOutputStreamInfo(0) }.ctx("GetOutputStreamInfo")?;
        self.provides_samples = (info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
        self.out_buf_size = info.cbSize.max(1 << 20);

        // Récupère l'en-tête de séquence (VPS/SPS/PPS) si le MFT l'expose.
        self.refresh_seq_header();

        unsafe {
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).ctx("BEGIN_STREAMING")?;
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).ctx("START_OF_STREAM")?;
        }
        Ok(())
    }

    fn refresh_seq_header(&mut self) {
        if let Ok(t) = unsafe { self.mft.GetOutputCurrentType(0) } {
            if let Ok(sz) = unsafe { t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) } {
                let mut buf = vec![0u8; sz as usize];
                if unsafe { t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None) }.is_ok() {
                    self.seq_header = buf;
                }
            }
        }
    }

    /// Backend utilisé (nom du MFT).
    pub fn backend(&self) -> &EncoderBackend {
        &self.backend
    }

    pub fn config(&self) -> &EncoderConfig {
        &self.cfg
    }

    /// Réglages non appliqués par le driver (diagnostic).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Demande une image IDR pour la prochaine image encodée.
    pub fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    /// Compromis qualité/vitesse du preset NVENC, 0..=100 (défaut 0 = le plus rapide).
    /// Mesuré sur RTX 4070 Ti en 1440p : 0 -> ~2 ms, 50 -> ~5 ms, 100 -> ~11 ms par image.
    pub fn set_quality_vs_speed(&mut self, v: u32) -> Result<()> {
        set_u32(&self.codec_api, &CODECAPI_AVEncCommonQualityVsSpeed, v.min(100)).ctx("set_quality_vs_speed")
    }

    /// Change le débit cible (CBR) à chaud, en kbit/s.
    pub fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        self.cfg.bitrate_kbps = kbps;
        let r = set_u32(&self.codec_api, &CODECAPI_AVEncCommonMeanBitRate, kbps * 1000);
        let _ = set_u32(&self.codec_api, &CODECAPI_AVEncCommonMaxBitRate, kbps * 1000);
        let vbv = (kbps as u64 * 1000 * 3 / 2 / self.cfg.fps.max(1) as u64) as u32;
        let _ = set_u32(&self.codec_api, &CODECAPI_AVEncCommonBufferSize, vbv);
        r.ctx("set_bitrate")
    }

    /// Encode une frame capturée et attend le bitstream correspondant (bloquant, quelques ms).
    pub fn encode(&mut self, frame: &CapturedFrame) -> Result<EncodedFrame> {
        self.encode_texture(&frame.texture, frame.width, frame.height)
    }

    /// Encode une texture D3D11 (BGRA/RGB10A2…) vivant sur le device de l'encodeur.
    pub fn encode_texture(&mut self, tex: &ID3D11Texture2D, _w: u32, _h: u32) -> Result<EncodedFrame> {
        let dev = unsafe { tex.GetDevice() }.ctx("ID3D11Texture2D::GetDevice")?;
        if dev.as_raw() != self.device.as_raw() {
            return Err(VideoError::Other(
                "texture belongs to another D3D11 device: build the Encoder with Encoder::with_device(capturer.device(), ..)"
                    .into(),
            ));
        }
        let nv12 = self.conv.convert(tex, _w, _h)?;
        self.submit_nv12(&nv12)
    }

    /// Chemin lent pour les tests/outils : encode des pixels BGRA CPU (`stride` = octets par ligne).
    pub fn encode_bgra(&mut self, bgra: &[u8], width: u32, height: u32, stride: u32) -> Result<EncodedFrame> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: bgra.as_ptr() as *const _, SysMemPitch: stride, SysMemSlicePitch: 0 };
        let mut t = None;
        unsafe { self.device.CreateTexture2D(&desc, Some(&init), Some(&mut t)) }.ctx("CreateTexture2D(upload)")?;
        self.encode_texture(&t.unwrap(), width, height)
    }

    fn submit_nv12(&mut self, nv12: &ID3D11Texture2D) -> Result<EncodedFrame> {
        let now = Instant::now();
        let start = *self.start.get_or_insert(now);
        let mut pts = (now - start).as_nanos() as i64 / 100;
        if pts <= self.last_pts {
            pts = self.last_pts + 1;
        }
        self.last_pts = pts;

        let sample = unsafe { MFCreateSample() }.ctx("MFCreateSample")?;
        unsafe {
            let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, nv12, 0, false).ctx("MFCreateDXGISurfaceBuffer")?;
            // Certains MFT exigent une longueur non nulle pour les buffers de surface DXGI.
            if let Ok(b2d) = buf.cast::<IMF2DBuffer>() {
                if let Ok(len) = b2d.GetContiguousLength() {
                    let _ = buf.SetCurrentLength(len);
                }
            }
            sample.AddBuffer(&buf).ctx("AddBuffer")?;
            sample.SetSampleTime(pts).ctx("SetSampleTime")?;
            sample.SetSampleDuration(10_000_000 / self.cfg.fps.max(1) as i64).ctx("SetSampleDuration")?;
        }

        // Attend que le MFT réclame une entrée.
        self.pump(Duration::from_millis(2000), |s| s.need_input > 0)?;
        if self.force_idr {
            self.force_idr = false;
            let _ = set_u32(&self.codec_api, &CODECAPI_AVEncVideoForceKeyFrame, 1);
        }
        unsafe { self.mft.ProcessInput(0, &sample, 0) }.ctx("ProcessInput")?;
        self.need_input -= 1;
        self.frames_in += 1;

        // Attend la sortie (low-latency : 1 entrée -> 1 sortie).
        self.pump(Duration::from_millis(2000), |s| !s.out_q.is_empty())?;
        let raw = self.out_q.pop_front().unwrap();
        Ok(self.finish(raw))
    }

    fn finish(&mut self, raw: RawOutput) -> EncodedFrame {
        let mut data = raw.data;
        let is_key = raw.clean_point || nal::contains_idr(self.cfg.codec, &data);
        if is_key && !self.seq_header.is_empty() && !nal::contains_parameter_sets(self.cfg.codec, &data) {
            let mut v = self.seq_header.clone();
            v.extend_from_slice(&data);
            data = v;
        }
        EncodedFrame {
            data,
            is_keyframe: is_key,
            pts: (raw.pts_100ns.max(0) / 10) as u64,
            width: self.cfg.width,
            height: self.cfg.height,
        }
    }

    /// Fait tourner la machine à événements du MFT jusqu'à ce que `cond` soit vraie.
    fn pump(&mut self, timeout: Duration, cond: impl Fn(&Self) -> bool) -> Result<()> {
        let t0 = Instant::now();
        loop {
            if cond(self) {
                return Ok(());
            }
            match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(ev) => {
                    let ty = unsafe { ev.GetType() }.ctx("IMFMediaEvent::GetType")?;
                    let ty = MF_EVENT_TYPE(ty as i32);
                    if ty == METransformNeedInput {
                        self.need_input += 1;
                    } else if ty == METransformHaveOutput {
                        self.process_output()?;
                    } else if ty == MEError {
                        let hr = unsafe { ev.GetStatus() }.unwrap_or_default();
                        return Err(VideoError::Other(format!("MFT error event: {hr:?}")));
                    }
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    let el = t0.elapsed();
                    if el > timeout {
                        return Err(VideoError::Timeout("waiting for MFT event"));
                    }
                    // Spin court (latence minimale) puis sommeil 1 ms (timeBeginPeriod(1)).
                    if el < Duration::from_millis(3) {
                        std::hint::spin_loop();
                    } else {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
                Err(e) => return Err(VideoError::Win { context: "IMFMediaEventGenerator::GetEvent", source: e }),
            }
        }
    }

    fn process_output(&mut self) -> Result<()> {
        unsafe {
            let mut buf = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            };
            if !self.provides_samples {
                let s = MFCreateSample().ctx("MFCreateSample(out)")?;
                let b = MFCreateMemoryBuffer(self.out_buf_size).ctx("MFCreateMemoryBuffer")?;
                s.AddBuffer(&b).ctx("AddBuffer(out)")?;
                buf.pSample = std::mem::ManuallyDrop::new(Some(s));
            }
            let mut status = 0u32;
            let r = self.mft.ProcessOutput(0, std::slice::from_mut(&mut buf), &mut status);
            let sample = std::mem::ManuallyDrop::take(&mut buf.pSample);
            let _events = std::mem::ManuallyDrop::take(&mut buf.pEvents);
            match r {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // Renégociation du type de sortie.
                    if let Ok(t) = self.mft.GetOutputAvailableType(0, 0) {
                        let _ = self.mft.SetOutputType(0, &t, 0);
                    }
                    self.refresh_seq_header();
                    return Ok(());
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(VideoError::Win { context: "IMFTransform::ProcessOutput", source: e }),
            }
            let Some(sample) = sample else {
                return Ok(());
            };
            let mb = sample.ConvertToContiguousBuffer().ctx("ConvertToContiguousBuffer")?;
            let mut p: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            mb.Lock(&mut p, None, Some(&mut len)).ctx("IMFMediaBuffer::Lock")?;
            let data = std::slice::from_raw_parts(p, len as usize).to_vec();
            let _ = mb.Unlock();
            let clean_point = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let pts_100ns = sample.GetSampleTime().unwrap_or(0);
            self.out_q.push_back(RawOutput { data, clean_point, pts_100ns });
        }
        Ok(())
    }
}

// SAFETY : le device D3D11 est créé avec la protection multithread et COM est initialisé en MTA ;
// l'Encoder n'est jamais utilisé depuis deux threads à la fois (&mut self).
unsafe impl Send for Encoder {}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
            timeEndPeriod(1);
        }
    }
}
