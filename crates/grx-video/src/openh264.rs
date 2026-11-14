//! OpenH264 (Cisco, licence BSD) chargé dynamiquement : encodeur et décodeur H.264 logiciels de
//! secours (host sans encodeur matériel, client sans décodeur matériel utilisable).
//!
//! * Seul le binaire officiel de Cisco est accepté : `openh264-2.6.0-win64.dll`, SHA-256 vérifié
//!   **avant** le chargement (aucune autre DLL de ce nom n'est chargée). Cisco paie les redevances
//!   H.264 (MPEG LA) pour ce binaire à condition qu'il soit téléchargé depuis ses serveurs : il
//!   n'est donc pas intégré au setup, l'installateur (ou `grx-host fetch-openh264`) le télécharge
//!   depuis `ciscobinary.openh264.org` (voir `grx-update::openh264`).
//! * Recherche, dans l'ordre : `%GRX_OPENH264_DIR%`, le dossier de l'exécutable, `exe\codecs`
//!   (jamais le `PATH`). Absent ou empreinte différente : voie logicielle H.264 désactivée.
//! * ABI : interfaces C++ `ISVCEncoder`/`ISVCDecoder` (vtables MSVC x64, ordre de déclaration de
//!   `codec_api.h` v2.6.0) ; tailles et offsets des structures mesurés en compilant un programme C
//!   contre les en-têtes 2.6.0 (`SEncParamExt` 924 o, `SFrameBSInfo` 7192 o, `SLayerBSInfo` 56 o,
//!   `SBufferInfo` 72 o). Version vérifiée à l'exécution (`WelsGetCodecVersionEx` = 2.6.x).
//!
//! Profil Constrained Baseline (CAVLC), tranches multiples = threads (latence d'une seule image).

#![allow(non_upper_case_globals)]

use std::ffi::{c_int, c_long, c_void};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::blob::Blob;
use sha2::{Digest, Sha256};
use windows::core::{HSTRING, PCSTR};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
};

/// Nom du binaire officiel attendu.
pub const DLL_NAME: &str = "openh264-2.6.0-win64.dll";
/// SHA-256 de la DLL décompressée (vérifié avant tout chargement).
pub const DLL_SHA256: &str = "2076cb5675ec6c1a4c70e7a2a322552f547b6eeed649d6dfcd9e02a543b24691";
/// Archive officielle (bzip2) et son SHA-256 (MD5 signé par Cisco : ce1282f5845f56761954282e4992730d).
pub const BZ2_URL: &str = "https://ciscobinary.openh264.org/openh264-2.6.0-win64.dll.bz2";
pub const BZ2_SHA256: &str = "dab5f2a872777f9a58b69bfa9fbcf20d9f82f2d6ec91383fd70bff49bd34ac9f";

// --- ABI (mesurée sur les en-têtes v2.6.0) ---
const SIZE_SEncParamExt: usize = 924;
const SIZE_SSpatialLayerConfig: usize = 200;
const OFF_P_USAGE: usize = 0;
const OFF_P_WIDTH: usize = 4;
const OFF_P_HEIGHT: usize = 8;
const OFF_P_BITRATE: usize = 12;
const OFF_P_RC_MODE: usize = 16;
const OFF_P_MAX_FPS: usize = 20;
const OFF_P_TEMPORAL_LAYERS: usize = 24;
const OFF_P_SPATIAL_LAYERS: usize = 28;
const OFF_P_LAYER0: usize = 32;
const OFF_P_INTRA_PERIOD: usize = 836;
const OFF_P_NUM_REF: usize = 840;
const OFF_P_SPSPPS_STRATEGY: usize = 844;
const OFF_P_FRAME_SKIP: usize = 860;
const OFF_P_MAX_BITRATE: usize = 864;
const OFF_P_THREADS: usize = 892;
const OFF_P_DENOISE: usize = 908;
const OFF_P_BG_DETECT: usize = 909;
const OFF_L_WIDTH: usize = 0;
const OFF_L_HEIGHT: usize = 4;
const OFF_L_FPS: usize = 8;
const OFF_L_BITRATE: usize = 12;
const OFF_L_MAX_BITRATE: usize = 16;
const OFF_L_PROFILE: usize = 20;
const OFF_L_SLICE_MODE: usize = 32;
const OFF_L_SLICE_NUM: usize = 36;
const OFF_L_VST_PRESENT: usize = 184;
const OFF_L_VIDEO_FORMAT: usize = 185;
const OFF_L_FULL_RANGE: usize = 186;
const OFF_L_COLOR_PRESENT: usize = 187;
const OFF_L_PRIMARIES: usize = 188;
const OFF_L_TRANSFER: usize = 189;
const OFF_L_MATRIX: usize = 190;
const SIZE_SSourcePicture: usize = 80;
const OFF_SRC_FORMAT: usize = 0;
const OFF_SRC_STRIDE: usize = 4;
const OFF_SRC_DATA: usize = 24;
const OFF_SRC_WIDTH: usize = 56;
const OFF_SRC_HEIGHT: usize = 60;
const OFF_SRC_TS: usize = 64;
const SIZE_SLayerBSInfo: usize = 56;
const OFF_LAYER_NAL_COUNT: usize = 16;
const OFF_LAYER_NAL_LEN: usize = 24;
const OFF_LAYER_BUF: usize = 32;
const SIZE_SFrameBSInfo: usize = 7192;
const OFF_FBS_LAYER_NUM: usize = 0;
const OFF_FBS_LAYERS: usize = 8;
const OFF_FBS_FRAME_TYPE: usize = 7176;
const SIZE_SDecodingParam: usize = 32;
const OFF_D_EC: usize = 16;
const OFF_D_VIDEO_PROP: usize = 24;
const SIZE_SBufferInfo: usize = 72;
const OFF_B_STATUS: usize = 0;
const OFF_B_WIDTH: usize = 24;
const OFF_B_HEIGHT: usize = 28;
const OFF_B_STRIDE: usize = 36;
const OFF_B_DST: usize = 48;
const MAX_LAYERS: usize = 128;

const videoFormatI420: i32 = 23;
const SCREEN_CONTENT_REAL_TIME: i32 = 1;
const RC_BITRATE_MODE: i32 = 1;
const SM_FIXEDSLCNUM_SLICE: i32 = 1;
const SM_SINGLE_SLICE: i32 = 0;
const ENCODER_OPTION_BITRATE: c_int = 5;
const ENCODER_OPTION_MAX_BITRATE: c_int = 6;
const ENCODER_OPTION_TRACE_LEVEL: c_int = 25;
const DECODER_OPTION_TRACE_LEVEL: c_int = 9;
const SPATIAL_LAYER_ALL: i32 = 4;
const PRO_BASELINE: i32 = 66;
const videoFrameTypeIDR: i32 = 1;
const videoFrameTypeSkip: i32 = 4;
const CONSTANT_ID: i32 = 0;
const VIDEO_BITSTREAM_AVC: i32 = 0;
const ERROR_CON_DISABLE: i32 = 0;
const WELS_LOG_QUIET: c_int = 0;

// Emplacements des vtables (ordre de déclaration, MSVC x64 ; destructeur virtuel en dernier).
const ENC_INITIALIZE_EXT: usize = 1;
const ENC_GET_DEFAULT_PARAMS: usize = 2;
const ENC_UNINITIALIZE: usize = 3;
const ENC_ENCODE_FRAME: usize = 4;
const ENC_FORCE_INTRA: usize = 6;
const ENC_SET_OPTION: usize = 7;
const DEC_INITIALIZE: usize = 0;
const DEC_UNINITIALIZE: usize = 1;
const DEC_DECODE_NO_DELAY: usize = 3;
const DEC_SET_OPTION: usize = 8;

type FnPtr = unsafe extern "system" fn() -> isize;

#[derive(Debug)]
struct Api {
    dir: PathBuf,
    version: String,
    create_enc: unsafe extern "system" fn(*mut *mut c_void) -> c_int,
    destroy_enc: unsafe extern "system" fn(*mut c_void),
    create_dec: unsafe extern "system" fn(*mut *mut c_void) -> c_long,
    destroy_dec: unsafe extern "system" fn(*mut c_void),
}

/// Dossiers de recherche, dans l'ordre (jamais le PATH).
pub fn search_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(d) = std::env::var_os("GRX_OPENH264_DIR") {
        v.push(PathBuf::from(d));
    }
    if let Some(exe) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        v.push(exe.clone());
        v.push(exe.join("codecs"));
    }
    v
}

/// SHA-256 hexadécimal (minuscules) d'un fichier.
pub fn sha256_file(p: &Path) -> std::io::Result<String> {
    let data = std::fs::read(p)?;
    Ok(Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect())
}

fn sym(m: windows::Win32::Foundation::HMODULE, name: &str) -> Result<FnPtr, String> {
    let c = std::ffi::CString::new(name).unwrap();
    unsafe { GetProcAddress(m, PCSTR(c.as_ptr() as *const u8)) }.ok_or_else(|| format!("symbole {name} absent"))
}

fn load_from(dir: &Path) -> Result<Api, String> {
    let p = dir.join(DLL_NAME);
    if !p.is_file() {
        return Err(format!("{} absent", p.display()));
    }
    // Empreinte vérifiée AVANT LoadLibrary : seul le binaire officiel de Cisco est exécuté.
    let h = sha256_file(&p).map_err(|e| format!("lecture de {} : {e}", p.display()))?;
    if h != DLL_SHA256 {
        return Err(format!("{} : SHA-256 inattendu ({h}), DLL ignorée", p.display()));
    }
    let flags = LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS;
    let m = unsafe { LoadLibraryExW(&HSTRING::from(p.as_os_str()), None, flags) }
        .map_err(|e| format!("chargement de {} : {e}", p.display()))?;
    #[repr(C)]
    #[derive(Default)]
    struct Ver {
        major: u32,
        minor: u32,
        revision: u32,
        reserved: u32,
    }
    // SAFETY : signatures de codec_api.h v2.6.0 (WelsCreateSVCEncoder & co, conventions C).
    unsafe {
        let get_ver: unsafe extern "system" fn(*mut Ver) = std::mem::transmute(sym(m, "WelsGetCodecVersionEx")?);
        let mut v = Ver::default();
        get_ver(&mut v);
        if (v.major, v.minor) != (2, 6) {
            return Err(format!("OpenH264 {}.{}.{} non supporté (2.6.x attendu)", v.major, v.minor, v.revision));
        }
        Ok(Api {
            dir: dir.to_path_buf(),
            version: format!("OpenH264 {}.{}.{}", v.major, v.minor, v.revision),
            create_enc: std::mem::transmute(sym(m, "WelsCreateSVCEncoder")?),
            destroy_enc: std::mem::transmute(sym(m, "WelsDestroySVCEncoder")?),
            create_dec: std::mem::transmute(sym(m, "WelsCreateDecoder")?),
            destroy_dec: std::mem::transmute(sym(m, "WelsDestroyDecoder")?),
        })
    }
}

static API: OnceLock<Result<Api, String>> = OnceLock::new();

fn api() -> Result<&'static Api, String> {
    API.get_or_init(|| {
        if std::env::var_os("GRX_DISABLE_OPENH264").is_some() {
            return Err("OpenH264 désactivé (GRX_DISABLE_OPENH264)".into());
        }
        let mut errs = Vec::new();
        for d in search_dirs() {
            match load_from(&d) {
                Ok(a) => return Ok(a),
                Err(e) => errs.push(e),
            }
        }
        Err(format!(
            "{DLL_NAME} introuvable (installez-le avec « grx-host fetch-openh264 ») : {}",
            errs.iter().find(|e| !e.ends_with("absent")).or(errs.first()).cloned().unwrap_or_else(|| "aucun dossier".into())
        ))
    })
    .as_ref()
    .map_err(|e| e.clone())
}

/// OpenH264 est-il chargeable ? `Ok("OpenH264 2.6.0 (dossier)")` ou la raison.
pub fn availability() -> Result<String, String> {
    api().map(|a| format!("{} ({})", a.version, a.dir.display()))
}

/// Entrée `slot` de la vtable de l'objet C++ `obj`.
unsafe fn vslot(obj: *mut c_void, slot: usize) -> FnPtr {
    let vt = *(obj as *const *const FnPtr);
    *vt.add(slot)
}

/// Nombre de threads (= tranches) de l'encodeur : la moitié des cœurs logiques, de 1 à 4.
pub fn encoder_threads() -> i32 {
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    (n / 2).clamp(1, 4) as i32
}

/// Encodeur H.264 logiciel (I420 en entrée, Annex-B en sortie).
pub struct H264Encoder {
    api: &'static Api,
    enc: *mut c_void,
    width: u32,
    height: u32,
    bs: Blob,
    force_idr: bool,
    pub threads: i32,
}

// SAFETY : l'encodeur n'est utilisé que via &mut self (un thread à la fois).
unsafe impl Send for H264Encoder {}

impl H264Encoder {
    /// Encodeur `width`x`height` (pairs), `bitrate_bps` CBR, réglé pour le contenu d'écran temps réel.
    pub fn new(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Result<H264Encoder, String> {
        let api = api()?;
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(format!("taille {width}x{height} invalide (paire attendue)"));
        }
        let threads = encoder_threads();
        let mut enc: *mut c_void = std::ptr::null_mut();
        // SAFETY : ABI vérifiée (voir l'en-tête du module) ; `enc` détruit dans Drop.
        unsafe {
            let r = (api.create_enc)(&mut enc);
            if r != 0 || enc.is_null() {
                return Err(format!("WelsCreateSVCEncoder : {r}"));
            }
            let set_opt: unsafe extern "system" fn(*mut c_void, c_int, *mut c_void) -> c_int =
                std::mem::transmute(vslot(enc, ENC_SET_OPTION));
            let mut quiet: c_int = WELS_LOG_QUIET;
            set_opt(enc, ENCODER_OPTION_TRACE_LEVEL, &mut quiet as *mut c_int as *mut c_void);
            let mut p = Blob::new(SIZE_SEncParamExt);
            let get_def: unsafe extern "system" fn(*mut c_void, *mut c_void) -> c_int =
                std::mem::transmute(vslot(enc, ENC_GET_DEFAULT_PARAMS));
            get_def(enc, p.ptr());
            p.put_i32(OFF_P_USAGE, SCREEN_CONTENT_REAL_TIME);
            p.put_i32(OFF_P_WIDTH, width as i32);
            p.put_i32(OFF_P_HEIGHT, height as i32);
            p.put_i32(OFF_P_BITRATE, bitrate_bps as i32);
            p.put_i32(OFF_P_RC_MODE, RC_BITRATE_MODE);
            p.put_f32(OFF_P_MAX_FPS, fps.max(1) as f32);
            p.put_i32(OFF_P_TEMPORAL_LAYERS, 1);
            p.put_i32(OFF_P_SPATIAL_LAYERS, 1);
            p.put_i32(OFF_P_INTRA_PERIOD, 0); // IDR uniquement à la demande
            p.put_i32(OFF_P_NUM_REF, 1);
            p.put_i32(OFF_P_SPSPPS_STRATEGY, CONSTANT_ID);
            p.put_u8(OFF_P_FRAME_SKIP, 0); // jamais d'image sautée : latence constante
            p.put_i32(OFF_P_MAX_BITRATE, (bitrate_bps as u64 * 3 / 2).min(i32::MAX as u64) as i32);
            p.put_i32(OFF_P_THREADS, threads);
            p.put_u8(OFF_P_DENOISE, 0);
            p.put_u8(OFF_P_BG_DETECT, 0);
            let l = OFF_P_LAYER0;
            debug_assert!(l + SIZE_SSpatialLayerConfig <= SIZE_SEncParamExt);
            p.put_i32(l + OFF_L_WIDTH, width as i32);
            p.put_i32(l + OFF_L_HEIGHT, height as i32);
            p.put_f32(l + OFF_L_FPS, fps.max(1) as f32);
            p.put_i32(l + OFF_L_BITRATE, bitrate_bps as i32);
            p.put_i32(l + OFF_L_MAX_BITRATE, (bitrate_bps as u64 * 3 / 2).min(i32::MAX as u64) as i32);
            p.put_i32(l + OFF_L_PROFILE, PRO_BASELINE);
            if threads > 1 {
                p.put_i32(l + OFF_L_SLICE_MODE, SM_FIXEDSLCNUM_SLICE);
                p.put_i32(l + OFF_L_SLICE_NUM, threads);
            } else {
                p.put_i32(l + OFF_L_SLICE_MODE, SM_SINGLE_SLICE);
            }
            // VUI : BT.709 plage limitée (comme les encodeurs matériels).
            p.put_u8(l + OFF_L_VST_PRESENT, 1);
            p.put_u8(l + OFF_L_VIDEO_FORMAT, 5);
            p.put_u8(l + OFF_L_FULL_RANGE, 0);
            p.put_u8(l + OFF_L_COLOR_PRESENT, 1);
            p.put_u8(l + OFF_L_PRIMARIES, 1);
            p.put_u8(l + OFF_L_TRANSFER, 1);
            p.put_u8(l + OFF_L_MATRIX, 1);
            let init: unsafe extern "system" fn(*mut c_void, *const c_void) -> c_int =
                std::mem::transmute(vslot(enc, ENC_INITIALIZE_EXT));
            let r = init(enc, p.ptr());
            if r != 0 {
                (api.destroy_enc)(enc);
                return Err(format!("ISVCEncoder::InitializeExt : {r}"));
            }
        }
        Ok(H264Encoder { api, enc, width, height, bs: Blob::new(SIZE_SFrameBSInfo), force_idr: true, threads })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    pub fn set_bitrate(&mut self, bps: u32) -> Result<(), String> {
        #[repr(C)]
        struct BitrateInfo {
            layer: c_int,
            bitrate: c_int,
        }
        // SAFETY : SBitrateInfo = {int, int} (8 o).
        unsafe {
            let set_opt: unsafe extern "system" fn(*mut c_void, c_int, *mut c_void) -> c_int =
                std::mem::transmute(vslot(self.enc, ENC_SET_OPTION));
            let mut mx = BitrateInfo { layer: SPATIAL_LAYER_ALL, bitrate: (bps as u64 * 3 / 2).min(i32::MAX as u64) as c_int };
            set_opt(self.enc, ENCODER_OPTION_MAX_BITRATE, &mut mx as *mut _ as *mut c_void);
            let mut b = BitrateInfo { layer: SPATIAL_LAYER_ALL, bitrate: bps as c_int };
            let r = set_opt(self.enc, ENCODER_OPTION_BITRATE, &mut b as *mut _ as *mut c_void);
            if r != 0 {
                return Err(format!("ENCODER_OPTION_BITRATE : {r}"));
            }
        }
        Ok(())
    }

    /// Encode une image I420 (plans `y`, `u`, `v`, pas en octets). Renvoie l'access unit Annex-B
    /// (vide si l'encodeur a sauté l'image) et `true` pour un IDR.
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &mut self,
        y: &[u8],
        y_stride: usize,
        u: &[u8],
        v: &[u8],
        c_stride: usize,
        ts_ms: u64,
    ) -> Result<(Vec<u8>, bool), String> {
        let (w, h) = (self.width as usize, self.height as usize);
        if y.len() < y_stride * (h - 1) + w || u.len() < c_stride * (h / 2 - 1) + w / 2 || v.len() < u.len().min(c_stride * (h / 2 - 1) + w / 2) {
            return Err("plans I420 trop petits".into());
        }
        // SAFETY : pointeurs valides pendant l'appel ; SSourcePicture/SFrameBSInfo dimensionnés à l'ABI.
        unsafe {
            if self.force_idr {
                let force: unsafe extern "system" fn(*mut c_void, bool, c_int) -> c_int =
                    std::mem::transmute(vslot(self.enc, ENC_FORCE_INTRA));
                force(self.enc, true, -1);
                self.force_idr = false;
            }
            let mut src = Blob::new(SIZE_SSourcePicture);
            src.put_i32(OFF_SRC_FORMAT, videoFormatI420);
            src.put_i32(OFF_SRC_STRIDE, y_stride as i32);
            src.put_i32(OFF_SRC_STRIDE + 4, c_stride as i32);
            src.put_i32(OFF_SRC_STRIDE + 8, c_stride as i32);
            src.put_u64(OFF_SRC_DATA, y.as_ptr() as u64);
            src.put_u64(OFF_SRC_DATA + 8, u.as_ptr() as u64);
            src.put_u64(OFF_SRC_DATA + 16, v.as_ptr() as u64);
            src.put_i32(OFF_SRC_WIDTH, w as i32);
            src.put_i32(OFF_SRC_HEIGHT, h as i32);
            src.put_u64(OFF_SRC_TS, ts_ms);
            self.bs.bytes().fill(0);
            let encode: unsafe extern "system" fn(*mut c_void, *const c_void, *mut c_void) -> c_int =
                std::mem::transmute(vslot(self.enc, ENC_ENCODE_FRAME));
            let r = encode(self.enc, src.ptr(), self.bs.ptr());
            if r != 0 {
                return Err(format!("ISVCEncoder::EncodeFrame : {r}"));
            }
            let ftype = self.bs.get_i32(OFF_FBS_FRAME_TYPE);
            if ftype == videoFrameTypeSkip {
                return Ok((Vec::new(), false));
            }
            let layers = (self.bs.get_i32(OFF_FBS_LAYER_NUM).max(0) as usize).min(MAX_LAYERS);
            let mut out = Vec::new();
            for i in 0..layers {
                let base = OFF_FBS_LAYERS + i * SIZE_SLayerBSInfo;
                let nals = self.bs.get_i32(base + OFF_LAYER_NAL_COUNT).max(0) as usize;
                let lens = self.bs.get_ptr(base + OFF_LAYER_NAL_LEN) as *const c_int;
                let buf = self.bs.get_ptr(base + OFF_LAYER_BUF) as *const u8;
                if lens.is_null() || buf.is_null() {
                    continue;
                }
                let total: usize = (0..nals).map(|k| (*lens.add(k)).max(0) as usize).sum();
                out.extend_from_slice(std::slice::from_raw_parts(buf, total));
            }
            Ok((out, ftype == videoFrameTypeIDR))
        }
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        // SAFETY : objet créé par WelsCreateSVCEncoder, détruit une seule fois.
        unsafe {
            let uninit: unsafe extern "system" fn(*mut c_void) -> c_int = std::mem::transmute(vslot(self.enc, ENC_UNINITIALIZE));
            uninit(self.enc);
            (self.api.destroy_enc)(self.enc);
        }
    }
}

/// Image décodée (plans I420 empruntés au décodeur jusqu'au prochain appel).
pub struct I420<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub y_stride: usize,
    pub c_stride: usize,
    pub width: u32,
    pub height: u32,
}

/// Décodeur H.264 logiciel (Annex-B en entrée, I420 en sortie), sans délai d'image.
pub struct H264Decoder {
    api: &'static Api,
    dec: *mut c_void,
    info: Blob,
}

// SAFETY : utilisé via &mut self uniquement.
unsafe impl Send for H264Decoder {}

impl H264Decoder {
    pub fn new() -> Result<H264Decoder, String> {
        let api = api()?;
        let mut dec: *mut c_void = std::ptr::null_mut();
        // SAFETY : ABI vérifiée ; `dec` détruit dans Drop.
        unsafe {
            let r = (api.create_dec)(&mut dec);
            if r != 0 || dec.is_null() {
                return Err(format!("WelsCreateDecoder : {r}"));
            }
            let set_opt: unsafe extern "system" fn(*mut c_void, c_int, *mut c_void) -> c_long =
                std::mem::transmute(vslot(dec, DEC_SET_OPTION));
            let mut quiet: c_int = WELS_LOG_QUIET;
            set_opt(dec, DECODER_OPTION_TRACE_LEVEL, &mut quiet as *mut c_int as *mut c_void);
            let mut p = Blob::new(SIZE_SDecodingParam);
            p.put_i32(OFF_D_EC, ERROR_CON_DISABLE);
            p.put_i32(OFF_D_VIDEO_PROP, 8); // SVideoProperty.size
            p.put_i32(OFF_D_VIDEO_PROP + 4, VIDEO_BITSTREAM_AVC);
            let init: unsafe extern "system" fn(*mut c_void, *const c_void) -> c_long =
                std::mem::transmute(vslot(dec, DEC_INITIALIZE));
            let r = init(dec, p.ptr());
            if r != 0 {
                (api.destroy_dec)(dec);
                return Err(format!("ISVCDecoder::Initialize : {r}"));
            }
        }
        Ok(H264Decoder { api, dec, info: Blob::new(SIZE_SBufferInfo) })
    }

    pub fn version(&self) -> &str {
        &self.api.version
    }

    /// Décode une access unit Annex-B ; `Ok(None)` si aucune image n'est sortie.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<I420<'_>>, String> {
        if data.is_empty() || data.len() > i32::MAX as usize {
            return Err("paquet vide ou trop grand".into());
        }
        // SAFETY : pointeurs de sortie possédés par le décodeur, valides jusqu'au prochain appel.
        unsafe {
            let mut dst: [*mut u8; 3] = [std::ptr::null_mut(); 3];
            self.info.bytes().fill(0);
            let f: unsafe extern "system" fn(*mut c_void, *const u8, c_int, *mut *mut u8, *mut c_void) -> c_int =
                std::mem::transmute(vslot(self.dec, DEC_DECODE_NO_DELAY));
            let st = f(self.dec, data.as_ptr(), data.len() as c_int, dst.as_mut_ptr(), self.info.ptr());
            if st != 0 {
                return Err(format!("DecodeFrameNoDelay : état {st:#x}"));
            }
            if self.info.get_i32(OFF_B_STATUS) != 1 {
                return Ok(None);
            }
            let w = self.info.get_i32(OFF_B_WIDTH).max(0) as usize;
            let h = self.info.get_i32(OFF_B_HEIGHT).max(0) as usize;
            let ys = self.info.get_i32(OFF_B_STRIDE).max(0) as usize;
            let cs = self.info.get_i32(OFF_B_STRIDE + 4).max(0) as usize;
            let (py, pu, pv) = (
                self.info.get_ptr(OFF_B_DST) as *const u8,
                self.info.get_ptr(OFF_B_DST + 8) as *const u8,
                self.info.get_ptr(OFF_B_DST + 16) as *const u8,
            );
            if w == 0 || h == 0 || py.is_null() || pu.is_null() || pv.is_null() {
                return Ok(None);
            }
            let ch = h.div_ceil(2);
            Ok(Some(I420 {
                y: std::slice::from_raw_parts(py, ys * h),
                u: std::slice::from_raw_parts(pu, cs * ch),
                v: std::slice::from_raw_parts(pv, cs * ch),
                y_stride: ys,
                c_stride: cs,
                width: w as u32,
                height: h as u32,
            }))
        }
    }
}

impl Drop for H264Decoder {
    fn drop(&mut self) {
        // SAFETY : objet créé par WelsCreateDecoder, détruit une seule fois.
        unsafe {
            let uninit: unsafe extern "system" fn(*mut c_void) -> c_long = std::mem::transmute(vslot(self.dec, DEC_UNINITIALIZE));
            uninit(self.dec);
            (self.api.destroy_dec)(self.dec);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_roundtrip_and_alignment() {
        let mut b = Blob::new(SIZE_SFrameBSInfo);
        assert_eq!(b.ptr() as usize % 8, 0);
        assert!(b.bytes().len() >= SIZE_SFrameBSInfo);
        b.put_i32(OFF_FBS_FRAME_TYPE, videoFrameTypeIDR);
        assert_eq!(b.get_i32(OFF_FBS_FRAME_TYPE), 1);
        b.put_u64(OFF_FBS_LAYERS + OFF_LAYER_BUF, 0x1234_5678_9abc);
        assert_eq!(b.get_ptr(OFF_FBS_LAYERS + OFF_LAYER_BUF) as usize, 0x1234_5678_9abc);
        // Les 128 couches tiennent avant le type d'image.
        assert!(OFF_FBS_LAYERS + MAX_LAYERS * SIZE_SLayerBSInfo <= OFF_FBS_FRAME_TYPE);
        assert!(OFF_P_LAYER0 + 4 * SIZE_SSpatialLayerConfig <= OFF_P_INTRA_PERIOD);
    }

    #[test]
    fn missing_or_foreign_dll_is_refused() {
        let dir = std::env::temp_dir().join(format!("grx-oh264-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_from(&dir).unwrap_err().ends_with("absent"));
        // Une DLL du bon nom mais d'une autre empreinte n'est jamais chargée.
        std::fs::write(dir.join(DLL_NAME), b"MZ pas la DLL de Cisco").unwrap();
        let e = load_from(&dir).unwrap_err();
        assert!(e.contains("SHA-256 inattendu"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn threads_bounded() {
        assert!((1..=4).contains(&encoder_threads()));
    }
}
