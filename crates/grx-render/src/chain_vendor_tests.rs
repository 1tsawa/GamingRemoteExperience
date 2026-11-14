use super::*;
use grx_video::Codec;

#[test]
fn intel_auto_chain() {
    let mut kinds = DecoderKind::ALL.to_vec();
    order_for_vendor(&mut kinds, VENDOR_INTEL);
    assert_eq!(kinds, vec![
        DecoderKind::FfmpegD3d11va,
        DecoderKind::MediaFoundation,
        DecoderKind::OpenH264,
        DecoderKind::FfmpegSoftware,
    ]);
}

#[test]
fn nvidia_amd_unknown_unchanged() {
    for vendor_id in [0x10DE, 0x1002, 0u32] {
        let mut chain = DecoderKind::CHAIN.to_vec();
        order_for_vendor(&mut chain, vendor_id);
        assert_eq!(chain, DecoderKind::CHAIN.to_vec());

        let mut all = DecoderKind::ALL.to_vec();
        order_for_vendor(&mut all, vendor_id);
        assert_eq!(all, DecoderKind::ALL.to_vec());
    }
}

#[test]
fn software_chain_unchanged_on_intel() {
    let mut kinds = DecoderKind::SOFTWARE_CHAIN.to_vec();
    order_for_vendor(&mut kinds, VENDOR_INTEL);
    assert_eq!(kinds, DecoderKind::SOFTWARE_CHAIN.to_vec());
}

#[test]
fn intel_falls_back_to_mf() {
    let disabled = DisabledDecoders::default();
    let pref = DecoderPref::Auto;
    let vendor_id = VENDOR_INTEL;
    let codec = Codec::Hevc;

    let result = create_with_pref_vendor(
        codec,
        &disabled,
        pref,
        vendor_id,
        |kind| {
            if kind == DecoderKind::FfmpegD3d11va {
                Err("x".into())
            } else {
                Ok(kind)
            }
        },
    );

    assert!(result.is_ok());
    let (t, kind, attempts) = result.unwrap();
    assert_eq!(kind, DecoderKind::MediaFoundation);
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].kind, DecoderKind::FfmpegD3d11va);
    assert_eq!(t, DecoderKind::MediaFoundation);
}

#[test]
fn nvidia_tries_mf_first() {
    let disabled = DisabledDecoders::default();
    let pref = DecoderPref::Auto;
    let vendor_id = 0x10DE;
    let codec = Codec::Hevc;

    let result = create_with_pref_vendor(
        codec,
        &disabled,
        pref,
        vendor_id,
        |kind| Ok(kind),
    );

    assert!(result.is_ok());
    let (t, kind, attempts) = result.unwrap();
    assert_eq!(kind, DecoderKind::MediaFoundation);
    assert!(attempts.is_empty());
    assert_eq!(t, DecoderKind::MediaFoundation);
}
