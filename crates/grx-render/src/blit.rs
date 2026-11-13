//! Conversion NV12 -> BGRA + mise à l'échelle (letterbox) sur le GPU via ID3D11VideoProcessor,
//! directement vers le back buffer de la swapchain.

use crate::error::{Result, WinCtx};
use windows::core::Interface;
use windows::Win32::Foundation::{BOOL, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// Rectangle `(x, y, w, h)` de l'image `src_w x src_h` ajustée dans `dst_w x dst_h` en conservant
/// le ratio (bandes noires).
pub fn fit_rect(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> (u32, u32, u32, u32) {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return (0, 0, dst_w, dst_h);
    }
    // Comparaison par produits croisés (exacte) : src_w/src_h vs dst_w/dst_h.
    let (w, h) = if (src_w as u64) * (dst_h as u64) >= (dst_w as u64) * (src_h as u64) {
        // Source plus « large » : remplit la largeur.
        let h = ((src_h as u64 * dst_w as u64 + src_w as u64 / 2) / src_w as u64) as u32;
        (dst_w, h.clamp(1, dst_h))
    } else {
        let w = ((src_w as u64 * dst_h as u64 + src_h as u64 / 2) / src_h as u64) as u32;
        (w.clamp(1, dst_w), dst_h)
    };
    ((dst_w - w) / 2, (dst_h - h) / 2, w, h)
}

pub(crate) struct Blitter {
    device: ID3D11Device,
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    state: Option<State>,
}

struct State {
    in_w: u32,
    in_h: u32,
    out_w: u32,
    out_h: u32,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    out_view: Option<(usize, ID3D11VideoProcessorOutputView)>,
    // Vues d'entrée par (texture, sous-ressource) : le pool du décodeur est petit et stable.
    in_views: Vec<(usize, u32, ID3D11VideoProcessorInputView)>,
}

impl Blitter {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let vdev: ID3D11VideoDevice = device.cast().ctx("ID3D11VideoDevice")?;
        let ctx = unsafe { device.GetImmediateContext() }.ctx("GetImmediateContext")?;
        let vctx: ID3D11VideoContext = ctx.cast().ctx("ID3D11VideoContext")?;
        Ok(Blitter { device: device.clone(), vdev, vctx, state: None })
    }

    fn ensure(&mut self, in_w: u32, in_h: u32, out_w: u32, out_h: u32) -> Result<&mut State> {
        let ok = matches!(&self.state, Some(s) if s.in_w == in_w && s.in_h == in_h && s.out_w == out_w && s.out_h == out_h);
        if !ok {
            let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
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
            let enumerator =
                unsafe { self.vdev.CreateVideoProcessorEnumerator(&desc) }.ctx("CreateVideoProcessorEnumerator")?;
            let processor = unsafe { self.vdev.CreateVideoProcessor(&enumerator, 0) }.ctx("CreateVideoProcessor")?;
            unsafe {
                let vp = &processor;
                self.vctx.VideoProcessorSetStreamFrameFormat(vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
                self.vctx.VideoProcessorSetStreamAutoProcessingMode(vp, 0, false);
                // Entrée : YCbCr BT.709, plage limitée 16-235 (cohérent avec l'encodeur grx-video).
                let in_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
                // Sortie : RGB plein débit 0-255.
                let out_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
                self.vctx.VideoProcessorSetStreamColorSpace(vp, 0, &in_cs);
                self.vctx.VideoProcessorSetOutputColorSpace(vp, &out_cs);
                let black = D3D11_VIDEO_COLOR {
                    Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } },
                };
                self.vctx.VideoProcessorSetOutputBackgroundColor(vp, false, &black);
            }
            self.state = Some(State { in_w, in_h, out_w, out_h, enumerator, processor, out_view: None, in_views: Vec::new() });
        }
        Ok(self.state.as_mut().unwrap())
    }

    /// Dessine `src` (sous-ressource `sub`, zone visible `vis_w x vis_h`) dans `backbuffer`
    /// (taille `out_w x out_h`) avec bandes noires.
    pub fn blit(
        &mut self,
        src: &ID3D11Texture2D,
        sub: u32,
        vis_w: u32,
        vis_h: u32,
        backbuffer: &ID3D11Texture2D,
        out_w: u32,
        out_h: u32,
    ) -> Result<()> {
        let mut sd = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut sd) };
        let vdev = self.vdev.clone();
        let vctx = self.vctx.clone();
        let _ = &self.device;
        let st = self.ensure(sd.Width, sd.Height, out_w, out_h)?;

        let bb_key = backbuffer.as_raw() as usize;
        if !matches!(&st.out_view, Some((k, _)) if *k == bb_key) {
            let vd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut v = None;
            unsafe { vdev.CreateVideoProcessorOutputView(backbuffer, &st.enumerator, &vd, Some(&mut v)) }
                .ctx("CreateVideoProcessorOutputView(backbuffer)")?;
            st.out_view = v.map(|v| (bb_key, v));
        }
        let out_view = st.out_view.as_ref().unwrap().1.clone();

        let key = src.as_raw() as usize;
        let in_view = match st.in_views.iter().find(|(k, s, _)| *k == key && *s == sub) {
            Some((_, _, v)) => v.clone(),
            None => {
                let vd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: sub },
                    },
                };
                let mut v = None;
                unsafe { vdev.CreateVideoProcessorInputView(src, &st.enumerator, &vd, Some(&mut v)) }
                    .ctx("CreateVideoProcessorInputView(decoded frame)")?;
                let v = v.unwrap();
                // Le pool du décodeur peut tourner ; on borne le cache.
                if st.in_views.len() >= 48 {
                    st.in_views.remove(0);
                }
                st.in_views.push((key, sub, v.clone()));
                v
            }
        };

        let (dx, dy, dw, dh) = fit_rect(vis_w, vis_h, out_w, out_h);
        unsafe {
            let vp = &st.processor;
            let src_r = RECT { left: 0, top: 0, right: vis_w as i32, bottom: vis_h as i32 };
            let dst_r = RECT { left: dx as i32, top: dy as i32, right: (dx + dw) as i32, bottom: (dy + dh) as i32 };
            let tgt = RECT { left: 0, top: 0, right: out_w as i32, bottom: out_h as i32 };
            vctx.VideoProcessorSetStreamSourceRect(vp, 0, true, Some(&src_r));
            vctx.VideoProcessorSetStreamDestRect(vp, 0, true, Some(&dst_r));
            vctx.VideoProcessorSetOutputTargetRect(vp, true, Some(&tgt));
        }
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL(1),
            pInputSurface: std::mem::ManuallyDrop::new(Some(in_view)),
            ..Default::default()
        };
        let r = unsafe { vctx.VideoProcessorBlt(&st.processor, &out_view, 0, &[stream.clone()]) };
        let mut stream = stream;
        unsafe { std::mem::ManuallyDrop::drop(&mut stream.pInputSurface) };
        r.ctx("VideoProcessorBlt")
    }

    /// Libère les vues liées au back buffer (avant `ResizeBuffers`).
    pub fn release_output(&mut self) {
        if let Some(s) = &mut self.state {
            s.out_view = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_same_ratio() {
        assert_eq!(fit_rect(1920, 1080, 1280, 720), (0, 0, 1280, 720));
        assert_eq!(fit_rect(1920, 1080, 3840, 2160), (0, 0, 3840, 2160));
    }

    #[test]
    fn fit_pillarbox_and_letterbox() {
        // 16:9 dans une fenêtre 4:3 : bandes en haut/bas.
        assert_eq!(fit_rect(1920, 1080, 1024, 768), (0, 96, 1024, 576));
        // 16:9 dans une fenêtre ultra-large : bandes à gauche/droite.
        assert_eq!(fit_rect(1920, 1080, 2560, 720), (640, 0, 1280, 720));
        // Fenêtre portrait.
        assert_eq!(fit_rect(1920, 1080, 500, 1000), (0, 359, 500, 281));
    }

    #[test]
    fn fit_degenerate() {
        assert_eq!(fit_rect(0, 0, 100, 50), (0, 0, 100, 50));
        assert_eq!(fit_rect(1920, 1080, 0, 0), (0, 0, 0, 0));
        let (_, _, w, h) = fit_rect(1920, 1080, 1, 1);
        assert!(w >= 1 && h >= 1);
    }
}
