use super::*;
use grx_video::Codec;
use std::cell::RefCell;

#[test]
fn test_decoder_pref_parse_and_label() {
    assert_eq!(DecoderPref::parse("auto").unwrap(), DecoderPref::Auto);
    assert_eq!(DecoderPref::parse("AUTO").unwrap(), DecoderPref::Auto);
    assert_eq!(DecoderPref::parse("  hardware  ").unwrap(), DecoderPref::Hardware);
    assert_eq!(DecoderPref::parse("matériel").unwrap(), DecoderPref::Hardware);
    assert_eq!(DecoderPref::parse("hw").unwrap(), DecoderPref::Hardware);
    assert_eq!(DecoderPref::parse("software").unwrap(), DecoderPref::Software);
    assert_eq!(DecoderPref::parse("logiciel").unwrap(), DecoderPref::Software);
    assert_eq!(DecoderPref::parse("sw").unwrap(), DecoderPref::Software);

    let err = DecoderPref::parse("x").unwrap_err();
    assert!(err.contains("inconnu"));

    assert_eq!(DecoderPref::Auto.label(), "Auto");
    assert_eq!(DecoderPref::Hardware.label(), "Matériel");
    assert_eq!(DecoderPref::Software.label(), "Logiciel");
}

#[test]
fn test_decoder_kind_constants_and_props() {
    assert_eq!(DecoderPref::Auto.chain(), &DecoderKind::ALL);
    assert_eq!(DecoderPref::Hardware.chain(), &DecoderKind::CHAIN);
    assert_eq!(DecoderPref::Software.chain(), &DecoderKind::SOFTWARE_CHAIN);

    assert_eq!(DecoderKind::ALL.len(), 4);
    assert_eq!(DecoderKind::CHAIN.len(), 2);
    assert_eq!(DecoderKind::SOFTWARE_CHAIN.len(), 2);

    assert!(!DecoderKind::MediaFoundation.is_software());
    assert!(!DecoderKind::FfmpegD3d11va.is_software());
    assert!(DecoderKind::OpenH264.is_software());
    assert!(DecoderKind::FfmpegSoftware.is_software());

    assert_eq!(DecoderKind::MediaFoundation.backend_id(), 1);
    assert_eq!(DecoderKind::FfmpegD3d11va.backend_id(), 2);
    assert_eq!(DecoderKind::OpenH264.backend_id(), 3);
    assert_eq!(DecoderKind::FfmpegSoftware.backend_id(), 3);

    let keys = [
        DecoderKind::MediaFoundation.key(),
        DecoderKind::FfmpegD3d11va.key(),
        DecoderKind::OpenH264.key(),
        DecoderKind::FfmpegSoftware.key(),
    ];
    assert_eq!(keys, ["mf", "ffmpeg", "openh264", "ffsw"]);
    assert_eq!(keys.iter().collect::<std::collections::HashSet<_>>().len(), 4);
}

#[test]
fn test_create_with_pref_software_pref() {
    let recorded = RefCell::new(Vec::new());
    let result = create_with_pref(
        Codec::H264,
        &DisabledDecoders::default(),
        DecoderPref::Software,
        |kind| {
            recorded.borrow_mut().push(kind);
            match kind {
                DecoderKind::OpenH264 => Err("fail oh264".into()),
                DecoderKind::FfmpegSoftware => Ok(42),
                _ => unreachable!(),
            }
        },
    );
    assert!(result.is_ok());
    let (val, kind, attempts) = result.unwrap();
    assert_eq!(val, 42);
    assert_eq!(kind, DecoderKind::FfmpegSoftware);
    assert_eq!(attempts, vec![Attempt { kind: DecoderKind::OpenH264, error: "fail oh264".into() }]);
    assert_eq!(*recorded.borrow(), vec![DecoderKind::OpenH264, DecoderKind::FfmpegSoftware]);
    assert!(!recorded.borrow().iter().any(|k| !k.is_software()));
}

#[test]
fn test_create_with_pref_auto_pref_hardware_fail() {
    let result = create_with_pref(
        Codec::H264,
        &DisabledDecoders::default(),
        DecoderPref::Auto,
        |kind| match kind {
            DecoderKind::MediaFoundation => Err("mf fail".into()),
            DecoderKind::FfmpegD3d11va => Err("ffm fail".into()),
            DecoderKind::OpenH264 => Ok(99),
            DecoderKind::FfmpegSoftware => unreachable!(),
        },
    );
    assert!(result.is_ok());
    let (_, kind, attempts) = result.unwrap();
    assert_eq!(kind, DecoderKind::OpenH264);
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].kind, DecoderKind::MediaFoundation);
    assert_eq!(attempts[1].kind, DecoderKind::FfmpegD3d11va);
}

#[test]
fn test_create_with_pref_all_fail_display() {
    let make = |_: DecoderKind| -> Result<i32, String> { Err("impossible".into()) };
    let err_hw = create_with_pref(Codec::H264, &DisabledDecoders::default(), DecoderPref::Hardware, make).unwrap_err();
    assert_eq!(err_hw.attempts.len(), 2);
    assert!(err_hw.to_string().contains("matériel"));

    let err_sw = create_with_pref(Codec::H264, &DisabledDecoders::default(), DecoderPref::Software, make).unwrap_err();
    assert!(err_sw.to_string().contains("logiciel"));
}

#[test]
fn test_disabled_decoders_hevc_h264() {
    let disabled = DisabledDecoders::parse("openh264,ffsw-hevc").unwrap();
    assert!(disabled.is_disabled(DecoderKind::OpenH264, Codec::H264));
    assert!(disabled.is_disabled(DecoderKind::FfmpegSoftware, Codec::Hevc));
    assert!(!disabled.is_disabled(DecoderKind::FfmpegSoftware, Codec::H264));

    let err_hevc = create_with_pref(
        Codec::Hevc,
        &disabled,
        DecoderPref::Software,
        |_| Ok::<_, String>("ok"),
    ).unwrap_err();
    assert_eq!(err_hevc.attempts.len(), 2);
    assert!(err_hevc.attempts[0].error.contains("désactivé"));
    assert!(err_hevc.attempts[1].error.contains("désactivé"));

    let res_h264 = create_with_pref(
        Codec::H264,
        &disabled,
        DecoderPref::Software,
        |_| Ok::<_, String>("ok"),
    ).unwrap();
    assert_eq!(res_h264.1, DecoderKind::FfmpegSoftware);
    assert_eq!(res_h264.2.len(), 1);
    assert!(res_h264.2[0].error.contains("désactivé"));
}

#[test]
fn test_create_with_old_api_hardware_only() {
    let recorded = RefCell::new(Vec::new());
    let _ = create_with::<i32>(
        Codec::H264,
        &DisabledDecoders::default(),
        |kind| {
            recorded.borrow_mut().push(kind);
            Err("fail".into())
        },
    );
    assert_eq!(*recorded.borrow(), vec![DecoderKind::MediaFoundation, DecoderKind::FfmpegD3d11va]);
}
