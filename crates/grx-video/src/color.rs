//! Module de conversion de couleurs et de formats de plans vidéo.
//! Fournit des références CPU pour les conversions RGB<->YCbCr, PQ/SDR,
//! les transferts de gamut (BT.709 <-> BT.2020), et les réarrangements
//! de plans vidéo (NV12, I420, AYUV, Y410, etc.).
//!
//! Toutes les fonctions respectent les gammes étendues (full-range) par défaut
//! et utilisent des arrondis standards pour les conversions entières.


/// Matrice de coefficients de luminance pour les conversions YCbCr.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Matrix {
    /// BT.709 (HD).
    Bt709,
    /// BT.2020 (UHD).
    Bt2020,
}

impl Matrix {
    /// Retourne les coefficients Kr et Kb pour la matrice sélectionnée.
    pub fn kr_kb(self) -> (f32, f32) {
        match self {
            Matrix::Bt709 => (0.2126, 0.0722),
            Matrix::Bt2020 => (0.2627, 0.0593),
        }
    }
}

/// Convertit une image RGB 8 bits étendue en YCbCr 8 bits limité.
///
/// Les valeurs d'entrée sont normalisées sur [0, 255].
/// La sortie respecte l'étendue limitée : Y ∈ [16, 235], Cb/Cr ∈ [16, 240].
pub fn rgb_to_ycbcr8(rgb: [u8; 3], m: Matrix) -> [u8; 3] {
    let (kr, kb) = m.kr_kb();
    let kg = 1.0 - kr - kb;
    let r = rgb[0] as f32 / 255.0;
    let g = rgb[1] as f32 / 255.0;
    let b = rgb[2] as f32 / 255.0;

    let y = kr * r + kg * g + kb * b;
    let cb = (b - y) / (2.0 * (1.0 - kb));
    let cr = (r - y) / (2.0 * (1.0 - kr));

    let y8 = (16.0 + 219.0 * y).round().clamp(0.0, 255.0) as u8;
    let cb8 = (128.0 + 224.0 * cb).round().clamp(0.0, 255.0) as u8;
    let cr8 = (128.0 + 224.0 * cr).round().clamp(0.0, 255.0) as u8;

    [y8, cb8, cr8]
}

/// Convertit un YCbCr 8 bits limité en RGB 8 bits étendu.
///
/// Inverse exact des formales flottantes de [`rgb_to_ycbcr8`].
pub fn ycbcr8_to_rgb(ycc: [u8; 3], m: Matrix) -> [u8; 3] {
    let (kr, kb) = m.kr_kb();
    let y = (ycc[0] as f32 - 16.0) / 219.0;
    let cb = (ycc[1] as f32 - 128.0) / 224.0;
    let cr = (ycc[2] as f32 - 128.0) / 224.0;

    let r = y + 2.0 * (1.0 - kr) * cr;
    let b = y + 2.0 * (1.0 - kb) * cb;
    let kg = 1.0 - kr - kb;
    let g = (y - kr * r - kb * b) / kg;

    let r8 = (r * 255.0).round().clamp(0.0, 255.0) as u8;
    let g8 = (g * 255.0).round().clamp(0.0, 255.0) as u8;
    let b8 = (b * 255.0).round().clamp(0.0, 255.0) as u8;

    [r8, g8, b8]
}

/// Convertit une image RGB flottante (0..1) en YCbCr 10 bits.
///
/// Les valeurs d'entrée sont d'abord clampées sur [0, 1].
/// La sortie utilise l'étendue 10 bits : Y ∈ [64, 940], Cb/Cr ∈ [64, 960].
pub fn rgb_to_ycbcr10(rgb: [f32; 3], m: Matrix) -> [u16; 3] {
    let rgb = [
        rgb[0].clamp(0.0, 1.0),
        rgb[1].clamp(0.0, 1.0),
        rgb[2].clamp(0.0, 1.0),
    ];
    let (kr, kb) = m.kr_kb();
    let kg = 1.0 - kr - kb;

    let y = kr * rgb[0] + kg * rgb[1] + kb * rgb[2];
    let cb = (rgb[2] - y) / (2.0 * (1.0 - kb));
    let cr = (rgb[0] - y) / (2.0 * (1.0 - kr));

    let y10 = (64.0 + 876.0 * y).round().clamp(0.0, 1023.0) as u16;
    let cb10 = (512.0 + 896.0 * cb).round().clamp(0.0, 1023.0) as u16;
    let cr10 = (512.0 + 896.0 * cr).round().clamp(0.0, 1023.0) as u16;

    [y10, cb10, cr10]
}

/// Encode SMPTE ST 2084 (PQ) à partir d'une luminance en nits.
///
/// Retourne une valeur normalisée [0, 1] correspondant au code de trame PQ.
pub fn pq_from_nits(nits: f32) -> f32 {
    let m1 = 2610.0 / 16384.0;
    let m2 = 2523.0 / 4096.0 * 128.0;
    let c1 = 3424.0 / 4096.0;
    let c2 = 2413.0 / 4096.0 * 32.0;
    let c3 = 2392.0 / 4096.0 * 32.0;

    let y = (nits / 10000.0).clamp(0.0, 1.0);
    let y_pow_m1 = y.powf(m1);
    let numerator = c1 + c2 * y_pow_m1;
    let denominator = 1.0 + c3 * y_pow_m1;

    (numerator / denominator).powf(m2)
}

/// Décodage SMPTE ST 2084 (PQ) vers une luminance en nits.
///
/// Inverse exact de [`pq_from_nits`]. Retourne une valeur dans [0, 10000].
pub fn nits_from_pq(e: f32) -> f32 {
    let m1 = 2610.0 / 16384.0;
    let c1 = 3424.0 / 4096.0;
    let c2 = 2413.0 / 4096.0 * 32.0;
    let c3 = 2392.0 / 4096.0 * 32.0;

    let m2 = 2523.0 / 4096.0 * 128.0;
    let ep = e.clamp(0.0, 1.0).powf(1.0 / m2);
    let y_pow_m1 = (ep - c1).max(0.0) / (c2 - c3 * ep);
    let y = y_pow_m1.powf(1.0 / m1);

    (y * 10000.0).clamp(0.0, 10000.0)
}

/// Convertit des composantes BT.709 vers BT.2020.
pub fn bt709_to_bt2020(c: [f32; 3]) -> [f32; 3] {
    [
        0.627404 * c[0] + 0.329283 * c[1] + 0.043313 * c[2],
        0.069097 * c[0] + 0.919540 * c[1] + 0.011362 * c[2],
        0.016391 * c[0] + 0.088013 * c[1] + 0.895595 * c[2],
    ]
}

/// Convertit des composantes BT.2020 vers BT.709.
pub fn bt2020_to_bt709(c: [f32; 3]) -> [f32; 3] {
    [
        1.660491 * c[0] - 0.587641 * c[1] - 0.072850 * c[2],
        -0.124550 * c[0] + 1.132900 * c[1] - 0.008349 * c[2],
        -0.018151 * c[0] - 0.100579 * c[1] + 1.118730 * c[2],
    ]
}

/// Convertit des composantes scRGB (linéaires BT.709, 1.0 = 80 nits) vers PQ BT.2020.
///
/// Les valeurs négatives sont clampées à 0. Le résultat est dans l'espace PQ.
pub fn scrgb_to_pq2020(c: [f32; 3]) -> [f32; 3] {
    let linear_80 = [
        c[0].max(0.0) * 80.0,
        c[1].max(0.0) * 80.0,
        c[2].max(0.0) * 80.0,
    ];
    let bt2020 = bt709_to_bt2020(linear_80);
    [
        pq_from_nits(bt2020[0]),
        pq_from_nits(bt2020[1]),
        pq_from_nits(bt2020[2]),
    ]
}

/// Encode une valeur linéaire [0, 1] vers le gamma sRGB.
pub fn srgb_encode(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    if x <= 0.0031308 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// Décodage du gamma sRGB vers une valeur linéaire [0, 1].
pub fn srgb_decode(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    if x <= 0.04045 {
        x / 12.92
    } else {
        ((x + 0.055) / 1.055).powf(2.4)
    }
}

/// Niveau de blanc SDR de référence en nits.
pub const SDR_WHITE_NITS: f32 = 203.0;

/// Courbe de tone-mapping SDR : identité jusqu'au genou `TONEMAP_KNEE` (en multiples du blanc
/// SDR de 203 nits), puis compression exponentielle continue (dérivée 1 au genou) vers 1.
pub const TONEMAP_KNEE: f32 = 0.75;
pub fn tonemap_curve(x: f32) -> f32 {
    let x = x.max(0.0);
    if x <= TONEMAP_KNEE {
        x
    } else {
        let r = 1.0 - TONEMAP_KNEE;
        TONEMAP_KNEE + r * (1.0 - (-(x - TONEMAP_KNEE) / r).exp())
    }
}

/// Tone-mapping PQ BT.2020 -> sRGB 8 bits (référence CPU du shader du client et du host) :
/// nits -> BT.709 linéaire -> [`tonemap_curve`] par canal -> sRGB. `peak_nits` est réservé.
pub fn tonemap_pq2020_to_srgb8(pq: [f32; 3], peak_nits: f32) -> [u8; 3] {
    let nits = [
        nits_from_pq(pq[0]),
        nits_from_pq(pq[1]),
        nits_from_pq(pq[2]),
    ];
    let bt709 = bt2020_to_bt709(nits);
    let bt709_clamped = [
        bt709[0].max(0.0),
        bt709[1].max(0.0),
        bt709[2].max(0.0),
    ];

    let _ = peak_nits;
    let tonemapped = bt709_clamped.map(|n| srgb_encode(tonemap_curve(n / SDR_WHITE_NITS)));

    [
        (tonemapped[0] * 255.0).round().clamp(0.0, 255.0) as u8,
        (tonemapped[1] * 255.0).round().clamp(0.0, 255.0) as u8,
        (tonemapped[2] * 255.0).round().clamp(0.0, 255.0) as u8,
    ]
}

/// Convertit un plan NV12 vers un plan I420 (YUV 4:2:0 planar).
///
/// Les strides sont exprimés en éléments. Les dimensions peuvent être impaires.
pub fn nv12_to_i420(
    y: &[u8], y_stride: usize, uv: &[u8], uv_stride: usize, w: usize, h: usize,
    out_y: &mut [u8], out_u: &mut [u8], out_v: &mut [u8],
) {
    debug_assert!(out_y.len() >= h * w);
    debug_assert!(out_u.len() >= (h + 1) / 2 * (w + 1) / 2);
    debug_assert!(out_v.len() >= (h + 1) / 2 * (w + 1) / 2);

    let chroma_w = (w + 1) / 2;
    let chroma_h = (h + 1) / 2;

    for row in 0..h {
        let src_y_row = &y[row * y_stride..row * y_stride + w];
        let dst_y_row = &mut out_y[row * w..row * w + w];
        dst_y_row.copy_from_slice(src_y_row);
    }

    for row in 0..chroma_h {
        let src_uv_row = &uv[row * uv_stride..row * uv_stride + 2 * chroma_w];
        let dst_u_row = &mut out_u[row * chroma_w..row * chroma_w + chroma_w];
        let dst_v_row = &mut out_v[row * chroma_w..row * chroma_w + chroma_w];
        for col in 0..chroma_w {
            let idx = col * 2;
            dst_u_row[col] = src_uv_row[idx];
            dst_v_row[col] = src_uv_row[idx + 1];
        }
    }
}

/// Convertit un plan I420 vers un plan NV12.
///
/// `out` contient d'abord les lignes Y, puis les lignes UV entrelacées.
pub fn i420_to_nv12(
    y: &[u8], y_stride: usize, u: &[u8], v: &[u8], c_stride: usize, w: usize, h: usize,
    out: &mut [u8], pitch: usize,
) {
    debug_assert!(out.len() >= h * pitch + (h + 1) / 2 * pitch);

    let chroma_w = (w + 1) / 2;
    let chroma_h = (h + 1) / 2;

    for row in 0..h {
        let src_y_row = &y[row * y_stride..row * y_stride + w];
        let dst_y_row = &mut out[row * pitch..row * pitch + pitch];
        dst_y_row[..w].copy_from_slice(src_y_row);
    }

    let uv_offset = h * pitch;
    for row in 0..chroma_h {
        let src_u_row = &u[row * c_stride..row * c_stride + chroma_w];
        let src_v_row = &v[row * c_stride..row * c_stride + chroma_w];
        let dst_uv_row = &mut out[uv_offset + row * pitch..uv_offset + row * pitch + pitch];
        for col in 0..chroma_w {
            let idx = col * 2;
            dst_uv_row[idx] = src_u_row[col];
            dst_uv_row[idx + 1] = src_v_row[col];
        }
    }
}

/// Convertit un plan I420 10 bits vers un plan P010 (NV12 10 bits).
///
/// Les valeurs d'entrée sont masquées sur 10 bits puis décalées vers le haut de 6.
pub fn i420p10_to_p010(
    y: &[u16], y_stride: usize, u: &[u16], v: &[u16], c_stride: usize, w: usize, h: usize,
    out: &mut [u16], pitch_elems: usize,
) {
    debug_assert!(out.len() >= h * pitch_elems + (h + 1) / 2 * pitch_elems);

    let chroma_w = (w + 1) / 2;
    let chroma_h = (h + 1) / 2;

    for row in 0..h {
        let src_y_row = &y[row * y_stride..row * y_stride + w];
        let dst_y_row = &mut out[row * pitch_elems..row * pitch_elems + pitch_elems];
        for (i, &val) in src_y_row.iter().enumerate() {
            dst_y_row[i] = (val & 0x3FF) << 6;
        }
    }

    let uv_offset = h * pitch_elems;
    for row in 0..chroma_h {
        let src_u_row = &u[row * c_stride..row * c_stride + chroma_w];
        let src_v_row = &v[row * c_stride..row * c_stride + chroma_w];
        let dst_uv_row = &mut out[uv_offset + row * pitch_elems..uv_offset + row * pitch_elems + pitch_elems];
        for col in 0..chroma_w {
            let idx = col * 2;
            dst_uv_row[idx] = (src_u_row[col] & 0x3FF) << 6;
            dst_uv_row[idx + 1] = (src_v_row[col] & 0x3FF) << 6;
        }
    }
}

/// Convertit un plan YUV 4:4:4 planar vers DXGI_FORMAT_AYUV.
///
/// L'ordre des octets par pixel est [V, U, Y, A=255].
pub fn yuv444p_to_ayuv(
    y: &[u8], u: &[u8], v: &[u8], stride: usize, w: usize, h: usize,
    out: &mut [u8], pitch_bytes: usize,
) {
    debug_assert!(out.len() >= h * pitch_bytes);

    for row in 0..h {
        let src_y_row = &y[row * stride..row * stride + w];
        let src_u_row = &u[row * stride..row * stride + w];
        let src_v_row = &v[row * stride..row * stride + w];
        let dst_row = &mut out[row * pitch_bytes..row * pitch_bytes + w * 4];
        for col in 0..w {
            let idx = col * 4;
            dst_row[idx] = src_v_row[col];
            dst_row[idx + 1] = src_u_row[col];
            dst_row[idx + 2] = src_y_row[col];
            dst_row[idx + 3] = 255;
        }
    }
}

/// Convertit un plan YUV 4:4:4 10 bits planar vers DXGI_FORMAT_Y410.
///
/// Packing 32 bits : U[0-9], Y[10-19], V[20-29], Alpha[30-31]=3.
pub fn yuv444p10_to_y410(
    y: &[u16], u: &[u16], v: &[u16], stride: usize, w: usize, h: usize,
    out: &mut [u32], pitch_elems: usize,
) {
    debug_assert!(out.len() >= h * pitch_elems);

    for row in 0..h {
        let src_y_row = &y[row * stride..row * stride + w];
        let src_u_row = &u[row * stride..row * stride + w];
        let src_v_row = &v[row * stride..row * stride + w];
        let dst_row = &mut out[row * pitch_elems..row * pitch_elems + w];
        for col in 0..w {
            let y_val = (src_y_row[col] & 0x3FF) as u32;
            let u_val = (src_u_row[col] & 0x3FF) as u32;
            let v_val = (src_v_row[col] & 0x3FF) as u32;
            dst_row[col] = (3 << 30) | (v_val << 20) | (y_val << 10) | u_val;
        }
    }
}

/// Convertit un plan BGRA vers un plan YUV 4:4:4 planar.
///
/// L'ordre d'entrée est B, G, R, A. La sortie est Y, puis U, puis V.
pub fn bgra_to_yuv444_planar(
    bgra: &[u8], stride_bytes: usize, w: usize, h: usize, m: Matrix, out: &mut [u8],
) {
    debug_assert!(out.len() >= w * h * 3);

    let (y_plane, rest) = out.split_at_mut(w * h);
    let (u_plane, rest) = rest.split_at_mut(w * h);
    let v_plane = &mut rest[..w * h];

    for row in 0..h {
        let src_row = &bgra[row * stride_bytes..row * stride_bytes + w * 4];
        let dst_y_row = &mut y_plane[row * w..row * w + w];
        let dst_u_row = &mut u_plane[row * w..row * w + w];
        let dst_v_row = &mut v_plane[row * w..row * w + w];
        for col in 0..w {
            let idx = col * 4;
            let b = src_row[idx];
            let g = src_row[idx + 1];
            let r = src_row[idx + 2];
            let yuv = rgb_to_ycbcr8([r, g, b], m);
            dst_y_row[col] = yuv[0];
            dst_u_row[col] = yuv[1];
            dst_v_row[col] = yuv[2];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rgb_to_ycbcr8_white_bt709() {
        let res = rgb_to_ycbcr8([255, 255, 255], Matrix::Bt709);
        assert_eq!(res, [235, 128, 128]);
    }

    #[test]
    fn test_rgb_to_ycbcr8_black_bt709() {
        let res = rgb_to_ycbcr8([0, 0, 0], Matrix::Bt709);
        assert_eq!(res, [16, 128, 128]);
    }

    #[test]
    fn test_rgb_to_ycbcr8_white_bt2020() {
        let res = rgb_to_ycbcr8([255, 255, 255], Matrix::Bt2020);
        assert_eq!(res, [235, 128, 128]);
    }

    #[test]
    fn test_rgb_to_ycbcr8_black_bt2020() {
        let res = rgb_to_ycbcr8([0, 0, 0], Matrix::Bt2020);
        assert_eq!(res, [16, 128, 128]);
    }

    #[test]
    fn test_rgb_to_ycbcr8_pure_red_bt709() {
        let res = rgb_to_ycbcr8([255, 0, 0], Matrix::Bt709);
        assert!((res[0] as i32 - 63).abs() <= 1);
        assert!((res[1] as i32 - 102).abs() <= 1);
        assert!((res[2] as i32 - 240).abs() <= 1);
    }

    #[test]
    fn test_rgb_ycbcr8_roundtrip() {
        let mut max_err = 0.0;
        for r in (0..=255).step_by(17) {
            for g in (0..=255).step_by(17) {
                for b in (0..=255).step_by(17) {
                    let rgb = [r, g, b];
                    let ycc = rgb_to_ycbcr8(rgb, Matrix::Bt709);
                    let rgb_back = ycbcr8_to_rgb(ycc, Matrix::Bt709);
                    for i in 0..3 {
                        let err = (rgb[i] as i32 - rgb_back[i] as i32).abs() as f32;
                        if err > max_err { max_err = err; }
                    }
                }
            }
        }
        assert!(max_err <= 2.0, "Max roundtrip error: {}", max_err);
    }

    #[test]
    fn test_rgb_to_ycbcr10_white() {
        let res = rgb_to_ycbcr10([1.0, 1.0, 1.0], Matrix::Bt709);
        assert_eq!(res, [940, 512, 512]);
    }

    #[test]
    fn test_rgb_to_ycbcr10_black() {
        let res = rgb_to_ycbcr10([0.0, 0.0, 0.0], Matrix::Bt709);
        assert_eq!(res, [64, 512, 512]);
    }

    #[test]
    fn test_pq_from_nits_10000() {
        let res = pq_from_nits(10000.0);
        assert!((res - 1.0).abs() < 1e-4);
    }

    #[test]
    fn test_pq_from_nits_0() {
        // PQ(0) = c1^m2 ~ 7e-7 (pas exactement 0).
        let res = pq_from_nits(0.0);
        assert!(res < 1e-5, "{res}");
    }

    #[test]
    fn test_pq_from_nits_100() {
        let res = pq_from_nits(100.0);
        assert!((res - 0.5081).abs() < 0.002);
    }

    #[test]
    fn test_pq_from_nits_203() {
        let res = pq_from_nits(203.0);
        assert!((res - 0.5806).abs() < 0.002);
    }

    #[test]
    fn test_nits_from_pq_inverse() {
        for nits in [1.0, 10.0, 100.0, 1000.0, 4000.0] {
            let pq = pq_from_nits(nits);
            let recovered = nits_from_pq(pq);
            let rel_err = (recovered - nits).abs() / nits;
            assert!(rel_err < 1e-3, "Rel error at {} nits: {}", nits, rel_err);
        }
    }

    #[test]
    fn test_bt709_bt2020_identity() {
        let test_vals = [[0.1, 0.2, 0.3], [0.5, 0.5, 0.5], [1.0, 0.0, 0.0]];
        for c in test_vals {
            let bt2020 = bt709_to_bt2020(c);
            let recovered = bt2020_to_bt709(bt2020);
            for i in 0..3 {
                assert!((recovered[i] - c[i]).abs() < 1e-3, "Ch {} err: {}", i, recovered[i] - c[i]);
            }
        }
    }

    #[test]
    fn test_scrgb_to_pq2020_white() {
        let res = scrgb_to_pq2020([1.0, 1.0, 1.0]);
        let expected = pq_from_nits(80.0);
        for i in 0..3 {
            assert!((res[i] - expected).abs() < 1e-4, "Ch {} err: {}", i, res[i] - expected);
        }
    }

    #[test]
    fn test_tonemap_pq2020_white_peak1000() {
        let pq_white = [pq_from_nits(203.0); 3];
        let res = tonemap_pq2020_to_srgb8(pq_white, 1000.0);
        for i in 0..3 {
            assert!(res[i] >= 200, "Ch {} value: {}", i, res[i]);
        }
        assert_eq!(res[0], res[1]);
        assert_eq!(res[1], res[2]);
    }

    #[test]
    fn test_tonemap_monotonic() {
        let mut prev = tonemap_pq2020_to_srgb8([pq_from_nits(100.0); 3], 1000.0);
        for nits in 200..=10000 {
            let curr = tonemap_pq2020_to_srgb8([pq_from_nits(nits as f32); 3], 1000.0);
            for i in 0..3 {
                assert!(curr[i] >= prev[i], "Not monotonic at nits={}, ch={}", nits, i);
            }
            prev = curr;
        }
    }

    #[test]
    fn test_nv12_i420_nv12_roundtrip() {
        let w = 5;
        let h = 3;
        let y_stride = 8;
        let uv_stride = 8;
        let mut nv12_y = vec![0u8; h * y_stride];
        let mut nv12_uv = vec![0u8; (h + 1) / 2 * uv_stride];
        for i in 0..h * y_stride { nv12_y[i] = (i % 256) as u8; }
        for i in 0..(h + 1) / 2 * uv_stride { nv12_uv[i] = (i % 256) as u8; }

        let mut i420_y = vec![0u8; h * w];
        let mut i420_u = vec![0u8; (h + 1) / 2 * (w + 1) / 2];
        let mut i420_v = vec![0u8; (h + 1) / 2 * (w + 1) / 2];
        nv12_to_i420(&nv12_y, y_stride, &nv12_uv, uv_stride, w, h, &mut i420_y, &mut i420_u, &mut i420_v);

        let mut nv12_out = vec![0u8; h * y_stride + (h + 1) / 2 * uv_stride];
        i420_to_nv12(&i420_y, w, &i420_u, &i420_v, (w + 1) / 2, w, h, &mut nv12_out, y_stride);
        // Seule la zone utile (largeur w, chroma 2 x ceil(w/2)) est conservée ; le bourrage non.
        for r in 0..h {
            assert_eq!(&nv12_y[r * y_stride..r * y_stride + w], &nv12_out[r * y_stride..r * y_stride + w]);
        }
        let cw2 = 2 * ((w + 1) / 2);
        for r in 0..(h + 1) / 2 {
            let o = h * y_stride + r * uv_stride;
            assert_eq!(&nv12_uv[r * uv_stride..r * uv_stride + cw2], &nv12_out[o..o + cw2]);
        }
    }

    #[test]
    fn test_i420p10_to_p010_shift() {
        // Une ligne UV entrelacée occupe 2 x largeur chroma : pas >= 2.
        let mut out = vec![0u16; 4];
        i420p10_to_p010(&[1023], 1, &[512], &[64], 1, 1, 1, &mut out, 2);
        assert_eq!(out[0], 0xFFC0);
        assert_eq!(&out[2..4], &[512 << 6, 64 << 6]);
    }

    #[test]
    fn test_yuv444p_to_ayuv_order() {
        let mut out = vec![0u8; 4];
        yuv444p_to_ayuv(&[10], &[20], &[30], 1, 1, 1, &mut out, 4);
        assert_eq!(out, [30, 20, 10, 255]);
    }

    #[test]
    fn test_yuv444p10_to_y410_packing() {
        let mut out = vec![0u32; 1];
        yuv444p10_to_y410(&[940], &[512], &[64], 1, 1, 1, &mut out, 1);
        assert_eq!(out[0], (3 << 30) | (64 << 20) | (940 << 10) | 512);
    }

    #[test]
    fn test_bgra_to_yuv444_planar() {
        let bgra = [255, 255, 255, 255, 0, 0, 0, 255];
        let mut out = vec![0u8; 2 * 1 * 3];
        bgra_to_yuv444_planar(&bgra, 8, 2, 1, Matrix::Bt709, &mut out);
        assert_eq!(&out[0..2], &[235, 16]);
        assert_eq!(&out[2..4], &[128, 128]);
        assert_eq!(&out[4..6], &[128, 128]);
    }
}
