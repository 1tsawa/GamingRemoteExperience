//! grx-render : décodage vidéo matériel (H.265/H.264, Media Foundation + DXVA/NVDEC) et affichage
//! à faible latence (winit + swapchain DXGI flip-model).
//!
//! ```text
//! EncodedFrame (Annex-B) -> VideoDecoder::decode -> DecodedFrame (texture NV12 GPU)
//!   -> PresenterHandle::submit -> ID3D11VideoProcessor (NV12->BGRA + scaling letterbox)
//!   -> swapchain flip-model (waitable, tearing) -> écran
//! ```
//!
//! Prérequis H.265 : l'extension Windows « HEVC Video Extensions » (MFT `HEVCVideoExtension`)
//! doit être installée ; sinon `VideoDecoder::new(Codec::Hevc)` renvoie `RenderError::NoDecoder`
//! et l'appelant doit négocier H.264.
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
mod cursor;
mod decoder;
mod error;
mod gpu;
mod overlay;
mod presenter;

pub use annexb::{
    contains_idr, contains_parameter_sets, contains_random_access, nal_types, sniff_codec, split_access_units,
    split_nals, Nal,
};
pub use blit::fit_rect;
pub use cursor::{cursor_quad, CursorShape, RemoteCursor};
pub use decoder::{DecodedFrame, DecoderStats, VideoDecoder};
pub use error::{RenderError, Result};
pub use gpu::create_device;
pub use overlay::{Align, Color, Font, OverlayItem, OverlayScene, Rect};
pub use presenter::{
    winit, LatencyStats, Presenter, PresenterConfig, PresenterEvent, PresenterHandle, PresenterStats,
};
