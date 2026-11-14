//! Décodeur de secours : FFmpeg (libavcodec) avec hwaccel D3D11VA, chargé DYNAMIQUEMENT.
//!
//! Sert quand Media Foundation n'a pas de décodeur pour le codec (cas typique : HEVC sans
//! l'extension « HEVC Video Extensions » du Store). Le décodage reste matériel (ID3D11VideoDecoder
//! du GPU, comme le MFT) et la sortie est une texture D3D11 NV12 du device du présentateur :
//! zéro copie, même chemin d'affichage que Media Foundation.
//!
//! Les DLL **LGPL** `avcodec-62.dll`, `avutil-60.dll` et `swresample-6.dll` (FFmpeg 8.x, build
//! « lgpl-shared ») sont cherchées, dans l'ordre, dans `%GRX_FFMPEG_DIR%`, à côté de l'exécutable
//! puis dans son sous-dossier `ffmpeg\` — jamais dans le `PATH` (pas de détournement de DLL).
//! Absentes ou d'une autre version majeure : cette voie est simplement désactivée.
//!
//! Seuls les points d'entrée stables de l'ABI sont utilisés ; les trois champs de structures lus
//! ou écrits (`AVCodecContext.hw_device_ctx`, `AVHWDeviceContext.hwctx`, `AVFrame.{data,width,
//! height,format}`) ont des offsets mesurés sur les en-têtes de la version majeure vérifiée au
//! chargement (avcodec 62 / avutil 60).

// Les pointeurs de fonctions sont typés par le champ de destination (`Api`).
#![allow(clippy::missing_transmute_annotations)]

use std::ffi::{c_char, c_int, c_uint, c_void, CString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use grx_video::Codec;
use windows::core::{Interface, HSTRING, PCSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
};

use crate::annexb;
use crate::error::{RenderError, Result};

/// Versions majeures supportées (noms des DLL et offsets ci-dessous).
pub const AVCODEC_MAJOR: u32 = 62;
pub const AVUTIL_MAJOR: u32 = 60;
/// avutil et avcodec obligatoires ; swresample chargé s'il est présent (dépendance des builds complètes).
pub const DLLS: [&str; 3] = ["avutil-60.dll", "swresample-6.dll", "avcodec-62.dll"];

// Offsets (octets, x64) mesurés sur les en-têtes FFmpeg 8.1 (avcodec 62.x / avutil 60.x).
const OFF_CTX_HW_DEVICE_CTX: usize = 560;
const OFF_HWDEV_HWCTX: usize = 16;
const OFF_BUFREF_DATA: usize = 8;
const OFF_FRAME_WIDTH: usize = 104;
const OFF_FRAME_HEIGHT: usize = 108;
const OFF_FRAME_FORMAT: usize = 116;
const AV_PIX_FMT_D3D11: c_int = 171;
const AV_HWDEVICE_TYPE_D3D11VA: c_int = 7;
const AVERROR_EAGAIN: c_int = -11;
const AV_INPUT_BUFFER_PADDING_SIZE: usize = 64;
const AV_LOG_ERROR: c_int = 16;
const AV_LOG_QUIET: c_int = -8;

type FnPtr = unsafe extern "system" fn() -> isize;

struct Api {
    dir: PathBuf,
    version: String,
    // avutil
    av_hwdevice_ctx_alloc: unsafe extern "C" fn(c_int) -> *mut c_void,
    av_hwdevice_ctx_init: unsafe extern "C" fn(*mut c_void) -> c_int,
    av_buffer_ref: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    av_buffer_unref: unsafe extern "C" fn(*mut *mut c_void),
    av_frame_alloc: unsafe extern "C" fn() -> *mut c_void,
    av_frame_free: unsafe extern "C" fn(*mut *mut c_void),
    av_malloc: unsafe extern "C" fn(usize) -> *mut c_void,
    av_free: unsafe extern "C" fn(*mut c_void),
    av_opt_set: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, c_int) -> c_int,
    av_log_set_level: unsafe extern "C" fn(c_int),
    av_strerror: unsafe extern "C" fn(c_int, *mut c_char, usize) -> c_int,
    // avcodec
    avcodec_find_decoder_by_name: unsafe extern "C" fn(*const c_char) -> *const c_void,
    avcodec_alloc_context3: unsafe extern "C" fn(*const c_void) -> *mut c_void,
    avcodec_open2: unsafe extern "C" fn(*mut c_void, *const c_void, *mut *mut c_void) -> c_int,
    avcodec_free_context: unsafe extern "C" fn(*mut *mut c_void),
    avcodec_send_packet: unsafe extern "C" fn(*mut c_void, *const c_void) -> c_int,
    avcodec_receive_frame: unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int,
    avcodec_flush_buffers: unsafe extern "C" fn(*mut c_void),
    av_packet_alloc: unsafe extern "C" fn() -> *mut c_void,
    av_packet_free: unsafe extern "C" fn(*mut *mut c_void),
    av_packet_from_data: unsafe extern "C" fn(*mut c_void, *mut u8, c_int) -> c_int,
}

fn sym(m: HMODULE, name: &str) -> std::result::Result<FnPtr, String> {
    let c = CString::new(name).unwrap();
    unsafe { GetProcAddress(m, PCSTR(c.as_ptr() as *const u8)) }.ok_or_else(|| format!("symbole {name} absent"))
}

macro_rules! load_fn {
    ($m:expr, $name:ident) => {
        // SAFETY : signature recopiée des en-têtes FFmpeg de la version majeure vérifiée.
        unsafe { std::mem::transmute::<FnPtr, _>(sym($m, stringify!($name))?) }
    };
}

/// Dossiers où chercher les DLL, dans l'ordre (jamais le PATH).
pub fn search_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(d) = std::env::var_os("GRX_FFMPEG_DIR") {
        v.push(PathBuf::from(d));
    }
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        v.push(exe_dir.join("ffmpeg"));
        v.insert(v.len() - 1, exe_dir);
    }
    v
}

fn load_from(dir: &Path) -> std::result::Result<Api, String> {
    let flags = LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS;
    let mut mods = Vec::new();
    for dll in DLLS {
        let p = dir.join(dll);
        if !p.is_file() {
            // swresample n'est qu'une dépendance de certaines builds (absente d'une build minimale).
            if dll.starts_with("swresample") {
                mods.push(None);
                continue;
            }
            return Err(format!("{} absent", p.display()));
        }
        // Chemin absolu + LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR : les dépendances viennent du même dossier.
        let m = unsafe { LoadLibraryExW(&HSTRING::from(p.as_os_str()), None, flags) }
            .map_err(|e| format!("chargement de {} : {e}", p.display()))?;
        mods.push(Some(m));
    }
    let (Some(u), Some(c)) = (mods[0], mods[2]) else { unreachable!("avutil/avcodec obligatoires") };
    let avcodec_license: unsafe extern "C" fn() -> *const c_char = load_fn!(c, avcodec_license);
    let license = unsafe { std::ffi::CStr::from_ptr(avcodec_license()) }.to_string_lossy().into_owned();
    let avutil_version: unsafe extern "C" fn() -> c_uint = load_fn!(u, avutil_version);
    let avcodec_version: unsafe extern "C" fn() -> c_uint = load_fn!(c, avcodec_version);
    let (vu, vc) = unsafe { (avutil_version(), avcodec_version()) };
    if vu >> 16 != AVUTIL_MAJOR || vc >> 16 != AVCODEC_MAJOR {
        return Err(format!(
            "version non supportée : avutil {}.{} / avcodec {}.{} (attendu {AVUTIL_MAJOR}.x / {AVCODEC_MAJOR}.x)",
            vu >> 16,
            (vu >> 8) & 0xFF,
            vc >> 16,
            (vc >> 8) & 0xFF
        ));
    }
    let api = Api {
        dir: dir.to_path_buf(),
        version: format!("avcodec {}.{}.{}, {license}", vc >> 16, (vc >> 8) & 0xFF, vc & 0xFF),
        av_hwdevice_ctx_alloc: load_fn!(u, av_hwdevice_ctx_alloc),
        av_hwdevice_ctx_init: load_fn!(u, av_hwdevice_ctx_init),
        av_buffer_ref: load_fn!(u, av_buffer_ref),
        av_buffer_unref: load_fn!(u, av_buffer_unref),
        av_frame_alloc: load_fn!(u, av_frame_alloc),
        av_frame_free: load_fn!(u, av_frame_free),
        av_malloc: load_fn!(u, av_malloc),
        av_free: load_fn!(u, av_free),
        av_opt_set: load_fn!(u, av_opt_set),
        av_log_set_level: load_fn!(u, av_log_set_level),
        av_strerror: load_fn!(u, av_strerror),
        avcodec_find_decoder_by_name: load_fn!(c, avcodec_find_decoder_by_name),
        avcodec_alloc_context3: load_fn!(c, avcodec_alloc_context3),
        avcodec_open2: load_fn!(c, avcodec_open2),
        avcodec_free_context: load_fn!(c, avcodec_free_context),
        avcodec_send_packet: load_fn!(c, avcodec_send_packet),
        avcodec_receive_frame: load_fn!(c, avcodec_receive_frame),
        avcodec_flush_buffers: load_fn!(c, avcodec_flush_buffers),
        av_packet_alloc: load_fn!(c, av_packet_alloc),
        av_packet_free: load_fn!(c, av_packet_free),
        av_packet_from_data: load_fn!(c, av_packet_from_data),
    };
    let lvl = if std::env::var_os("GRX_FFMPEG_LOG").is_some() { AV_LOG_ERROR } else { AV_LOG_QUIET };
    unsafe { (api.av_log_set_level)(lvl) };
    // Les modules restent chargés jusqu'à la fin du processus (pointeurs de fonctions statiques).
    Ok(api)
}

static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();

fn api() -> std::result::Result<&'static Api, String> {
    API.get_or_init(|| {
        let mut errs = Vec::new();
        for d in search_dirs() {
            match load_from(&d) {
                Ok(a) => return Ok(a),
                Err(e) => errs.push(e),
            }
        }
        Err(format!(
            "DLL FFmpeg LGPL introuvables ({}) : {}",
            DLLS.join(", "),
            errs.first().cloned().unwrap_or_else(|| "aucun dossier".into())
        ))
    })
    .as_ref()
    .map_err(|e| e.clone())
}

/// FFmpeg est-il chargeable ? `Ok("avcodec 62.x.y (dossier)")` ou la raison.
pub fn availability() -> std::result::Result<String, String> {
    api().map(|a| format!("{} ({})", a.version, a.dir.display()))
}

fn av_err(a: &Api, code: c_int) -> String {
    let mut buf = [0 as c_char; 128];
    unsafe { (a.av_strerror)(code, buf.as_mut_ptr(), buf.len()) };
    let s = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned();
    format!("{s} ({code})")
}

fn ff_err(context: &str, msg: impl std::fmt::Display) -> RenderError {
    RenderError::Other(format!("FFmpeg {context} : {msg}"))
}

/// Référence sur une image FFmpeg (surface du pool D3D11 du décodeur) : relâchée au drop.
pub(crate) struct FrameRef(*mut c_void);

// SAFETY : les références AVFrame/AVBuffer sont atomiques et le pool est protégé par un mutex ;
// libérer une image depuis un autre thread (présentateur) est sûr.
unsafe impl Send for FrameRef {}
unsafe impl Sync for FrameRef {}

impl Drop for FrameRef {
    fn drop(&mut self) {
        if let Ok(a) = api() {
            unsafe { (a.av_frame_free)(&mut self.0) };
        }
    }
}

/// Image sortie : (texture, tranche, largeur, hauteur, référence à garder).
pub(crate) type FfFrame = (ID3D11Texture2D, u32, u32, u32, FrameRef);

pub(crate) struct FfDecoder {
    ctx: *mut c_void,
    pkt: *mut c_void,
    codec: Codec,
    wait_idr: bool,
    name: String,
    width: u32,
    height: u32,
}

// SAFETY : le contexte n'est utilisé que via &mut self (un seul thread à la fois) ; le device
// D3D11 est protégé multithread (FFmpeg ajoute son propre verrou).
unsafe impl Send for FfDecoder {}

fn decoder_name(codec: Codec) -> &'static str {
    // Décodeurs natifs (ceux qui ont le hwaccel D3D11VA ; libdav1d n'en a pas).
    match codec {
        Codec::H264 => "h264",
        Codec::Hevc => "hevc",
        Codec::Av1 => "av1",
    }
}

impl FfDecoder {
    pub fn new(device: &ID3D11Device, codec: Codec) -> Result<FfDecoder> {
        let a = api().map_err(RenderError::Other)?;
        unsafe {
            let name = CString::new(decoder_name(codec)).unwrap();
            let dec = (a.avcodec_find_decoder_by_name)(name.as_ptr());
            if dec.is_null() {
                return Err(ff_err("décodeur", format!("« {} » absent de cette build", decoder_name(codec))));
            }
            // Contexte de device D3D11VA sur NOTRE device (celui du présentateur) : zéro copie.
            let mut hw = (a.av_hwdevice_ctx_alloc)(AV_HWDEVICE_TYPE_D3D11VA);
            if hw.is_null() {
                return Err(ff_err("av_hwdevice_ctx_alloc", "échec"));
            }
            let devctx = *((hw as *const u8).add(OFF_BUFREF_DATA) as *const *mut u8);
            let hwctx = *(devctx.add(OFF_HWDEV_HWCTX) as *const *mut *mut c_void);
            // AVD3D11VADeviceContext.device (1er champ) : FFmpeg le relâche à la destruction.
            *hwctx = device.clone().into_raw();
            let r = (a.av_hwdevice_ctx_init)(hw);
            if r < 0 {
                (a.av_buffer_unref)(&mut hw);
                return Err(ff_err("av_hwdevice_ctx_init(D3D11VA)", av_err(a, r)));
            }
            let mut ctx = (a.avcodec_alloc_context3)(dec);
            if ctx.is_null() {
                (a.av_buffer_unref)(&mut hw);
                return Err(ff_err("avcodec_alloc_context3", "échec"));
            }
            *((ctx as *mut u8).add(OFF_CTX_HW_DEVICE_CTX) as *mut *mut c_void) = (a.av_buffer_ref)(hw);
            (a.av_buffer_unref)(&mut hw);
            // Faible latence : pas de threads de trames (1 image de retard chacun), sortie immédiate.
            for (k, v) in [("threads", "1"), ("flags", "+low_delay"), ("extra_hw_frames", "4")] {
                let (k, v) = (CString::new(k).unwrap(), CString::new(v).unwrap());
                let _ = (a.av_opt_set)(ctx, k.as_ptr(), v.as_ptr(), 0);
            }
            let r = (a.avcodec_open2)(ctx, dec, std::ptr::null_mut());
            if r < 0 {
                (a.avcodec_free_context)(&mut ctx);
                return Err(ff_err("avcodec_open2", av_err(a, r)));
            }
            let pkt = (a.av_packet_alloc)();
            if pkt.is_null() {
                (a.avcodec_free_context)(&mut ctx);
                return Err(ff_err("av_packet_alloc", "échec"));
            }
            Ok(FfDecoder {
                ctx,
                pkt,
                codec,
                wait_idr: true,
                name: format!("FFmpeg {} D3D11VA ({})", decoder_name(codec), a.version),
                width: 0,
                height: 0,
            })
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    pub fn needs_keyframe(&self) -> bool {
        self.wait_idr
    }

    /// Décode une access unit ; renvoie la dernière image sortie. `Ok(None)` + `skipped` si l'on
    /// attend encore une image clé.
    pub fn decode(&mut self, data: &[u8], skipped: &mut u64) -> Result<Option<FfFrame>> {
        let a = api().map_err(RenderError::Other)?;
        if self.wait_idr {
            if !annexb::contains_random_access(self.codec, data) {
                *skipped += 1;
                return Ok(None);
            }
            self.wait_idr = false;
        }
        if data.is_empty() || data.len() > i32::MAX as usize - AV_INPUT_BUFFER_PADDING_SIZE {
            return Err(ff_err("paquet", "taille invalide"));
        }
        unsafe {
            let buf = (a.av_malloc)(data.len() + AV_INPUT_BUFFER_PADDING_SIZE) as *mut u8;
            if buf.is_null() {
                return Err(ff_err("av_malloc", "mémoire"));
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), buf, data.len());
            std::ptr::write_bytes(buf.add(data.len()), 0, AV_INPUT_BUFFER_PADDING_SIZE);
            // Le paquet devient propriétaire du tampon (libéré par av_packet_unref interne).
            let r = (a.av_packet_from_data)(self.pkt, buf, data.len() as c_int);
            if r < 0 {
                (a.av_free)(buf as *mut c_void);
                return Err(ff_err("av_packet_from_data", av_err(a, r)));
            }
            let r = (a.avcodec_send_packet)(self.ctx, self.pkt);
            // avcodec_send_packet a pris sa propre référence (ou a échoué) : on libère la nôtre.
            let mut p = self.pkt;
            (a.av_packet_free)(&mut p);
            self.pkt = (a.av_packet_alloc)();
            if self.pkt.is_null() {
                return Err(ff_err("av_packet_alloc", "échec"));
            }
            if r < 0 && r != AVERROR_EAGAIN {
                return Err(ff_err("avcodec_send_packet", av_err(a, r)));
            }
            let mut latest = None;
            loop {
                let mut f = (a.av_frame_alloc)();
                if f.is_null() {
                    return Err(ff_err("av_frame_alloc", "échec"));
                }
                let r = (a.avcodec_receive_frame)(self.ctx, f);
                if r < 0 {
                    (a.av_frame_free)(&mut f);
                    if r == AVERROR_EAGAIN {
                        break;
                    }
                    return Err(ff_err("avcodec_receive_frame", av_err(a, r)));
                }
                let keep = FrameRef(f);
                let fmt = *((f as *const u8).add(OFF_FRAME_FORMAT) as *const c_int);
                if fmt != AV_PIX_FMT_D3D11 {
                    // Le hwaccel a refusé le flux (profil non supporté par le GPU) : FFmpeg est
                    // passé en logiciel, ce que le chemin zéro copie ne gère pas.
                    return Err(ff_err("hwaccel D3D11VA", format!("image non matérielle (format {fmt}) : profil non supporté par le GPU ?")));
                }
                let data0 = *(f as *const *mut c_void);
                let index = *((f as *const *mut c_void).add(1)) as usize as u32;
                let w = *((f as *const u8).add(OFF_FRAME_WIDTH) as *const c_int) as u32;
                let h = *((f as *const u8).add(OFF_FRAME_HEIGHT) as *const c_int) as u32;
                let tex = ID3D11Texture2D::from_raw_borrowed(&data0)
                    .cloned()
                    .ok_or_else(|| ff_err("image", "texture D3D11 nulle"))?;
                self.width = w;
                self.height = h;
                latest = Some((tex, index, w, h, keep));
            }
            Ok(latest)
        }
    }

    pub fn reset(&mut self) {
        if let Ok(a) = api() {
            unsafe { (a.avcodec_flush_buffers)(self.ctx) };
        }
        self.wait_idr = true;
    }
}

impl Drop for FfDecoder {
    fn drop(&mut self) {
        if let Ok(a) = api() {
            unsafe {
                (a.av_packet_free)(&mut self.pkt);
                (a.avcodec_free_context)(&mut self.ctx);
            }
        }
    }
}
