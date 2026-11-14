//! grx-render : décodage vidéo matériel (H.265/H.264, Media Foundation + DXVA/NVDEC) et affichage
//! à faible latence (winit + swapchain DXGI flip-model).
//!
//! ```text
//! EncodedFrame (Annex-B) -> VideoDecoder::decode -> DecodedFrame (texture NV12 GPU)
//!   -> PresenterHandle::submit -> ID3D11VideoProcessor (NV12->BGRA + scaling letterbox)
//!   -> swapchain flip-model (waitable, tearing) -> écran
//! ```
//!
//! Voies de décodage (chaîne de repli, [`VideoDecoder::create_best`]) : Media Foundation (H.264
//! intégré ; HEVC/AV1 via les extensions « HEVC Video Extensions » / « AV1 Video Extension » du
//! Store), puis FFmpeg D3D11VA (DLL LGPL facultatives chargées dynamiquement, voir `ffmpeg.rs`).
//! [`probe_decoders`] sonde ce que le GPU décode réellement ; le client l'annonce au host, qui
//! choisit le codec (H.264 si rien d'autre n'est disponible).
//!
//! ```no_run
//! use grx_render::*;
//! use grx_video::Codec;
//! let presenter = Presenter::new(PresenterConfig::default())?;
//! let handle = presenter.handle();
//! let device = presenter.device().clone();
//! std::thread::spawn(move || {
//!     let mut dec = VideoDecoder::with_device(&device, Codec::Hevc).unwrap();
//!     # let packets: Vec<Vec<u8>> = vec![];
//!     for pkt in packets {
//!         if let Ok(Some(frame)) = dec.decode(&pkt) { handle.submit(frame); }
//!     }
//! });
//! presenter.run(|_event, _window| false)?; // `_event` : événements winit bruts
//! # Ok::<(), RenderError>(())
//! ```
#![cfg(windows)]

mod annexb;
mod blit;
mod chain;
mod cursor;
mod decoder;
mod ffmpeg;
mod error;
mod gpu;
mod hdr_present;
mod overlay;
mod presenter;
mod soft_decoder;
mod video_decoder;

pub use annexb::{
    contains_idr, contains_parameter_sets, contains_random_access, nal_types, sniff_codec, split_access_units,
    split_nals, Nal,
};
pub use blit::fit_rect;
pub use cursor::{cursor_quad, CursorShape, RemoteCursor};
pub use chain::{codec_key, create_with, create_with_pref, Attempt, ChainError, DecoderKind, DecoderPref, DisabledDecoders};
pub use decoder::{DecodedFrame, DecoderStats};
pub use video_decoder::{gpu_supports, gpu_supports_format, kind_formats, probe_decoders, ProbeEntry, VideoDecoder};
/// Voie FFmpeg D3D11VA : `Ok(version et dossier)` si les DLL LGPL sont chargeables, sinon la raison.
pub fn ffmpeg_availability() -> std::result::Result<String, String> {
    ffmpeg::availability()
}
pub use error::{RenderError, Result};
pub use gpu::create_device;
/// Device D3D11 partagé décodeur/présentateur (réexporté pour les crates sans dépendance `windows`).
pub use windows::Win32::Graphics::Direct3D11::ID3D11Device as D3D11Device;
pub use overlay::{Align, Color, Font, OverlayItem, OverlayScene, Rect};
pub use presenter::{
    winit, LatencyStats, Presenter, PresenterConfig, PresenterEvent, PresenterHandle, PresenterStats,
};
