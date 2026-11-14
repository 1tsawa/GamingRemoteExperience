//! Passes GPU (pixel shaders D3D11) de la phase 2 :
//!
//! * [`HdrPass`] : capture HDR (FP16 scRGB linéaire) -> R10G10B10A2 PQ/BT.2020 (HDR10, ensuite
//!   converti en P010 par le VideoProcessor) ou -> BGRA 8 bits SDR (tone-mapping à genou)
//!   quand le flux est 8 bits.
//!
//! Formules identiques à la référence CPU de [`crate::color`] (testée sur de petites images connues).

use crate::error::{Result, VideoError, WinCtx};
use windows::core::PCSTR;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_SRV_DIMENSION_TEXTURE2D, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

const HLSL: &str = r#"
Texture2D<float4> src : register(t0);
cbuffer C : register(b0) {
    float4 row0; float4 row1; float4 row2;   // xyz = coefficients, w = décalage (code)
    float4 scale;                            // x = 8 bits : 1/255 ; 10 bits : 64/65535 ; y = hauteur H ; z = mode ; w = pic (nits)
};
struct VO { float4 pos : SV_Position; };
VO vs(uint id : SV_VertexID) {
    float2 uv = float2((id << 1) & 2, id & 2);
    VO o; o.pos = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1); return o;
}
float3 pq(float3 nits) {
    const float m1 = 2610.0 / 16384.0, m2 = 2523.0 / 4096.0 * 128.0;
    const float c1 = 3424.0 / 4096.0, c2 = 2413.0 / 4096.0 * 32.0, c3 = 2392.0 / 4096.0 * 32.0;
    float3 y = pow(saturate(nits / 10000.0), m1);
    return pow((c1 + c2 * y) / (1.0 + c3 * y), m2);
}
float3 to2020(float3 c) {
    return float3(dot(c, float3(0.627404, 0.329283, 0.043313)),
                  dot(c, float3(0.069097, 0.919540, 0.011362)),
                  dot(c, float3(0.016391, 0.088013, 0.895595)));
}
float srgb(float x) { x = saturate(x); return x <= 0.0031308 ? 12.92 * x : 1.055 * pow(x, 1.0 / 2.4) - 0.055; }
// HDR : mode 0 = scRGB -> PQ BT.2020 ; mode 1 = scRGB -> sRGB 8 bits (Reinhard étendu).
float4 ps_hdr(VO i) : SV_Target {
    float3 c = max(src.Load(int3(i.pos.xy, 0)).rgb, 0.0);
    if (scale.z < 0.5) {
        return float4(pq(to2020(c) * 80.0), 1.0);
    }
    // Même courbe que color::tonemap_curve (genou 0,75 x blanc SDR 203 nits).
    float3 x = c * (80.0 / 203.0);
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
            PCSTR(b"grx-video.hlsl\0".as_ptr()),
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
        return Err(VideoError::Other(format!("D3DCompile : {e} {msg}")));
    }
    let b = code.ok_or_else(|| VideoError::Other("D3DCompile : pas de code".into()))?;
    Ok(unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }.to_vec())
}

/// Pass plein écran générique : une texture source -> une cible de rendu.
struct Pass {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    cb: ID3D11Buffer,
    target: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    w: u32,
    h: u32,
    srvs: Vec<(usize, ID3D11ShaderResourceView)>,
}

impl Pass {
    fn new(device: &ID3D11Device, entry: &[u8], w: u32, h: u32, format: DXGI_FORMAT, extra_bind: u32) -> Result<Pass> {
        Self::with_view(device, entry, w, h, format, format, extra_bind)
    }

    /// `tex_format` : format de la texture (éventuellement TYPELESS) ; `rtv_format` : celui de la vue.
    fn with_view(
        device: &ID3D11Device,
        entry: &[u8],
        w: u32,
        h: u32,
        tex_format: DXGI_FORMAT,
        rtv_format: DXGI_FORMAT,
        extra_bind: u32,
    ) -> Result<Pass> {
        let vsb = compile(b"vs\0", b"vs_5_0\0")?;
        let psb = compile(entry, b"ps_5_0\0")?;
        let (mut vs, mut ps, mut cb, mut t, mut rtv) = (None, None, None, None, None);
        unsafe {
            device.CreateVertexShader(&vsb, None, Some(&mut vs)).ctx("CreateVertexShader")?;
            device.CreatePixelShader(&psb, None, Some(&mut ps)).ctx("CreatePixelShader")?;
            let bd = D3D11_BUFFER_DESC {
                ByteWidth: 64,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            device.CreateBuffer(&bd, None, Some(&mut cb)).ctx("CreateBuffer(cb)")?;
            let td = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: tex_format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32 | extra_bind,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            device.CreateTexture2D(&td, None, Some(&mut t)).ctx("CreateTexture2D(pass)")?;
            let rd = D3D11_RENDER_TARGET_VIEW_DESC {
                Format: rtv_format,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 } },
            };
            device.CreateRenderTargetView(t.as_ref().unwrap(), Some(&rd), Some(&mut rtv)).ctx("CreateRenderTargetView")?;
        }
        let ctx = unsafe { device.GetImmediateContext() }.ctx("GetImmediateContext")?;
        Ok(Pass {
            device: device.clone(),
            ctx,
            vs: vs.unwrap(),
            ps: ps.unwrap(),
            cb: cb.unwrap(),
            target: t.unwrap(),
            rtv: rtv.unwrap(),
            w,
            h,
            srvs: Vec::new(),
        })
    }

    fn srv(&mut self, src: &ID3D11Texture2D) -> Result<ID3D11ShaderResourceView> {
        use windows::core::Interface;
        let key = src.as_raw() as usize;
        if let Some((_, v)) = self.srvs.iter().find(|(k, _)| *k == key) {
            return Ok(v.clone());
        }
        let mut sd = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut sd) };
        let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: sd.Format,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_SRV { MostDetailedMip: 0, MipLevels: 1 } },
        };
        let mut v = None;
        unsafe { self.device.CreateShaderResourceView(src, Some(&desc), Some(&mut v)) }.ctx("CreateShaderResourceView")?;
        let v = v.unwrap();
        if self.srvs.len() >= 8 {
            self.srvs.remove(0);
        }
        self.srvs.push((key, v.clone()));
        Ok(v)
    }

    fn run(&mut self, src: &ID3D11Texture2D, cb: &[f32; 16]) -> Result<ID3D11Texture2D> {
        let srv = self.srv(src)?;
        unsafe {
            let c = &self.ctx;
            c.UpdateSubresource(&self.cb, 0, None, cb.as_ptr() as *const _, 0, 0);
            c.OMSetRenderTargets(Some(&[Some(self.rtv.clone())]), None);
            let vp = D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: self.w as f32, Height: self.h as f32, MinDepth: 0.0, MaxDepth: 1.0 };
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
            // Délie la source et la cible (la texture sera lue par l'encodeur / le VideoProcessor).
            c.PSSetShaderResources(0, Some(&[None]));
            c.OMSetRenderTargets(None, None);
        }
        Ok(self.target.clone())
    }
}

/// Capture HDR (FP16 scRGB) -> HDR10 (R10G10B10A2 PQ BT.2020) ou SDR 8 bits.
pub(crate) struct HdrPass {
    pass: Pass,
    cb: [f32; 16],
}

impl HdrPass {
    /// `to_pq` : sortie R10G10B10A2 PQ/BT.2020 ; sinon BGRA 8 bits SDR (pic de l'écran `peak_nits`).
    pub fn new(device: &ID3D11Device, w: u32, h: u32, to_pq: bool, peak_nits: f32) -> Result<HdrPass> {
        let fmt = if to_pq { DXGI_FORMAT_R10G10B10A2_UNORM } else { DXGI_FORMAT_B8G8R8A8_UNORM };
        let pass = Pass::new(device, b"ps_hdr\0", w, h, fmt, 0)?;
        let mut cb = [0f32; 16];
        cb[14] = if to_pq { 0.0 } else { 1.0 };
        cb[15] = peak_nits.max(203.0);
        Ok(HdrPass { pass, cb })
    }

    pub fn run(&mut self, src: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        let cb = self.cb;
        self.pass.run(src, &cb)
    }
}

#[cfg(test)]
mod gpu_tests {
    use super::*;
    use crate::color::{scrgb_to_pq2020, srgb_encode, tonemap_curve};

    fn half(f: f32) -> u16 {
        // f32 -> f16 (valeurs normales positives uniquement, suffisant ici).
        if f == 0.0 {
            return 0;
        }
        let b = f.to_bits();
        let e = ((b >> 23) & 0xFF) as i32 - 127 + 15;
        ((e as u16) << 10) | ((b >> 13) & 0x3FF) as u16
    }

    /// Passe HDR sur le GPU réel, comparée à la référence CPU (scRGB -> PQ BT.2020, et SDR).
    #[test]
    fn hdr_pass_matches_cpu_reference() {
        crate::gpu::init_com();
        let Ok(adapter) = crate::gpu::preferred_adapter() else { return };
        let Ok((device, ctx)) = crate::gpu::create_device_for_adapter(&adapter) else { return };
        let colors: [[f32; 3]; 4] = [[1.0, 1.0, 1.0], [2.5375, 2.5375, 2.5375], [10.0, 0.5, 0.25], [0.2, 0.4, 0.8]];
        let (w, h) = (4u32, 1u32);
        let mut px = Vec::new();
        for c in colors {
            for v in [c[0], c[1], c[2], 1.0] {
                px.extend_from_slice(&half(v).to_le_bytes());
            }
        }
        let src = crate::facade::upload_texture(&device, DXGI_FORMAT_R16G16B16A16_FLOAT, &px, w, h, w * 8).unwrap();
        for to_pq in [true, false] {
            let mut pass = HdrPass::new(&device, w, h, to_pq, 1000.0).unwrap();
            let out = pass.run(&src).unwrap();
            let mut d = D3D11_TEXTURE2D_DESC::default();
            unsafe { out.GetDesc(&mut d) };
            d.Usage = D3D11_USAGE_STAGING;
            d.BindFlags = 0;
            d.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let mut st = None;
            unsafe { device.CreateTexture2D(&d, None, Some(&mut st)) }.unwrap();
            let st = st.unwrap();
            unsafe {
                ctx.CopyResource(&st, &out);
                let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
                for (k, c) in colors.iter().enumerate() {
                    let word = *(m.pData as *const u32).add(k);
                    if to_pq {
                        // R10G10B10A2 : R bits 0-9, G 10-19, B 20-29.
                        let got = [word & 0x3FF, (word >> 10) & 0x3FF, (word >> 20) & 0x3FF];
                        let want = scrgb_to_pq2020(*c).map(|v| (v * 1023.0).round() as u32);
                        for i in 0..3 {
                            assert!(got[i].abs_diff(want[i]) <= 2, "PQ {c:?}: {got:?} vs {want:?}");
                        }
                    } else {
                        // BGRA : B octet 0, G 1, R 2.
                        let got = [(word >> 16) & 0xFF, (word >> 8) & 0xFF, word & 0xFF];
                        let want = c.map(|v| (srgb_encode(tonemap_curve(v * 80.0 / 203.0)) * 255.0).round() as u32);
                        for i in 0..3 {
                            assert!(got[i].abs_diff(want[i]) <= 2, "SDR {c:?}: {got:?} vs {want:?}");
                        }
                    }
                }
                ctx.Unmap(&st, 0);
            }
        }
    }
}
