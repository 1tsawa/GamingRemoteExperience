use super::*;

struct Xs(u64);
impl Xs {
    fn next(&mut self, m: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        if m == 0 {
            0
        } else {
            self.0 % m
        }
    }
}

fn arrow(w: u16, h: u16, xor: bool) -> Shape {
    let n = w as usize * h as usize;
    let mut rgba = vec![0u8; n * 4];
    for i in 0..n {
        if i % 7 < 3 {
            rgba[i * 4..i * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
        } else if i % 11 == 0 {
            rgba[i * 4..i * 4 + 4].copy_from_slice(&[(i % 251) as u8, 3, 9, 128]);
        }
    }
    let xor = xor.then(|| {
        let mut x = vec![0u8; n * 4];
        x[..4].copy_from_slice(&[255, 255, 255, 255]);
        x
    });
    Shape { width: w, height: h, hot_x: 0, hot_y: h - 1, rgba, xor }
}

#[test]
fn rle_roundtrip_various() {
    let mut r = Xs(7);
    for case in 0..200 {
        let npix = r.next(600) as usize;
        let alphabet = 1 + r.next(if case % 2 == 0 { 3 } else { 200 });
        let data: Vec<u8> = (0..npix).flat_map(|_| {
            let c = r.next(alphabet) as u8;
            [c, c, c, c]
        }).collect();
        let mut enc = Vec::new();
        rle_encode(&data, &mut enc);
        let (dec, used) = rle_decode(&enc, npix).unwrap();
        assert_eq!(dec, data);
        assert_eq!(used, enc.len());
    }
    // Un curseur 32x32 typique (surtout transparent) se compresse bien.
    let s = arrow(32, 32, false);
    let body = s.encode_body().unwrap();
    assert!(body.len() < s.rgba.len() / 2, "{} octets", body.len());
}

#[test]
fn shape_roundtrip_with_and_without_xor() {
    for xor in [false, true] {
        let s = arrow(48, 40, xor);
        let body = s.encode_body().unwrap();
        assert_eq!(Shape::decode_body(&body), Some(s));
    }
    let max = arrow(256, 256, true);
    let body = max.encode_body().unwrap();
    assert!(body.len() <= MAX_BODY);
    assert_eq!(Shape::decode_body(&body), Some(max));
}

#[test]
fn invalid_shapes_are_refused() {
    let mut s = arrow(8, 8, false);
    s.hot_x = 8;
    assert!(s.encode_body().is_none());
    assert!(arrow(8, 8, false).encode_body().is_some());
    let big = Shape { width: 257, height: 1, hot_x: 0, hot_y: 0, rgba: vec![0; 257 * 4], xor: None };
    assert!(big.encode_body().is_none());
    let body = arrow(8, 8, false).encode_body().unwrap();
    // Octet en trop, tronqué, dimensions forgées, drapeau inconnu.
    let mut extra = body.clone();
    extra.push(0);
    assert!(Shape::decode_body(&extra).is_none());
    assert!(Shape::decode_body(&body[..body.len() - 1]).is_none());
    let mut forged = body.clone();
    forged[0..2].copy_from_slice(&1000u16.to_le_bytes());
    assert!(Shape::decode_body(&forged).is_none());
    let mut flag = body.clone();
    flag[8] = 2;
    assert!(Shape::decode_body(&flag).is_none());
    assert!(Shape::decode_body(&[]).is_none());
}

#[test]
fn chunks_reassemble_and_check_id() {
    let s = arrow(200, 200, true);
    let body = s.encode_body().unwrap();
    let id = shape_id(&body);
    let chunks = shape_chunks(id, &body);
    assert!(chunks.len() > 1);
    assert!(chunks.iter().all(|c| c.len() <= CHUNK_HDR + CHUNK_DATA));
    let mut a = ShapeAssembler::default();
    let mut got = None;
    for c in &chunks {
        if let Some(r) = a.push(c) {
            got = Some(r);
        }
    }
    assert_eq!(got, Some((id, s.clone())));

    // Id forgé (ne correspond pas au contenu) : refusé.
    let mut a = ShapeAssembler::default();
    let bad = shape_chunks(id ^ 1, &body);
    assert!(bad.iter().all(|c| a.push(c).is_none()));

    // Morceau manquant : abandon, puis la forme suivante passe.
    let mut a = ShapeAssembler::default();
    for (i, c) in chunks.iter().enumerate() {
        if i != 1 {
            assert!(a.push(c).is_none());
        }
    }
    let small = arrow(16, 16, false).encode_body().unwrap();
    let sid = shape_id(&small);
    let r = shape_chunks(sid, &small).iter().find_map(|c| a.push(c));
    assert_eq!(r.map(|(i, _)| i), Some(sid));
}

#[test]
fn pos_roundtrip_and_fixed_point() {
    let p = CursorPos { seq: 9, visible: true, shape_id: 0xdead_beef, x: -5, y: 70_000, src_w: 2560, src_h: 1440, ts_us: 123 };
    let e = p.encode();
    assert_eq!(e.len(), POS_LEN);
    assert_eq!(CursorPos::decode(&e), Some(p));
    assert!(CursorPos::decode(&e[..POS_LEN - 1]).is_none());
    let mut bad = e.clone();
    bad[0] = b'X';
    assert!(CursorPos::decode(&bad).is_none());
    assert_eq!(to_fixed(960, 1920), 32768);
    assert_eq!(to_fixed(-1920, 1920), -65536);
    assert_eq!(to_fixed(5, 0), 0);
    assert_eq!(to_fixed(i32::MAX, 1), i32::MAX);
    assert!(seq_newer(1, 0) && !seq_newer(0, 1) && seq_newer(0, u32::MAX) && !seq_newer(5, 5));
}

#[test]
fn fifo_cache_evicts_oldest() {
    let mut c = FifoCache::new(2);
    c.insert(1, "a");
    c.insert(2, "b");
    c.insert(1, "z"); // déjà présent : ignoré
    assert_eq!(c.get(1), Some(&"a"));
    c.insert(3, "c");
    assert!(!c.contains(1) && c.contains(2) && c.contains(3));
    assert_eq!(c.len(), 2);
}

#[test]
fn fuzz_decoders_never_panic() {
    let mut r = Xs(0x1234_5678_9abc_def1);
    let good = arrow(24, 24, true).encode_body().unwrap();
    let mut asm = ShapeAssembler::default();
    for i in 0..20_000 {
        let len = r.next(if i % 3 == 0 { 64 } else { 2000 }) as usize;
        let mut m: Vec<u8> = (0..len).map(|_| r.next(256) as u8).collect();
        // Un tiers des cas : en-têtes plausibles, contenu muté.
        match i % 4 {
            0 if m.len() >= 4 => m[..4].copy_from_slice(SHAPE_MAGIC),
            1 if m.len() >= 4 => m[..4].copy_from_slice(POS_MAGIC),
            2 => {
                m = good.clone();
                for _ in 0..1 + r.next(4) {
                    let k = r.next(m.len() as u64) as usize;
                    m[k] = r.next(256) as u8;
                }
            }
            _ => {}
        }
        let _ = Shape::decode_body(&m);
        let _ = CursorPos::decode(&m);
        if let Some((_, s)) = asm.push(&m) {
            assert!(s.is_valid());
        }
        let _ = rle_decode(&m, r.next(70_000) as usize);
        if let Some(s) = Shape::decode_body(&m) {
            assert!(s.is_valid());
        }
    }
    // Morceau annonçant un total énorme : refusé sans allocation.
    let mut huge = Vec::from(&SHAPE_MAGIC[..]);
    huge.extend_from_slice(&1u32.to_le_bytes());
    huge.extend_from_slice(&u32::MAX.to_le_bytes());
    huge.extend_from_slice(&0u32.to_le_bytes());
    huge.push(0);
    assert!(asm.push(&huge).is_none());
}
