//! Présentation des flux HDR10 (PQ, BT.2020).
//!
//! Le VideoProcessor convertit l'image décodée (P010/Y410, YCbCr PQ BT.2020) en RGB PQ BT.2020
//! dans une texture intermédiaire `R10G10B10A2` (conversion supportée par les pilotes, sans
//! tone-mapping), puis ce pixel shader écrit le back buffer :
//!
//! * écran SDR : tone-mapping vers sRGB 8 bits (courbe à genou de [`grx_video::color::tonemap_curve`],
//!   référence CPU [`grx_video::color::tonemap_pq2020_to_srgb8`]) ;
//! * écran HDR : scRGB linéaire (FP16, `DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709`), les nits du flux
//!   sont conservés (1.0 = 80 nits) ; l'overlay D2D se dessine aussi sur ce back buffer.

use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_SRV_DIMENSION_TEXTURE2D, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::error::{RenderError, Result, WinCtx};

const HLSL: &str = r#"
Texture2D<float4> src : register(t0);
cbuffer C : register(b0) { float4 p; };   // p.x = 0 : SDR tone-mappé ; 1 : scRGB
struct VO { float4 pos : SV_Position; };
VO vs(uint id : SV_VertexID) {
    float2 uv = float2((id << 1) & 2, id & 2);
    VO o; o.pos = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1); return o;
}
float3 pq_nits(float3 e) {
    const float m1 = 2610.0 / 16384.0, m2 = 2523.0 / 4096.0 * 128.0;
    const float c1 = 3424.0 / 4096.0, c2 = 2413.0 / 4096.0 * 32.0, c3 = 2392.0 / 4096.0 * 32.0;
    float3 ep = pow(saturate(e), 1.0 / m2);
    return 10000.0 * pow(max(ep - c1, 0.0) / (c2 - c3 * ep), 1.0 / m1);
}
float3 to709(float3 c) {
    return float3(dot(c, float3(1.660491, -0.587641, -0.072850)),
                  dot(c, float3(-0.124550, 1.132900, -0.008349)),
                  dot(c, float3(-0.018151, -0.100579, 1.118730)));
}
float srgb(float x) { x = saturate(x); return x <= 0.0031308 ? 12.92 * x : 1.055 * pow(x, 1.0 / 2.4) - 0.055; }
float4 ps(VO i) : SV_Target {
    float3 nits = to709(pq_nits(src.Load(int3(i.pos.xy, 0)).rgb));
    if (p.x > 0.5) {
        return float4(nits / 80.0, 1.0);
    }
    float3 x = max(nits, 0.0) / 203.0;
    const float k = 0.75;
    float3 t = x <= k ? x : k + (1.0 - k) * (1.0 - exp(-(x - k) / (1.0 - k)));
    return float4(srgb(t.r), srgb(t.g), srgb(t.b), 1.0);
}
"#;

fn compile(entry: &[u8], target: &[u8]) -> Result<Vec<u8>> {
    let mut code: Option<ID3DBlob> = None;
    let mut errs: Option<ID3DBlob> = None;
    let r = unsafe {
        D3DCompile(
            HLSL.as_ptr() as *const _,
            HLSL.len(),
            PCSTR(b"hdr_present.hlsl\0".as_ptr()),
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
                String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())).into_owned()
            })
            .unwrap_or_default();
        return Err(RenderError::Other(format!("D3DCompile HDR : {e} {msg}")));
    }
    let b = code.ok_or_else(|| RenderError::Other("D3DCompile : pas de code".into()))?;
    Ok(unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }.to_vec())
}

pub(crate) struct HdrPresent {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    cb: ID3D11Buffer,
    /// Texture intermédiaire RGB PQ BT.2020 (cible du VideoProcessor) et sa vue.
    inter: Option<(ID3D11Texture2D, ID3D11ShaderResourceView, u32, u32)>,
    rtv: Option<(usize, ID3D11RenderTargetView)>,
}

impl HdrPresent {
    pub fn new(device: &ID3D11Device) -> Result<HdrPresent> {
        let (vsb, psb) = (compile(b"vs\0", b"vs_5_0\0")?, compile(b"ps\0", b"ps_5_0\0")?);
        let (mut vs, mut ps, mut cb) = (None, None, None);
        unsafe {
            device.CreateVertexShader(&vsb, None, Some(&mut vs)).ctx("CreateVertexShader(HDR)")?;
            device.CreatePixelShader(&psb, None, Some(&mut ps)).ctx("CreatePixelShader(HDR)")?;
            let bd = D3D11_BUFFER_DESC { ByteWidth: 16, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
            device.CreateBuffer(&bd, None, Some(&mut cb)).ctx("CreateBuffer(HDR)")?;
        }
        let ctx = unsafe { device.GetImmediateContext() }.ctx("GetImmediateContext")?;
        Ok(HdrPresent { device: device.clone(), ctx, vs: vs.unwrap(), ps: ps.unwrap(), cb: cb.unwrap(), inter: None, rtv: None })
    }

    /// Texture intermédiaire `R10G10B10A2` de `w`x`h` (recréée si la taille change).
    pub fn intermediate(&mut self, w: u32, h: u32) -> Result<ID3D11Texture2D> {
        if !matches!(&self.inter, Some((_, _, iw, ih)) if (*iw, *ih) == (w, h)) {
            let td = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R10G10B10A2_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let (mut t, mut v) = (None, None);
            unsafe {
                self.device.CreateTexture2D(&td, None, Some(&mut t)).ctx("CreateTexture2D(HDR intermédiaire)")?;
                let sd = D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: DXGI_FORMAT_R10G10B10A2_UNORM,
                    ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_SRV { MostDetailedMip: 0, MipLevels: 1 } },
                };
                self.device.CreateShaderResourceView(t.as_ref().unwrap(), Some(&sd), Some(&mut v)).ctx("SRV(HDR)")?;
            }
            self.inter = Some((t.unwrap(), v.unwrap(), w, h));
        }
        Ok(self.inter.as_ref().unwrap().0.clone())
    }

    /// Libère la vue sur le back buffer (avant `ResizeBuffers`).
    pub fn release_target(&mut self) {
        self.rtv = None;
    }

    /// Intermédiaire -> `target` (back buffer BGRA tone-mappé, ou FP16 scRGB si `scrgb`).
    pub fn draw(&mut self, target: &ID3D11Texture2D, w: u32, h: u32, scrgb: bool) -> Result<()> {
        let Some((_, srv, _, _)) = self.inter.clone() else { return Err(RenderError::Other("HDR : pas d'intermédiaire".into())) };
        let key = target.as_raw() as usize;
        if !matches!(&self.rtv, Some((k, _)) if *k == key) {
            let mut v = None;
            unsafe { self.device.CreateRenderTargetView(target, None, Some(&mut v)) }.ctx("RTV(back buffer HDR)")?;
            self.rtv = Some((key, v.unwrap()));
        }
        let rtv = self.rtv.as_ref().unwrap().1.clone();
        let cb = [if scrgb { 1.0f32 } else { 0.0 }, 0.0, 0.0, 0.0];
        unsafe {
            let c = &self.ctx;
            c.UpdateSubresource(&self.cb, 0, None, cb.as_ptr() as *const _, 0, 0);
            c.OMSetRenderTargets(Some(&[Some(rtv)]), None);
            let vp = D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: w as f32, Height: h as f32, MinDepth: 0.0, MaxDepth: 1.0 };
            c.RSSetViewports(Some(&[vp]));
            c.IASetInputLayout(None);
            c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            c.VSSetShader(&self.vs, None);
            c.PSSetShader(&self.ps, None);
            c.PSSetShaderResources(0, Some(&[Some(srv)]));
            c.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            c.OMSetBlendState(None, None, 0xffff_ffff);
            c.RSSetState(None);
            c.Draw(3, 0);
            c.PSSetShaderResources(0, Some(&[None]));
            c.OMSetRenderTargets(None, None);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grx_video::color::{pq_from_nits, tonemap_pq2020_to_srgb8};

    /// Tone-mapping GPU comparé à la référence CPU (petite image PQ connue).
    #[test]
    fn tonemap_matches_cpu_reference() {
        let Ok(device) = crate::gpu::create_device() else { return };
        let nits = [0.0f32, 50.0, 203.0, 600.0, 1000.0, 4000.0];
        let w = nits.len() as u32;
        let words: Vec<u32> = nits
            .iter()
            .map(|&n| {
                let e = (pq_from_nits(n) * 1023.0).round() as u32;
                e | (e << 10) | (e << 20) | (3 << 30)
            })
            .collect();
        let mut hp = HdrPresent::new(&device).unwrap();
        let inter = hp.intermediate(w, 1).unwrap();
        let ctx = unsafe { device.GetImmediateContext() }.unwrap();
        unsafe { ctx.UpdateSubresource(&inter, 0, None, words.as_ptr() as *const _, w * 4, 0) };
        let td = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: 1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let (mut out, mut st) = (None, None);
        unsafe { device.CreateTexture2D(&td, None, Some(&mut out)) }.unwrap();
        let out = out.unwrap();
        hp.draw(&out, w, 1, false).unwrap();
        let sd = D3D11_TEXTURE2D_DESC { Usage: D3D11_USAGE_STAGING, BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, ..td };
        unsafe { device.CreateTexture2D(&sd, None, Some(&mut st)) }.unwrap();
        let st = st.unwrap();
        unsafe {
            ctx.CopyResource(&st, &out);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
            for (k, &n) in nits.iter().enumerate() {
                let p = (m.pData as *const u8).add(k * 4);
                let got = [*p.add(2), *p.add(1), *p];
                let e = (pq_from_nits(n) * 1023.0).round() / 1023.0;
                let want = tonemap_pq2020_to_srgb8([e; 3], 1000.0);
                for i in 0..3 {
                    assert!((got[i] as i32 - want[i] as i32).abs() <= 2, "{n} nits : {got:?} vs {want:?}");
                }
            }
            ctx.Unmap(&st, 0);
        }
    }
}
