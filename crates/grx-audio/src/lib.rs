//! grx-audio : audio host -> client a faible latence.
//!
//! * HOST   : [`AudioCapturer`] (WASAPI loopback) -> trames PCM 48 kHz stereo f32 de 10 ms,
//!            puis [`OpusEncoder`].
//! * CLIENT : [`JitterBuffer`] -> [`OpusDecoder`] -> [`StreamResampler`] -> [`AudioPlayer`] (WASAPI).
//!
//! Le codec est libopus 1.6.1 (sources vendorisees, compilees par `build.rs` avec `cc` + MSVC) : pas de cmake.
//!
//! Choix libopus : sources officielles 1.6.1 vendorisees dans `vendor/opus` (BSD-3, voir `vendor/opus/COPYING`),
//! compilees par `build.rs` avec la crate `cc` (cl.exe de la toolchain MSVC deja requise par Rust) ; pas de cmake.
//! Rejete : `unsafe-libopus` (c2rust, sans C) -> son PLC/FEC SILK/hybride produit du bruit pleine echelle
//! (mesure : rms 0,98 au lieu de ~0,1) ; `audiopus_sys`/`opusic-sys`/`opus-static-sys` exigent cmake.
//! Mise a jour : recopier celt/ silk/ silk/float/ src/ include/ depuis une release libopus et regenerer `vendor/sources.txt`.
//!
//! Les modules `capture` et `player` n'existent que sous Windows ; codec, jitter buffer et
//! resampler sont portables et testables sans peripherique audio.

pub mod ffi;
pub mod jitter;
pub mod opus;
pub mod resample;

#[cfg(windows)]
mod winutil;
#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod player;

pub use jitter::{DriftController, JitterBuffer, JitterConfig, JitterStats, Pop};
pub use opus::{Application, FrameDuration, OpusConfig, OpusDecoder, OpusEncoder, RateMode};
pub use resample::StreamResampler;

#[cfg(windows)]
pub use capture::{AudioCapturer, AudioFrame, CaptureConfig, CaptureStats};
#[cfg(windows)]
pub use player::{AudioPlayer, PlayerConfig, PlayerStats};

/// Frequence d'echantillonnage interne (Hz) : celle d'Opus et du flux reseau.
pub const SAMPLE_RATE: u32 = 48_000;
/// Nombre de canaux (stereo).
pub const CHANNELS: usize = 2;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("opus: {0}")]
    Opus(String),
    #[error("parametre invalide: {0}")]
    BadArg(String),
    #[error("wasapi: {0}")]
    Device(String),
}

pub type Result<T> = std::result::Result<T, AudioError>;

/// f32 [-1,1] -> i16 (saturant).
pub fn f32_to_i16(src: &[f32]) -> Vec<i16> {
    src.iter()
        .map(|&s| (s * 32768.0).round().clamp(-32768.0, 32767.0) as i16)
        .collect()
}

/// i16 -> f32 [-1,1).
pub fn i16_to_f32(src: &[i16]) -> Vec<f32> {
    src.iter().map(|&s| s as f32 / 32768.0).collect()
}
