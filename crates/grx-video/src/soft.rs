//! Encodeur logiciel de secours (host sans encodeur matériel) : capture -> NV12 réduit sur le GPU
//! (VideoProcessor) -> copie de lecture CPU -> I420 -> OpenH264 (H.264 Constrained Baseline).
//!
//! La résolution et le débit sont bornés selon le nombre de cœurs ([`crate::select::soft_size`],
//! [`crate::select::soft_max_kbps`]) ; le client suit la résolution via le SPS.

use std::time::Instant;

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::convert::Converter;
use crate::error::{Result, VideoError, WinCtx};
use crate::openh264::H264Encoder;
use crate::select::{soft_max_kbps, soft_size};
use crate::{color, Codec, EncodedFrame, EncoderConfig, PixelFormat};

pub(crate) struct SoftEncoder {
    enc: H264Encoder,
    conv: Converter,
    ctx: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    out_w: u32,
    out_h: u32,
    max_kbps: u32,
    start: Instant,
    pub name: String,
}

// SAFETY : device protégé multithread, utilisé via &mut self.
unsafe impl Send for SoftEncoder {}

impl SoftEncoder {
    pub fn new(device: &ID3D11Device, cfg: &EncoderConfig) -> Result<SoftEncoder> {
        if cfg.codec != Codec::H264 || cfg.format != PixelFormat::Yuv420 {
            return Err(VideoError::Other("l'encodeur logiciel ne fait que du H.264 4:2:0 8 bits".into()));
        }
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
        let (w, h) = soft_size(cfg.width, cfg.height, cores);
        let max_kbps = soft_max_kbps(w, h);
        let kbps = cfg.bitrate_kbps.min(max_kbps);
        let enc = H264Encoder::new(w, h, cfg.fps, kbps * 1000).map_err(VideoError::Other)?;
        let conv = Converter::new(device, cfg.width, cfg.height, w, h, cfg.fps)?;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut t = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut t)) }.ctx("CreateTexture2D(NV12 staging)")?;
        let ctx = unsafe { device.GetImmediateContext() }.ctx("GetImmediateContext")?;
        let (cw, ch) = (w.div_ceil(2) as usize, h.div_ceil(2) as usize);
        let name = format!("OpenH264 logiciel {w}x{h}, {} threads", enc.threads);
        Ok(SoftEncoder {
            enc,
            conv,
            ctx,
            staging: t.unwrap(),
            y: vec![0; (w * h) as usize],
            u: vec![0; cw * ch],
            v: vec![0; cw * ch],
            out_w: w,
            out_h: h,
            max_kbps,
            start: Instant::now(),
            name,
        })
    }

    pub fn output_size(&self) -> (u32, u32) {
        (self.out_w, self.out_h)
    }

    pub fn request_keyframe(&mut self) {
        self.enc.request_keyframe();
    }

    pub fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        self.enc.set_bitrate(kbps.min(self.max_kbps) * 1000).map_err(VideoError::Other)
    }

    pub fn encode_texture(&mut self, tex: &ID3D11Texture2D, w: u32, h: u32) -> Result<EncodedFrame> {
        let nv12 = self.conv.convert(tex, w, h)?;
        let (ow, oh) = (self.out_w as usize, self.out_h as usize);
        unsafe {
            self.ctx.CopyResource(&self.staging, &nv12);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut m)).ctx("Map(NV12 staging)")?;
            let pitch = m.RowPitch as usize;
            let base = m.pData as *const u8;
            let ysl = std::slice::from_raw_parts(base, pitch * oh);
            let uvsl = std::slice::from_raw_parts(base.add(pitch * oh), pitch * oh.div_ceil(2));
            color::nv12_to_i420(ysl, pitch, uvsl, pitch, ow, oh, &mut self.y, &mut self.u, &mut self.v);
            self.ctx.Unmap(&self.staging, 0);
        }
        let ts = self.start.elapsed().as_millis() as u64;
        let cw = ow.div_ceil(2);
        let (data, key) = self.enc.encode(&self.y, ow, &self.u, &self.v, cw, ts).map_err(VideoError::Other)?;
        Ok(EncodedFrame { data, is_keyframe: key, pts: ts * 1000, width: self.out_w, height: self.out_h })
    }
}
