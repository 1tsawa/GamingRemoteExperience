use super::*;
use crate::{Codec, PixelFormat};

#[test]
fn test_vendor_from_pci() {
    assert_eq!(Vendor::from_pci(0x10DE), Vendor::Nvidia);
    assert_eq!(Vendor::from_pci(0x1002), Vendor::Amd);
    assert_eq!(Vendor::from_pci(0x8086), Vendor::Intel);
    assert_eq!(Vendor::from_pci(0x1234), Vendor::Other);
}

#[test]
fn test_vendor_from_mft() {
    assert_eq!(Vendor::from_mft(Some("VEN_10DE"), "x"), Vendor::Nvidia);
    assert_eq!(Vendor::from_mft(Some("ven_8086&dev_1234"), ""), Vendor::Intel);
    assert_eq!(Vendor::from_mft(None, "AMDh265Encoder"), Vendor::Amd);
    assert_eq!(Vendor::from_mft(None, "Intel® Quick Sync Video H.264 Encoder MFT"), Vendor::Intel);
    assert_eq!(Vendor::from_mft(None, "NVIDIA HEVC Encoder MFT"), Vendor::Nvidia);
    assert_eq!(Vendor::from_mft(Some("garbage"), "foo"), Vendor::Other);
}

#[test]
fn test_encoder_pref_parse() {
    for s in ["auto", "NVIDIA", "nvenc", "amd", "amf", "intel", "qsv", "software", "openh264", "  Auto  "] {
        assert!(EncoderPref::parse(s).is_ok(), "Failed to parse: {}", s);
    }
    let res = EncoderPref::parse("x");
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("inconnu"));
}

#[test]
fn test_plan_routes_cases() {
    assert_eq!(plan_routes(EncoderPref::Auto, Codec::Hevc, PixelFormat::Yuv420, Vendor::Nvidia, &[Vendor::Nvidia], false, true), vec![Route::Mf(0)]);
    assert_eq!(plan_routes(EncoderPref::Auto, Codec::H264, PixelFormat::Yuv420, Vendor::Intel, &[Vendor::Intel, Vendor::Other], false, true), vec![Route::Mf(0), Route::Mf(1), Route::OpenH264]);
    assert_eq!(plan_routes(EncoderPref::Auto, Codec::H264, PixelFormat::Yuv420, Vendor::Amd, &[Vendor::Other, Vendor::Amd], false, false), vec![Route::Mf(1), Route::Mf(0)]);
    assert_eq!(plan_routes(EncoderPref::Auto, Codec::H264, PixelFormat::Yuv420, Vendor::Nvidia, &[], false, true), vec![Route::OpenH264]);
    assert_eq!(plan_routes(EncoderPref::Software, Codec::H264, PixelFormat::Yuv420, Vendor::Nvidia, &[Vendor::Nvidia], true, true), vec![Route::OpenH264]);
    assert_eq!(plan_routes(EncoderPref::Software, Codec::Hevc, PixelFormat::Yuv420, Vendor::Nvidia, &[Vendor::Nvidia], true, true), Vec::<Route>::new());
    assert_eq!(plan_routes(EncoderPref::Software, Codec::H264, PixelFormat::Yuv420P10, Vendor::Nvidia, &[Vendor::Nvidia], true, true), Vec::<Route>::new());
    assert_eq!(plan_routes(EncoderPref::Intel, Codec::H264, PixelFormat::Yuv420, Vendor::Intel, &[Vendor::Nvidia, Vendor::Intel], false, true), vec![Route::Mf(1)]);
    assert_eq!(plan_routes(EncoderPref::Amd, Codec::Hevc, PixelFormat::Yuv420, Vendor::Nvidia, &[Vendor::Nvidia], false, false), Vec::<Route>::new());

    // 4:4:4 cases
    assert_eq!(plan_routes(EncoderPref::Auto, Codec::Hevc, PixelFormat::Yuv444, Vendor::Nvidia, &[Vendor::Nvidia], true, true), vec![Route::Nvenc]);
    assert_eq!(plan_routes(EncoderPref::Nvidia, Codec::H264, PixelFormat::Yuv444P10, Vendor::Nvidia, &[Vendor::Nvidia], true, false), vec![Route::Nvenc]);
    assert_eq!(plan_routes(EncoderPref::Auto, Codec::Hevc, PixelFormat::Yuv444, Vendor::Nvidia, &[Vendor::Nvidia], false, true), Vec::<Route>::new());
    assert_eq!(plan_routes(EncoderPref::Auto, Codec::Hevc, PixelFormat::Yuv444, Vendor::Intel, &[Vendor::Intel], true, true), Vec::<Route>::new());
    assert_eq!(plan_routes(EncoderPref::Intel, Codec::Hevc, PixelFormat::Yuv444, Vendor::Nvidia, &[Vendor::Nvidia], true, true), Vec::<Route>::new());

    // 10-bit 4:2:0 with Auto on Nvidia
    assert_eq!(plan_routes(EncoderPref::Auto, Codec::H264, PixelFormat::Yuv420P10, Vendor::Nvidia, &[Vendor::Nvidia], false, true), vec![Route::Mf(0)]);
}

#[test]
fn test_plan_routes_exhaustive() {
    let prefs = [EncoderPref::Auto, EncoderPref::Nvidia, EncoderPref::Amd, EncoderPref::Intel, EncoderPref::Software];
    let codecs = [Codec::H264, Codec::Hevc, Codec::Av1];
    let formats = PixelFormat::ALL;
    let adapters = [Vendor::Nvidia, Vendor::Amd, Vendor::Intel, Vendor::Other];
    let _vendors = [Vendor::Nvidia, Vendor::Amd, Vendor::Intel];
    let nvenc_vals = [false, true];
    let oh_vals = [false, true];

    let mft_lists: Vec<&[Vendor]> = vec![
        &[],
        &[Vendor::Nvidia], &[Vendor::Amd], &[Vendor::Intel],
        &[Vendor::Nvidia, Vendor::Nvidia], &[Vendor::Nvidia, Vendor::Amd], &[Vendor::Nvidia, Vendor::Intel],
        &[Vendor::Amd, Vendor::Nvidia], &[Vendor::Amd, Vendor::Amd], &[Vendor::Amd, Vendor::Intel],
        &[Vendor::Intel, Vendor::Nvidia], &[Vendor::Intel, Vendor::Amd], &[Vendor::Intel, Vendor::Intel],
    ];

    for &pref in &prefs {
        for &codec in &codecs {
            for &format in formats.iter() {
                for &adapter in &adapters {
                    for &nvenc in &nvenc_vals {
                        for &oh in &oh_vals {
                            for &mfts in &mft_lists {
                                let routes = plan_routes(pref, codec, format, adapter, mfts, nvenc, oh);

                                // no route index out of range
                                for r in &routes {
                                    if let Route::Mf(i) = r {
                                        assert!(*i < mfts.len(), "Index out of range: {} vs {}", i, mfts.len());
                                    }
                                }

                                // no duplicate routes
                                let mut seen = std::collections::HashSet::new();
                                for r in &routes {
                                    assert!(seen.insert(r), "Duplicate route: {:?}", r);
                                }

                                // OpenH264 only if codec==H264 && format==Yuv420 && oh
                                if routes.contains(&Route::OpenH264) {
                                    assert_eq!(codec, Codec::H264);
                                    assert_eq!(format, PixelFormat::Yuv420);
                                    assert!(oh);
                                }

                                // Nvenc only if format.is_444()
                                if routes.contains(&Route::Nvenc) {
                                    assert!(format.is_444());
                                }

                                // for a vendor pref every Mf(i) has mfts[i]==that vendor
                                if let Some(want) = pref.vendor() {
                                    for r in &routes {
                                        if let Route::Mf(i) = r {
                                            assert_eq!(mfts[*i], want, "Mismatch at index {}: expected {:?}, got {:?}", i, want, mfts[*i]);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn test_soft_size() {
    assert_eq!(soft_size(2560, 1440, 16), (1920, 1080));
    assert_eq!(soft_size(2560, 1440, 4), (1280, 720));
    assert_eq!(soft_size(1920, 1080, 2), (960, 540));
    assert_eq!(soft_size(1280, 720, 16), (1280, 720));

    let (w, h) = soft_size(3440, 1440, 8);
    assert!(w <= 1600 && h <= 900);
    assert_eq!(w % 2, 0);
    assert_eq!(h % 2, 0);
    let aspect = 3440.0 / 1440.0;
    let result_aspect = w as f64 / h as f64;
    assert!((aspect - result_aspect).abs() / aspect < 0.01);

    assert_eq!(soft_size(0, 0, 8), (2, 2));

    // results always even
    for w in [0, 1, 100, 1920, 3440] {
        for h in [0, 1, 100, 1080, 1440] {
            for c in 1..=20 {
                let (rw, rh) = soft_size(w, h, c);
                assert_eq!(rw % 2, 0);
                assert_eq!(rh % 2, 0);
            }
        }
    }
}

#[test]
fn test_soft_max_kbps() {
    assert_eq!(soft_max_kbps(1920, 1080), 12441);
    assert_eq!(soft_max_kbps(100, 100), 2000);
    assert_eq!(soft_max_kbps(3840, 2160), 15000);
}
