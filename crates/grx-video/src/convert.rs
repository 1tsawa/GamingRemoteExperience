//! Conversion BGRA -> NV12 (+ mise à l'échelle) entièrement sur le GPU via ID3D11VideoProcessor.

use crate::error::{Result, VideoError, WinCtx};
use windows::core::Interface;
use windows::Win32::Foundation::{BOOL, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

const POOL_SIZE: usize = 6;

pub(crate) struct Converter {
    device: ID3D11Device,
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    in_w: u32,
    in_h: u32,
    in_format: DXGI_FORMAT,
    out_w: u32,
    out_h: u32,
    fps: u32,
    pool: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    pos: usize,
    // Cache de vues d'entrée par texture source (pointeur COM brut).
    input_views: Vec<(usize, ID3D11VideoProcessorInputView)>,
    context: ID3D11DeviceContext,
    // Texture intermédiaire (BIND_RENDER_TARGET) pour les sources qui n'ont pas ce flag.
    staging: Option<ID3D11Texture2D>,
}

impl Converter {
    pub fn new(device: &ID3D11Device, in_w: u32, in_h: u32, out_w: u32, out_h: u32, fps: u32) -> Result<Self> {
        let vdev: ID3D11VideoDevice = device.cast().ctx("ID3D11VideoDevice")?;
        let ctx = unsafe { device.GetImmediateContext() }.ctx("GetImmediateContext")?;
        let vctx: ID3D11VideoContext = ctx.cast().ctx("ID3D11VideoContext")?;
        let (enumerator, processor) = Self::make_processor(&vdev, in_w, in_h, out_w, out_h, fps)?;
        if std::env::var("GRX_VP_DEBUG").is_ok() {
            for f in [DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_R10G10B10A2_UNORM] {
                eprintln!("VP format {:?}: flags {:#x}", f, unsafe { enumerator.CheckVideoProcessorFormat(f) }.unwrap_or(0));
            }
        }
        let mut c = Converter {
            device: device.clone(),
            vdev,
            vctx,
            enumerator,
            processor,
            in_w,
            in_h,
            in_format: DXGI_FORMAT(0),
            out_w,
            out_h,
            fps,
            pool: Vec::new(),
            pos: 0,
            input_views: Vec::new(),
            context: ctx,
            staging: None,
        };
        c.setup_processor();
        c.create_pool()?;
        Ok(c)
    }

    fn make_processor(
        vdev: &ID3D11VideoDevice,
        in_w: u32,
        in_h: u32,
        out_w: u32,
        out_h: u32,
        fps: u32,
    ) -> Result<(ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor)> {
        let rate = DXGI_RATIONAL { Numerator: fps.max(1), Denominator: 1 };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: in_w,
            InputHeight: in_h,
            OutputFrameRate: rate,
            OutputWidth: out_w,
            OutputHeight: out_h,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumerator = unsafe { vdev.CreateVideoProcessorEnumerator(&desc) }.ctx("CreateVideoProcessorEnumerator")?;
        let processor = unsafe { vdev.CreateVideoProcessor(&enumerator, 0) }.ctx("CreateVideoProcessor")?;
        Ok((enumerator, processor))
    }

    fn setup_processor(&mut self) {
        unsafe {
            let vp = &self.processor;
            self.vctx.VideoProcessorSetStreamFrameFormat(vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            self.vctx.VideoProcessorSetStreamAutoProcessingMode(vp, 0, false);
            // Entrée : RGB plein débit (0-255). Sortie : YCbCr BT.709, plage limitée 16-235.
            let in_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            let out_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
            self.vctx.VideoProcessorSetStreamColorSpace(vp, 0, &in_cs);
            self.vctx.VideoProcessorSetOutputColorSpace(vp, &out_cs);
            let src = RECT { left: 0, top: 0, right: self.in_w as i32, bottom: self.in_h as i32 };
            let dst = RECT { left: 0, top: 0, right: self.out_w as i32, bottom: self.out_h as i32 };
            self.vctx.VideoProcessorSetStreamSourceRect(vp, 0, true, Some(&src));
            self.vctx.VideoProcessorSetStreamDestRect(vp, 0, true, Some(&dst));
            self.vctx.VideoProcessorSetOutputTargetRect(vp, true, Some(&dst));
        }
    }

    fn create_pool(&mut self) -> Result<()> {
        self.pool.clear();
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.out_w,
            Height: self.out_h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        for _ in 0..POOL_SIZE {
            let mut t = None;
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut t)) }.ctx("CreateTexture2D(NV12)")?;
            let t = t.unwrap();
            let vd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut v = None;
            unsafe { self.vdev.CreateVideoProcessorOutputView(&t, &self.enumerator, &vd, Some(&mut v)) }
                .ctx("CreateVideoProcessorOutputView")?;
            self.pool.push((t, v.unwrap()));
        }
        Ok(())
    }

    /// Convertit `src` (taille `w`x`h`) en NV12 `out_w`x`out_h`. La texture rendue vient d'un pool tournant.
    pub fn convert(&mut self, src: &ID3D11Texture2D, w: u32, h: u32) -> Result<ID3D11Texture2D> {
        let mut sd = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut sd) };
        // Le VideoProcessor (NVIDIA) refuse les vues d'entrée sur des textures sans BIND_RENDER_TARGET :
        // on passe alors par une copie GPU->GPU (quelques dizaines de µs).
        let mut src = src.clone();
        if sd.BindFlags & D3D11_BIND_RENDER_TARGET.0 as u32 == 0 {
            let reuse = match &self.staging {
                Some(t) => {
                    let mut d = D3D11_TEXTURE2D_DESC::default();
                    unsafe { t.GetDesc(&mut d) };
                    d.Width == sd.Width && d.Height == sd.Height && d.Format == sd.Format
                }
                None => false,
            };
            if !reuse {
                let desc = D3D11_TEXTURE2D_DESC {
                    MipLevels: 1,
                    ArraySize: 1,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                    ..sd
                };
                let mut t = None;
                unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut t)) }.ctx("CreateTexture2D(staging)")?;
                self.staging = t;
            }
            let st = self.staging.clone().unwrap();
            unsafe { self.context.CopyResource(&st, &src) };
            src = st;
        }
        let src = &src;
        if self.in_format.0 == 0 {
            self.in_format = sd.Format;
        }
        if sd.Width != self.in_w || sd.Height != self.in_h || sd.Format != self.in_format {
            // Changement de résolution/format source : on recrée le processeur (les vues sont liées à l'énumérateur).
            let _ = (w, h);
            self.in_w = sd.Width;
            self.in_h = sd.Height;
            self.in_format = sd.Format;
            let (e, p) = Self::make_processor(&self.vdev, self.in_w, self.in_h, self.out_w, self.out_h, self.fps)?;
            self.enumerator = e;
            self.processor = p;
            self.input_views.clear();
            self.setup_processor();
            self.create_pool()?;
        }
        let key = src.as_raw() as usize;
        let in_view = match self.input_views.iter().find(|(k, _)| *k == key) {
            Some((_, v)) => v.clone(),
            None => {
                let vd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                    },
                };
                let mut v = None;
                unsafe { self.vdev.CreateVideoProcessorInputView(src, &self.enumerator, &vd, Some(&mut v)) }
                    .ctx("CreateVideoProcessorInputView")?;
                let v = v.unwrap();
                if self.input_views.len() >= 16 {
                    self.input_views.remove(0);
                }
                self.input_views.push((key, v.clone()));
                v
            }
        };
        let idx = self.pos;
        self.pos = (self.pos + 1) % self.pool.len();
        let (tex, out_view) = &self.pool[idx];
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL(1),
            pInputSurface: std::mem::ManuallyDrop::new(Some(in_view)),
            ..Default::default()
        };
        let r = unsafe { self.vctx.VideoProcessorBlt(&self.processor, out_view, 0, &[stream.clone()]) };
        // Libère la référence que `stream` tenait sur la vue d'entrée.
        let mut stream = stream;
        unsafe { std::mem::ManuallyDrop::drop(&mut stream.pInputSurface) };
        r.map_err(|source| VideoError::Win { context: "VideoProcessorBlt", source })?;
        Ok(tex.clone())
    }
}
