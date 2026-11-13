use super::*;

fn px(img: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
    let o = ((y * w + x) * 4) as usize;
    img[o..o + 4].try_into().unwrap()
}

#[test]
fn monochrome_four_cases() {
    // 8x2 visibles : hauteur DXGI = 4 (AND puis XOR), pitch 1 octet. MSB = x=0.
    // Ligne 0 : x0 AND0 XOR0 (noir), x1 AND0 XOR1 (blanc), x2 AND1 XOR0 (transparent), x3 AND1 XOR1 (inversion).
    let and = [0b0011_1111u8, 0xFF];
    let xor = [0b0101_0000u8, 0x00];
    let buf = [and[0], and[1], xor[0], xor[1]];
    let img = convert_pointer_shape(SHAPE_MONOCHROME, 8, 4, 1, 1, 1, &buf).unwrap();
    assert_eq!((img.width, img.height), (8, 2));
    assert_eq!(px(&img.rgba, 8, 0, 0), [0, 0, 0, 255]);
    assert_eq!(px(&img.rgba, 8, 1, 0), [255, 255, 255, 255]);
    assert_eq!(px(&img.rgba, 8, 2, 0), [0, 0, 0, 0]);
    assert_eq!(px(&img.rgba, 8, 3, 0), [0, 0, 0, 0]);
    let x = img.xor.as_ref().expect("calque d'inversion");
    assert_eq!(px(x, 8, 3, 0), [255, 255, 255, 255]);
    assert_eq!(px(x, 8, 1, 0), [0, 0, 0, 0]);
    // Ligne 1 entièrement transparente.
    assert!((0..8).all(|i| px(&img.rgba, 8, i, 1) == [0, 0, 0, 0]));
}

#[test]
fn monochrome_without_inversion_has_no_xor_layer() {
    let buf = [0x00u8, 0xFF, 0xF0, 0x00]; // ligne 0 : noir/blanc ; ligne 1 : transparente
    let img = convert_pointer_shape(SHAPE_MONOCHROME, 8, 4, 1, 0, 0, &buf).unwrap();
    assert!(img.xor.is_none());
    assert_eq!(px(&img.rgba, 8, 0, 0), [255, 255, 255, 255]);
    assert_eq!(px(&img.rgba, 8, 7, 0), [0, 0, 0, 255]);
}

#[test]
fn color_bgra_to_rgba_with_padding() {
    // 2x2, pitch 12 (4 octets de bourrage par ligne).
    let mut buf = vec![0xEEu8; 24];
    buf[0..4].copy_from_slice(&[1, 2, 3, 4]);
    buf[4..8].copy_from_slice(&[5, 6, 7, 8]);
    buf[12..16].copy_from_slice(&[9, 10, 11, 0]);
    buf[16..20].copy_from_slice(&[13, 14, 15, 255]);
    let img = convert_pointer_shape(SHAPE_COLOR, 2, 2, 12, 0, 0, &buf).unwrap();
    assert_eq!(img.rgba, vec![3, 2, 1, 4, 7, 6, 5, 8, 11, 10, 9, 0, 15, 14, 13, 255]);
    assert!(img.xor.is_none());
}

#[test]
fn masked_color_cases() {
    let buf = [
        0x10, 0x20, 0x30, 0x00, // masque 0 : couleur opaque
        0x00, 0x00, 0x00, 0xFF, // masque FF, noir : transparent
        0x10, 0x20, 0x30, 0xFF, // masque FF, couleur : XOR
    ];
    let img = convert_pointer_shape(SHAPE_MASKED_COLOR, 3, 1, 12, 0, 0, &buf).unwrap();
    assert_eq!(px(&img.rgba, 3, 0, 0), [0x30, 0x20, 0x10, 255]);
    assert_eq!(px(&img.rgba, 3, 1, 0), [0, 0, 0, 0]);
    assert_eq!(px(&img.rgba, 3, 2, 0), [0, 0, 0, 0]);
    let x = img.xor.unwrap();
    assert_eq!(px(&x, 3, 1, 0), [0, 0, 0, 0]);
    assert_eq!(px(&x, 3, 2, 0), [0x30, 0x20, 0x10, 255]);
    // Sans pixel XOR : pas de calque.
    let img = convert_pointer_shape(SHAPE_MASKED_COLOR, 3, 1, 12, 0, 0, &buf[..8].iter().chain(&[0, 0, 0, 0xFF]).copied().collect::<Vec<_>>()).unwrap();
    assert!(img.xor.is_none());
}

#[test]
fn invalid_inputs_return_none() {
    assert!(convert_pointer_shape(SHAPE_COLOR, 2, 2, 8, 0, 0, &[0; 15]).is_none());
    assert!(convert_pointer_shape(SHAPE_COLOR, 2, 2, 4, 0, 0, &[0; 64]).is_none()); // pitch trop petit
    assert!(convert_pointer_shape(3, 2, 2, 8, 0, 0, &[0; 64]).is_none());
    assert!(convert_pointer_shape(SHAPE_COLOR, 0, 2, 8, 0, 0, &[0; 64]).is_none());
    assert!(convert_pointer_shape(SHAPE_MONOCHROME, 8, 1, 1, 0, 0, &[0; 64]).is_none());
    assert!(convert_pointer_shape(SHAPE_MONOCHROME, 8, 4, 1, 0, 0, &[0; 3]).is_none());
    assert!(convert_pointer_shape(SHAPE_MONOCHROME, 16, 4, 1, 0, 0, &[0; 64]).is_none());
}

#[test]
fn hotspot_is_clamped() {
    let img = convert_pointer_shape(SHAPE_COLOR, 2, 2, 8, 99, 99, &[0; 16]).unwrap();
    assert_eq!((img.hot_x, img.hot_y), (1, 1));
}

#[test]
fn downscale_keeps_ratio_and_hotspot() {
    let img = CursorImage {
        width: 512,
        height: 256,
        hot_x: 100,
        hot_y: 50,
        rgba: vec![7; 512 * 256 * 4],
        xor: Some(vec![9; 512 * 256 * 4]),
    };
    let d = downscale_to_fit(img, 256);
    assert_eq!((d.width, d.height, d.hot_x, d.hot_y), (256, 128, 50, 25));
    assert_eq!(d.rgba.len(), 256 * 128 * 4);
    assert_eq!(d.xor.as_ref().unwrap().len(), 256 * 128 * 4);
    let small = CursorImage { width: 4, height: 4, hot_x: 1, hot_y: 1, rgba: vec![1; 64], xor: None };
    assert_eq!(downscale_to_fit(small.clone(), 256), small);
    let big = convert_pointer_shape(SHAPE_COLOR, 300, 300, 1200, 0, 0, &vec![0u8; 1200 * 300]).unwrap();
    assert_eq!((big.width, big.height), (256, 256));
}

#[test]
fn tracker_serials() {
    let t = CursorTracker::new();
    assert_eq!(t.serial(), 0);
    t.update(Some((true, 10, 20)), None, 1920, 1080);
    let s1 = t.serial();
    assert!(s1 > 0);
    t.update(Some((true, 10, 20)), None, 1920, 1080);
    assert_eq!(t.serial(), s1, "même état : pas de changement");
    let img = CursorImage { width: 2, height: 2, hot_x: 1, hot_y: 1, rgba: vec![255; 16], xor: None };
    t.update(None, Some(img.clone()), 1920, 1080);
    let s = t.snapshot();
    assert_eq!(s.shape_serial, 1);
    assert_eq!(s.hotspot_pos(), (11, 21));
    t.update(None, Some(img), 1920, 1080);
    assert_eq!(t.snapshot().shape_serial, 1, "forme identique : pas renvoyée");
    t.update(Some((false, 10, 20)), None, 1920, 1080);
    assert!(!t.snapshot().visible);
}

#[test]
fn fuzz_never_panics() {
    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    let mut rnd = |m: u64| {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        if m == 0 { 0 } else { s % m }
    };
    for _ in 0..2000 {
        let ty = rnd(6) as u32;
        let (w, h, p) = (rnd(70) as u32, rnd(70) as u32, rnd(300) as u32);
        let (hx, hy) = (rnd(100) as u32, rnd(100) as u32);
        let len = rnd(20_000) as usize;
        let buf: Vec<u8> = (0..len).map(|_| rnd(256) as u8).collect();
        if let Some(img) = convert_pointer_shape(ty, w, h, p, hx, hy, &buf) {
            assert_eq!(img.rgba.len(), (img.width * img.height * 4) as usize);
            assert!(img.width <= MAX_CURSOR_DIM && img.height <= MAX_CURSOR_DIM);
            assert!(img.hot_x < img.width && img.hot_y < img.height);
            if let Some(x) = &img.xor {
                assert_eq!(x.len(), img.rgba.len());
            }
        }
    }
}
