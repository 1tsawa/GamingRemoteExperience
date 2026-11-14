//! Encodeur NVENC direct (`nvEncodeAPI64.dll` du pilote NVIDIA, chargée dynamiquement depuis
//! System32) : H.264 High 4:4:4 et HEVC RExt 4:4:4 (8/10 bits), que le MFT NVIDIA n'expose pas.
//!
//! Entrée : la texture BGRA de la capture elle-même (`NV_ENC_BUFFER_FORMAT_ARGB`, zéro copie),
//! enregistrée une fois auprès de NVENC ; NVENC convertit en YUV 4:4:4 selon la VUI (BT.709 plage
//! limitée : valeurs décodées identiques à la référence CPU, voir le test `grx-render`
//! `hevc_444_colors_match_reference`). Mesuré : les textures YUV 4:4:4 préparées par shader
//! (planaires R8/R16 3 x H : `NV_ENC_ERR_UNSUPPORTED_PARAM` ; AYUV : lu comme du RGB) ne
//! conviennent pas en D3D11, et le 10 bits 4:4:4 n'est pas proposé : 4:4:4 = 8 bits.
//! Mode synchrone (`enableEncodeAsync = 0`) : `nvEncEncodePicture` puis `nvEncLockBitstream`
//! bloquant, une image en entrée = une access unit en sortie (pas d'images B, GOP infini, IDR à la
//! demande). Préréglage P1 + `NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY`, CBR, VBV ~ 1,5 image.
//!
//! ABI : `nvenc_abi.rs` (offsets mesurés sur nv-codec-headers 12.1, MIT) ; version minimale du
//! pilote : API 12.1 (pilotes 531+).

use std::ffi::{c_char, c_int, c_void};
use std::sync::OnceLock;
use std::time::Instant;

use windows::core::{Interface, GUID, PCSTR};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};

use crate::blob::Blob;
use crate::error::{Result, VideoError};
use crate::nvenc_abi::*;
use crate::{nal, Codec, EncodedFrame, EncoderConfig};

type Status = c_int;
type FnPtr = unsafe extern "system" fn() -> isize;

struct Lib {
    create_instance: unsafe extern "system" fn(*mut c_void) -> Status,
    max_version: u32,
}

static LIB: OnceLock<std::result::Result<Lib, String>> = OnceLock::new();

fn lib() -> std::result::Result<&'static Lib, String> {
    LIB.get_or_init(|| unsafe {
        let m = LoadLibraryExW(windows::core::w!("nvEncodeAPI64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
            .map_err(|e| format!("nvEncodeAPI64.dll : {e} (pas de pilote NVIDIA ?)"))?;
        let sym = |n: &[u8]| GetProcAddress(m, PCSTR(n.as_ptr())).ok_or_else(|| "symbole NVENC absent".to_string());
        let get_max: unsafe extern "system" fn(*mut u32) -> Status =
            std::mem::transmute::<FnPtr, _>(sym(b"NvEncodeAPIGetMaxSupportedVersion\0")?);
        let mut v = 0u32;
        if get_max(&mut v) != 0 {
            return Err("NvEncodeAPIGetMaxSupportedVersion a échoué".into());
        }
        // v = (majeur << 4) | mineur ; 12.1 requis (structures de nvenc_abi.rs).
        if v < (12 << 4) | 1 {
            return Err(format!("pilote NVIDIA trop ancien (API NVENC {}.{}, 12.1 requise)", v >> 4, v & 0xF));
        }
        Ok(Lib { create_instance: std::mem::transmute::<FnPtr, _>(sym(b"NvEncodeAPICreateInstance\0")?), max_version: v })
    })
    .as_ref()
    .map_err(|e| e.clone())
}

/// API NVENC utilisable ? `Ok("API 13.0")` ou la raison.
pub fn availability() -> std::result::Result<String, String> {
    lib().map(|l| format!("API NVENC {}.{}", l.max_version >> 4, l.max_version & 0xF))
}

/// Table de fonctions + session d'encodage ouverte sur un device D3D11.
struct Session {
    fns: Blob,
    enc: *mut c_void,
}

macro_rules! nvfn {
    ($s:expr, $off:expr, $ty:ty) => {
        // SAFETY : pointeur de fonction de la table NVENC remplie par NvEncodeAPICreateInstance,
        // signature recopiée de nvEncodeAPI.h 12.1.
        unsafe { std::mem::transmute::<*mut c_void, $ty>($s.fns.get_ptr($off)) }
    };
}

fn codec_guid(c: Codec) -> GUID {
    match c {
        Codec::Hevc => NV_ENC_CODEC_HEVC_GUID,
        _ => NV_ENC_CODEC_H264_GUID,
    }
}

impl Session {
    fn open(device: &ID3D11Device) -> std::result::Result<Session, String> {
        let l = lib()?;
        let mut fns = Blob::new(SIZE_NV_ENCODE_API_FUNCTION_LIST);
        fns.put_i32(0, NV_ENCODE_API_FUNCTION_LIST_VER as i32);
        let r = unsafe { (l.create_instance)(fns.ptr()) };
        if r != 0 {
            return Err(format!("NvEncodeAPICreateInstance : {r}"));
        }
        let mut p = Blob::new(SIZE_NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS);
        p.put_i32(0, NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER as i32);
        p.put_i32(OFF_OPEN_DEVICE_TYPE, NV_ENC_DEVICE_TYPE_DIRECTX as i32);
        p.put_ptr(OFF_OPEN_DEVICE, device.as_raw());
        p.put_i32(OFF_OPEN_API_VERSION, NVENCAPI_VERSION as i32);
        let mut s = Session { fns, enc: std::ptr::null_mut() };
        let open = nvfn!(s, OFF_FN_OPEN_SESSION_EX, unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> Status);
        let mut enc = std::ptr::null_mut();
        let r = unsafe { open(p.ptr(), &mut enc) };
        if r != 0 || enc.is_null() {
            return Err(format!("nvEncOpenEncodeSessionEx : {r}"));
        }
        s.enc = enc;
        Ok(s)
    }

    fn last_error(&mut self, what: &str, st: Status) -> String {
        let f = nvfn!(self, OFF_FN_LAST_ERROR, unsafe extern "system" fn(*mut c_void) -> *const c_char);
        let msg = unsafe {
            let p = f(self.enc);
            if p.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        format!("{what} : NVENCSTATUS {st} {msg}")
    }

    fn cap(&mut self, codec: Codec, cap: u32) -> i32 {
        let f = nvfn!(self, OFF_FN_GET_CAPS, unsafe extern "system" fn(*mut c_void, GUID, *mut c_void, *mut c_int) -> Status);
        let mut p = Blob::new(SIZE_NV_ENC_CAPS_PARAM);
        p.put_i32(0, NV_ENC_CAPS_PARAM_VER as i32);
        p.put_i32(OFF_CAPS_QUERY, cap as i32);
        let mut v: c_int = 0;
        if unsafe { f(self.enc, codec_guid(codec), p.ptr(), &mut v) } != 0 {
            return 0;
        }
        v
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.enc.is_null() {
            let f = nvfn!(self, OFF_FN_DESTROY_ENCODER, unsafe extern "system" fn(*mut c_void) -> Status);
            unsafe { f(self.enc) };
        }
    }
}

/// Formats 4:4:4 encodables par NVENC sur ce device : `(codec, 10 bits possible)`.
/// Résultat mis en cache par device (pointeur brut).
pub fn yuv444_caps(device: &ID3D11Device) -> Vec<(Codec, bool)> {
    static CACHE: std::sync::Mutex<Vec<(usize, Vec<(Codec, bool)>)>> = std::sync::Mutex::new(Vec::new());
    let key = device.as_raw() as usize;
    if let Some((_, v)) = CACHE.lock().unwrap().iter().find(|(k, _)| *k == key) {
        return v.clone();
    }
    let v = match Session::open(device) {
        Ok(mut s) => {
            let mut v = Vec::new();
            // HEVC seulement : en H.264 4:4:4, le contrôle de débit n'est pas respecté sur un
            // motif difficile (mesuré : ~180 Mb/s pour 15 demandés), le HEVC l'est.
            for c in [Codec::Hevc] {
                if s.cap(c, NV_ENC_CAPS_SUPPORT_YUV444_ENCODE) != 0 {
                    // 10 bits 4:4:4 : refusé en D3D11 (voir NvencEncoder::new).
                    v.push((c, false));
                }
            }
            v
        }
        Err(_) => Vec::new(),
    };
    CACHE.lock().unwrap().push((key, v.clone()));
    v
}

/// Valeurs VUI (primaries, transfer, matrix) : BT.709 SDR ou BT.2020/PQ (HDR10).
fn vui_codes(hdr: bool) -> (u32, u32, u32) {
    if hdr {
        (9, 16, 9)
    } else {
        (1, 1, 1)
    }
}

pub(crate) struct NvencEncoder {
    s: Session,
    cfg: EncoderConfig,
    device: ID3D11Device,
    init: Blob,
    config: Blob,
    bitstream: *mut c_void,
    // La texture est gardée en vie : son pointeur (clé du cache) ne peut pas être réutilisé.
    registered: Vec<(usize, ID3D11Texture2D, *mut c_void)>,
    buffer_fmt: u32,
    frame_idx: u32,
    force_idr: bool,
    start: Instant,
    pub name: String,
}

// SAFETY : session NVENC et device D3D11 (protégé multithread) utilisés via &mut self uniquement.
unsafe impl Send for NvencEncoder {}

impl NvencEncoder {
    pub fn new(device: &ID3D11Device, cfg: EncoderConfig) -> Result<NvencEncoder> {
        let err = |m: String| VideoError::Other(format!("NVENC : {m}"));
        if !cfg.format.is_444() {
            return Err(err("seul le 4:4:4 passe par l'API NVENC directe".into()));
        }
        if !matches!(cfg.codec, Codec::H264 | Codec::Hevc) {
            return Err(err(format!("{:?} 4:4:4 non supporté", cfg.codec)));
        }
        let ten = cfg.format.is_10bit();
        if ten {
            // Mesuré (RTX 4070 Ti, pilote 621) : aucune texture D3D11 planaire 16 bits n'est
            // acceptée par nvEncRegisterResource (NV_ENC_ERR_UNSUPPORTED_PARAM) ; 4:4:4 = 8 bits.
            return Err(err("4:4:4 10 bits non supporté via D3D11".into()));
        }
        let mut s = Session::open(device).map_err(err)?;
        if s.cap(cfg.codec, NV_ENC_CAPS_SUPPORT_YUV444_ENCODE) == 0 {
            return Err(err(format!("ce GPU n'encode pas {:?} 4:4:4", cfg.codec)));
        }
        if ten && s.cap(cfg.codec, NV_ENC_CAPS_SUPPORT_10BIT_ENCODE) == 0 {
            return Err(err("ce GPU n'encode pas en 10 bits".into()));
        }
        let guid = codec_guid(cfg.codec);
        let preset = NV_ENC_PRESET_P1_GUID;
        // Configuration par défaut du préréglage (ultra faible latence), puis nos réglages.
        let mut pc = Blob::new(SIZE_NV_ENC_PRESET_CONFIG);
        pc.put_i32(0, NV_ENC_PRESET_CONFIG_VER as i32);
        pc.put_i32(OFF_PRESET_CFG, NV_ENC_CONFIG_VER as i32);
        let f = nvfn!(s, OFF_FN_PRESET_CONFIG_EX, unsafe extern "system" fn(*mut c_void, GUID, GUID, u32, *mut c_void) -> Status);
        let r = unsafe { f(s.enc, guid, preset, NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY, pc.ptr()) };
        if r != 0 {
            return Err(err(s.last_error("nvEncGetEncodePresetConfigEx", r)));
        }
        let mut config = Blob::new(SIZE_NV_ENC_CONFIG);
        let raw = pc.bytes()[OFF_PRESET_CFG..OFF_PRESET_CFG + SIZE_NV_ENC_CONFIG].to_vec();
        config.copy_from(0, &raw);
        config.put_i32(0, NV_ENC_CONFIG_VER as i32);
        let profile = match cfg.codec {
            Codec::Hevc => NV_ENC_HEVC_PROFILE_FREXT_GUID,
            _ => NV_ENC_H264_PROFILE_HIGH_444_GUID,
        };
        config.put_guid(OFF_CFG_PROFILE, &profile);
        config.put_i32(OFF_CFG_GOP, NVENC_INFINITE_GOPLENGTH as i32);
        config.put_i32(OFF_CFG_FRAME_INTERVAL_P, 1);
        let mut enc = NvencEncoder {
            s,
            device: device.clone(),
            init: Blob::new(SIZE_NV_ENC_INITIALIZE_PARAMS),
            config,
            bitstream: std::ptr::null_mut(),
            registered: Vec::new(),
            buffer_fmt: NV_ENC_BUFFER_FORMAT_ARGB,
            frame_idx: 0,
            force_idr: true,
            start: Instant::now(),
            name: format!(
                "NVENC {} 4:4:4 {} bits",
                if cfg.codec == Codec::Hevc { "HEVC RExt" } else { "H.264 High 4:4:4" },
                if ten { 10 } else { 8 }
            ),
            cfg,
        };
        enc.write_rc();
        let c = OFF_CFG_CODEC;
        let (prim, trc, mat) = vui_codes(enc.cfg.hdr);
        let vui = match enc.cfg.codec {
            Codec::Hevc => {
                let (w, b) = BIT_HEVC_CHROMA;
                enc.config.put_bits(c + w, b, 2, 3);
                let (w, b) = BIT_HEVC_DEPTH;
                enc.config.put_bits(c + w, b, 3, if ten { 2 } else { 0 });
                let (w, b) = BIT_HEVC_REPEAT_SPSPPS;
                enc.config.put_bits(c + w, b, 1, 1);
                enc.config.put_i32(c + OFF_HEVC_IDR_PERIOD, NVENC_INFINITE_GOPLENGTH as i32);
                c + OFF_HEVC_VUI
            }
            _ => {
                enc.config.put_i32(c + OFF_H264_CHROMA, 3);
                let (w, b) = BIT_H264_REPEAT_SPSPPS;
                enc.config.put_bits(c + w, b, 1, 1);
                enc.config.put_i32(c + OFF_H264_IDR_PERIOD, NVENC_INFINITE_GOPLENGTH as i32);
                c + OFF_H264_VUI
            }
        };
        for (off, v) in [
            (OFF_VUI_SIGNAL_PRESENT, 1),
            (OFF_VUI_FORMAT, 5),
            (OFF_VUI_FULL_RANGE, 0),
            (OFF_VUI_COLOUR_PRESENT, 1),
            (OFF_VUI_PRIMARIES, prim),
            (OFF_VUI_TRANSFER, trc),
            (OFF_VUI_MATRIX, mat),
        ] {
            enc.config.put_i32(vui + off, v as i32);
        }
        let (w, h, fps) = (enc.cfg.width, enc.cfg.height, enc.cfg.fps.max(1));
        let cfg_ptr = enc.config.ptr();
        let i = &mut enc.init;
        i.put_i32(0, NV_ENC_INITIALIZE_PARAMS_VER as i32);
        i.put_guid(OFF_INIT_ENCODE_GUID, &guid);
        i.put_guid(OFF_INIT_PRESET_GUID, &preset);
        for (off, v) in [
            (OFF_INIT_WIDTH, w),
            (OFF_INIT_HEIGHT, h),
            (OFF_INIT_DAR_W, w),
            (OFF_INIT_DAR_H, h),
            (OFF_INIT_FPS_NUM, fps),
            (OFF_INIT_FPS_DEN, 1),
            (OFF_INIT_ASYNC, 0),
            (OFF_INIT_PTD, 1),
            (OFF_INIT_MAX_W, w),
            (OFF_INIT_MAX_H, h),
            (OFF_INIT_TUNING, NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY),
        ] {
            i.put_i32(off, v as i32);
        }
        i.put_ptr(OFF_INIT_CONFIG, cfg_ptr);
        let f = nvfn!(enc.s, OFF_FN_INITIALIZE, unsafe extern "system" fn(*mut c_void, *mut c_void) -> Status);
        let r = unsafe { f(enc.s.enc, enc.init.ptr()) };
        if r != 0 {
            return Err(err(enc.s.last_error("nvEncInitializeEncoder", r)));
        }
        let mut bs = Blob::new(SIZE_NV_ENC_CREATE_BITSTREAM_BUFFER);
        bs.put_i32(0, NV_ENC_CREATE_BITSTREAM_BUFFER_VER as i32);
        let f = nvfn!(enc.s, OFF_FN_CREATE_BITSTREAM, unsafe extern "system" fn(*mut c_void, *mut c_void) -> Status);
        let r = unsafe { f(enc.s.enc, bs.ptr()) };
        if r != 0 {
            return Err(err(enc.s.last_error("nvEncCreateBitstreamBuffer", r)));
        }
        enc.bitstream = bs.get_ptr(OFF_BS_BUFFER);
        Ok(enc)
    }

    fn write_rc(&mut self) {
        let rc = OFF_CFG_RC;
        let bps = self.cfg.bitrate_kbps.saturating_mul(1000);
        let vbv = (bps as u64 * 3 / 2 / self.cfg.fps.max(1) as u64) as u32;
        self.config.put_i32(rc + OFF_RC_MODE, NV_ENC_PARAMS_RC_CBR as i32);
        self.config.put_i32(rc + OFF_RC_AVG, bps as i32);
        self.config.put_i32(rc + OFF_RC_MAX, bps as i32);
        self.config.put_i32(rc + OFF_RC_VBV, vbv as i32);
        self.config.put_i32(rc + OFF_RC_VBV_INIT, vbv as i32);
        let (w, b) = BIT_RC_ZERO_REORDER;
        self.config.put_bits(rc + w, b, 1, 1);
        let (w, b) = BIT_RC_LOOKAHEAD;
        self.config.put_bits(rc + w, b, 1, 0);
    }

    pub fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    pub fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        self.cfg.bitrate_kbps = kbps;
        self.write_rc();
        let mut r = Blob::new(SIZE_NV_ENC_RECONFIGURE_PARAMS);
        r.put_i32(0, NV_ENC_RECONFIGURE_PARAMS_VER as i32);
        let init = self.init.bytes()[..SIZE_NV_ENC_INITIALIZE_PARAMS].to_vec();
        r.copy_from(OFF_RECONF_INIT, &init);
        let f = nvfn!(self.s, OFF_FN_RECONFIGURE, unsafe extern "system" fn(*mut c_void, *mut c_void) -> Status);
        let st = unsafe { f(self.s.enc, r.ptr()) };
        if st != 0 {
            return Err(VideoError::Other(self.s.last_error("nvEncReconfigureEncoder", st)));
        }
        Ok(())
    }

    fn register(&mut self, tex: &ID3D11Texture2D) -> Result<*mut c_void> {
        let key = tex.as_raw() as usize;
        if let Some((_, _, r)) = self.registered.iter().find(|(k, _, _)| *k == key) {
            return Ok(*r);
        }
        let mut p = Blob::new(SIZE_NV_ENC_REGISTER_RESOURCE);
        p.put_i32(0, NV_ENC_REGISTER_RESOURCE_VER as i32);
        p.put_i32(OFF_REG_TYPE, NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX as i32);
        p.put_i32(OFF_REG_WIDTH, self.cfg.width as i32);
        p.put_i32(OFF_REG_HEIGHT, self.cfg.height as i32);
        p.put_i32(OFF_REG_PITCH, 0);
        p.put_i32(OFF_REG_SUB, 0);
        p.put_ptr(OFF_REG_RESOURCE, tex.as_raw());
        p.put_i32(OFF_REG_FORMAT, self.buffer_fmt as i32);
        p.put_i32(OFF_REG_USAGE, NV_ENC_INPUT_IMAGE as i32);
        let f = nvfn!(self.s, OFF_FN_REGISTER_RESOURCE, unsafe extern "system" fn(*mut c_void, *mut c_void) -> Status);
        let st = unsafe { f(self.s.enc, p.ptr()) };
        if st != 0 {
            return Err(VideoError::Other(self.s.last_error("nvEncRegisterResource", st)));
        }
        let r = p.get_ptr(OFF_REG_REGISTERED);
        if self.registered.len() >= 8 {
            // Textures éphémères (tests, mire) : on libère la plus ancienne.
            let (_, _, old) = self.registered.remove(0);
            let unreg = nvfn!(self.s, OFF_FN_UNREGISTER_RESOURCE, unsafe extern "system" fn(*mut c_void, *mut c_void) -> Status);
            unsafe { unreg(self.s.enc, old) };
        }
        self.registered.push((key, tex.clone(), r));
        Ok(r)
    }

    /// Encode une texture de capture (BGRA SDR, ou R10G10B10A2 PQ/BT.2020 si `cfg.hdr`).
    pub fn encode_texture(&mut self, tex: &ID3D11Texture2D) -> Result<EncodedFrame> {
        let dev = unsafe { tex.GetDevice() }.map_err(|e| VideoError::Win { context: "GetDevice", source: e })?;
        if dev.as_raw() != self.device.as_raw() {
            return Err(VideoError::Other("texture d'un autre device D3D11".into()));
        }
        let mut sd = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut sd) };
        if sd.Format != windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(VideoError::Other(format!("NVENC 4:4:4 : entrée BGRA attendue (format {})", sd.Format.0)));
        }
        let reg = self.register(tex)?;
        let mut map = Blob::new(SIZE_NV_ENC_MAP_INPUT_RESOURCE);
        map.put_i32(0, NV_ENC_MAP_INPUT_RESOURCE_VER as i32);
        map.put_ptr(OFF_MAP_REGISTERED, reg);
        let f = nvfn!(self.s, OFF_FN_MAP_INPUT, unsafe extern "system" fn(*mut c_void, *mut c_void) -> Status);
        let st = unsafe { f(self.s.enc, map.ptr()) };
        if st != 0 {
            return Err(VideoError::Other(self.s.last_error("nvEncMapInputResource", st)));
        }
        let mapped = map.get_ptr(OFF_MAP_MAPPED);
        let res = self.encode_mapped(mapped);
        let f = nvfn!(self.s, OFF_FN_UNMAP_INPUT, unsafe extern "system" fn(*mut c_void, *mut c_void) -> Status);
        unsafe { f(self.s.enc, mapped) };
        res
    }

    fn encode_mapped(&mut self, mapped: *mut c_void) -> Result<EncodedFrame> {
        let pts = self.start.elapsed().as_micros() as u64;
        let idr = std::mem::take(&mut self.force_idr);
        let mut pic = Blob::new(SIZE_NV_ENC_PIC_PARAMS);
        pic.put_i32(0, NV_ENC_PIC_PARAMS_VER as i32);
        pic.put_i32(OFF_PIC_WIDTH, self.cfg.width as i32);
        pic.put_i32(OFF_PIC_HEIGHT, self.cfg.height as i32);
        pic.put_i32(OFF_PIC_PITCH, self.cfg.width as i32);
        let flags = if idr { NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS } else { 0 };
        pic.put_i32(OFF_PIC_FLAGS, flags as i32);
        pic.put_i32(OFF_PIC_FRAME_IDX, self.frame_idx as i32);
        pic.put_u64(OFF_PIC_TIMESTAMP, pts);
        pic.put_ptr(OFF_PIC_INPUT, mapped);
        pic.put_ptr(OFF_PIC_OUTPUT, self.bitstream);
        pic.put_i32(OFF_PIC_FORMAT, self.buffer_fmt as i32);
        pic.put_i32(OFF_PIC_STRUCT, NV_ENC_PIC_STRUCT_FRAME as i32);
        self.frame_idx = self.frame_idx.wrapping_add(1);
        let f = nvfn!(self.s, OFF_FN_ENCODE_PICTURE, unsafe extern "system" fn(*mut c_void, *mut c_void) -> Status);
        let st = unsafe { f(self.s.enc, pic.ptr()) };
        if st != 0 {
            if idr {
                self.force_idr = true;
            }
            return Err(VideoError::Other(self.s.last_error("nvEncEncodePicture", st)));
        }
        let mut lock = Blob::new(SIZE_NV_ENC_LOCK_BITSTREAM);
        lock.put_i32(0, NV_ENC_LOCK_BITSTREAM_VER as i32);
        lock.put_ptr(OFF_LOCK_OUTPUT, self.bitstream);
        let f = nvfn!(self.s, OFF_FN_LOCK_BITSTREAM, unsafe extern "system" fn(*mut c_void, *mut c_void) -> Status);
        let st = unsafe { f(self.s.enc, lock.ptr()) };
        if st != 0 {
            return Err(VideoError::Other(self.s.last_error("nvEncLockBitstream", st)));
        }
        let size = lock.get_u32(OFF_LOCK_SIZE) as usize;
        let ptr = lock.get_ptr(OFF_LOCK_PTR) as *const u8;
        let ptype = lock.get_u32(OFF_LOCK_PIC_TYPE);
        let data = if ptr.is_null() { Vec::new() } else { unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec() };
        let f = nvfn!(self.s, OFF_FN_UNLOCK_BITSTREAM, unsafe extern "system" fn(*mut c_void, *mut c_void) -> Status);
        unsafe { f(self.s.enc, self.bitstream) };
        let is_key = ptype == NV_ENC_PIC_TYPE_IDR || nal::contains_idr(self.cfg.codec, &data);
        Ok(EncodedFrame { data, is_keyframe: is_key, pts, width: self.cfg.width, height: self.cfg.height })
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        let unreg = nvfn!(self.s, OFF_FN_UNREGISTER_RESOURCE, unsafe extern "system" fn(*mut c_void, *mut c_void) -> Status);
        for (_, _, r) in self.registered.drain(..) {
            unsafe { unreg(self.s.enc, r) };
        }
        if !self.bitstream.is_null() {
            let f = nvfn!(self.s, OFF_FN_DESTROY_BITSTREAM, unsafe extern "system" fn(*mut c_void, *mut c_void) -> Status);
            unsafe { f(self.s.enc, self.bitstream) };
        }
        // La session (Session::drop) détruit l'encodeur.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_sanity() {
        // Les champs écrits tiennent dans leurs structures.
        assert!(OFF_CFG_CODEC + OFF_H264_CHROMA + 4 <= SIZE_NV_ENC_CONFIG);
        assert!(OFF_CFG_CODEC + OFF_HEVC_VUI + OFF_VUI_MATRIX + 4 <= SIZE_NV_ENC_CONFIG);
        assert!(OFF_RECONF_INIT + SIZE_NV_ENC_INITIALIZE_PARAMS <= BIT_RECONF_RESET.0);
        assert!(OFF_PRESET_CFG + SIZE_NV_ENC_CONFIG <= SIZE_NV_ENC_PRESET_CONFIG);
        assert_eq!(NVENCAPI_VERSION, 12 | (1 << 24));
        // Version de structure = API | (ver << 16) | (7 << 28) (+ bit 31 pour certaines).
        assert_eq!(NV_ENC_PIC_PARAMS_VER & 0x00FF_FFFF, (6 << 16) | 12);
    }

    #[test]
    fn bits_written_where_measured() {
        let mut b = Blob::new(SIZE_NV_ENC_CONFIG);
        let (w, bit) = BIT_HEVC_CHROMA;
        b.put_bits(OFF_CFG_CODEC + w, bit, 2, 3);
        let (w2, bit2) = BIT_HEVC_DEPTH;
        b.put_bits(OFF_CFG_CODEC + w2, bit2, 3, 2);
        let word = b.get_u32(OFF_CFG_CODEC + w);
        assert_eq!((word >> bit) & 3, 3);
        assert_eq!((word >> bit2) & 7, 2);
        b.put_bits(OFF_CFG_CODEC + w, bit, 2, 1);
        assert_eq!((b.get_u32(OFF_CFG_CODEC + w) >> bit) & 3, 1);
        assert_eq!((b.get_u32(OFF_CFG_CODEC + w) >> bit2) & 7, 2, "champ voisin préservé");
        assert_eq!(vui_codes(true), (9, 16, 9));
        assert_eq!(vui_codes(false), (1, 1, 1));
    }
}
