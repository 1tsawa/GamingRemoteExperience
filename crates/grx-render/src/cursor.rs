//! Curseur distant dessiné par-dessus la vidéo (après le blit, avant l'overlay et `Present`).
//!
//! * L'intégration pousse l'état avec [`crate::PresenterHandle::set_remote_cursor`] depuis
//!   n'importe quel thread ; le présentateur redessine aussitôt la DERNIÈRE image vidéo avec le
//!   curseur à sa nouvelle position (sans attendre une nouvelle image) : le curseur reste réactif
//!   même quand l'écran du host est statique.
//! * Deux passes D3D11 (quad texturé, shaders compilés au démarrage par `D3DCompile`) :
//!   calque couleur en alpha prémultiplié (`ONE, INV_SRC_ALPHA`), puis calque d'inversion
//!   (`INV_DEST_COLOR, INV_SRC_COLOR` : `src*(1-dst) + dst*(1-src)`, un XOR exact pour des canaux
//!   à 0/255, comme les curseurs monochromes de Windows).
//! * Échelle : la forme est en pixels de l'image capturée par le host ; elle suit le facteur de
//!   mise à l'échelle de la vidéo dans la fenêtre (letterbox compris). Découpée au rectangle vidéo.
//! * Aucun coût tant qu'aucun curseur n'a été reçu (rien n'est créé).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{RenderError, Result, WinCtx};
use windows::core::{Interface, PCSTR};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// Forme du curseur : `rgba`/`xor` = `width*height*4` octets (R,G,B,A, alpha non prémultiplié).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorShape {
    /// Identifiant (hachage du contenu) : une forme de même id n'est pas réuploadée.
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub rgba: Vec<u8>,
    /// Calque d'inversion de l'écran (RGB = couleur XOR), `None` s'il est vide.
    pub xor: Option<Vec<u8>>,
}

/// État du curseur distant à dessiner.
#[derive(Clone, Debug)]
pub struct RemoteCursor {
    pub visible: bool,
    pub shape: Option<Arc<CursorShape>>,
    /// Point chaud, fraction de la largeur/hauteur de l'image capturée (peut sortir de [0,1]).
    pub x: f32,
    pub y: f32,
    /// Taille de l'image capturée par le host (pixels) : échelle de la forme.
    pub src_w: u32,
    pub src_h: u32,
    /// Horodatage côté host (µs, epoch Unix) de la mise à jour, pour la mesure de latence ; 0 = aucun.
    pub ts_us: u64,
}

/// Rectangle `(x, y, w, h)` en pixels de fenêtre (flottants) du quad de la forme, pour une vidéo
/// affichée dans `video_rect` (`fit_rect`). `None` si rien à dessiner.
pub fn cursor_quad(c: &RemoteCursor, video_rect: (u32, u32, u32, u32)) -> Option<(f32, f32, f32, f32)> {
    let s = c.shape.as_ref()?;
    let (rx, ry, rw, rh) = video_rect;
    if !c.visible || c.src_w == 0 || c.src_h == 0 || rw == 0 || rh == 0 || !c.x.is_finite() || !c.y.is_finite() {
        return None;
    }
    let sx = rw as f32 / c.src_w as f32;
    let sy = rh as f32 / c.src_h as f32;
    let hx = rx as f32 + c.x * rw as f32;
    let hy = ry as f32 + c.y * rh as f32;
    let (w, h) = (s.width as f32 * sx, s.height as f32 * sy);
    // À l'échelle 1 on aligne sur les pixels (rendu net, échantillonnage exact).
    let (mut x, mut y) = (hx - s.hot_x as f32 * sx, hy - s.hot_y as f32 * sy);
    if (sx - 1.0).abs() < 1e-3 && (sy - 1.0).abs() < 1e-3 {
        x = x.round();
        y = y.round();
    }
    // Entièrement hors du rectangle vidéo : rien.
    if x + w <= rx as f32 || y + h <= ry as f32 || x >= (rx + rw) as f32 || y >= (ry + rh) as f32 {
        return None;
    }
    Some((x, y, w, h))
}

/// Alpha non prémultiplié -> prémultiplié (pour un filtrage bilinéaire sans franges).
pub fn premultiply(rgba: &[u8]) -> Vec<u8> {
    let mut v = rgba.to_vec();
    for p in v.chunks_exact_mut(4) {
        let a = p[3] as u32;
        for c in &mut p[..3] {
            *c = ((*c as u32 * a + 127) / 255) as u8;
        }
    }
    v
}

fn unix_micros() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}

/// État partagé entre l'intégration (écrivain) et le thread de fenêtre (dessin).
#[derive(Default)]
pub(crate) struct CursorShared {
    cur: Mutex<Option<RemoteCursor>>,
    /// Un réveil du thread de fenêtre est déjà demandé (coalescence des mises à jour).
    pending: AtomicBool,
    /// Latences « mise à jour sur le host -> dessin » (µs), voir [`crate::PresenterHandle::take_cursor_latency_stats`].
    pub(crate) latencies: Mutex<Vec<u32>>,
}

impl CursorShared {
    /// Remplace l'état ; `true` si le thread de fenêtre doit être réveillé.
    pub(crate) fn set(&self, c: Option<RemoteCursor>) -> bool {
        *self.cur.lock().unwrap() = c;
        !self.pending.swap(true, Ordering::AcqRel)
    }
    pub(crate) fn clear_pending(&self) {
        self.pending.store(false, Ordering::Release);
    }
    fn get(&self) -> Option<RemoteCursor> {
        self.cur.lock().unwrap().clone()
    }
}

const HLSL: &str = r#"
cbuffer C : register(b0) { float4 rect; };
struct VO { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VO vs(uint id : SV_VertexID) {
    float2 t = float2(id & 1, id >> 1);
    VO o;
    o.pos = float4(lerp(rect.x, rect.z, t.x), lerp(rect.y, rect.w, t.y), 0, 1);
    o.uv = t;
    return o;
}
Texture2D tex : register(t0);
SamplerState smp : register(s0);
float4 ps(VO i) : SV_Target { return tex.Sample(smp, i.uv); }
"#;

fn compile(entry: &[u8], target: &[u8]) -> Result<Vec<u8>> {
    let mut code: Option<ID3DBlob> = None;
    let mut errs: Option<ID3DBlob> = None;
    let r = unsafe {
        D3DCompile(
            HLSL.as_ptr() as *const _,
            HLSL.len(),
            PCSTR(b"cursor.hlsl\0".as_ptr()),
            None,
            None,
            PCSTR(entry.as_ptr()),
            PCSTR(target.as_ptr()),
            0,
            0,
            &mut code,
            Some(&mut errs),
        )
    };
    if let Err(e) = r {
        let msg = errs
            .map(|b| unsafe {
                String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()))
                    .into_owned()
            })
            .unwrap_or_default();
        return Err(RenderError::Other(format!("D3DCompile curseur : {e} {msg}")));
    }
    let b = code.ok_or_else(|| RenderError::Other("D3DCompile: pas de code".into()))?;
    Ok(unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }.to_vec())
}

struct Pipeline {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    cb: ID3D11Buffer,
    blend_alpha: ID3D11BlendState,
    blend_invert: ID3D11BlendState,
    raster: ID3D11RasterizerState,
    linear: ID3D11SamplerState,
    point: ID3D11SamplerState,
}

impl Pipeline {
    fn new(dev: &ID3D11Device) -> Result<Self> {
        let vsb = compile(b"vs\0", b"vs_4_0\0")?;
        let psb = compile(b"ps\0", b"ps_4_0\0")?;
        let mut vs = None;
        let mut ps = None;
        unsafe { dev.CreateVertexShader(&vsb, None, Some(&mut vs)) }.ctx("CreateVertexShader(curseur)")?;
        unsafe { dev.CreatePixelShader(&psb, None, Some(&mut ps)) }.ctx("CreatePixelShader(curseur)")?;
        let cbd = D3D11_BUFFER_DESC {
            ByteWidth: 16,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        let mut cb = None;
        unsafe { dev.CreateBuffer(&cbd, None, Some(&mut cb)) }.ctx("CreateBuffer(curseur)")?;
        let blend = |src: D3D11_BLEND, dst: D3D11_BLEND| -> Result<ID3D11BlendState> {
            let mut d = D3D11_BLEND_DESC::default();
            d.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                SrcBlend: src,
                DestBlend: dst,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ZERO,
                DestBlendAlpha: D3D11_BLEND_ONE,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: (D3D11_COLOR_WRITE_ENABLE_RED.0
                    | D3D11_COLOR_WRITE_ENABLE_GREEN.0
                    | D3D11_COLOR_WRITE_ENABLE_BLUE.0) as u8,
            };
            let mut s = None;
            unsafe { dev.CreateBlendState(&d, Some(&mut s)) }.ctx("CreateBlendState(curseur)")?;
            Ok(s.unwrap())
        };
        let rd = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            ScissorEnable: true.into(),
            DepthClipEnable: true.into(),
            ..Default::default()
        };
        let mut raster = None;
        unsafe { dev.CreateRasterizerState(&rd, Some(&mut raster)) }.ctx("CreateRasterizerState(curseur)")?;
        let sampler = |f: D3D11_FILTER| -> Result<ID3D11SamplerState> {
            let sd = D3D11_SAMPLER_DESC {
                Filter: f,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut s = None;
            unsafe { dev.CreateSamplerState(&sd, Some(&mut s)) }.ctx("CreateSamplerState(curseur)")?;
            Ok(s.unwrap())
        };
        Ok(Pipeline {
            vs: vs.unwrap(),
            ps: ps.unwrap(),
            cb: cb.unwrap(),
            blend_alpha: blend(D3D11_BLEND_ONE, D3D11_BLEND_INV_SRC_ALPHA)?,
            blend_invert: blend(D3D11_BLEND_INV_DEST_COLOR, D3D11_BLEND_INV_SRC_COLOR)?,
            raster: raster.unwrap(),
            linear: sampler(D3D11_FILTER_MIN_MAG_MIP_LINEAR)?,
            point: sampler(D3D11_FILTER_MIN_MAG_MIP_POINT)?,
        })
    }
}

/// Textures de la forme courante.
struct ShapeTex {
    id: u32,
    color: ID3D11ShaderResourceView,
    xor: Option<ID3D11ShaderResourceView>,
}

fn make_srv(dev: &ID3D11Device, w: u32, h: u32, rgba: &[u8]) -> Result<ID3D11ShaderResourceView> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init = D3D11_SUBRESOURCE_DATA { pSysMem: rgba.as_ptr() as *const _, SysMemPitch: w * 4, SysMemSlicePitch: 0 };
    let mut t = None;
    unsafe { dev.CreateTexture2D(&desc, Some(&init), Some(&mut t)) }.ctx("CreateTexture2D(curseur)")?;
    let mut v = None;
    unsafe { dev.CreateShaderResourceView(&t.unwrap(), None, Some(&mut v)) }.ctx("CreateShaderResourceView(curseur)")?;
    Ok(v.unwrap())
}

/// Ressources GPU du curseur, propriété du thread de fenêtre (`Gfx`).
#[derive(Default)]
pub(crate) struct CursorLayer {
    shared: Option<Arc<CursorShared>>,
    pipe: Option<Pipeline>,
    failed: bool,
    tex: Option<ShapeTex>,
    rtv: Option<(usize, ID3D11RenderTargetView)>,
    last_ts: u64,
}

impl CursorLayer {
    pub(crate) fn attach(&mut self, s: Arc<CursorShared>) {
        self.shared = Some(s);
    }

    /// Libère la vue sur le back buffer (avant `ResizeBuffers`).
    pub(crate) fn release_target(&mut self) {
        self.rtv = None;
    }

    /// Dessine le curseur sur `bb` (taille `w x h`) pour une vidéo `vid_w x vid_h` en letterbox.
    /// Les erreurs sont journalisées une fois et désactivent le curseur (la vidéo continue).
    pub(crate) fn draw(
        &mut self,
        dev: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        bb: &ID3D11Texture2D,
        w: u32,
        h: u32,
        vid_w: u32,
        vid_h: u32,
    ) {
        if self.failed {
            return;
        }
        let Some(c) = self.shared.as_ref().and_then(|s| s.get()) else { return };
        let rect = crate::blit::fit_rect(vid_w, vid_h, w, h);
        let Some(q) = cursor_quad(&c, rect) else { return };
        if let Err(e) = self.draw_inner(dev, ctx, bb, w, h, rect, q, &c) {
            eprintln!("grx-render: curseur désactivé : {e}");
            self.failed = true;
            return;
        }
        if c.ts_us != 0 && c.ts_us != self.last_ts {
            self.last_ts = c.ts_us;
            let us = unix_micros().saturating_sub(c.ts_us).min(u32::MAX as u64) as u32;
            if let Some(s) = &self.shared {
                let mut l = s.latencies.lock().unwrap();
                if l.len() < 65_536 {
                    l.push(us);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_inner(
        &mut self,
        dev: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        bb: &ID3D11Texture2D,
        w: u32,
        h: u32,
        rect: (u32, u32, u32, u32),
        q: (f32, f32, f32, f32),
        c: &RemoteCursor,
    ) -> Result<()> {
        let shape = c.shape.as_ref().ok_or_else(|| RenderError::Other("pas de forme".into()))?;
        if self.pipe.is_none() {
            self.pipe = Some(Pipeline::new(dev)?);
        }
        if self.tex.as_ref().is_none_or(|t| t.id != shape.id) {
            let n = (shape.width * shape.height * 4) as usize;
            if shape.width == 0 || shape.height == 0 || shape.rgba.len() != n || shape.xor.as_ref().is_some_and(|x| x.len() != n) {
                return Err(RenderError::Other("forme de curseur incohérente".into()));
            }
            self.tex = Some(ShapeTex {
                id: shape.id,
                color: make_srv(dev, shape.width, shape.height, &premultiply(&shape.rgba))?,
                xor: match &shape.xor {
                    Some(x) => Some(make_srv(dev, shape.width, shape.height, x)?),
                    None => None,
                },
            });
        }
        let key = bb.as_raw() as usize;
        if !matches!(&self.rtv, Some((k, _)) if *k == key) {
            let mut v = None;
            unsafe { dev.CreateRenderTargetView(bb, None, Some(&mut v)) }.ctx("CreateRenderTargetView(curseur)")?;
            self.rtv = Some((key, v.unwrap()));
        }
        let p = self.pipe.as_ref().unwrap();
        let t = self.tex.as_ref().unwrap();
        let rtv = self.rtv.as_ref().unwrap().1.clone();
        let (x, y, qw, qh) = q;
        let ndc = [x / w as f32 * 2.0 - 1.0, 1.0 - y / h as f32 * 2.0, (x + qw) / w as f32 * 2.0 - 1.0, 1.0 - (y + qh) / h as f32 * 2.0];
        let unit = (qw - shape.width as f32).abs() < 1e-3 && (qh - shape.height as f32).abs() < 1e-3;
        unsafe {
            ctx.UpdateSubresource(&p.cb, 0, None, ndc.as_ptr() as *const _, 0, 0);
            ctx.OMSetRenderTargets(Some(&[Some(rtv)]), None);
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: w as f32,
                Height: h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            let (rx, ry, rw, rh) = rect;
            ctx.RSSetScissorRects(Some(&[RECT {
                left: rx as i32,
                top: ry as i32,
                right: (rx + rw) as i32,
                bottom: (ry + rh) as i32,
            }]));
            ctx.RSSetState(&p.raster);
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            ctx.VSSetShader(&p.vs, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(p.cb.clone())]));
            ctx.PSSetShader(&p.ps, None);
            ctx.PSSetSamplers(0, Some(&[Some(if unit { p.point.clone() } else { p.linear.clone() })]));
            if let Some(xv) = &t.xor {
                ctx.OMSetBlendState(&p.blend_invert, None, 0xffff_ffff);
                ctx.PSSetShaderResources(0, Some(&[Some(xv.clone())]));
                ctx.Draw(4, 0);
            }
            ctx.OMSetBlendState(&p.blend_alpha, None, 0xffff_ffff);
            ctx.PSSetShaderResources(0, Some(&[Some(t.color.clone())]));
            ctx.Draw(4, 0);
            // Rien ne doit rester lié au back buffer (ResizeBuffers, Direct2D, processeur vidéo).
            ctx.PSSetShaderResources(0, Some(&[None]));
            ctx.OMSetRenderTargets(None, None);
            ctx.OMSetBlendState(None, None, 0xffff_ffff);
            ctx.RSSetState(None);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cur(x: f32, y: f32, w: u32, h: u32) -> RemoteCursor {
        RemoteCursor {
            visible: true,
            shape: Some(Arc::new(CursorShape { id: 1, width: w, height: h, hot_x: 2, hot_y: 4, rgba: vec![0; (w * h * 4) as usize], xor: None })),
            x,
            y,
            src_w: 1920,
            src_h: 1080,
            ts_us: 0,
        }
    }

    #[test]
    fn quad_at_scale_one_is_pixel_aligned() {
        let q = cursor_quad(&cur(0.5, 0.5, 32, 32), (0, 0, 1920, 1080)).unwrap();
        assert_eq!(q, (958.0, 536.0, 32.0, 32.0));
    }

    #[test]
    fn quad_follows_letterbox_and_scale() {
        // 1920x1080 dans 1000x1000 : image 1000x563 en y=218.
        let rect = crate::blit::fit_rect(1920, 1080, 1000, 1000);
        let (x, y, w, h) = cursor_quad(&cur(0.0, 0.0, 32, 32), rect).unwrap();
        let s = 1000.0 / 1920.0;
        assert!((w - 32.0 * s).abs() < 1e-3 && (h - 32.0 * (563.0 / 1080.0)).abs() < 1e-3);
        assert!((x - (-2.0 * s)).abs() < 1e-3);
        assert!((y - (218.0 - 4.0 * 563.0 / 1080.0)).abs() < 1e-3);
    }

    #[test]
    fn hidden_offscreen_or_shapeless_cursor_is_not_drawn() {
        let rect = (0, 0, 1920, 1080);
        let mut c = cur(0.5, 0.5, 32, 32);
        c.visible = false;
        assert!(cursor_quad(&c, rect).is_none());
        assert!(cursor_quad(&cur(2.0, 0.5, 32, 32), rect).is_none());
        assert!(cursor_quad(&cur(f32::NAN, 0.5, 32, 32), rect).is_none());
        let mut c = cur(0.5, 0.5, 32, 32);
        c.shape = None;
        assert!(cursor_quad(&c, rect).is_none());
        let mut c = cur(0.5, 0.5, 32, 32);
        c.src_w = 0;
        assert!(cursor_quad(&c, rect).is_none());
        // Partiellement hors champ (bord gauche) : dessiné, découpé par le scissor.
        assert!(cursor_quad(&cur(0.0, 0.5, 32, 32), rect).is_some());
    }

    #[test]
    fn premultiply_alpha() {
        assert_eq!(premultiply(&[255, 128, 0, 128, 10, 20, 30, 255, 99, 99, 99, 0]), vec![128, 64, 0, 128, 10, 20, 30, 255, 0, 0, 0, 0]);
    }

    #[test]
    fn shared_state_coalesces_wakes() {
        let s = CursorShared::default();
        assert!(s.set(Some(cur(0.1, 0.1, 4, 4))));
        assert!(!s.set(Some(cur(0.2, 0.1, 4, 4))), "réveil déjà demandé");
        s.clear_pending();
        assert!(s.set(None));
        assert!(s.get().is_none());
    }
}
