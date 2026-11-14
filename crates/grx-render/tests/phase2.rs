//! Phase 2 (GPU réel) : décodage des nouveaux formats par la chaîne MF (HEVC Main10 -> P010,
//! HEVC RExt 4:4:4 de NVENC -> AYUV si le décodeur Microsoft le sait), mesure du temps de décodage.
#![cfg(windows)]

use grx_render::*;
use grx_video::{Codec, Encoder, EncoderConfig, PixelFormat};
use std::time::Instant;
use windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC;

fn pattern(w: u32, h: u32, t: u32) -> Vec<u8> {
    let mut v = vec![0u8; (w * h * 4) as usize];
    for (i, px) in v.chunks_exact_mut(4).enumerate() {
        let (x, y) = (i as u32 % w, i as u32 / w);
        px.copy_from_slice(&[((x + t) & 0xFF) as u8, (y & 0xFF) as u8, (((x ^ y) + t) & 0xFF) as u8, 255]);
    }
    v
}

/// Encode 20 images puis les décode ; renvoie (format DXGI de sortie, ms moyen par décodage).
fn roundtrip(codec: Codec, format: PixelFormat) -> Option<(i32, f64, String)> {
    let (w, h) = (1280u32, 720u32);
    let cfg = EncoderConfig { codec, width: w, height: h, bitrate_kbps: 10_000, format, ..Default::default() };
    let mut enc = match Encoder::new(cfg) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("SKIP encodeur {codec:?} {} : {e}", format.label());
            return None;
        }
    };
    let mut dec = match VideoDecoder::new(codec) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIP décodeur {codec:?} : {e}");
            return None;
        }
    };
    let (mut fmt, mut total, mut n) = (0, 0.0, 0);
    for t in 0..20 {
        let f = enc.encode_bgra(&pattern(w, h, t), w, h, w * 4).unwrap();
        let t0 = Instant::now();
        match dec.decode(&f.data) {
            Ok(Some(fr)) => {
                dec.sync_gpu().ok();
                total += t0.elapsed().as_secs_f64() * 1000.0;
                n += 1;
                let mut d = D3D11_TEXTURE2D_DESC::default();
                unsafe { fr.texture.GetDesc(&mut d) };
                fmt = d.Format.0;
                assert_eq!((fr.width, fr.height), (w, h));
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("décodage {codec:?} {} : {e}", format.label());
                return Some((0, 0.0, dec.backend_name().to_string()));
            }
        }
    }
    Some((fmt, total / n.max(1) as f64, dec.backend_name().to_string()))
}

#[test]
fn hevc_main10_decodes_to_p010() {
    if let Some((fmt, ms, name)) = roundtrip(Codec::Hevc, PixelFormat::Yuv420P10) {
        println!("MESURE décodage HEVC Main10 ({name}) : {ms:.2} ms, format DXGI {fmt}");
        assert_eq!(fmt, 104, "P010 attendu");
    }
}

#[test]
fn hevc_444_from_nvenc() {
    if let Some((fmt, ms, name)) = roundtrip(Codec::Hevc, PixelFormat::Yuv444) {
        // 100 = AYUV (4:4:4 conservé), 103 = NV12 (le décodeur aurait sous-échantillonné), 0 = échec.
        println!("MESURE décodage HEVC 4:4:4 ({name}) : {ms:.2} ms, format DXGI {fmt}");
    }
}

/// 4:4:4 : les couleurs décodées (AYUV) correspondent à la référence CPU BT.709 (blocs unis).
#[test]
fn hevc_444_colors_match_reference() {
    use grx_video::color::{rgb_to_ycbcr8, Matrix};
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D11::*;
    let (w, h) = (640u32, 360u32);
    let colors: [[u8; 3]; 6] = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255], [40, 120, 200], [250, 200, 30]];
    let mut px = vec![0u8; (w * h * 4) as usize];
    for (i, p) in px.chunks_exact_mut(4).enumerate() {
        let c = colors[((i as u32 % w) / (w / 6)).min(5) as usize];
        p.copy_from_slice(&[c[2], c[1], c[0], 255]);
    }
    let cfg = EncoderConfig { codec: Codec::Hevc, width: w, height: h, bitrate_kbps: 8000, format: PixelFormat::Yuv444, ..Default::default() };
    let Ok(mut enc) = Encoder::new(cfg) else { return };
    let Ok(mut dec) = VideoDecoder::new(Codec::Hevc) else { return };
    let mut last = None;
    for _ in 0..5 {
        let f = enc.encode_bgra(&px, w, h, w * 4).unwrap();
        if let Ok(Some(fr)) = dec.decode(&f.data) {
            last = Some(fr);
        }
    }
    let fr = last.expect("image décodée");
    let dev = dec.device().clone();
    let ctx = unsafe { dev.GetImmediateContext() }.unwrap();
    let mut d = D3D11_TEXTURE2D_DESC::default();
    unsafe { fr.texture.GetDesc(&mut d) };
    if d.Format.0 != 100 {
        eprintln!("SKIP : sortie {} (pas AYUV)", d.Format.0);
        return;
    }
    let sd = D3D11_TEXTURE2D_DESC {
        Width: d.Width,
        Height: d.Height,
        MipLevels: 1,
        ArraySize: 1,
        Format: d.Format,
        SampleDesc: d.SampleDesc,
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut st = None;
    unsafe { dev.CreateTexture2D(&sd, None, Some(&mut st)) }.unwrap();
    let st = st.unwrap();
    unsafe {
        ctx.CopySubresourceRegion(&st, 0, 0, 0, 0, &fr.texture, fr.subresource, None);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
        let mut worst = 0i32;
        for (k, c) in colors.iter().enumerate() {
            let x = (k as u32 * (w / 6) + w / 12) as usize;
            let y = (h / 2) as usize;
            let p = (m.pData as *const u8).add(y * m.RowPitch as usize + x * 4);
            let got = [*p.add(2), *p.add(1), *p];
            let want = rgb_to_ycbcr8(*c, Matrix::Bt709);
            println!("bloc {k} rgb {c:?} : décodé YUV {got:?}, attendu {want:?}");
            for i in 0..3 {
                worst = worst.max((got[i] as i32 - want[i] as i32).abs());
            }
        }
        ctx.Unmap(&st, 0);
        let _ = st.as_raw();
        println!("MESURE 4:4:4 : écart max YCbCr décodé / référence = {worst}");
        assert!(worst <= 4, "écart {worst}");
    }
}

/// Voie logicielle complète (si la DLL OpenH264 de Cisco est installée) : encodeur OpenH264 ->
/// décodeur OpenH264 -> texture NV12 téléversée ; mesure des temps.
#[test]
fn openh264_software_roundtrip() {
    if let Err(e) = grx_video::openh264::availability() {
        eprintln!("SKIP OpenH264 : {e}");
        return;
    }
    let (w, h) = (1920u32, 1080u32);
    let cfg = EncoderConfig { codec: Codec::H264, width: w, height: h, bitrate_kbps: 6000, pref: grx_video::EncoderPref::Software, ..Default::default() };
    let mut enc = Encoder::new(cfg).expect("encodeur logiciel");
    let (ow, oh) = enc.output_size();
    let device = create_device().unwrap();
    let mut dec = VideoDecoder::with_kind(&device, Codec::H264, DecoderKind::OpenH264).expect("décodeur OpenH264");
    let (mut te, mut td, mut n, mut bytes) = (0.0, 0.0, 0, 0usize);
    for t in 0..30 {
        let px = pattern(w, h, t);
        let t0 = Instant::now();
        let f = enc.encode_bgra(&px, w, h, w * 4).unwrap();
        te += t0.elapsed().as_secs_f64() * 1000.0;
        bytes += f.data.len();
        let t1 = Instant::now();
        if let Some(fr) = dec.decode(&f.data).unwrap() {
            td += t1.elapsed().as_secs_f64() * 1000.0;
            n += 1;
            assert_eq!((fr.width, fr.height), (ow, oh));
        }
    }
    assert!(n >= 29, "{n} images décodées");
    println!(
        "MESURE OpenH264 {} -> {ow}x{oh} : encodage {:.2} ms, décodage+téléversement {:.2} ms, {:.1} Mb/s à 60 img/s",
        enc.backend().name(),
        te / 30.0,
        td / n as f64,
        bytes as f64 * 8.0 * 2.0 / 1e6
    );
}
