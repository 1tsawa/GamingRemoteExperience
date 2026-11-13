//! Décodeur H.264/H.265 matériel via Media Foundation (DXVA2/D3D11 Video Decoder).
//!
//! Le MFT (`Microsoft H264 Video Decoder MFT` ou `HEVCVideoExtension`) est piloté en mode
//! *D3D11 aware* : le gestionnaire de device DXGI lui est donné, il décode avec l'ID3D11VideoDecoder
//! du GPU (NVDEC sur NVIDIA) et rend des textures NV12 GPU. Aucun pixel ne passe par le CPU.
//! Mode `MF_LOW_LATENCY` : une entrée = une sortie, sans réordonnancement.

use crate::annexb;
use crate::error::{RenderError, Result, WinCtx};
use crate::gpu;
use grx_video::{Codec, EncodedFrame};
use std::sync::Once;
use windows::core::{Interface, PWSTR, VARIANT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;

static MF_INIT: Once = Once::new();

pub(crate) fn mf_startup() -> Result<()> {
    let mut res: Result<()> = Ok(());
    MF_INIT.call_once(|| {
        res = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.ctx("MFStartup");
    });
    res
}

/// Image décodée : texture NV12 sur le GPU (zéro copie CPU).
///
/// La texture appartient au pool du décodeur (`texture` est en général un *texture array* et
/// `subresource` l'index de la tranche). **Relâchez la frame rapidement** (le décodeur n'a qu'un
/// petit nombre de surfaces ; en retenir trop bloque le décodage).
pub struct DecodedFrame {
    pub texture: ID3D11Texture2D,
    /// Index de la tranche dans `texture` (sous-ressource D3D11).
    pub subresource: u32,
    /// Taille visible de l'image (peut être inférieure à celle de la texture : alignement 16/32).
    pub width: u32,
    pub height: u32,
    /// Horodatage de présentation (µs), recopié depuis l'entrée.
    pub pts: u64,
    _sample: IMFSample,
}

impl std::fmt::Debug for DecodedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedFrame")
            .field("size", &(self.width, self.height))
            .field("subresource", &self.subresource)
            .field("pts", &self.pts)
            .finish()
    }
}

/// Statistiques cumulées du décodeur.
#[derive(Debug, Clone, Copy, Default)]
pub struct DecoderStats {
    pub packets_in: u64,
    pub frames_out: u64,
    /// Paquets ignorés car aucun IDR n'avait encore été reçu depuis `reset()`/le démarrage.
    pub skipped_waiting_idr: u64,
}

// SAFETY : la texture et l'IMFSample sont des objets D3D11/MF agiles ; le device est protégé
// contre les accès concurrents (ID3D11Multithread). La frame peut donc passer d'un thread à l'autre.
unsafe impl Send for DecodedFrame {}
unsafe impl Sync for DecodedFrame {}

pub struct VideoDecoder {
    query: Option<ID3D11Query>,
    codec: Codec,
    device: ID3D11Device,
    mft: IMFTransform,
    _mgr: IMFDXGIDeviceManager,
    name: String,
    width: u32,
    height: u32,
    wait_idr: bool,
    discontinuity: bool,
    next_pts: u64,
    stats: DecoderStats,
}

fn subtype(codec: Codec) -> windows::core::GUID {
    match codec {
        Codec::H264 => MFVideoFormat_H264,
        Codec::Hevc => MFVideoFormat_HEVC,
    }
}

impl VideoDecoder {
    /// Crée un décodeur avec son propre device D3D11 (adaptateur NVIDIA préféré).
    /// Pour afficher les images avec un [`crate::PresenterHandle`], utiliser [`VideoDecoder::with_device`].
    pub fn new(codec: Codec) -> Result<Self> {
        let device = gpu::create_device()?;
        Self::with_device(&device, codec)
    }

    /// Crée un décodeur sur le device D3D11 donné (doit avoir `D3D11_CREATE_DEVICE_VIDEO_SUPPORT`
    /// et la protection multithread ; voir [`crate::create_device`]).
    pub fn with_device(device: &ID3D11Device, codec: Codec) -> Result<Self> {
        gpu::init_com();
        mf_startup()?;
        let (mft, name) = create_mft(codec)?;

        let attrs = unsafe { mft.GetAttributes() }.ctx("IMFTransform::GetAttributes")?;
        let aware = unsafe { attrs.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) != 0;
        if !aware {
            return Err(RenderError::Other(format!("MFT '{name}' is not D3D11 aware")));
        }
        let _ = unsafe { attrs.SetUINT32(&MF_LOW_LATENCY, 1) };
        if let Ok(api) = mft.cast::<ICodecAPI>() {
            let _ = unsafe { api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true)) };
        }

        let mut token = 0u32;
        let mut mgr: Option<IMFDXGIDeviceManager> = None;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut mgr) }.ctx("MFCreateDXGIDeviceManager")?;
        let mgr = mgr.ok_or_else(|| RenderError::Other("no IMFDXGIDeviceManager".into()))?;
        unsafe { mgr.ResetDevice(device, token) }.ctx("IMFDXGIDeviceManager::ResetDevice")?;
        unsafe { mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize) }
            .ctx("MFT_MESSAGE_SET_D3D_MANAGER")?;

        let mut dec = VideoDecoder {
            query: None,
            codec,
            device: device.clone(),
            mft,
            _mgr: mgr,
            name,
            width: 0,
            height: 0,
            wait_idr: true,
            discontinuity: true,
            next_pts: 0,
            stats: DecoderStats::default(),
        };
        dec.configure()?;
        Ok(dec)
    }

    fn configure(&mut self) -> Result<()> {
        // Type d'entrée : codec seul ; la taille vient du SPS (MF_E_TRANSFORM_STREAM_CHANGE).
        let t = unsafe { MFCreateMediaType() }.ctx("MFCreateMediaType")?;
        unsafe {
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ctx("MAJOR_TYPE")?;
            t.SetGUID(&MF_MT_SUBTYPE, &subtype(self.codec)).ctx("SUBTYPE")?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).ctx("INTERLACE")?;
            // Taille indicative : le MFT HEVC refuse un type de sortie sans MF_MT_FRAME_SIZE.
            // La vraie taille arrive avec le SPS (MF_E_TRANSFORM_STREAM_CHANGE).
            t.SetUINT64(&MF_MT_FRAME_SIZE, (1920u64 << 32) | 1080).ctx("FRAME_SIZE")?;
            self.mft.SetInputType(0, &t, 0).ctx("SetInputType")?;
        }
        // Le type de sortie NV12 peut ne pas être disponible avant le premier SPS : on réessaiera.
        let r = self.negotiate_output();
        if std::env::var("GRX_DEC_DEBUG").is_ok() {
            eprintln!("initial negotiate_output: {r:?}");
            for i in 0..10 {
                match unsafe { self.mft.GetOutputAvailableType(0, i) } {
                    Ok(t) => eprintln!("  out type {i}: {:?}", unsafe { t.GetGUID(&MF_MT_SUBTYPE) }),
                    Err(e) => { eprintln!("  out type {i}: err {e}"); break }
                }
            }
        }
        // Certains MFT (HEVC) refusent ces messages tant que le type de sortie est inconnu ;
        // ils démarrent alors tout seuls au premier ProcessInput.
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }
        Ok(())
    }

    /// Sélectionne NV12 comme type de sortie et lit la taille visible.
    fn negotiate_output(&mut self) -> Result<()> {
        for i in 0.. {
            let t = match unsafe { self.mft.GetOutputAvailableType(0, i) } {
                Ok(t) => t,
                Err(e) => return Err(RenderError::Win { context: "GetOutputAvailableType(NV12)", source: e }),
            };
            if unsafe { t.GetGUID(&MF_MT_SUBTYPE) }.ok() != Some(MFVideoFormat_NV12) {
                continue;
            }
            if unsafe { t.GetUINT64(&MF_MT_FRAME_SIZE) }.is_err() {
                unsafe { t.SetUINT64(&MF_MT_FRAME_SIZE, (1920u64 << 32) | 1080) }.ctx("FRAME_SIZE(out)")?;
            }
            unsafe { self.mft.SetOutputType(0, &t, 0) }.ctx("SetOutputType(NV12)")?;
            self.read_output_size();
            return Ok(());
        }
        unreachable!()
    }

    fn read_output_size(&mut self) {
        if let Ok(t) = unsafe { self.mft.GetOutputCurrentType(0) } {
            if let Ok(v) = unsafe { t.GetUINT64(&MF_MT_FRAME_SIZE) } {
                self.width = (v >> 32) as u32;
                self.height = v as u32;
            }
            // Ouverture d'affichage (recadrage) si le MFT en fournit une plus petite.
            let mut area = MFVideoArea::default();
            let sz = std::mem::size_of::<MFVideoArea>() as u32;
            let r = unsafe {
                t.GetBlob(
                    &MF_MT_MINIMUM_DISPLAY_APERTURE,
                    std::slice::from_raw_parts_mut(&mut area as *mut _ as *mut u8, sz as usize),
                    None,
                )
            };
            if r.is_ok() && area.Area.cx > 0 && area.Area.cy > 0 {
                self.width = self.width.min(area.Area.cx as u32);
                self.height = self.height.min(area.Area.cy as u32);
            }
        }
    }

    /// Nom du MFT utilisé.
    pub fn backend_name(&self) -> &str {
        &self.name
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Device D3D11 sur lequel vivent les textures décodées.
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// Taille visible du dernier flux décodé (0x0 avant la première image).
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn stats(&self) -> DecoderStats {
        self.stats
    }

    /// `true` tant qu'un IDR n'a pas été reçu (au démarrage et après [`VideoDecoder::reset`]) :
    /// les paquets non-IDR sont ignorés. Le client doit alors demander un IDR au host.
    pub fn needs_keyframe(&self) -> bool {
        self.wait_idr
    }

    /// Décode une access unit Annex-B. Renvoie la dernière image sortie (`None` si le MFT n'a
    /// encore rien produit, ou si on attend un IDR). Les horodatages sont synthétiques ;
    /// utiliser [`VideoDecoder::decode_pts`] / [`VideoDecoder::decode_frame`] pour les conserver.
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
        self.stats.packets_in += 1;
        if self.wait_idr {
            if !annexb::contains_random_access(self.codec, data) {
                self.stats.skipped_waiting_idr += 1;
                return Ok(None);
            }
            self.wait_idr = false;
        }
        let sample = unsafe { MFCreateSample() }.ctx("MFCreateSample")?;
        unsafe {
            let buf = MFCreateMemoryBuffer(data.len() as u32).ctx("MFCreateMemoryBuffer")?;
            let mut p: *mut u8 = std::ptr::null_mut();
            buf.Lock(&mut p, None, None).ctx("IMFMediaBuffer::Lock")?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            buf.Unlock().ctx("IMFMediaBuffer::Unlock")?;
            buf.SetCurrentLength(data.len() as u32).ctx("SetCurrentLength")?;
            sample.AddBuffer(&buf).ctx("AddBuffer")?;
            sample.SetSampleTime((pts_us * 10) as i64).ctx("SetSampleTime")?;
            sample.SetSampleDuration(166_670).ok();
            if self.discontinuity {
                let _ = sample.SetUINT32(&MFSampleExtension_Discontinuity, 1);
                self.discontinuity = false;
            }
        }

        let mut latest: Option<DecodedFrame> = None;
        let mut retried = false;
        loop {
            match unsafe { self.mft.ProcessInput(0, &sample, 0) } {
                Ok(()) => break,
                Err(e) if e.code() == MF_E_NOTACCEPTING && !retried => {
                    // Sorties en attente : on les draine puis on réessaie.
                    retried = true;
                    self.drain_into(&mut latest)?;
                }
                Err(e) => return Err(RenderError::Win { context: "ProcessInput", source: e }),
            }
        }
        self.drain_into(&mut latest)?;
        if latest.is_some() {
            self.stats.frames_out += 1;
        }
        Ok(latest)
    }

    /// Récupère toutes les sorties disponibles ; garde la plus récente.
    fn drain_into(&mut self, latest: &mut Option<DecodedFrame>) -> Result<()> {
        loop {
            let mut buf = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            };
            let mut status = 0u32;
            let r = unsafe { self.mft.ProcessOutput(0, std::slice::from_mut(&mut buf), &mut status) };
            let sample = unsafe { std::mem::ManuallyDrop::take(&mut buf.pSample) };
            let _events = unsafe { std::mem::ManuallyDrop::take(&mut buf.pEvents) };
            match r {
                Ok(()) => {
                    if let Some(s) = sample {
                        *latest = Some(self.make_frame(s)?);
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.negotiate_output()?;
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(RenderError::Win { context: "ProcessOutput", source: e }),
            }
        }
    }

    fn make_frame(&mut self, sample: IMFSample) -> Result<DecodedFrame> {
        let pts = (unsafe { sample.GetSampleTime() }.unwrap_or(0).max(0) / 10) as u64;
        let buf = unsafe { sample.GetBufferByIndex(0) }.ctx("GetBufferByIndex")?;
        let dxgi: IMFDXGIBuffer = buf.cast().ctx("IMFDXGIBuffer (decoder is not producing GPU surfaces)")?;
        let mut raw: *mut std::ffi::c_void = std::ptr::null_mut();
        unsafe { dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw) }.ctx("IMFDXGIBuffer::GetResource")?;
        let texture = unsafe { ID3D11Texture2D::from_raw(raw) };
        let subresource = unsafe { dxgi.GetSubresourceIndex() }.ctx("GetSubresourceIndex")?;
        if self.width == 0 {
            self.read_output_size();
        }
        Ok(DecodedFrame { texture, subresource, width: self.width, height: self.height, pts, _sample: sample })
    }

    /// Bloque jusqu'à ce que le GPU ait fini toutes les commandes soumises sur le device
    /// (diagnostic : mesurer le temps de décodage réel et non celui de l'appel).
    pub fn sync_gpu(&mut self) -> Result<()> {
        if self.query.is_none() {
            let mut q = None;
            unsafe { self.device.CreateQuery(&D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 }, Some(&mut q)) }
                .ctx("CreateQuery(EVENT)")?;
            self.query = q;
        }
        let q = self.query.as_ref().unwrap();
        let ctx = unsafe { self.device.GetImmediateContext() }.ctx("GetImmediateContext")?;
        unsafe {
            ctx.End(q);
            ctx.Flush();
            let mut done = 0i32;
            let t0 = std::time::Instant::now();
            loop {
                // S_FALSE (1) tant que le GPU n'a pas fini ; windows-rs confond S_OK et S_FALSE dans Result.
                let hr = (Interface::vtable(&ctx).GetData)(
                    Interface::as_raw(&ctx),
                    Interface::as_raw(q),
                    &mut done as *mut i32 as *mut _,
                    4,
                    0,
                );
                if hr.0 == 0 || hr.is_err() {
                    break;
                }
                if t0.elapsed() > std::time::Duration::from_secs(1) {
                    return Err(RenderError::Timeout);
                }
                std::hint::spin_loop();
            }
        }
        Ok(())
    }

    /// Vide le décodeur et renvoie une éventuelle image restante (inutile en mode low latency,
    /// fourni pour la complétude). Le décodeur attend ensuite un IDR (comme après `reset()`).
    pub fn flush(&mut self) -> Result<Option<DecodedFrame>> {
        unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }.ctx("COMMAND_DRAIN")?;
        let mut latest = None;
        self.drain_into(&mut latest)?;
        // Après un drain le MFT perd ses références : on repart d'un IDR, comme après reset().
        self.reset()?;
        Ok(latest)
    }

    /// Repart de zéro après une perte de paquets : le MFT est vidé et ignore les paquets
    /// jusqu'au prochain IDR (voir [`VideoDecoder::needs_keyframe`]).
    pub fn reset(&mut self) -> Result<()> {
        unsafe {
            self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0).ctx("COMMAND_FLUSH")?;
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).ctx("BEGIN_STREAMING")?;
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).ctx("START_OF_STREAM")?;
        }
        self.wait_idr = true;
        self.discontinuity = true;
        Ok(())
    }
}

// SAFETY : device D3D11 protégé multithread, COM en MTA ; le décodeur n'est utilisé que via &mut self.
unsafe impl Send for VideoDecoder {}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
        }
    }
}

/// Décodeurs candidats, matériels d'abord (les MFT synchrones DXVA de Microsoft sont classés
/// « logiciels » par MF mais décodent sur le GPU quand le device manager leur est fourni).
fn create_mft(codec: Codec) -> Result<(IMFTransform, String)> {
    let info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype(codec) };
    let mut pp: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG(MFT_ENUM_FLAG_ALL.0 | MFT_ENUM_FLAG_SORTANDFILTER.0),
            Some(&info),
            None,
            &mut pp,
            &mut count,
        )
    }
    .ctx("MFTEnumEx")?;
    if pp.is_null() || count == 0 {
        return Err(RenderError::NoDecoder(codec));
    }
    let mut cands: Vec<(IMFActivate, String)> = Vec::new();
    for i in 0..count as usize {
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
    let mut last_err = None;
    for (a, n) in cands {
        match unsafe { a.ActivateObject::<IMFTransform>() } {
            Ok(t) => {
                // Les MFT asynchrones ne sont pas pilotés ici.
                let is_async = unsafe { t.GetAttributes() }
                    .ok()
                    .and_then(|at| unsafe { at.GetUINT32(&MF_TRANSFORM_ASYNC) }.ok())
                    .unwrap_or(0)
                    != 0;
                if is_async {
                    continue;
                }
                return Ok((t, n));
            }
            Err(e) => last_err = Some(e),
        }
    }
    match last_err {
        Some(e) => Err(RenderError::Win { context: "IMFActivate::ActivateObject", source: e }),
        None => Err(RenderError::NoDecoder(codec)),
    }
}
