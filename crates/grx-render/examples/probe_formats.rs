//! Diagnostic phase 2 : profils de décodage D3D11 du GPU (8/10 bits, 4:4:4), formats acceptés
//! par le VideoProcessor (entrée/sortie) et espaces couleur HDR des sorties.
//! `cargo run -p grx-render --example probe_formats`
use windows::core::{Interface, GUID};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

const PROFILES: &[(&str, GUID, DXGI_FORMAT)] = &[
    ("H.264 4:2:0 8 bits", D3D11_DECODER_PROFILE_H264_VLD_NOFGT, DXGI_FORMAT_NV12),
    ("HEVC Main", D3D11_DECODER_PROFILE_HEVC_VLD_MAIN, DXGI_FORMAT_NV12),
    ("HEVC Main10", D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10, DXGI_FORMAT_P010),
    ("AV1 Profile0 8 bits", D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0, DXGI_FORMAT_NV12),
    ("AV1 Profile0 10 bits", D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0, DXGI_FORMAT_P010),
    ("AV1 Profile1 4:4:4 8 bits", D3D11_DECODER_PROFILE_AV1_VLD_PROFILE1, DXGI_FORMAT_AYUV),
    // DXVA HEVC RExt (dxva.h) : Main 4:4:4 8 bits / Main 4:4:4 10 bits.
    ("HEVC RExt 4:4:4 8 bits", GUID::from_u128(0x4008018f_f537_4b36_98cf_61af8a2c1a33), DXGI_FORMAT_AYUV),
    ("HEVC RExt 4:4:4 10 bits", GUID::from_u128(0x0dabeffa_4458_4602_bc03_0795659d617c), DXGI_FORMAT_Y410),
];

fn main() -> windows::core::Result<()> {
    let device = grx_render::create_device().expect("device D3D11");
    let vdev: ID3D11VideoDevice = device.cast()?;
    let n = unsafe { vdev.GetVideoDecoderProfileCount() };
    let all: Vec<GUID> = (0..n).filter_map(|i| unsafe { vdev.GetVideoDecoderProfile(i) }.ok()).collect();
    println!("{} profils de décodage D3D11", all.len());
    for (name, g, f) in PROFILES {
        let has = all.contains(g);
        let fmt = has && unsafe { vdev.CheckVideoDecoderFormat(g, *f) }.is_ok_and(|b| b.as_bool());
        println!("  {name:<28} profil {} format {:?} {}", if has { "oui" } else { "non" }, f, if fmt { "oui" } else { "non" });
    }
    for g in &all {
        println!("  GUID {g:?}");
    }
    let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
        InputWidth: 1920,
        InputHeight: 1080,
        OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
        OutputWidth: 1920,
        OutputHeight: 1080,
        Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
    };
    let en = unsafe { vdev.CreateVideoProcessorEnumerator(&desc) }?;
    println!("VideoProcessor (bit 0 = entrée, bit 1 = sortie) :");
    for f in [
        DXGI_FORMAT_B8G8R8A8_UNORM,
        DXGI_FORMAT_R16G16B16A16_FLOAT,
        DXGI_FORMAT_R10G10B10A2_UNORM,
        DXGI_FORMAT_NV12,
        DXGI_FORMAT_P010,
        DXGI_FORMAT_AYUV,
        DXGI_FORMAT_Y410,
    ] {
        println!("  {f:?}: {:#x}", unsafe { en.CheckVideoProcessorFormat(f) }.unwrap_or(0));
    }
    if let Ok(en1) = en.cast::<ID3D11VideoProcessorEnumerator1>() {
        let ok = |i, ic, o, oc| unsafe { en1.CheckVideoProcessorFormatConversion(i, ic, o, oc) }.is_ok_and(|b| b.as_bool());
        println!(
            "  P010 PQ/BT.2020 -> BGRA sRGB : {} ; P010 PQ -> R10G10B10A2 PQ : {} ; FP16 scRGB -> P010 PQ : {}",
            ok(DXGI_FORMAT_P010, DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709),
            ok(DXGI_FORMAT_P010, DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020, DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020),
            ok(DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709, DXGI_FORMAT_P010, DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020),
        );
    }
    let f: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
    let mut i = 0;
    while let Ok(a) = unsafe { f.EnumAdapters1(i) } {
        let mut o = 0;
        while let Ok(out) = unsafe { a.EnumOutputs(o) } {
            if let Ok(o6) = out.cast::<IDXGIOutput6>() {
                if let Ok(d) = unsafe { o6.GetDesc1() } {
                    let name = String::from_utf16_lossy(&d.DeviceName).trim_end_matches('\0').to_string();
                    println!("sortie {name} : espace {:?} ({} bits, max {:.0} nits)", d.ColorSpace, d.BitsPerColor, d.MaxLuminance);
                }
            }
            o += 1;
        }
        i += 1;
    }
    Ok(())
}
