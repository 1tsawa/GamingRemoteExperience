//! Phase 2 (GPU réel, ignoré proprement sans l'encodeur) : sélection multi-vendeurs, NVENC
//! direct 4:4:4, MFT 10 bits (P010 / Main10), mesures courtes de latence d'encodage.
#![cfg(windows)]

use grx_video::*;
use std::time::Instant;

fn pattern(w: u32, h: u32, t: u32) -> Vec<u8> {
    let mut v = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            // Texte coloré type bureau : fines rayures rouge/bleu (là où le 4:2:0 bave).
            let stripe = ((x + t) / 2) % 2 == 0;
            v[i..i + 4].copy_from_slice(if stripe { &[0, 0, 255, 255] } else { &[255, 0, 0, 255] });
            if y % 32 < 16 {
                v[i + 1] = ((x + y) & 0xFF) as u8;
            }
        }
    }
    v
}

/// Premier NAL de type `ty` (HEVC : (b0 >> 1) & 0x3F ; H.264 : b0 & 0x1F).
fn find_nal(data: &[u8], hevc: bool, ty: u8) -> Option<&[u8]> {
    let mut i = 0;
    let mut starts = Vec::new();
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts.iter().enumerate().find_map(|(k, &s)| {
        let t = if hevc { (data[s] >> 1) & 0x3F } else { data[s] & 0x1F };
        (t == ty).then(|| &data[s..starts.get(k + 1).copied().unwrap_or(data.len())])
    })
}

/// `general_profile_idc` d'un SPS HEVC (octet 3 après l'en-tête NAL de 2 octets).
fn hevc_profile(sps: &[u8]) -> u8 {
    sps[3] & 0x1F
}

fn encode_n(enc: &mut Encoder, w: u32, h: u32, n: u32) -> (Vec<EncodedFrame>, f64) {
    let mut out = Vec::new();
    let mut total = 0.0;
    for t in 0..n {
        let px = pattern(w, h, t);
        let t0 = Instant::now();
        let f = enc.encode_bgra(&px, w, h, w * 4).unwrap_or_else(|e| panic!("encode {t}: {e}"));
        if t > 0 {
            total += t0.elapsed().as_secs_f64() * 1000.0;
        }
        out.push(f);
    }
    (out, total / (n - 1).max(1) as f64)
}

#[test]
fn nvenc_444_hevc_and_h264() {
    if nvenc_availability().is_err() {
        eprintln!("SKIP : {:?}", nvenc_availability());
        return;
    }
    let (w, h) = (1280u32, 720u32);
    // H.264 4:4:4 n'est plus proposé (débit non respecté, voir nvenc.rs) : HEVC seulement.
    let h264 = EncoderConfig { codec: Codec::H264, width: 640, height: 360, format: PixelFormat::Yuv444, ..Default::default() };
    assert!(Encoder::new(h264).is_err());
    for codec in [Codec::Hevc] {
        {
            let format = PixelFormat::Yuv444;
            let cfg = EncoderConfig { codec, width: w, height: h, bitrate_kbps: 15_000, format, ..Default::default() };
            let mut enc = Encoder::new(cfg).expect("NVENC 4:4:4");
            assert!(matches!(enc.backend(), EncoderBackend::Nvenc { .. }), "{:?}", enc.backend());
            let (frames, ms) = encode_n(&mut enc, w, h, 30);
            assert!(frames[0].is_keyframe, "première image = IDR");
            assert!(frames.iter().skip(1).all(|f| !f.is_keyframe));
            let bytes: usize = frames.iter().map(|f| f.data.len()).sum();
            println!("tailles {codec:?} : {:?}", frames.iter().map(|f| f.data.len() / 1024).collect::<Vec<_>>());
            let k = &frames[0].data;
            match codec {
                Codec::Hevc => {
                    let sps = find_nal(k, true, 33).expect("SPS HEVC");
                    assert_eq!(hevc_profile(sps), 4, "profil RExt (4)");
                    assert!(find_nal(k, true, 32).is_some(), "VPS");
                }
                _ => {
                    let sps = find_nal(k, false, 7).expect("SPS H.264");
                    assert_eq!(sps[1], 244, "profil High 4:4:4 Predictive (244)");
                }
            }
            enc.request_keyframe();
            let f = enc.encode_bgra(&pattern(w, h, 99), w, h, w * 4).unwrap();
            assert!(f.is_keyframe, "IDR à la demande");
            enc.set_bitrate(5_000).expect("reconfiguration du débit");
            let f = enc.encode_bgra(&pattern(w, h, 100), w, h, w * 4).unwrap();
            assert!(!f.data.is_empty());
            println!(
                "MESURE NVENC {codec:?} {} {w}x{h} : {ms:.2} ms/image (appel), {:.0} ko pour 30 images",
                format.label(),
                bytes as f64 / 1024.0
            );
        }
    }
}

#[test]
fn nvenc_444_10bit_is_refused_cleanly() {
    let cfg = EncoderConfig { codec: Codec::Hevc, width: 640, height: 360, format: PixelFormat::Yuv444P10, ..Default::default() };
    assert!(Encoder::new(cfg).is_err());
}

#[test]
fn mf_hevc_main10_p010() {
    let (w, h) = (1280u32, 720u32);
    let cfg = EncoderConfig { codec: Codec::Hevc, width: w, height: h, bitrate_kbps: 10_000, format: PixelFormat::Yuv420P10, ..Default::default() };
    let mut enc = match Encoder::new(cfg) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("SKIP HEVC Main10 : {e}");
            return;
        }
    };
    let (frames, ms) = encode_n(&mut enc, w, h, 20);
    let sps = find_nal(&frames[0].data, true, 33).expect("SPS");
    assert_eq!(hevc_profile(sps), 2, "profil Main10 (2)");
    println!("MESURE MF HEVC Main10 {w}x{h} ({:?}) : {ms:.2} ms/image", enc.backend().name());
}

#[test]
fn vendor_selection_on_this_adapter() {
    let encs = encoders_for_adapter(None);
    println!("encodeurs : {encs:#?}");
    // Un vendeur absent ne doit jamais être choisi.
    let cfg = |pref| EncoderConfig { codec: Codec::H264, width: 640, height: 360, pref, ..Default::default() };
    for (pref, ven) in [(EncoderPref::Amd, Vendor::Amd), (EncoderPref::Intel, Vendor::Intel), (EncoderPref::Nvidia, Vendor::Nvidia)] {
        let present = encs.iter().any(|e| e.kind == EncoderKind::Mf(ven) && e.codec == Codec::H264);
        match Encoder::new(cfg(pref)) {
            Ok(e) => {
                assert!(present, "{pref:?} choisi sans MFT {ven:?}");
                assert_eq!(e.backend().vendor(), ven);
            }
            Err(err) => {
                assert!(!present, "{pref:?} présent mais échec : {err}");
                println!("{pref:?} : {err}");
            }
        }
    }
    // Logiciel : OpenH264 absent ici => erreur claire, présent => backend logiciel.
    match Encoder::new(cfg(EncoderPref::Software)) {
        Ok(e) => assert_eq!(e.backend().vendor(), Vendor::Software),
        Err(err) => {
            assert!(openh264::availability().is_err());
            assert!(err.to_string().contains("logiciel"), "{err}");
        }
    }
}
