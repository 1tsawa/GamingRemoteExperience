use super::*;

fn r(kind: EncoderKind, codec: Codec, format: PixelFormat, ok: bool) -> ProbeResult {
    ProbeResult {
        kind,
        name: "x".to_string(),
        codec,
        format,
        ok,
        reason: if ok { String::new() } else { "refus".to_string() },
        init_ms: 10,
        frame_ms: 2,
        transient: false,
    }
}

#[test]
fn test_verdict() {
    let results = vec![
        r(EncoderKind::Mf(Vendor::Nvidia), Codec::H264, PixelFormat::Yuv420, true),
        r(EncoderKind::Mf(Vendor::Nvidia), Codec::H264, PixelFormat::Yuv420, false),
    ];
    assert_eq!(verdict(&results, EncoderKind::Mf(Vendor::Nvidia), Codec::H264, PixelFormat::Yuv420), Some(Ok(())));
    assert_eq!(verdict(&results, EncoderKind::Mf(Vendor::Nvidia), Codec::H264, PixelFormat::Yuv420P10), None);
    assert_eq!(verdict(&[r(EncoderKind::Mf(Vendor::Nvidia), Codec::H264, PixelFormat::Yuv420, false)], EncoderKind::Mf(Vendor::Nvidia), Codec::H264, PixelFormat::Yuv420), Some(Err("refus".to_string())));
}

#[test]
fn test_infos_from_results() {
    let results = vec![
        r(EncoderKind::Mf(Vendor::Nvidia), Codec::Hevc, PixelFormat::Yuv420, true),
        r(EncoderKind::Mf(Vendor::Nvidia), Codec::Hevc, PixelFormat::Yuv420P10, true),
        r(EncoderKind::Nvenc, Codec::Hevc, PixelFormat::Yuv444, true),
        r(EncoderKind::Nvenc, Codec::Hevc, PixelFormat::Yuv444P10, false),
        r(EncoderKind::OpenH264, Codec::H264, PixelFormat::Yuv420, true),
    ];
    let infos = infos_from_results(&results);
    assert_eq!(infos.len(), 3);
    assert_eq!(infos[0].codec, Codec::Hevc);
    assert_eq!(infos[0].kind, EncoderKind::Mf(Vendor::Nvidia));
    assert_eq!(infos[0].formats, vec![PixelFormat::Yuv420, PixelFormat::Yuv420P10]);
    assert_eq!(infos[1].codec, Codec::Hevc);
    assert_eq!(infos[1].kind, EncoderKind::Nvenc);
    assert_eq!(infos[1].formats, vec![PixelFormat::Yuv444]);
    assert_eq!(infos[2].codec, Codec::H264);
    assert_eq!(infos[2].kind, EncoderKind::OpenH264);
    assert_eq!(infos[2].formats, vec![PixelFormat::Yuv420]);

    assert!(infos_from_results(&[]).is_empty());
}

#[test]
fn test_install_installed_uninstall() {
    let luid1 = 0x7001;
    let luid2 = 0x7002;
    let res1 = vec![r(EncoderKind::Mf(Vendor::Nvidia), Codec::H264, PixelFormat::Yuv420, true)];
    let res2 = vec![r(EncoderKind::Nvenc, Codec::Hevc, PixelFormat::Yuv444, true)];

    assert!(installed(luid1).is_none());
    install(luid1, res1.clone());
    assert_eq!(installed(luid1), Some(res1.clone()));

    install(luid1, res2.clone());
    assert_eq!(installed(luid1), Some(res2.clone()));

    uninstall(luid1);
    assert!(installed(luid1).is_none());

    assert!(installed(luid2).is_none());
    install(luid2, res1.clone());
    assert_eq!(installed(luid2), Some(res1.clone()));
    uninstall(luid2);
}

#[test]
fn test_id_conversions() {
    let kinds = [
        EncoderKind::Mf(Vendor::Nvidia),
        EncoderKind::Mf(Vendor::Amd),
        EncoderKind::Mf(Vendor::Intel),
        EncoderKind::Mf(Vendor::Other),
        EncoderKind::Nvenc,
        EncoderKind::OpenH264,
    ];
    for k in kinds {
        let id = kind_id(k);
        assert_eq!(kind_from_id(id), Some(k));
        assert_eq!(kind_from_id("unknown"), None);
    }

    let codecs = [Codec::H264, Codec::Hevc, Codec::Av1];
    for c in codecs {
        let id = codec_id(c);
        assert_eq!(codec_from_id(id), Some(c));
        assert_eq!(codec_from_id("unknown"), None);
    }

    let formats = [
        PixelFormat::Yuv420,
        PixelFormat::Yuv420P10,
        PixelFormat::Yuv444,
        PixelFormat::Yuv444P10,
    ];
    for f in formats {
        let id = format_id(f);
        assert_eq!(format_from_id(id), Some(f));
        assert_eq!(format_from_id("unknown"), None);
    }
}

#[test]
fn test_probe_result_vendor() {
    let nvenc = r(EncoderKind::Nvenc, Codec::H264, PixelFormat::Yuv420, true);
    assert_eq!(nvenc.vendor(), Vendor::Nvidia);

    let openh264 = r(EncoderKind::OpenH264, Codec::H264, PixelFormat::Yuv420, true);
    assert_eq!(openh264.vendor(), Vendor::Software);

    let mf_amd = r(EncoderKind::Mf(Vendor::Amd), Codec::H264, PixelFormat::Yuv420, true);
    assert_eq!(mf_amd.vendor(), Vendor::Amd);
}
