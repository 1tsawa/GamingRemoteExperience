//! Décodeurs logiciels de secours : OpenH264 (H.264 4:2:0 8 bits) et FFmpeg sans hwaccel
//! (H.264/HEVC/AV1, 4:2:0/4:4:4, 8/10 bits). L'image CPU est téléversée dans une texture du
//! présentateur : NV12, P010, AYUV ou Y410 (formats d'entrée du VideoProcessor), via une texture
//! intermédiaire STAGING puis `CopyResource` (pool tournant de 3 textures).

use grx_video::color;
use grx_video::openh264::H264Decoder;
use grx_video::Codec;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::annexb;
use crate::decoder::{DecodedFrame, DecoderStats, FrameKeep};
use crate::error::{RenderError, Result, WinCtx};
use crate::ffmpeg::{CpuPicture, FfDecoder, Planar};

const POOL: usize = 3;

struct Uploader {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    key: (DXGI_FORMAT, u32, u32),
    staging: Option<ID3D11Texture2D>,
    pool: Vec<ID3D11Texture2D>,
    pos: usize,
}

impl Uploader {
    fn new(device: &ID3D11Device) -> Result<Uploader> {
        let ctx = unsafe { device.GetImmediateContext() }.ctx("GetImmediateContext")?;
        Ok(Uploader { device: device.clone(), ctx, key: (DXGI_FORMAT(0), 0, 0), staging: None, pool: Vec::new(), pos: 0 })
    }

    fn ensure(&mut self, fmt: DXGI_FORMAT, w: u32, h: u32) -> Result<()> {
        if self.key == (fmt, w, h) && self.staging.is_some() {
            return Ok(());
        }
        let mut desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: fmt,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
        };
        let mut st = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut st)) }.ctx("CreateTexture2D(staging logiciel)")?;
        desc.Usage = D3D11_USAGE_DEFAULT;
        desc.CPUAccessFlags = 0;
        // RENDER_TARGET : le VideoProcessor (NVIDIA) refuse les vues d'entrée sans ce flag.
        desc.BindFlags = (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32;
        self.pool.clear();
        for _ in 0..POOL {
            let mut t = None;
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut t)) }.ctx("CreateTexture2D(image logicielle)")?;
            self.pool.push(t.unwrap());
        }
        self.staging = st;
        self.key = (fmt, w, h);
        Ok(())
    }

    /// Remplit la texture STAGING (`fill(octets, pas)`) et la copie dans la prochaine texture du pool.
    fn upload(&mut self, fmt: DXGI_FORMAT, w: u32, h: u32, fill: impl FnOnce(&mut [u8], usize)) -> Result<ID3D11Texture2D> {
        self.ensure(fmt, w, h)?;
        let st = self.staging.clone().unwrap();
        let planar = fmt == DXGI_FORMAT_NV12 || fmt == DXGI_FORMAT_P010;
        unsafe {
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(&st, 0, D3D11_MAP_WRITE, 0, Some(&mut m)).ctx("Map(staging logiciel)")?;
            let pitch = m.RowPitch as usize;
            // NV12/P010 : plan UV juste après les `h` lignes de luma (même pas).
            let rows = if planar { h as usize + (h as usize).div_ceil(2) } else { h as usize };
            let buf = std::slice::from_raw_parts_mut(m.pData as *mut u8, pitch * rows);
            fill(buf, pitch);
            self.ctx.Unmap(&st, 0);
        }
        let t = self.pool[self.pos].clone();
        self.pos = (self.pos + 1) % POOL;
        unsafe { self.ctx.CopyResource(&t, &st) };
        Ok(t)
    }
}

enum Engine {
    OpenH264(H264Decoder),
    Ff(FfDecoder),
}

pub(crate) struct SoftDecoder {
    engine: Engine,
    up: Uploader,
    codec: Codec,
    wait_idr: bool,
    width: u32,
    height: u32,
    name: String,
    pub stats: DecoderStats,
    pub device: ID3D11Device,
    /// Transfert PQ signalé par le flux (FFmpeg) ; OpenH264 : toujours SDR.
    pub pq: bool,
}

// SAFETY : décodeurs utilisés via &mut self ; device protégé multithread.
unsafe impl Send for SoftDecoder {}

/// Reinterprète des octets alignés comme des u16 (plans 10 bits petit-boutistes).
fn as_u16(b: &[u8]) -> &[u16] {
    // SAFETY : tampons FFmpeg alignés (>= 16 o) ; longueur arrondie au pair.
    unsafe { std::slice::from_raw_parts(b.as_ptr() as *const u16, b.len() / 2) }
}

fn as_u16_mut(b: &mut [u8]) -> &mut [u16] {
    // SAFETY : mémoire mappée D3D11 alignée (>= 16 o).
    unsafe { std::slice::from_raw_parts_mut(b.as_mut_ptr() as *mut u16, b.len() / 2) }
}

fn as_u32_mut(b: &mut [u8]) -> &mut [u32] {
    // SAFETY : mémoire mappée D3D11 alignée (>= 16 o).
    unsafe { std::slice::from_raw_parts_mut(b.as_mut_ptr() as *mut u32, b.len() / 4) }
}

impl SoftDecoder {
    pub fn openh264(device: &ID3D11Device) -> Result<SoftDecoder> {
        let d = H264Decoder::new().map_err(RenderError::Other)?;
        let name = format!("{} (logiciel)", d.version());
        Self::make(device, Engine::OpenH264(d), Codec::H264, name)
    }

    pub fn ffmpeg(device: &ID3D11Device, codec: Codec) -> Result<SoftDecoder> {
        let d = FfDecoder::new_software(codec)?;
        let name = d.name().to_string();
        Self::make(device, Engine::Ff(d), codec, name)
    }

    fn make(device: &ID3D11Device, engine: Engine, codec: Codec, name: String) -> Result<SoftDecoder> {
        Ok(SoftDecoder {
            engine,
            up: Uploader::new(device)?,
            codec,
            wait_idr: true,
            width: 0,
            height: 0,
            name,
            stats: DecoderStats::default(),
            device: device.clone(),
            pq: false,
        })
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

    pub fn reset(&mut self) {
        self.wait_idr = true;
        if let Engine::Ff(d) = &mut self.engine {
            d.reset();
        }
    }

    pub fn decode_pts(&mut self, data: &[u8], pts: u64) -> Result<Option<DecodedFrame>> {
        self.stats.packets_in += 1;
        if self.wait_idr {
            if !annexb::contains_random_access(self.codec, data) {
                self.stats.skipped_waiting_idr += 1;
                return Ok(None);
            }
            self.wait_idr = false;
            if let Engine::Ff(d) = &mut self.engine {
                // FFmpeg a sa propre attente d'IDR : déjà satisfaite ici.
                let _ = d.needs_keyframe();
            }
        }
        let tex = match &mut self.engine {
            Engine::OpenH264(d) => {
                let Some(p) = d.decode(data).map_err(|e| RenderError::Other(format!("OpenH264 : {e}")))? else {
                    return Ok(None);
                };
                let (w, h) = (p.width & !1, p.height & !1);
                let (y, u, v, ys, cs) = (p.y, p.u, p.v, p.y_stride, p.c_stride);
                let t = self.up.upload(DXGI_FORMAT_NV12, w, h, |buf, pitch| {
                    color::i420_to_nv12(y, ys, u, v, cs, w as usize, h as usize, buf, pitch)
                })?;
                self.width = w;
                self.height = h;
                t
            }
            Engine::Ff(d) => {
                let mut skipped = 0;
                let Some(p) = d.decode_cpu(data, &mut skipped)? else { return Ok(None) };
                self.pq = p.pq;
                let (t, w, h) = upload_picture(&mut self.up, &p)?;
                self.width = w;
                self.height = h;
                t
            }
        };
        self.stats.frames_out += 1;
        Ok(Some(DecodedFrame {
            texture: tex,
            subresource: 0,
            width: self.width,
            height: self.height,
            pts,
            hdr: self.pq,
            _keep: FrameKeep::Soft,
        }))
    }
}

/// Téléverse une image FFmpeg : 4:2:0 -> NV12/P010, 4:4:4 -> AYUV/Y410.
fn upload_picture(up: &mut Uploader, p: &CpuPicture) -> Result<(ID3D11Texture2D, u32, u32)> {
    let plane = |i: usize, rows: usize| -> &[u8] {
        // SAFETY : plans de l'AVFrame gardée par `p._keep`, `rows` lignes de `strides[i]` octets.
        unsafe { std::slice::from_raw_parts(p.planes[i], p.strides[i] * rows) }
    };
    let (w, h) = (p.width, p.height);
    match p.layout {
        Planar::Yuv420 | Planar::Yuv420P10 => {
            let (w2, h2) = (w & !1, h & !1);
            let ch = (h2 as usize).div_ceil(2);
            let (y, u, v) = (plane(0, h2 as usize), plane(1, ch), plane(2, ch));
            if p.strides[1] != p.strides[2] {
                return Err(RenderError::Other("plans U/V de pas différents".into()));
            }
            let t = if p.layout == Planar::Yuv420 {
                up.upload(DXGI_FORMAT_NV12, w2, h2, |buf, pitch| {
                    color::i420_to_nv12(y, p.strides[0], u, v, p.strides[1], w2 as usize, h2 as usize, buf, pitch)
                })?
            } else {
                up.upload(DXGI_FORMAT_P010, w2, h2, |buf, pitch| {
                    color::i420p10_to_p010(
                        as_u16(y),
                        p.strides[0] / 2,
                        as_u16(u),
                        as_u16(v),
                        p.strides[1] / 2,
                        w2 as usize,
                        h2 as usize,
                        as_u16_mut(buf),
                        pitch / 2,
                    )
                })?
            };
            Ok((t, w2, h2))
        }
        Planar::Yuv444 | Planar::Yuv444P10 => {
            if p.strides[0] != p.strides[1] || p.strides[1] != p.strides[2] {
                return Err(RenderError::Other("plans 4:4:4 de pas différents".into()));
            }
            let (y, u, v) = (plane(0, h as usize), plane(1, h as usize), plane(2, h as usize));
            let t = if p.layout == Planar::Yuv444 {
                up.upload(DXGI_FORMAT_AYUV, w, h, |buf, pitch| {
                    color::yuv444p_to_ayuv(y, u, v, p.strides[0], w as usize, h as usize, buf, pitch)
                })?
            } else {
                up.upload(DXGI_FORMAT_Y410, w, h, |buf, pitch| {
                    color::yuv444p10_to_y410(as_u16(y), as_u16(u), as_u16(v), p.strides[0] / 2, w as usize, h as usize, as_u32_mut(buf), pitch / 4)
                })?
            };
            Ok((t, w, h))
        }
    }
}
