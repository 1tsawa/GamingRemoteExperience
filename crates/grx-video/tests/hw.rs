//! Tests nécessitant un GPU/encodeur matériel (ignorés proprement s'il est absent).
#![cfg(windows)]

use grx_video::*;

fn skip<T>(r: Result<T>, what: &str) -> Option<T> {
    match r {
        Ok(v) => Some(v),
        Err(e @ (VideoError::NoEncoder(_) | VideoError::NoOutput)) => {
            eprintln!("SKIP {what}: {e}");
            None
        }
        Err(e) => panic!("{what}: {e}"),
    }
}

/// Image BGRA synthétique : dégradé qui défile + carré qui bouge.
fn gradient(w: u32, h: u32, t: u32) -> Vec<u8> {
    let mut v = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            v[i] = ((x + t * 4) & 0xFF) as u8;
            v[i + 1] = ((y + t * 2) & 0xFF) as u8;
            v[i + 2] = (((x + y) / 4) & 0xFF) as u8;
            v[i + 3] = 255;
        }
    }
    let bx = (t * 8) % (w - 64);
    for y in 100..164.min(h) {
        for x in bx..bx + 64 {
            let i = ((y * w + x) * 4) as usize;
            v[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
    }
    v
}

fn noise(w: u32, h: u32, seed: &mut u64) -> Vec<u8> {
    let mut v = vec![0u8; (w * h * 4) as usize];
    for px in v.chunks_exact_mut(4) {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        px[0] = *seed as u8;
        px[1] = (*seed >> 8) as u8;
        px[2] = (*seed >> 16) as u8;
        px[3] = 255;
    }
    v
}

fn run_stream(codec: Codec) {
    let (w, h) = (1280u32, 720u32);
    let cfg = EncoderConfig { codec, width: w, height: h, fps: 60, bitrate_kbps: 8000, ..Default::default() };
    let Some(mut enc) = skip(Encoder::new(cfg), "Encoder::new") else { return };
    let mut frames = Vec::new();
    for t in 0..40 {
        if t == 25 {
            enc.request_keyframe();
        }
        let f = enc.encode_bgra(&gradient(w, h, t), w, h, w * 4).expect("encode");
        frames.push(f);
    }
    // 1re image : IDR avec jeux de paramètres.
    assert!(frames[0].is_keyframe);
    assert!(nal::contains_idr(codec, &frames[0].data));
    assert!(nal::contains_parameter_sets(codec, &frames[0].data), "IDR must carry parameter sets");
    // GOP long : pas d'IDR spontané, sauf celui demandé à l'image 25.
    let idrs: Vec<usize> = frames.iter().enumerate().filter(|(_, f)| f.is_keyframe).map(|(i, _)| i).collect();
    assert_eq!(idrs, vec![0, 25], "IDR only at start and on request");
    assert!(nal::contains_idr(codec, &frames[25].data));
    assert!(frames.iter().all(|f| !f.data.is_empty() && f.width == w && f.height == h));
    // PTS strictement croissants.
    assert!(frames.windows(2).all(|p| p[1].pts > p[0].pts));
    // Pas de B-frames : exactement 1 sortie par entrée, sans délai ni réordonnancement.
    assert_eq!(frames.len(), 40);
}

#[test]
fn hevc_synthetic_stream() {
    run_stream(Codec::Hevc);
}

#[test]
fn h264_synthetic_stream() {
    run_stream(Codec::H264);
}

#[test]
fn bitrate_change_takes_effect() {
    let (w, h) = (1280u32, 720u32);
    let cfg = EncoderConfig { codec: Codec::Hevc, width: w, height: h, fps: 60, bitrate_kbps: 12_000, ..Default::default() };
    let Some(mut enc) = skip(Encoder::new(cfg), "Encoder::new") else { return };
    let mut seed = 0x9E3779B97F4A7C15u64;
    let mut avg = |enc: &mut Encoder, n: usize| -> f64 {
        let mut sum = 0usize;
        for _ in 0..n {
            sum += enc.encode_bgra(&noise(w, h, &mut seed), w, h, w * 4).unwrap().data.len();
        }
        sum as f64 / n as f64
    };
    avg(&mut enc, 20); // montée en régime (IDR + VBV)
    let high = avg(&mut enc, 30);
    enc.set_bitrate(1_500).unwrap();
    avg(&mut enc, 30);
    let low = avg(&mut enc, 30);
    eprintln!("avg frame size: {high:.0} B @12 Mbps -> {low:.0} B @1.5 Mbps");
    assert!(low < high * 0.5, "bitrate decrease not applied: {high} -> {low}");
}

#[test]
fn source_resolution_change_is_scaled() {
    let cfg = EncoderConfig { codec: Codec::Hevc, width: 1280, height: 720, fps: 60, bitrate_kbps: 6000, ..Default::default() };
    let Some(mut enc) = skip(Encoder::new(cfg), "Encoder::new") else { return };
    for (w, h) in [(640u32, 360u32), (1920, 1080), (1280, 720), (800, 600)] {
        let f = enc.encode_bgra(&gradient(w, h, 1), w, h, w * 4).unwrap();
        assert!(!f.data.is_empty());
        assert_eq!((f.width, f.height), (1280, 720)); // la sortie garde la taille configurée
    }
}

#[test]
fn texture_from_other_device_is_rejected() {
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    let cfg = EncoderConfig { codec: Codec::Hevc, width: 640, height: 360, fps: 60, bitrate_kbps: 2000, ..Default::default() };
    let Some(mut enc) = skip(Encoder::new(cfg), "Encoder::new") else { return };
    let adapters = list_adapters().unwrap();
    let Some(ai) = adapters.iter().find(|a| !a.is_software) else { return };
    let f = unsafe { windows::Win32::Graphics::Dxgi::CreateDXGIFactory1::<windows::Win32::Graphics::Dxgi::IDXGIFactory1>() }.unwrap();
    let adapter = unsafe { f.EnumAdapters1(ai.index) }.unwrap();
    let (other, _ctx) = create_device_for_adapter(&adapter).unwrap();
    let desc = D3D11_TEXTURE2D_DESC {
        Width: 640,
        Height: 360,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut t = None;
    unsafe { other.CreateTexture2D(&desc, None, Some(&mut t)) }.unwrap();
    assert!(enc.encode_texture(&t.unwrap(), 640, 360).is_err());
}

#[test]
fn capture_and_encode_screen() {
    let Some(mut cap) = skip(ScreenCapturer::new(), "ScreenCapturer::new") else { return };
    let (w, h) = cap.size();
    assert!(w > 0 && h > 0);
    let cfg = EncoderConfig { codec: Codec::Hevc, width: w, height: h, fps: 60, bitrate_kbps: 20_000, ..Default::default() };
    let Some(mut enc) = skip(Encoder::with_device(cap.device(), cfg), "Encoder::with_device") else { return };
    // Le premier AcquireNextFrame après (re)création rend toujours l'image courante.
    let f = cap.capture(2000).unwrap().expect("first frame");
    assert_eq!((f.width, f.height), (w, h));
    let e = enc.encode(&f).unwrap();
    assert!(e.is_keyframe && nal::contains_parameter_sets(Codec::Hevc, &e.data));
    // last_frame : ré-encodage de l'image statique (réponse à un request_keyframe).
    enc.request_keyframe();
    let again = cap.last_frame().expect("last frame");
    assert!(enc.encode(&again).unwrap().is_keyframe);
    // Recréation forcée de la duplication (simule ACCESS_LOST) : la capture continue.
    cap.reset();
    let f2 = cap.capture(2000).unwrap().expect("frame after reset");
    assert_eq!((f2.width, f2.height), (w, h));
    assert!(cap.reopen_count >= 2);
    assert!(enc.encode(&f2).is_ok());
}
