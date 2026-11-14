use super::*;
const H: VideoCodec = VideoCodec::H264;
const V: VideoCodec = VideoCodec::Hevc;
const A: VideoCodec = VideoCodec::Av1;

fn set(v: &[VideoCodec]) -> CodecSet { v.iter().copied().collect() }
fn cc(s: CodecSet, p: Option<VideoCodec>) -> ClientCodecs { ClientCodecs::simple(s, BackendId::MEDIA_FOUNDATION, p) }

#[test]
fn roundtrip_client_codecs() {
    // vide
    let c1 = cc(CodecSet::EMPTY, None);
    assert_eq!(ClientCodecs::decode(&c1.encode()), Some(c1.clone()));

    // [H MF]
    let c2 = cc(set(&[H]), None);
    assert_eq!(ClientCodecs::decode(&c2.encode()), Some(c2.clone()));

    // [V FFMPEG_D3D11VA, H MF, A MF]
    let mut c3 = ClientCodecs::default();
    c3.decoders = vec![
        CodecCap::new(V, BackendId::FFMPEG_D3D11VA),
        CodecCap::new(H, BackendId::MEDIA_FOUNDATION),
        CodecCap::new(A, BackendId::MEDIA_FOUNDATION),
    ];
    assert_eq!(ClientCodecs::decode(&c3.encode()), Some(c3.clone()));

    // capacité avec formats YUV420_8.union(YUV444_10)
    let mut c4 = ClientCodecs::default();
    c4.decoders = vec![CodecCap { codec: H, backend: BackendId::MEDIA_FOUNDATION, formats: FormatSet::YUV420_8.union(FormatSet::YUV444_10) }];
    assert_eq!(ClientCodecs::decode(&c4.encode()), Some(c4.clone()));

    // preferred None/Some(chaque codec)
    for pref in [None, Some(H), Some(V), Some(A)] {
        let c = cc(set(&[H, V]), pref);
        assert_eq!(ClientCodecs::decode(&c.encode()), Some(c.clone()));
    }

    // renegotiate true/false
    for reneg in [true, false] {
        let mut c = cc(set(&[H]), None);
        c.renegotiate = reneg;
        assert_eq!(ClientCodecs::decode(&c.encode()), Some(c.clone()));
    }

    // decoder_mode Auto/Hardware/Software
    for mode in [DecoderMode::Auto, DecoderMode::Hardware, DecoderMode::Software] {
        let mut c = cc(set(&[H]), None);
        c.decoder_mode = mode;
        assert_eq!(ClientCodecs::decode(&c.encode()), Some(c.clone()));
    }
}

#[test]
fn roundtrip_host_codec() {
    let enc = |s: CodecSet, b: BackendId| s.iter().map(|c| CodecCap::new(c, b)).collect::<Vec<_>>();

    for codec in VideoCodec::ALL {
        for reason in [ChoiceReason::Auto, ChoiceReason::ClientPreference, ChoiceReason::HostForced, ChoiceReason::NoClientInfo, ChoiceReason::Fallback] {
            for fmt in [FormatSet::YUV420_8, FormatSet::YUV444_8] {
                let encoders = enc(set(&[H, V, A]), BackendId::MF_NVIDIA);
                let h = HostCodec { codec, format: fmt, encoders: encoders.clone(), reason };
                assert_eq!(HostCodec::decode(&h.encode()), Some(h.clone()));
            }
        }
    }
    // encoders vide
    let h_empty = HostCodec { codec: H, format: FormatSet::YUV420_8, encoders: Vec::new(), reason: ChoiceReason::Auto };
    assert_eq!(HostCodec::decode(&h_empty.encode()), Some(h_empty.clone()));
}

#[test]
fn reject_invalid_messages() {
    // vide
    assert!(ClientCodecs::decode(&[]).is_none());
    assert!(HostCodec::decode(&[]).is_none());

    // préfixe strict d'un message valide
    let valid_cc = cc(CodecSet::EMPTY, None);
    let enc = valid_cc.encode();
    for i in 1..enc.len() {
        assert!(ClientCodecs::decode(&enc[..i]).is_none(), "prefix len {}", i);
    }

    // message valide + 1 octet
    let mut long = enc.clone();
    long.push(0);
    assert!(ClientCodecs::decode(&long).is_none());

    // mauvaise magie
    let mut bad_magic = enc.clone();
    bad_magic[0] = 0;
    assert!(ClientCodecs::decode(&bad_magic).is_none());

    // version 2
    let mut bad_ver = enc.clone();
    bad_ver[7] = 2;
    assert!(ClientCodecs::decode(&bad_ver).is_none());

    // count = 17
    let mut bad_count = enc.clone();
    bad_count[8] = 17;
    assert!(ClientCodecs::decode(&bad_count).is_none());

    // message > MAX_LEN
    let mut too_long = vec![0; MAX_LEN + 1];
    too_long[..7].copy_from_slice(CLIENT_CODECS_MAGIC);
    too_long[7] = VERSION;
    assert!(ClientCodecs::decode(&too_long).is_none());

    // clé K_CODEC (2) de longueur 2
    let mut k2_bad = Vec::new();
    k2_bad.extend_from_slice(CLIENT_CODECS_MAGIC);
    k2_bad.push(VERSION);
    k2_bad.push(1);
    k2_bad.push(2); k2_bad.push(2); k2_bad.push(0); k2_bad.push(0);
    assert!(ClientCodecs::decode(&k2_bad).is_none());

    // préférence 4
    let mut msg4 = Vec::new();
    msg4.extend_from_slice(CLIENT_CODECS_MAGIC);
    msg4.push(VERSION);
    msg4.push(1);
    msg4.push(2); msg4.push(1); msg4.push(4);
    assert!(ClientCodecs::decode(&msg4).is_none());

    // K_FLAGS valeur 2 côté client
    let mut msgf = Vec::new();
    msgf.extend_from_slice(CLIENT_CODECS_MAGIC);
    msgf.push(VERSION);
    msgf.push(1);
    msgf.push(3); msgf.push(1); msgf.push(2);
    assert!(ClientCodecs::decode(&msgf).is_none());

    // K_MODE 3 côté client
    let mut msgm = Vec::new();
    msgm.extend_from_slice(CLIENT_CODECS_MAGIC);
    msgm.push(VERSION);
    msgm.push(1);
    msgm.push(4); msgm.push(1); msgm.push(3);
    assert!(ClientCodecs::decode(&msgm).is_none());

    // liste de capacités de longueur 5
    let mut msgc = Vec::new();
    msgc.extend_from_slice(CLIENT_CODECS_MAGIC);
    msgc.push(VERSION);
    msgc.push(1);
    msgc.push(1); msgc.push(5); msgc.push(0); msgc.push(0); msgc.push(0); msgc.push(0); msgc.push(0);
    assert!(ClientCodecs::decode(&msgc).is_none());

    // plus de MAX_CAPS capacités (33*4 = 132 octets)
    let mut msgcap = Vec::new();
    msgcap.extend_from_slice(CLIENT_CODECS_MAGIC);
    msgcap.push(VERSION);
    msgcap.push(1);
    msgcap.push(1); msgcap.push(132);
    msgcap.extend(vec![0; 132]);
    assert!(ClientCodecs::decode(&msgcap).is_none());

    // HostCodec sans clé K_CODEC
    let mut hmsg = Vec::new();
    hmsg.extend_from_slice(HOST_CODEC_MAGIC);
    hmsg.push(VERSION);
    hmsg.push(1);
    hmsg.push(1); hmsg.push(0);
    assert!(HostCodec::decode(&hmsg).is_none());

    // HostCodec codec 0 ou 4
    let mut h0 = Vec::new();
    h0.extend_from_slice(HOST_CODEC_MAGIC); h0.push(VERSION); h0.push(1);
    h0.push(2); h0.push(1); h0.push(0);
    assert!(HostCodec::decode(&h0).is_none());
    let mut h4 = Vec::new();
    h4.extend_from_slice(HOST_CODEC_MAGIC); h4.push(VERSION); h4.push(1);
    h4.push(2); h4.push(1); h4.push(4);
    assert!(HostCodec::decode(&h4).is_none());

    // raison 5
    let mut hr5 = Vec::new();
    hr5.extend_from_slice(HOST_CODEC_MAGIC); hr5.push(VERSION); hr5.push(1);
    hr5.push(2); hr5.push(1); hr5.push(1);
    hr5.push(3); hr5.push(1); hr5.push(5);
    assert!(HostCodec::decode(&hr5).is_none());

    // format host 0, 3 (deux bits) ou 0x10
    let mut hf0 = Vec::new();
    hf0.extend_from_slice(HOST_CODEC_MAGIC); hf0.push(VERSION); hf0.push(1);
    hf0.push(2); hf0.push(1); hf0.push(1);
    hf0.push(4); hf0.push(1); hf0.push(0);
    assert!(HostCodec::decode(&hf0).is_none());
    let mut hf3 = Vec::new();
    hf3.extend_from_slice(HOST_CODEC_MAGIC); hf3.push(VERSION); hf3.push(1);
    hf3.push(2); hf3.push(1); hf3.push(1);
    hf3.push(4); hf3.push(1); hf3.push(3);
    assert!(HostCodec::decode(&hf3).is_none());
    let mut hf10 = Vec::new();
    hf10.extend_from_slice(HOST_CODEC_MAGIC); hf10.push(VERSION); hf10.push(1);
    hf10.push(2); hf10.push(1); hf10.push(1);
    hf10.push(4); hf10.push(1); hf10.push(0x10);
    assert!(HostCodec::decode(&hf10).is_none());

    // GRX-COD n'est pas décodé comme HostCodec et inversement
    assert!(HostCodec::decode(&valid_cc.encode()).is_none());
    let valid_h = HostCodec { codec: H, format: FormatSet::YUV420_8, encoders: vec![], reason: ChoiceReason::Auto };
    assert!(ClientCodecs::decode(&valid_h.encode()).is_none());

    // is_client_codecs/is_host_codec distinguent et refusent b"GRX-BYE"
    assert!(is_client_codecs(b"GRX-COD"));
    assert!(is_host_codec(b"GRX-CDC"));
    assert!(!is_client_codecs(b"GRX-BYE"));
    assert!(!is_host_codec(b"GRX-BYE"));
    assert!(!is_client_codecs(b"GRX-CDC"));
    assert!(!is_host_codec(b"GRX-COD"));
}

#[test]
fn extensibility() {
    let c = cc(CodecSet::EMPTY, None);
    let mut enc = c.encode();
    // add unknown key 77 len 3
    enc[8] += 1;
    enc.push(77); enc.push(3); enc.push(1); enc.push(2); enc.push(3);
    let decoded = ClientCodecs::decode(&enc).unwrap();
    assert_eq!(decoded, c);

    // unknown codec id 9 or backend 200 ignored
    let mut msg = Vec::new();
    msg.extend_from_slice(CLIENT_CODECS_MAGIC);
    msg.push(VERSION);
    msg.push(1);
    msg.push(1); msg.push(4); msg.push(9); msg.push(1); msg.push(1); msg.push(0);
    let dec = ClientCodecs::decode(&msg).unwrap();
    assert!(dec.decoders.is_empty());

    let mut msg2 = Vec::new();
    msg2.extend_from_slice(CLIENT_CODECS_MAGIC);
    msg2.push(VERSION);
    msg2.push(1);
    msg2.push(1); msg2.push(4); msg2.push(1); msg2.push(200); msg2.push(1); msg2.push(0);
    let dec2 = ClientCodecs::decode(&msg2).unwrap();
    assert!(dec2.decoders.is_empty());

    // unknown format bits 0xF1 masked to 0x01
    let mut msg3 = Vec::new();
    msg3.extend_from_slice(CLIENT_CODECS_MAGIC);
    msg3.push(VERSION);
    msg3.push(1);
    msg3.push(1); msg3.push(4); msg3.push(1); msg3.push(1); msg3.push(0xF1); msg3.push(0);
    let dec3 = ClientCodecs::decode(&msg3).unwrap();
    assert_eq!(dec3.decoders[0].formats, FormatSet::YUV420_8);

    // repeated key: last wins
    let mut msg4 = Vec::new();
    msg4.extend_from_slice(CLIENT_CODECS_MAGIC);
    msg4.push(VERSION);
    msg4.push(2);
    msg4.push(2); msg4.push(1); msg4.push(1);
    msg4.push(2); msg4.push(1); msg4.push(2);
    let dec4 = ClientCodecs::decode(&msg4).unwrap();
    assert_eq!(dec4.preferred, Some(V));
}

#[test]
fn fuzz_deterministic() {
    let mut seed = 123456789u64;
    let mut rng = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut accepted = 0;
    for _ in 0..200_000 {
        let buf = if rng() % 2 == 0 {
            let len = (rng() % 48) as usize;
            let mut b = Vec::with_capacity(len);
            for _ in 0..len { b.push((rng() & 0xFF) as u8); }
            b
        } else {
            let mut b = Vec::new();
            b.extend_from_slice(CLIENT_CODECS_MAGIC);
            b.push(VERSION);
            let count = (rng() % 5) as u8;
            b.push(count);
            for _ in 0..count {
                let key = (rng() % 6) as u8;
                let len = (rng() % 9) as u8;
                b.push(key);
                b.push(len);
                for _ in 0..len { b.push((rng() & 0xFF) as u8); }
            }
            b
        };
        if let Some(c) = ClientCodecs::decode(&buf) {
            accepted += 1;
            assert!(c.decoders.len() <= MAX_CAPS);
            for d in &c.decoders { assert!(d.backend.known()); }
            assert_eq!(ClientCodecs::decode(&c.encode()), Some(c));
        }
    }
    assert!(accepted >= 1000, "Expected at least 1000 accepted, got {}", accepted);
}

#[test]
fn choose_exact_cases() {
    let host_all = set(&[H, V, A]);

    assert_eq!(choose(CodecPref::Auto, host_all, None), (H, ChoiceReason::NoClientInfo));
    assert_eq!(choose(CodecPref::Force(V), host_all, None), (V, ChoiceReason::HostForced));
    assert_eq!(choose(CodecPref::Force(A), set(&[H, V]), None), (H, ChoiceReason::Fallback));
    assert_eq!(choose(CodecPref::Auto, host_all, Some(&cc(set(&[H, V, A]), None))), (V, ChoiceReason::Auto));
    assert_eq!(choose(CodecPref::Auto, host_all, Some(&cc(set(&[H]), None))), (H, ChoiceReason::Auto));
    assert_eq!(choose(CodecPref::Auto, host_all, Some(&cc(set(&[H, A]), None))), (H, ChoiceReason::Auto));
    assert_eq!(choose(CodecPref::Auto, host_all, Some(&cc(set(&[H, V, A]), Some(A)))), (A, ChoiceReason::ClientPreference));
    assert_eq!(choose(CodecPref::Auto, set(&[H, V]), Some(&cc(set(&[H, V, A]), Some(A)))), (V, ChoiceReason::Fallback));
    assert_eq!(choose(CodecPref::Auto, host_all, Some(&cc(set(&[H]), Some(V)))), (H, ChoiceReason::Fallback));
    assert_eq!(choose(CodecPref::Force(A), host_all, Some(&cc(set(&[H, V, A]), None))), (A, ChoiceReason::HostForced));
    assert_eq!(choose(CodecPref::Force(V), host_all, Some(&cc(set(&[H, A]), None))), (H, ChoiceReason::Fallback));
    assert_eq!(choose(CodecPref::Auto, host_all, Some(&cc(CodecSet::EMPTY, None))), (H, ChoiceReason::Fallback));
    assert_eq!(choose(CodecPref::Auto, host_all, Some(&cc(set(&[A]), None))), (A, ChoiceReason::Fallback));
    assert_eq!(choose(CodecPref::Force(V), host_all, Some(&cc(set(&[H, V, A]), Some(H)))), (H, ChoiceReason::ClientPreference));

    // HEVC YUV444_8 only -> not in decodable()
    let mut c_hevc_444 = ClientCodecs::default();
    c_hevc_444.decoders = vec![CodecCap { codec: V, backend: BackendId::MEDIA_FOUNDATION, formats: FormatSet::YUV444_8 }];
    assert!(!c_hevc_444.decodable().contains(V));
}

#[test]
fn exhaustive_choose_property() {
    for c_bits in 0..=7u8 {
        let client_set = CodecSet::from_bits(c_bits).unwrap_or(CodecSet::EMPTY);
        for h_bits in 0..=7u8 {
            let host_set = CodecSet::from_bits(h_bits).unwrap_or(CodecSet::EMPTY);
            if !host_set.contains(H) { continue; }

            for c_pref in [None, Some(H), Some(V), Some(A)] {
                for h_pref in [CodecPref::Auto, CodecPref::Force(H), CodecPref::Force(V), CodecPref::Force(A)] {
                    let client = Some(cc(client_set, c_pref));

                    let (chosen, _) = choose(h_pref, host_set, client.as_ref());
                    let intersection = host_set.intersect(client.as_ref().map(|c| c.decodable()).unwrap_or(CodecSet::EMPTY));

                    if chosen != H {
                        assert!(intersection.contains(chosen), "chosen {:?} not in intersection {:?} for c_bits {:03b} h_bits {:03b} c_pref {:?} h_pref {:?}", chosen, intersection, c_bits, h_bits, c_pref, h_pref);
                    }
                    if intersection.contains(H) {
                        assert!(intersection.contains(chosen), "chosen {:?} not in intersection {:?} when H in intersection for c_bits {:03b} h_bits {:03b} c_pref {:?} h_pref {:?}", chosen, intersection, c_bits, h_bits, c_pref, h_pref);
                    }
                }
            }
        }
    }
}

#[test]
fn codec_set_format_set_backend_for() {
    let s = CodecSet::EMPTY.with(H).with(V);
    assert!(s.contains(H));
    assert!(s.contains(V));
    assert!(!s.contains(A));
    assert!(!s.is_empty());
    let s2 = s.without(H);
    assert!(!s2.contains(H));
    assert!(s2.contains(V));
    let iter: Vec<_> = s2.iter().collect();
    assert_eq!(iter, vec![V]);
    assert_eq!(format!("{}", s2), "HEVC");
    assert_eq!(format!("{}", CodecSet::EMPTY), "aucun");

    let f1 = FormatSet::YUV420_8;
    let f2 = FormatSet::YUV444_8;
    assert!(f1.contains(FormatSet::YUV420_8));
    assert!(!f1.contains(FormatSet::YUV444_8));
    let f_union = f1.union(f2);
    assert!(f_union.contains(f1));
    assert!(f_union.contains(f2));
    assert_eq!(FormatSet::from_bits(0x0F), Some(FormatSet(0x0F)));
    assert_eq!(FormatSet::from_bits(0x10), None);

    let mut c = ClientCodecs::default();
    c.decoders = vec![CodecCap::new(H, BackendId::MEDIA_FOUNDATION), CodecCap::new(V, BackendId::FFMPEG_D3D11VA)];
    assert_eq!(c.backend_for(H), Some(BackendId::MEDIA_FOUNDATION));
    assert_eq!(c.backend_for(V), Some(BackendId::FFMPEG_D3D11VA));
    assert_eq!(c.backend_for(A), None);
}
