//! Encodeur H.264/HEVC/AV1 matÃ©riel via les MFT Media Foundation des pilotes (NVIDIA NVENC,
//! AMD AMF, Intel Quick Sync), choisis sur l'adaptateur de l'Ã©cran capturÃ© (`MFTEnum2` +
//! `MFT_ENUM_ADAPTER_LUID`).
//!
//! EntrÃ©e : textures D3D11 (zÃ©ro copie CPU) converties en NV12 (8 bits) ou P010 (10 bits) par le
//! VideoProcessor GPU. Les MFT matÃ©riels sont *asynchrones* : leur machine Ã  Ã©vÃ©nements
//! (`METransformNeedInput` / `METransformHaveOutput`) est pilotÃ©e par [`crate::mft_async::pump`].
//! RÃ©glages basse latence communs (CODECAPI) : CBR, `AVLowLatencyMode`, 0 image B, GOP infini
//! (IDR Ã  la demande), `QualityVsSpeed` = 0, VBV ~ 1,5 image ; acceptÃ©s par les trois vendeurs
//! (un rÃ©glage refusÃ© est seulement journalisÃ©, voir [`MfEncoder::warnings`]).

use crate::convert::{ColorMode, Converter};
use crate::error::{Result, VideoError, WinCtx};
use crate::mft_async::{self, AsyncMft, MftEvent, PumpError};
use crate::select::Vendor;
use crate::{gpu, nal, Codec, EncodedFrame, EncoderConfig};
use std::collections::VecDeque;
use std::sync::Once;
use std::time::{Duration, Instant};
use windows::core::{Interface, GUID, PWSTR, VARIANT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Com::CoTaskMemFree;

static MF_INIT: Once = Once::new();

pub(crate) fn mf_startup() -> Result<()> {
    let mut res: Result<()> = Ok(());
    MF_INIT.call_once(|| {
        res = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.ctx("MFStartup");
    });
    res
}

fn alloc_string(a: &IMFActivate, key: &GUID) -> Option<String> {
    let mut p = PWSTR::null();
    let mut len = 0u32;
    unsafe { a.GetAllocatedString(key, &mut p, &mut len) }.ok()?;
    let s = unsafe { p.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

/// MFT encodeurs matÃ©riels de `codec` : `(activation, nom, vendeur)`. Avec `luid`, seulement
/// ceux de cet adaptateur (`MFTEnum2`, Windows 10 1703+) ; sinon tous.
pub(crate) fn enum_hw_encoders(codec: Codec, luid: Option<i64>) -> Result<Vec<(IMFActivate, String, Vendor)>> {
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
    let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
    let by_luid = luid.and_then(|l| {
        let mut attrs: Option<IMFAttributes> = None;
        unsafe { MFCreateAttributes(&mut attrs, 1) }.ok()?;
        let attrs = attrs?;
        unsafe { attrs.SetBlob(&MFT_ENUM_ADAPTER_LUID, &l.to_le_bytes()) }.ok()?;
        unsafe { MFTEnum2(MFT_CATEGORY_VIDEO_ENCODER, flags, None, Some(&out_info), &attrs, &mut pp, &mut count) }.ok()
    });
    if by_luid.is_none() {
        pp = std::ptr::null_mut();
        count = 0;
        unsafe { MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, None, Some(&out_info), &mut pp, &mut count) }
            .ctx("MFTEnumEx")?;
    }
    let mut cands: Vec<(IMFActivate, String, Vendor)> = Vec::new();
    if pp.is_null() {
        return Ok(cands);
    }
    for i in 0..count as usize {
        // Prend possession de chaque IMFActivate (libÃ©rÃ© au drop).
        if let Some(a) = unsafe { std::ptr::read(pp.add(i)) } {
            let name = alloc_string(&a, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_default();
            let ven = alloc_string(&a, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute);
            let vendor = Vendor::from_mft(ven.as_deref(), &name);
            cands.push((a, name, vendor));
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

pub(crate) struct MfEncoder {
    cfg: EncoderConfig,
    pub name: String,
    #[allow(dead_code)]
    pub vendor: Vendor,
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

impl MfEncoder {
    /// CrÃ©e l'encodeur du MFT `act` sur le device D3D11 `device` (celui du capturer : les textures
    /// doivent vivre sur le mÃªme device).
    pub fn with_activate(device: &ID3D11Device, cfg: EncoderConfig, act: &IMFActivate, name: String, vendor: Vendor) -> Result<Self> {
        gpu::init_com();
        mf_startup()?;
        if cfg.format.is_444() {
            return Err(VideoError::Other("les MFT matÃ©riels n'encodent pas en 4:4:4".into()));
        }
        if cfg.format.is_10bit() && cfg.codec == Codec::H264 {
            return Err(VideoError::Other("H.264 10 bits non supportÃ©".into()));
        }
        let mft = unsafe { act.ActivateObject::<IMFTransform>() }.ctx("IMFActivate::ActivateObject")?;
        unsafe { timeBeginPeriod(1) };

        // DÃ©bloque le mode asynchrone + low latency.
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

        let color = if cfg.hdr { ColorMode::Pq2020 } else { ColorMode::Sdr709 };
        let mut enc = MfEncoder {
            conv: Converter::with_format(device, cfg.width, cfg.height, cfg.width, cfg.height, cfg.fps, cfg.format.is_10bit(), color)?,
            name,
            vendor,
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


    fn configure(&mut self) -> Result<()> {
        let cfg = self.cfg.clone();
        let api = self.codec_api.clone();
        let mut warn = |what: &str, r: windows::core::Result<()>| {
            if let Err(e) = r {
                self.warnings.push(format!("{what}: {e}"));
            }
        };
        // --- RÃ©glages low-latency (avant SetOutputType) ---
        warn(
            "RateControlMode=CBR",
            set_u32(&api, &CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_CBR.0 as u32),
        );
        warn("MeanBitRate", set_u32(&api, &CODECAPI_AVEncCommonMeanBitRate, cfg.bitrate_kbps * 1000));
        warn("LowLatencyMode", unsafe { api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true)) });
        warn("BPictureCount=0", set_u32(&api, &CODECAPI_AVEncMPVDefaultBPictureCount, 0));
        // GOP Â« infini Â» : des IDR uniquement Ã  la demande (request_keyframe).
        warn("GOPSize=inf", set_u32(&api, &CODECAPI_AVEncMPVGOPSize, u32::MAX));
        warn("QualityVsSpeed", set_u32(&api, &CODECAPI_AVEncCommonQualityVsSpeed, 0));
        // VBV ~ 1,5 image : lisse le dÃ©bit sans ajouter de latence.
        let vbv_bits = (cfg.bitrate_kbps as u64 * 1000 * 3 / 2 / cfg.fps.max(1) as u64) as u32;
        warn("BufferSize", set_u32(&api, &CODECAPI_AVEncCommonBufferSize, vbv_bits));

        // --- Type de sortie (flux compressÃ©) ---
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
            let ten = cfg.format.is_10bit();
            let profile = match (cfg.codec, ten) {
                (Codec::H264, _) => eAVEncH264VProfile_High.0,
                (Codec::Hevc, false) => eAVEncH265VProfile_Main_420_8.0,
                (Codec::Hevc, true) => eAVEncH265VProfile_Main_420_10.0,
                (Codec::Av1, false) => eAVEncAV1VProfile_Main_420_8.0,
                (Codec::Av1, true) => eAVEncAV1VProfile_Main_420_10.0,
            };
            out_t.SetUINT32(&MF_MT_MPEG2_PROFILE, profile as u32).ctx("MPEG2_PROFILE")?;
            // MÃ©tadonnÃ©es colorimÃ©triques (VUI) : BT.709 SDR ou BT.2020/PQ (HDR10), plage
            // limitÃ©e â€” cohÃ©rent avec le VideoProcessor.
            set_color(&out_t, cfg.hdr);
            self.mft.SetOutputType(0, &out_t, 0).ctx("SetOutputType")?;
        }

        // --- Type d'entrÃ©e : NV12 (8 bits) ou P010 (10 bits) ---
        let want = if cfg.format.is_10bit() { MFVideoFormat_P010 } else { MFVideoFormat_NV12 };
        let mut set = false;
        for i in 0.. {
            let t = match unsafe { self.mft.GetInputAvailableType(0, i) } {
                Ok(t) => t,
                Err(_) => break,
            };
            let sub = unsafe { t.GetGUID(&MF_MT_SUBTYPE) }.ctx("input subtype")?;
            if sub == want {
                unsafe {
                    type_u64(&t, &MF_MT_FRAME_SIZE, cfg.width, cfg.height)?;
                    type_u64(&t, &MF_MT_FRAME_RATE, cfg.fps, 1)?;
                    type_u64(&t, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
                    let _ = t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
                    set_color(&t, cfg.hdr);
                    self.mft.SetInputType(0, &t, 0).ctx("SetInputType")?;
                }
                set = true;
                break;
            }
        }
        if !set {
            return Err(VideoError::Other(format!(
                "le MFT n'accepte pas l'entrÃ©e {}",
                if cfg.format.is_10bit() { "P010 (10 bits)" } else { "NV12" }
            )));
        }

        // Certains drivers ignorent les rÃ©glages faits avant SetOutputType : on les rejoue.
        let api = self.codec_api.clone();
        let _ = set_u32(&api, &CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_CBR.0 as u32);
        let _ = set_u32(&api, &CODECAPI_AVEncCommonMeanBitRate, cfg.bitrate_kbps * 1000);

        let info = unsafe { self.mft.GetOutputStreamInfo(0) }.ctx("GetOutputStreamInfo")?;
        self.provides_samples = (info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
        self.out_buf_size = info.cbSize.max(1 << 20);

        // RÃ©cupÃ¨re l'en-tÃªte de sÃ©quence (VPS/SPS/PPS) si le MFT l'expose.
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

    /// RÃ©glages non appliquÃ©s par le driver (diagnostic).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Demande une image IDR pour la prochaine image encodÃ©e.
    pub fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    /// Compromis qualitÃ©/vitesse du preset NVENC, 0..=100 (dÃ©faut 0 = le plus rapide).
    /// MesurÃ© sur RTX 4070 Ti en 1440p : 0 -> ~2 ms, 50 -> ~5 ms, 100 -> ~11 ms par image.
    pub fn set_quality_vs_speed(&mut self, v: u32) -> Result<()> {
        set_u32(&self.codec_api, &CODECAPI_AVEncCommonQualityVsSpeed, v.min(100)).ctx("set_quality_vs_speed")
    }

    /// Change le dÃ©bit cible (CBR) Ã  chaud, en kbit/s.
    pub fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        self.cfg.bitrate_kbps = kbps;
        let r = set_u32(&self.codec_api, &CODECAPI_AVEncCommonMeanBitRate, kbps * 1000);
        let _ = set_u32(&self.codec_api, &CODECAPI_AVEncCommonMaxBitRate, kbps * 1000);
        let vbv = (kbps as u64 * 1000 * 3 / 2 / self.cfg.fps.max(1) as u64) as u32;
        let _ = set_u32(&self.codec_api, &CODECAPI_AVEncCommonBufferSize, vbv);
        r.ctx("set_bitrate")
    }

    /// Encode une texture D3D11 (BGRA/RGB10A2â€¦) vivant sur le device de l'encodeur.
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

        // Attend que le MFT rÃ©clame une entrÃ©e.
        self.wait(|s| s.need_input > 0)?;
        if self.force_idr {
            self.force_idr = false;
            let _ = set_u32(&self.codec_api, &CODECAPI_AVEncVideoForceKeyFrame, 1);
        }
        unsafe { self.mft.ProcessInput(0, &sample, 0) }.ctx("ProcessInput")?;
        self.need_input -= 1;
        self.frames_in += 1;

        // Attend la sortie (low-latency : 1 entrÃ©e -> 1 sortie).
        self.wait(|s| !s.out_q.is_empty())?;
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

    /// Fait tourner la machine Ã  Ã©vÃ©nements du MFT jusqu'Ã  ce que `cond` soit vraie (2 s max).
    fn wait(&mut self, cond: impl Fn(&Self) -> bool) -> Result<()> {
        mft_async::pump(self, Duration::from_millis(2000), cond).map_err(|e| match e {
            PumpError::Timeout => VideoError::Timeout("waiting for MFT event"),
            e => VideoError::Other(format!("MFT Â« {} Â» : {e}", self.name)),
        })
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
                    // RenÃ©gociation du type de sortie.
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

/// Lit l'Ã©vÃ©nement suivant de la file du MFT sans attendre.
pub(crate) fn next_mf_event(events: &IMFMediaEventGenerator) -> std::result::Result<Option<MftEvent>, String> {
    match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
        Ok(ev) => {
            let ty = MF_EVENT_TYPE(unsafe { ev.GetType() }.map_err(|e| e.to_string())? as i32);
            Ok(Some(if ty == METransformNeedInput {
                MftEvent::NeedInput
            } else if ty == METransformHaveOutput {
                MftEvent::HaveOutput
            } else if ty == MEError {
                MftEvent::Error(unsafe { ev.GetStatus() }.map(|h| h.0).unwrap_or(-1))
            } else {
                MftEvent::Other
            }))
        }
        Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => Ok(None),
        Err(e) => Err(format!("IMFMediaEventGenerator::GetEvent : {e}")),
    }
}

impl AsyncMft for MfEncoder {
    fn next_event(&mut self) -> std::result::Result<Option<MftEvent>, String> {
        next_mf_event(&self.events)
    }
    fn on_need_input(&mut self) {
        self.need_input += 1;
    }
    fn on_have_output(&mut self) -> std::result::Result<(), String> {
        self.process_output().map_err(|e| e.to_string())
    }
}

/// Attributs colorimÃ©triques d'un type de mÃ©dia : BT.709 SDR ou BT.2020/PQ, plage limitÃ©e.
fn set_color(t: &IMFMediaType, hdr: bool) {
    let (prim, trc, mat) = if hdr {
        (MFVideoPrimaries_BT2020.0, MFVideoTransFunc_2084.0, MFVideoTransferMatrix_BT2020_10.0)
    } else {
        (MFVideoPrimaries_BT709.0, MFVideoTransFunc_709.0, MFVideoTransferMatrix_BT709.0)
    };
    unsafe {
        let _ = t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, prim as u32);
        let _ = t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, trc as u32);
        let _ = t.SetUINT32(&MF_MT_YUV_MATRIX, mat as u32);
        let _ = t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
    }
}

// SAFETY : le device D3D11 est crÃ©Ã© avec la protection multithread et COM est initialisÃ© en MTA ;
// l'encodeur n'est jamais utilisÃ© depuis deux threads Ã  la fois (&mut self).
unsafe impl Send for MfEncoder {}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
            timeEndPeriod(1);
        }
    }
}
