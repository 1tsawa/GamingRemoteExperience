//! Declarations FFI minimales de libopus (1.6.1, compilee par `build.rs`).
#![allow(non_camel_case_types, dead_code)]

use std::os::raw::c_char;

#[repr(C)]
pub struct OpusEncoder {
    _p: [u8; 0],
}
#[repr(C)]
pub struct OpusDecoder {
    _p: [u8; 0],
}

pub const OPUS_APPLICATION_VOIP: i32 = 2048;
pub const OPUS_APPLICATION_AUDIO: i32 = 2049;
pub const OPUS_APPLICATION_RESTRICTED_LOWDELAY: i32 = 2051;
pub const OPUS_AUTO: i32 = -1000;
pub const OPUS_FRAMESIZE_10_MS: i32 = 5003;
pub const OPUS_FRAMESIZE_20_MS: i32 = 5004;
pub const OPUS_SET_BITRATE_REQUEST: i32 = 4002;
pub const OPUS_SET_VBR_REQUEST: i32 = 4006;
pub const OPUS_SET_COMPLEXITY_REQUEST: i32 = 4010;
pub const OPUS_SET_INBAND_FEC_REQUEST: i32 = 4012;
pub const OPUS_SET_PACKET_LOSS_PERC_REQUEST: i32 = 4014;
pub const OPUS_SET_DTX_REQUEST: i32 = 4016;
pub const OPUS_SET_VBR_CONSTRAINT_REQUEST: i32 = 4020;
pub const OPUS_SET_SIGNAL_REQUEST: i32 = 4024;
pub const OPUS_SET_EXPERT_FRAME_DURATION_REQUEST: i32 = 4040;
pub const OPUS_SET_MAX_BANDWIDTH_REQUEST: i32 = 4004;
pub const OPUS_BANDWIDTH_NARROWBAND: i32 = 1101;
pub const OPUS_BANDWIDTH_WIDEBAND: i32 = 1103;
pub const OPUS_BANDWIDTH_SUPERWIDEBAND: i32 = 1104;

extern "C" {
    pub fn opus_encoder_create(fs: i32, channels: i32, application: i32, error: *mut i32) -> *mut OpusEncoder;
    pub fn opus_encoder_destroy(st: *mut OpusEncoder);
    pub fn opus_encoder_ctl(st: *mut OpusEncoder, request: i32, ...) -> i32;
    pub fn opus_encode_float(st: *mut OpusEncoder, pcm: *const f32, frame_size: i32, data: *mut u8, max_data_bytes: i32) -> i32;

    pub fn opus_decoder_create(fs: i32, channels: i32, error: *mut i32) -> *mut OpusDecoder;
    pub fn opus_decoder_destroy(st: *mut OpusDecoder);
    pub fn opus_decode_float(st: *mut OpusDecoder, data: *const u8, len: i32, pcm: *mut f32, frame_size: i32, decode_fec: i32) -> i32;

    pub fn opus_get_version_string() -> *const c_char;
}

/// Version de libopus compilee.
pub fn version() -> String {
    unsafe { std::ffi::CStr::from_ptr(opus_get_version_string()).to_string_lossy().into_owned() }
}

extern "C" {
    /// 1 si le paquet contient du FEC (LBRR), 0 sinon, <0 erreur.
    pub fn opus_packet_has_lbrr(packet: *const u8, len: i32) -> i32;
}

/// Vrai si le paquet embarque du FEC "in-band" (LBRR) de la trame precedente.
pub fn packet_has_fec(p: &[u8]) -> bool {
    unsafe { opus_packet_has_lbrr(p.as_ptr(), p.len() as i32) == 1 }
}
