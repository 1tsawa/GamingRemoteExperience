use super::*;
use grx_video::Codec;

#[test]
fn disabled_decoders_parse_empty_and_none() {
    assert!(DisabledDecoders::parse("").unwrap().is_empty());
    assert!(DisabledDecoders::parse("none").unwrap().is_empty());
}

#[test]
fn disabled_decoders_parse_mf_hevc() {
    let d = DisabledDecoders::parse("mf-hevc").unwrap();
    assert!(d.is_disabled(DecoderKind::MediaFoundation, Codec::Hevc));
    assert!(!d.is_disabled(DecoderKind::MediaFoundation, Codec::H264));
    assert!(!d.is_disabled(DecoderKind::FfmpegD3d11va, Codec::Hevc));
}

#[test]
fn disabled_decoders_parse_ffmpeg_all() {
    let d = DisabledDecoders::parse("ffmpeg").unwrap();
    for c in [Codec::H264, Codec::Hevc, Codec::Av1] {
        assert!(d.is_disabled(DecoderKind::FfmpegD3d11va, c));
    }
}

#[test]
fn disabled_decoders_parse_av1_only() {
    let d = DisabledDecoders::parse("av1").unwrap();
    for k in DecoderKind::CHAIN {
        assert!(d.is_disabled(k, Codec::Av1));
        assert!(!d.is_disabled(k, Codec::H264));
        assert!(!d.is_disabled(k, Codec::Hevc));
    }
}

#[test]
fn disabled_decoders_parse_whitespace_and_case() {
    let d = DisabledDecoders::parse(" MF-HEVC , ffmpeg-av1 ").unwrap();
    assert!(d.is_disabled(DecoderKind::MediaFoundation, Codec::Hevc));
    assert!(d.is_disabled(DecoderKind::FfmpegD3d11va, Codec::Av1));
}

#[test]
fn disabled_decoders_parse_errors() {
    assert!(DisabledDecoders::parse("foo").is_err());
    assert!(DisabledDecoders::parse("mf-vp9").is_err());
    assert!(DisabledDecoders::parse("x-hevc").is_err());
}

#[test]
fn create_with_mf_succeeds_ffmpeg_not_called() {
    let mut ffmpeg_calls = 0;
    let make = |kind| {
        if kind == DecoderKind::FfmpegD3d11va {
            ffmpeg_calls += 1;
        }
        if kind == DecoderKind::MediaFoundation {
            Ok("mf_decoder")
        } else {
            Err::<&str, String>("ffmpeg_err".into())
        }
    };
    let (val, kind, attempts) = create_with(Codec::Hevc, &DisabledDecoders::default(), make).unwrap();
    assert_eq!(val, "mf_decoder");
    assert_eq!(kind, DecoderKind::MediaFoundation);
    assert!(attempts.is_empty());
    assert_eq!(ffmpeg_calls, 0);
}

#[test]
fn create_with_mf_fails_ffmpeg_succeeds() {
    let make = |kind| {
        if kind == DecoderKind::MediaFoundation {
            Err::<&str, String>("pas d'extension".into())
        } else {
            Ok("ffmpeg_decoder".into())
        }
    };
    let (_val, kind, attempts) = create_with(Codec::Hevc, &DisabledDecoders::default(), make).unwrap();
    assert_eq!(kind, DecoderKind::FfmpegD3d11va);
    assert_eq!(attempts, vec![Attempt { kind: DecoderKind::MediaFoundation, error: "pas d'extension".into() }]);
}

#[test]
fn create_with_both_fail_display() {
    let make = |_| Err::<&str, String>("échec".into());
    let res = create_with(Codec::Hevc, &DisabledDecoders::default(), make);
    assert!(res.is_err());
    let err = res.unwrap_err();
    assert_eq!(err.codec, Codec::Hevc);
    assert_eq!(err.attempts.len(), 2);
    let display = format!("{}", err);
    assert!(display.contains("HEVC"));
    assert!(display.contains("MF matériel"));
    assert!(display.contains("FFmpeg D3D11VA"));
    assert!(display.contains("HEVC Video Extensions"));
    assert!(display.contains("H.264"));
    assert!(!display.contains("pare-feu"));
}

#[test]
fn create_with_disabled_mf_hevc() {
    let mut mf_called = false;
    let make = |kind| {
        if kind == DecoderKind::MediaFoundation {
            mf_called = true;
        }
        if kind == DecoderKind::MediaFoundation {
            Err::<&str, String>("mf_err".into())
        } else {
            Ok("ffmpeg_decoder".into())
        }
    };
    let disabled = DisabledDecoders::parse("mf-hevc").unwrap();
    let (val, kind, attempts) = create_with(Codec::Hevc, &disabled, make).unwrap();
    assert!(!mf_called);
    assert_eq!(kind, DecoderKind::FfmpegD3d11va);
    assert_eq!(val, "ffmpeg_decoder");
    assert_eq!(attempts[0].error, "désactivé (--disable-decoder)");
}

#[test]
fn create_with_disabled_all() {
    let mut calls = 0;
    let make = |_| { calls += 1; Err::<&str, String>("err".into()) };
    let disabled = DisabledDecoders::parse("mf,ffmpeg").unwrap();
    let res = create_with(Codec::Hevc, &disabled, make);
    assert!(res.is_err());
    assert_eq!(calls, 0);
}

#[test]
fn create_with_h264_advice_no_hevc_ext() {
    let make = |_| Err::<&str, String>("échec".into());
    let err = create_with(Codec::H264, &DisabledDecoders::default(), make).unwrap_err();
    let advice = err.advice();
    assert!(!advice.contains("HEVC Video Extensions"));
}

#[test]
fn create_with_av1_advice_mentions_av1_ext() {
    let make = |_| Err::<&str, String>("échec".into());
    let err = create_with(Codec::Av1, &DisabledDecoders::default(), make).unwrap_err();
    let advice = err.advice();
    assert!(advice.contains("AV1 Video Extension"));
}

#[test]
fn scenario_intel_no_hevc_ext() {
    let codecs = [Codec::Hevc, Codec::H264, Codec::Av1];
    for c in codecs {
        let mf_ok = c == Codec::H264 || c == Codec::Av1;
        let make = |kind| {
            if kind == DecoderKind::MediaFoundation {
                if mf_ok { Ok("mf".into()) } else { Err::<&str, String>("pas d'extension".into()) }
            } else {
                Err::<&str, String>("DLL absentes".into())
            }
        };
        let res = create_with(c, &DisabledDecoders::default(), make);
        if c == Codec::Hevc {
            assert!(res.is_err());
        } else {
            let (val, kind, _) = res.unwrap();
            assert_eq!(val, "mf");
            assert_eq!(kind, DecoderKind::MediaFoundation);
        }
    }

    for c in codecs {
        let make = |kind| {
            if kind == DecoderKind::MediaFoundation {
                Err::<&str, String>("pas d'extension".into())
            } else {
                Ok("ffmpeg".into())
            }
        };
        let res = create_with(c, &DisabledDecoders::default(), make);
        let (val, kind, _) = res.unwrap();
        assert_eq!(val, "ffmpeg");
        assert_eq!(kind, DecoderKind::FfmpegD3d11va);
    }
}

#[test]
fn decoder_kind_properties() {
    assert_eq!(DecoderKind::MediaFoundation.key(), "mf");
    assert_eq!(DecoderKind::FfmpegD3d11va.key(), "ffmpeg");
    assert_eq!(DecoderKind::MediaFoundation.label(), "MF matériel");
    assert_eq!(DecoderKind::FfmpegD3d11va.label(), "FFmpeg D3D11VA");
    assert_eq!(DecoderKind::MediaFoundation.backend_id(), 1);
    assert_eq!(DecoderKind::FfmpegD3d11va.backend_id(), 2);
    assert_eq!(DecoderKind::CHAIN[0], DecoderKind::MediaFoundation);
    assert_eq!(codec_key(Codec::H264), "h264");
    assert_eq!(codec_key(Codec::Hevc), "hevc");
    assert_eq!(codec_key(Codec::Av1), "av1");
}
