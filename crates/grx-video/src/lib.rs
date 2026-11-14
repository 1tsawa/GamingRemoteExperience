//! grx-video : capture d'écran (DXGI Desktop Duplication) + encodage matériel
//! H.264/H.265 (Media Foundation / MFT NVIDIA) avec le minimum de latence.
//!
//! Pipeline entièrement sur le GPU :
//!
//! ```text
//! DXGI Desktop Duplication  ->  texture BGRA (D3D11, ring de 3)
//!   -> ID3D11VideoProcessor (BGRA -> NV12, scaling éventuel)
//!   -> IMFSample(IMFDXGIBuffer) -> MFT NVENC (hevc/h264) -> bitstream Annex-B
//! ```
//!
//! Aucune copie CPU des pixels ; seul le bitstream compressé est lu côté CPU.
//!
//! ```no_run
//! use grx_video::*;
//! let mut cap = ScreenCapturer::new()?;
//! let (w, h) = cap.size();
//! let mut enc = Encoder::with_device(cap.device(), EncoderConfig {
//!     codec: Codec::Hevc, width: w, height: h, fps: 60, bitrate_kbps: 20_000, ..Default::default()
//! })?;
//! while let Some(frame) = cap.capture(100)? {
//!     let out = enc.encode(&frame)?;
//!     // out.data = Annex-B (VPS/SPS/PPS + IDR sur les keyframes)
//! }
//! # Ok::<(), grx_video::VideoError>(())
//! ```
#![cfg(windows)]

mod blob;
mod capture;
pub mod color;
mod convert;
mod displayconfig;
pub mod cursor;
mod encoder;
mod error;
mod facade;
mod gpu;
pub mod mft_async;
pub mod nal;
mod nvenc;
mod nvenc_abi;
pub mod obu;
pub mod openh264;
pub mod probe;
mod probe_run;
pub mod select;
mod shader;
mod soft;
pub mod streaminfo;

pub use capture::{capturable, hdr_test_pattern, list_displays, list_outputs, output_is_hdr, CapturedFrame, OutputInfo, ScreenCapturer};
pub use displayconfig::{monitor_names, MonitorName};
pub use cursor::{cursor_screen_pos, CursorImage, CursorState, CursorTracker};
pub use facade::{available_encoders, encoders_for_adapter, Encoder, EncoderBackend, EncoderInfo, EncoderKind};
pub use probe_run::{adapter_id, hardware_adapters, probe_adapter, AdapterId, PROBE_SIZE};
pub use select::{EncoderPref, Vendor};
/// Types D3D11 réexportés (crates sans dépendance `windows`).
pub use windows::Win32::Graphics::Direct3D11::{ID3D11Device as D3D11Device, ID3D11Texture2D as D3D11Texture};
/// API NVENC directe utilisable (`Ok("API NVENC x.y")`) ou la raison.
pub fn nvenc_availability() -> std::result::Result<String, String> {
    nvenc::availability()
}
pub use error::{Result, VideoError};
pub use gpu::{create_device_for_adapter, list_adapters, AdapterInfo};

/// Codec vidéo supporté.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    /// H.264 / AVC (profil High).
    H264,
    /// H.265 / HEVC (profil Main 8 bits 4:2:0).
    Hevc,
    /// AV1 (profil Main 8 bits 4:2:0), flux d'OBU « low overhead » (voir [`obu`]).
    Av1,
}

impl Codec {
    /// Extension de fichier conventionnelle pour un flux brut Annex-B.
    pub fn file_extension(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "h265",
            Codec::Av1 => "obu",
        }
    }
}

/// Format d'image encodé (sous-échantillonnage x profondeur).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PixelFormat {
    /// 4:2:0 8 bits (NV12) : le format de base.
    #[default]
    Yuv420,
    /// 4:2:0 10 bits (P010) : HEVC Main10 / AV1 10 bits.
    Yuv420P10,
    /// 4:4:4 8 bits : H.264 High 4:4:4 / HEVC RExt (NVENC direct).
    Yuv444,
    /// 4:4:4 10 bits : HEVC RExt (NVENC direct).
    Yuv444P10,
}

impl PixelFormat {
    pub const ALL: [PixelFormat; 4] = [PixelFormat::Yuv420, PixelFormat::Yuv420P10, PixelFormat::Yuv444, PixelFormat::Yuv444P10];

    pub fn new(yuv444: bool, ten_bit: bool) -> PixelFormat {
        match (yuv444, ten_bit) {
            (false, false) => PixelFormat::Yuv420,
            (false, true) => PixelFormat::Yuv420P10,
            (true, false) => PixelFormat::Yuv444,
            (true, true) => PixelFormat::Yuv444P10,
        }
    }
    pub fn is_444(self) -> bool {
        matches!(self, PixelFormat::Yuv444 | PixelFormat::Yuv444P10)
    }
    pub fn is_10bit(self) -> bool {
        matches!(self, PixelFormat::Yuv420P10 | PixelFormat::Yuv444P10)
    }
    pub fn label(self) -> &'static str {
        match self {
            PixelFormat::Yuv420 => "4:2:0 8 bits",
            PixelFormat::Yuv420P10 => "4:2:0 10 bits",
            PixelFormat::Yuv444 => "4:4:4 8 bits",
            PixelFormat::Yuv444P10 => "4:4:4 10 bits",
        }
    }
}

/// Paramètres de l'encodeur.
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub codec: Codec,
    /// Largeur encodée (la source est mise à l'échelle sur le GPU si différente).
    pub width: u32,
    /// Hauteur encodée.
    pub height: u32,
    /// Fréquence nominale (sert au contrôle de débit CBR).
    pub fps: u32,
    /// Débit cible en kbit/s (CBR).
    pub bitrate_kbps: u32,
    /// Encodeur voulu (`run --encoder`), `Auto` = adaptateur de l'écran capturé puis logiciel.
    pub pref: EncoderPref,
    /// Format encodé (4:2:0 8 bits par défaut).
    pub format: PixelFormat,
    /// Flux HDR10 (PQ, BT.2020) : source HDR et format 10 bits.
    pub hdr: bool,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        EncoderConfig {
            codec: Codec::Hevc,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20_000,
            pref: EncoderPref::Auto,
            format: PixelFormat::Yuv420,
            hdr: false,
        }
    }
}

/// Une image encodée (une access unit complète, format Annex-B).
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// Bitstream Annex-B. Les keyframes embarquent VPS/SPS/PPS (SPS/PPS en H.264).
    pub data: Vec<u8>,
    /// `true` pour une image IDR (point d'accès aléatoire).
    pub is_keyframe: bool,
    /// Horodatage de présentation en microsecondes (depuis le début du flux).
    pub pts: u64,
    /// Résolution encodée de cette image.
    pub width: u32,
    pub height: u32,
}
