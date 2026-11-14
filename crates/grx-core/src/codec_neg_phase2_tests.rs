use super::*;

#[test]
fn test_formatset_single_and_properties() {
    assert_eq!(FormatSet::single(false, false), FormatSet::YUV420_8);
    assert_eq!(FormatSet::single(false, true), FormatSet::YUV420_10);
    assert_eq!(FormatSet::single(true, false), FormatSet::YUV444_8);
    assert_eq!(FormatSet::single(true, true), FormatSet::YUV444_10);

    assert!(!FormatSet::YUV420_8.is_444());
    assert!(!FormatSet::YUV420_10.is_444());
    assert!(FormatSet::YUV444_8.is_444());
    assert!(FormatSet::YUV444_10.is_444());

    assert!(!FormatSet::YUV420_8.is_10bit());
    assert!(FormatSet::YUV420_10.is_10bit());
    assert!(!FormatSet::YUV444_8.is_10bit());
    assert!(FormatSet::YUV444_10.is_10bit());

    assert_eq!(FormatSet::YUV420_8.iter().collect::<Vec<_>>(), vec![FormatSet::YUV420_8]);
    assert_eq!(FormatSet::YUV444_10.iter().collect::<Vec<_>>(), vec![FormatSet::YUV444_10]);

    assert_eq!(FormatSet::YUV420_8.label(), "4:2:0 8 bits");
    assert_eq!(FormatSet::YUV444_10.label(), "4:4:4 10 bits");
    assert_eq!(FormatSet::default().label(), "aucun");
}

#[test]
fn test_formatprefs_roundtrip_and_mask() {
    for y444 in [false, true] {
        for t10 in [false, true] {
            for hdr in [false, true] {
                let prefs = FormatPrefs { yuv444: y444, ten_bit: t10, hdr_display: hdr };
                let byte = prefs.to_byte();
                assert_eq!(FormatPrefs::from_byte(byte), prefs);
                assert_eq!(FormatPrefs::from_byte(0xF8 | byte), prefs);
            }
        }
    }
}

#[test]
fn test_client_codecs_roundtrip_and_key5_validation() {
    let cc = ClientCodecs {
        decoders: vec![CodecCap::new(VideoCodec::Hevc, BackendId::NVENC)],
        preferred: Some(VideoCodec::Hevc),
        renegotiate: false,
        decoder_mode: DecoderMode::Auto,
        prefs: FormatPrefs { yuv444: true, ten_bit: true, hdr_display: true },
    };
    let encoded = cc.encode();
    assert_eq!(ClientCodecs::decode(&encoded), Some(cc));

    // Key 5 length 2 -> rejected
    let mut msg = Vec::new();
    msg.extend_from_slice(CLIENT_CODECS_MAGIC);
    msg.push(VERSION);
    msg.push(1);
    msg.push(5);
    msg.push(2);
    msg.extend_from_slice(&[0x00, 0x01]);
    assert!(ClientCodecs::decode(&msg).is_none());

    // Key 5 length 1, value 0xFF -> all prefs true
    let mut msg2 = Vec::new();
    msg2.extend_from_slice(CLIENT_CODECS_MAGIC);
    msg2.push(VERSION);
    msg2.push(1);
    msg2.push(5);
    msg2.push(1);
    msg2.push(0xFF);
    let dec2 = ClientCodecs::decode(&msg2).unwrap();
    assert!(dec2.prefs.yuv444);
    assert!(dec2.prefs.ten_bit);
    assert!(dec2.prefs.hdr_display);
}

#[test]
fn test_host_codec_roundtrip_and_key_validation() {
    let hc = HostCodec {
        codec: VideoCodec::Hevc,
        format: FormatSet::YUV444_10,
        encoders: vec![CodecCap::new(VideoCodec::Hevc, BackendId::NVENC)],
        reason: ChoiceReason::Auto,
        encoder: Some(BackendId::NVENC),
        transfer: Transfer::Pq,
    };
    let encoded = hc.encode();
    assert_eq!(HostCodec::decode(&encoded), Some(hc));

    // Key 5 unknown backend 99 -> encoder None
    let mut msg = Vec::new();
    msg.extend_from_slice(HOST_CODEC_MAGIC);
    msg.push(VERSION);
    msg.push(2);
    msg.push(2); msg.push(1); msg.push(VideoCodec::Hevc.id());
    msg.push(5); msg.push(1); msg.push(99);
    let dec = HostCodec::decode(&msg).unwrap();
    assert_eq!(dec.encoder, None);

    // Key 6 value 2 -> None
    let mut msg2 = Vec::new();
    msg2.extend_from_slice(HOST_CODEC_MAGIC);
    msg2.push(VERSION);
    msg2.push(2);
    msg2.push(2); msg2.push(1); msg2.push(VideoCodec::Hevc.id());
    msg2.push(6); msg2.push(1); msg2.push(2);
    assert!(HostCodec::decode(&msg2).is_none());

    // Key 6 length 0 -> None
    let mut msg3 = Vec::new();
    msg3.extend_from_slice(HOST_CODEC_MAGIC);
    msg3.push(VERSION);
    msg3.push(2);
    msg3.push(2); msg3.push(1); msg3.push(VideoCodec::Hevc.id());
    msg3.push(6); msg3.push(0);
    assert!(HostCodec::decode(&msg3).is_none());
}

#[test]
fn test_choose_format_cases() {
    let caps = |c: VideoCodec, b: BackendId, f: FormatSet| CodecCap { codec: c, backend: b, formats: f };

    // 1. no client
    assert_eq!(choose_format(VideoCodec::Hevc, &[], None, ChromaPref::Auto, false), FormatSet::YUV420_8);

    // 2. client prefs default -> YUV420_8 even if both support 444
    let host_enc = vec![caps(VideoCodec::Hevc, BackendId::NVENC, FormatSet::YUV444_8 | FormatSet::YUV420_8)];
    let client = Some(&ClientCodecs {
        decoders: vec![caps(VideoCodec::Hevc, BackendId::NVENC, FormatSet::YUV444_8 | FormatSet::YUV420_8)],
        ..Default::default()
    });
    assert_eq!(choose_format(VideoCodec::Hevc, &host_enc, client, ChromaPref::Auto, false), FormatSet::YUV420_8);

    // 3. client yuv444 pref + host NVENC HEVC 444_8|420_8 + client HEVC 444_8|420_8 -> YUV444_8
    let client3 = Some(&ClientCodecs {
        decoders: vec![caps(VideoCodec::Hevc, BackendId::NVENC, FormatSet::YUV444_8 | FormatSet::YUV420_8)],
        prefs: FormatPrefs { yuv444: true, ..Default::default() },
        ..Default::default()
    });
    assert_eq!(choose_format(VideoCodec::Hevc, &host_enc, client3, ChromaPref::Auto, false), FormatSet::YUV444_8);

    // 4. same with ChromaPref::Yuv420 -> YUV420_8
    assert_eq!(choose_format(VideoCodec::Hevc, &host_enc, client3, ChromaPref::Yuv420, false), FormatSet::YUV420_8);

    // 5. ChromaPref::Yuv444 with client prefs default -> YUV444_8
    let client5 = Some(&ClientCodecs {
        decoders: vec![caps(VideoCodec::Hevc, BackendId::NVENC, FormatSet::YUV444_8 | FormatSet::YUV420_8)],
        ..Default::default()
    });
    assert_eq!(choose_format(VideoCodec::Hevc, &host_enc, client5, ChromaPref::Yuv444, false), FormatSet::YUV444_8);

    // 6. client wants 444 and 10 bit, common has 444_8 and 420_10 but not 444_10 -> YUV444_8
    let host_enc6 = vec![caps(VideoCodec::Hevc, BackendId::NVENC, FormatSet::YUV444_8 | FormatSet::YUV420_10)];
    let client6 = Some(&ClientCodecs {
        decoders: vec![caps(VideoCodec::Hevc, BackendId::NVENC, FormatSet::YUV444_8 | FormatSet::YUV420_10)],
        prefs: FormatPrefs { yuv444: true, ten_bit: true, ..Default::default() },
        ..Default::default()
    });
    assert_eq!(choose_format(VideoCodec::Hevc, &host_enc6, client6, ChromaPref::Auto, false), FormatSet::YUV444_8);

    // 7. common has only 420_10 and 420_8 -> YUV420_10
    let host_enc7 = vec![caps(VideoCodec::Hevc, BackendId::NVENC, FormatSet::YUV420_10 | FormatSet::YUV420_8)];
    let client7 = Some(&ClientCodecs {
        decoders: vec![caps(VideoCodec::Hevc, BackendId::NVENC, FormatSet::YUV420_10 | FormatSet::YUV420_8)],
        prefs: FormatPrefs { yuv444: true, ten_bit: true, ..Default::default() },
        ..Default::default()
    });
    assert_eq!(choose_format(VideoCodec::Hevc, &host_enc7, client7, ChromaPref::Auto, false), FormatSet::YUV420_10);

    // 8. hdr_source=true, client hdr_display=true, ten_bit=false, common has 420_10 -> YUV420_10
    let host_enc8 = vec![caps(VideoCodec::Hevc, BackendId::NVENC, FormatSet::YUV420_10)];
    let client8 = Some(&ClientCodecs {
        decoders: vec![caps(VideoCodec::Hevc, BackendId::NVENC, FormatSet::YUV420_10)],
        prefs: FormatPrefs { yuv444: false, ten_bit: false, hdr_display: true },
        ..Default::default()
    });
    assert_eq!(choose_format(VideoCodec::Hevc, &host_enc8, client8, ChromaPref::Auto, true), FormatSet::YUV420_10);

    // 9. hdr_source=true but hdr_display=false -> YUV420_8
    let client9 = Some(&ClientCodecs {
        decoders: vec![caps(VideoCodec::Hevc, BackendId::NVENC, FormatSet::YUV420_10 | FormatSet::YUV420_8)],
        prefs: FormatPrefs { yuv444: false, ten_bit: false, hdr_display: false },
        ..Default::default()
    });
    assert_eq!(choose_format(VideoCodec::Hevc, &host_enc8, client9, ChromaPref::Auto, true), FormatSet::YUV420_8);

    // 10. formats of another codec not used (host H264 444 but codec Hevc -> 420_8)
    let host_enc10 = vec![caps(VideoCodec::H264, BackendId::NVENC, FormatSet::YUV444_8)];
    let client10 = Some(&ClientCodecs {
        decoders: vec![caps(VideoCodec::Hevc, BackendId::NVENC, FormatSet::YUV420_8)],
        ..Default::default()
    });
    assert_eq!(choose_format(VideoCodec::Hevc, &host_enc10, client10, ChromaPref::Auto, false), FormatSet::YUV420_8);
}

#[test]
fn test_choose_transfer_cases() {
    assert_eq!(choose_transfer(FormatSet::YUV420_10, true), Transfer::Pq);
    assert_eq!(choose_transfer(FormatSet::YUV420_8, true), Transfer::Sdr);
    assert_eq!(choose_transfer(FormatSet::YUV420_10, false), Transfer::Sdr);
}

#[test]
fn test_backendid_properties() {
    assert!(BackendId::NVENC.known());
    assert!(!BackendId(21).known());
    assert!(BackendId::NVENC.label().contains("NVENC"));
    assert!(BackendId::SOFTWARE.is_software());
}

#[test]
fn test_deterministic_fuzz() {
    struct XorShift64 { state: u64 }
    impl XorShift64 {
        fn new(seed: u64) -> Self { Self { state: seed } }
        fn next(&mut self) -> u64 {
            let mut x = self.state;
            x ^= x << 13; x ^= x >> 7; x ^= x << 17;
            self.state = x; x
        }
        fn next_u8(&mut self) -> u8 { self.next() as u8 }
    }

    let mut rng = XorShift64::new(0x9E3779B97F4A7C15);
    let magics = [CLIENT_CODECS_MAGIC, HOST_CODEC_MAGIC];
    for _ in 0..50_000 {
        let magic = magics[rng.next_u8() as usize % 2];
        let mut msg = Vec::new();
        msg.extend_from_slice(magic);
        msg.push(VERSION);
        let count = rng.next_u8() as usize % 7;
        msg.push(count as u8);
        for _ in 0..count {
            let key = (rng.next_u8() as usize % 8) + 1;
            let len = rng.next_u8() as usize % 4;
            msg.push(key as u8);
            msg.push(len as u8);
            for _ in 0..len {
                msg.push(rng.next_u8());
            }
        }

        if magic == CLIENT_CODECS_MAGIC {
            let decoded = ClientCodecs::decode(&msg);
            if let Some(val) = decoded {
                let encoded = val.encode();
                assert_eq!(ClientCodecs::decode(&encoded), Some(val));
            }
        } else {
            let decoded = HostCodec::decode(&msg);
            if let Some(val) = decoded {
                let encoded = val.encode();
                assert_eq!(HostCodec::decode(&encoded), Some(val));
            }
        }
    }
}
