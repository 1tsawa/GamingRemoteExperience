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
//!     codec: Codec::Hevc, width: w, height: h, fps: 60, bitrate_kbps: 20_000,
//! })?;
//! while let Some(frame) = cap.capture(100)? {
//!     let out = enc.encode(&frame)?;
//!     // out.data = Annex-B (VPS/SPS/PPS + IDR sur les keyframes)
//! }
//! # Ok::<(), grx_video::VideoError>(())
//! ```
#![cfg(windows)]

mod capture;
mod convert;
pub mod cursor;
mod encoder;
mod error;
mod gpu;
pub mod nal;

pub use capture::{list_outputs, CapturedFrame, OutputInfo, ScreenCapturer};
pub use cursor::{CursorImage, CursorState, CursorTracker};
pub use encoder::{Encoder, EncoderBackend};
pub use error::{Result, VideoError};
pub use gpu::{create_device_for_adapter, list_adapters, AdapterInfo};

/// Codec vidéo supporté.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    /// H.264 / AVC (profil High).
    H264,
    /// H.265 / HEVC (profil Main 8 bits 4:2:0).
    Hevc,
}

impl Codec {
    /// Extension de fichier conventionnelle pour un flux brut Annex-B.
    pub fn file_extension(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "h265",
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
