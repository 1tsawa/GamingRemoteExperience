//! Utilitaires Annex-B (start codes `00 00 01` / `00 00 00 01`).

use crate::Codec;

/// Itère sur les NAL units d'un flux Annex-B et renvoie le `nal_unit_type`
/// de chacune (H.264 : 5 bits, H.265 : 6 bits).
pub fn nal_types(codec: Codec, data: &[u8]) -> Vec<u8> {
    if codec == Codec::Av1 {
        return crate::obu::obu_types(data);
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let hdr = data[i + 3];
            out.push(match codec {
                Codec::H264 => hdr & 0x1F,
                Codec::Hevc | Codec::Av1 => (hdr >> 1) & 0x3F,
            });
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

/// `true` si le flux contient une image IDR (H.264 : type 5 ; HEVC : 19/20 = IDR_W_RADL/IDR_N_LP).
pub fn contains_idr(codec: Codec, data: &[u8]) -> bool {
    if codec == Codec::Av1 {
        return crate::obu::is_key_frame(data);
    }
    nal_types(codec, data).iter().any(|&t| match codec {
        Codec::H264 => t == 5,
        Codec::Hevc => t == 19 || t == 20,
        Codec::Av1 => false,
    })
}

/// `true` si le flux contient les jeux de paramètres (HEVC : VPS(32)+SPS(33)+PPS(34) ; H.264 : SPS(7)+PPS(8)).
pub fn contains_parameter_sets(codec: Codec, data: &[u8]) -> bool {
    let t = nal_types(codec, data);
    match codec {
        Codec::H264 => t.contains(&7) && t.contains(&8),
        Codec::Hevc => t.contains(&32) && t.contains(&33) && t.contains(&34),
        Codec::Av1 => t.contains(&crate::obu::OBU_SEQUENCE_HEADER),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hevc_nals() {
        // VPS(32)=0x40, SPS(33)=0x42, PPS(34)=0x44, IDR_W_RADL(19)=0x26
        let s = [
            0, 0, 0, 1, 0x40, 1, 0xAA, 0, 0, 0, 1, 0x42, 1, 0xBB, 0, 0, 1, 0x44, 1, 0xCC, 0, 0, 1, 0x26, 1,
            0xDD,
        ];
        assert_eq!(nal_types(Codec::Hevc, &s), vec![32, 33, 34, 19]);
        assert!(contains_idr(Codec::Hevc, &s));
        assert!(contains_parameter_sets(Codec::Hevc, &s));
    }

    #[test]
    fn parses_h264_nals() {
        // SPS=0x67(7), PPS=0x68(8), IDR=0x65(5), non-IDR=0x41(1)
        let idr = [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 1, 0, 0, 1, 0x65, 1];
        assert!(contains_idr(Codec::H264, &idr));
        assert!(contains_parameter_sets(Codec::H264, &idr));
        let p = [0, 0, 0, 1, 0x41, 1, 2, 3];
        assert!(!contains_idr(Codec::H264, &p));
        assert!(!contains_parameter_sets(Codec::H264, &p));
    }
}
