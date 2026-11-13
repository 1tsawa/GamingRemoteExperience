//! Parseur Annex-B : découpe en NAL units et en access units (images).
//!
//! Sert à lire un fichier `.h264`/`.h265` brut image par image (le flux réseau fournit déjà une
//! access unit par `EncodedFrame`) et à inspecter un paquet (IDR, jeux de paramètres).

use grx_video::Codec;
use std::ops::Range;

/// Une NAL unit localisée dans un tampon Annex-B.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nal {
    /// Début du start code (`00 00 01` ou `00 00 00 01`).
    pub start: usize,
    /// Début de l'en-tête NAL (juste après le start code).
    pub header: usize,
    /// Fin de la NAL (exclue) : début du start code suivant, zéros de fin retirés.
    pub end: usize,
    /// `nal_unit_type` (H.264 : 5 bits, H.265 : 6 bits).
    pub nal_type: u8,
}

impl Nal {
    pub fn payload<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[self.header..self.end]
    }
}

fn nal_type(codec: Codec, hdr: u8) -> u8 {
    match codec {
        Codec::H264 => hdr & 0x1F,
        Codec::Hevc => (hdr >> 1) & 0x3F,
    }
}

/// Découpe `data` en NAL units.
pub fn split_nals(codec: Codec, data: &[u8]) -> Vec<Nal> {
    // Positions des start codes `00 00 01`.
    let mut codes: Vec<usize> = Vec::new();
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i + 2] > 1 {
            i += 3;
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            codes.push(i);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(codes.len());
    for (k, &c) in codes.iter().enumerate() {
        let header = c + 3;
        if header >= data.len() {
            break;
        }
        let mut end = codes.get(k + 1).copied().unwrap_or(data.len());
        // Retire les zéros de fin (début d'un start code 4 octets ou trailing_zero_8bits).
        while end > header + 1 && data[end - 1] == 0 {
            end -= 1;
        }
        // Start code de 4 octets : inclut le 00 initial dans `start`.
        let start = if c > 0 && data[c - 1] == 0 { c - 1 } else { c };
        nals.push(Nal { start, header, end, nal_type: nal_type(codec, data[header]) });
    }
    nals
}

/// `nal_unit_type` de chaque NAL du tampon.
pub fn nal_types(codec: Codec, data: &[u8]) -> Vec<u8> {
    split_nals(codec, data).iter().map(|n| n.nal_type).collect()
}

fn is_vcl(codec: Codec, t: u8) -> bool {
    match codec {
        Codec::H264 => (1..=5).contains(&t),
        Codec::Hevc => t <= 31,
    }
}

/// `true` pour une NAL qui ouvre forcément une nouvelle access unit (si une image est déjà ouverte).
fn starts_new_au(codec: Codec, t: u8) -> bool {
    match codec {
        // SEI, SPS, PPS, AUD, prefix NAL, subset SPS...
        Codec::H264 => matches!(t, 6..=9 | 14..=18),
        // VPS, SPS, PPS, AUD, prefix SEI, 41..=44, 48..=55
        Codec::Hevc => matches!(t, 32..=35 | 39 | 41..=44 | 48..=55),
    }
}

fn first_slice_flag(codec: Codec, data: &[u8], n: &Nal) -> bool {
    // H.264 : first_mb_in_slice (ue(v)) == 0  <=> premier bit du slice header à 1.
    // H.265 : first_slice_segment_in_pic_flag = premier bit après l'en-tête de 2 octets.
    let off = n.header + if codec == Codec::Hevc { 2 } else { 1 };
    data.get(off).map_or(false, |b| b & 0x80 != 0)
}

/// Découpe un flux Annex-B continu en access units (une par image).
/// Renvoie les plages d'octets (start code inclus) ; chaque plage est décodable telle quelle.
pub fn split_access_units(codec: Codec, data: &[u8]) -> Vec<Range<usize>> {
    let nals = split_nals(codec, data);
    let mut aus = Vec::new();
    let mut au_start: Option<usize> = None;
    let mut has_vcl = false;
    let mut last_end = 0;
    for n in &nals {
        let vcl = is_vcl(codec, n.nal_type);
        let new_au = has_vcl && (starts_new_au(codec, n.nal_type) || (vcl && first_slice_flag(codec, data, n)));
        if new_au {
            aus.push(au_start.unwrap()..last_end);
            au_start = None;
            has_vcl = false;
        }
        if au_start.is_none() {
            au_start = Some(n.start);
        }
        has_vcl |= vcl;
        last_end = n.end;
    }
    if let Some(s) = au_start {
        aus.push(s..last_end);
    }
    aus
}

/// `true` si le paquet contient une image IDR (H.264 : 5 ; HEVC : 19/20).
pub fn contains_idr(codec: Codec, data: &[u8]) -> bool {
    nal_types(codec, data).iter().any(|&t| match codec {
        Codec::H264 => t == 5,
        Codec::Hevc => t == 19 || t == 20,
    })
}

/// `true` si le paquet contient un point d'accès aléatoire (IDR, ou en HEVC BLA/CRA : types 16..=21).
pub fn contains_random_access(codec: Codec, data: &[u8]) -> bool {
    nal_types(codec, data).iter().any(|&t| match codec {
        Codec::H264 => t == 5,
        Codec::Hevc => (16..=21).contains(&t),
    })
}

/// `true` si le paquet contient les jeux de paramètres (H.264 : SPS+PPS ; HEVC : VPS+SPS+PPS).
pub fn contains_parameter_sets(codec: Codec, data: &[u8]) -> bool {
    let t = nal_types(codec, data);
    match codec {
        Codec::H264 => t.contains(&7) && t.contains(&8),
        Codec::Hevc => t.contains(&32) && t.contains(&33) && t.contains(&34),
    }
}

/// Devine le codec d'un fichier brut d'après sa première NAL : HEVC si c'est un VPS/SPS/PPS/AUD
/// (types 32..=35, `nuh_layer_id` 0, `temporal_id_plus1` valide), sinon H.264 (SPS/PPS/AUD/SEI/IDR).
pub fn sniff_codec(data: &[u8]) -> Option<Codec> {
    let first = data.windows(3).position(|w| w == [0, 0, 1])?;
    let hdr = *data.get(first + 3)?;
    let b1 = *data.get(first + 4)?;
    let hevc_type = (hdr >> 1) & 0x3F;
    if hdr & 0x81 == 0 && (32..=35).contains(&hevc_type) && b1 & 0xF8 == 0 && b1 & 7 != 0 {
        return Some(Codec::Hevc);
    }
    if hdr & 0x80 == 0 && matches!(hdr & 0x1F, 5 | 6 | 7 | 8 | 9) {
        return Some(Codec::H264);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // HEVC : VPS(0x40) SPS(0x42) PPS(0x44) IDR_W_RADL(0x26, first_slice=1), puis TRAIL_R(0x02, first_slice=1),
    // puis un second slice de la même image (first_slice=0), puis TRAIL_R image suivante.
    fn hevc_stream() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend([0, 0, 0, 1, 0x40, 1, 0xAA]);
        v.extend([0, 0, 0, 1, 0x42, 1, 0xBB]);
        v.extend([0, 0, 1, 0x44, 1, 0xCC]);
        v.extend([0, 0, 0, 1, 0x26, 1, 0x80, 0x11]); // IDR first_slice
        v.extend([0, 0, 0, 1, 0x02, 1, 0x80, 0x22]); // P first_slice
        v.extend([0, 0, 1, 0x02, 1, 0x00, 0x33]); // même image, slice 2
        v.extend([0, 0, 0, 1, 0x02, 1, 0x80, 0x44]); // image suivante
        v
    }

    #[test]
    fn splits_hevc_nals_and_types() {
        let s = hevc_stream();
        assert_eq!(nal_types(Codec::Hevc, &s), vec![32, 33, 34, 19, 1, 1, 1]);
        assert!(contains_idr(Codec::Hevc, &s));
        assert!(contains_parameter_sets(Codec::Hevc, &s));
        let n = split_nals(Codec::Hevc, &s);
        assert_eq!(n[0].start, 0);
        assert_eq!(n[0].header, 4);
        assert_eq!(n[0].payload(&s), &[0x40, 1, 0xAA]);
        assert_eq!(n[2].start, 14); // start code 3 octets
    }

    #[test]
    fn splits_hevc_access_units() {
        let s = hevc_stream();
        let aus = split_access_units(Codec::Hevc, &s);
        assert_eq!(aus.len(), 3);
        assert_eq!(nal_types(Codec::Hevc, &s[aus[0].clone()]), vec![32, 33, 34, 19]);
        assert_eq!(nal_types(Codec::Hevc, &s[aus[1].clone()]), vec![1, 1]);
        assert_eq!(nal_types(Codec::Hevc, &s[aus[2].clone()]), vec![1]);
        assert_eq!(aus[0].start, 0);
        assert_eq!(aus[2].end, s.len());
    }

    #[test]
    fn splits_h264_access_units() {
        let mut v = Vec::new();
        v.extend([0, 0, 0, 1, 0x67, 1, 2]); // SPS
        v.extend([0, 0, 0, 1, 0x68, 3]); // PPS
        v.extend([0, 0, 1, 0x65, 0x88, 9]); // IDR first_mb=0 (bit7=1)
        v.extend([0, 0, 0, 1, 0x41, 0x9A, 7]); // P first_mb=0
        v.extend([0, 0, 0, 1, 0x41, 0x1A, 7]); // P slice 2 (first_mb != 0) : même image
        v.extend([0, 0, 0, 1, 0x09, 0x10]); // AUD -> nouvelle AU
        v.extend([0, 0, 0, 1, 0x41, 0x9A, 8]);
        let aus = split_access_units(Codec::H264, &v);
        assert_eq!(aus.len(), 3);
        assert_eq!(nal_types(Codec::H264, &v[aus[0].clone()]), vec![7, 8, 5]);
        assert_eq!(nal_types(Codec::H264, &v[aus[1].clone()]), vec![1, 1]);
        assert_eq!(nal_types(Codec::H264, &v[aus[2].clone()]), vec![9, 1]);
        assert!(contains_idr(Codec::H264, &v[aus[0].clone()]));
        assert!(!contains_idr(Codec::H264, &v[aus[1].clone()]));
        assert!(contains_parameter_sets(Codec::H264, &v[aus[0].clone()]));
    }

    #[test]
    fn handles_garbage_and_empty() {
        assert!(split_nals(Codec::Hevc, &[]).is_empty());
        assert!(split_nals(Codec::Hevc, &[1, 2, 3, 4, 5]).is_empty());
        assert!(split_access_units(Codec::H264, &[0, 0, 1]).is_empty());
        // Zéros de fin retirés.
        let s = [0, 0, 1, 0x65, 0x80, 5, 0, 0, 0];
        let n = split_nals(Codec::H264, &s);
        assert_eq!(n[0].end, 6);
    }

    #[test]
    fn sniffs_codec() {
        assert_eq!(sniff_codec(&hevc_stream()), Some(Codec::Hevc));
        assert_eq!(sniff_codec(&[0, 0, 0, 1, 0x67, 1, 2]), Some(Codec::H264));
        // AUD en tête (cas de l'encodeur NVENC de grx-video).
        assert_eq!(sniff_codec(&[0, 0, 0, 1, 0x46, 1, 0x10, 0, 0, 0, 1, 0x40, 1]), Some(Codec::Hevc));
        assert_eq!(sniff_codec(&[0, 0, 0, 1, 0x09, 0x10, 0, 0, 0, 1, 0x67, 0x64]), Some(Codec::H264));
        assert_eq!(sniff_codec(&[1, 2, 3]), None);
    }
}
