//! Utilitaires AV1 : découpe d'un flux d'OBU (« low overhead bitstream format »).

use std::ops::Range;

pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
pub const OBU_FRAME_HEADER: u8 = 3;
pub const OBU_TILE_GROUP: u8 = 4;
pub const OBU_METADATA: u8 = 5;
pub const OBU_FRAME: u8 = 6;
pub const OBU_REDUNDANT_FRAME_HEADER: u8 = 7;
pub const OBU_TILE_LIST: u8 = 8;
pub const OBU_PADDING: u8 = 15;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Obu {
    pub obu_type: u8,
    /// offset de l'octet d'en-tête
    pub header: usize,
    /// plage du payload dans le tampon
    pub payload: Range<usize>,
}

/// Décode un entier LEB128 signé sur 7 bits par octet.
/// Renvoie `(valeur, nombre_d_octets_consommés)` ou `None` en cas d'erreur.
pub fn read_leb128(data: &[u8]) -> Option<(u64, usize)> {
    let mut val: u64 = 0;
    let mut shift: u32 = 0;

    for (i, &byte) in data.iter().enumerate() {
        val |= ((byte & 0x7F) as u64) << shift;
        shift += 7;

        if (byte & 0x80) == 0 {
            return if val > u32::MAX as u64 {
                None
            } else {
                Some((val, i + 1))
            };
        }

        // Plus de 8 octets autorisés ou bit de continuation sur le 8ème octet
        if shift >= 56 {
            return None;
        }
    }

    // Tampon tronqué : le dernier octet lu avait le bit de continuation à 1
    None
}

/// Découpe un flux binaire en une liste d'OBU valides.
/// Renvoie `None` si le flux est invalide (bit forbidden, tronqué, etc.).
pub fn split_obus(data: &[u8]) -> Option<Vec<Obu>> {
    if data.is_empty() {
        return Some(Vec::new());
    }

    let mut obus = Vec::new();
    let mut i = 0;

    while i < data.len() {
        let b = data[i];

        // Bit 7 (forbidden) doit valoir 0
        if b & 0x80 != 0 {
            return None;
        }

        let obu_type = (b >> 3) & 0x0F;
        let extension_flag = (b >> 2) & 1;
        let has_size = (b >> 1) & 1;

        let header_start = i;
        i += 1;

        // Gestion du flag d'extension
        if extension_flag == 1 {
            if i >= data.len() {
                return None;
            }
            i += 1;
        }

        let (payload_start, payload_end) = if has_size == 1 {
            let (size, consumed) = read_leb128(&data[i..])?;
            let start = i + consumed;
            let end = start.checked_add(usize::try_from(size).ok()?)?;
            if end > data.len() {
                return None;
            }
            (start, end)
        } else {
            // Pas de champ taille : le payload s'étend jusqu'à la fin du tampon
            (i, data.len())
        };
        i = payload_end;

        obus.push(Obu {
            obu_type,
            header: header_start,
            payload: payload_start..payload_end,
        });
    }

    Some(obus)
}

/// Renvoie les types d'OBU présents dans le flux, ou un vecteur vide en cas d'échec.
pub fn obu_types(data: &[u8]) -> Vec<u8> {
    split_obus(data)
        .map(|o| o.iter().map(|x| x.obu_type).collect())
        .unwrap_or_default()
}

/// Vérifie si le flux ressemble à un flux AV1 valide (low overhead).
pub fn looks_like_av1(data: &[u8]) -> bool {
    let Some(obus) = split_obus(data) else { return false };
    if obus.is_empty() {
        return false;
    }
    if obus[0].obu_type != OBU_TEMPORAL_DELIMITER && obus[0].obu_type != OBU_SEQUENCE_HEADER {
        return false;
    }
    obus.iter().all(|o| matches!(o.obu_type, 1..=8 | 15))
}

/// Vérifie si le flux contient un Sequence Header.
pub fn contains_sequence_header(data: &[u8]) -> bool {
    split_obus(data)
        .map(|o| o.iter().any(|x| x.obu_type == OBU_SEQUENCE_HEADER))
        .unwrap_or(false)
}

/// Détermine si le flux contient une image clé (KEY_FRAME).
pub fn is_key_frame(data: &[u8]) -> bool {
    let Some(obus) = split_obus(data) else { return false };

    // 1. Vérifie le premier Sequence Header (obligatoire sur une image clé émise par l'encodeur)
    let Some(sh) = obus.iter().find(|o| o.obu_type == OBU_SEQUENCE_HEADER) else { return false };
    {
        if !sh.payload.is_empty() {
            let first = sh.payload.start;
            // reduced_still_picture_header = bit 0x08
            if data[first] & 0x08 != 0 {
                return true;
            }
        }
    }

    // 2. Sinon, vérifie le premier Frame Header ou Frame
    if let Some(fh) = obus.iter().find(|o| o.obu_type == OBU_FRAME_HEADER || o.obu_type == OBU_FRAME) {
        if !fh.payload.is_empty() {
            let first = fh.payload.start;
            // show_existing_frame = bit 0x80
            if data[first] & 0x80 != 0 {
                return false;
            }
            // frame_type = bits 5-6
            let frame_type = (data[first] >> 5) & 0x3;
            return frame_type == 0;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_leb128() {
        assert_eq!(read_leb128(&[0x00]), Some((0, 1)));
        assert_eq!(read_leb128(&[0x7F]), Some((127, 1)));
        assert_eq!(read_leb128(&[0x80, 0x01]), Some((128, 2)));
        assert_eq!(read_leb128(&[0x80]), None); // tronqué
        assert_eq!(read_leb128(&[0x80; 9]), None); // plus de 8 octets
        assert_eq!(read_leb128(&[0xFF, 0xFF, 0xFF, 0xFF, 0x10]), None); // > u32::MAX
        assert_eq!(read_leb128(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]), Some((u32::MAX as u64, 5)));
    }

    #[test]
    fn test_split_obus_typical() {
        // [TD 0x12 0x00][SEQ 0x0A taille 2 payload [0x00,0x00]][FRAME 0x32 taille 2 payload [0x10, 0xAA]]
        let data = [0x12, 0x00, 0x0A, 0x02, 0x00, 0x00, 0x32, 0x02, 0x10, 0xAA];
        let obus = split_obus(&data).expect("split_obus failed");
        assert_eq!(obus.len(), 3);
        assert_eq!(obu_types(&data), vec![2, 1, 6]);
        assert!(looks_like_av1(&data));
        assert!(contains_sequence_header(&data));
        assert!(is_key_frame(&data));
    }

    #[test]
    fn test_inter_frame() {
        // [TD][FRAME payload [0x30 ...]]
        let data = [0x12, 0x00, 0x32, 0x01, 0x30];
        assert!(!is_key_frame(&data));
        assert!(!contains_sequence_header(&data));
    }

    #[test]
    fn test_extension_obu() {
        // type=2, ext=1, size=1 -> 0x14, size=0x01, payload=[0x00]
        let data = [0x14, 0x01, 0x00];
        let obus = split_obus(&data).expect("failed");
        assert_eq!(obus[0].obu_type, 2);
        assert_eq!(obus[0].payload, 2..3);
    }

    #[test]
    fn test_no_size_field() {
        // type=6, ext=0, size=0 -> 0x30, payload goes to end
        let data = [0x30, 0x10, 0xAA];
        let obus = split_obus(&data).expect("failed");
        assert_eq!(obus[0].obu_type, 6);
        assert_eq!(obus[0].payload, 1..3);
    }

    #[test]
    fn test_forbidden_bit() {
        let data = [0x80, 0x00];
        assert!(split_obus(&data).is_none());
    }

    #[test]
    fn test_truncated_payload() {
        // size says 2, but only 1 byte left
        let data = [0x12, 0x02, 0x00];
        assert!(split_obus(&data).is_none());
    }

    #[test]
    fn test_h264_annex_b() {
        let data = [0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x28];
        assert!(!looks_like_av1(&data));
    }

    #[test]
    fn test_fuzz_deterministic() {
        // xorshift32
        let mut seed = 123456789u32;
        let xorshift = |s: &mut u32| {
            *s ^= *s << 13;
            *s ^= *s >> 17;
            *s ^= *s << 5;
            *s
        };

        for _ in 0..100_000 {
            let len = (xorshift(&mut seed) % 64) as usize;
            let mut buf = vec![0u8; len];
            for b in buf.iter_mut() {
                *b = xorshift(&mut seed) as u8;
            }

            // Appelle toutes les fonctions sans paniquer
            let _ = split_obus(&buf);
            let _ = obu_types(&buf);
            let _ = looks_like_av1(&buf);
            let _ = contains_sequence_header(&buf);
            let _ = is_key_frame(&buf);

            // Vérifie que les payloads sont dans les bornes du tampon
            if let Some(obus) = split_obus(&buf) {
                for obu in &obus {
                    assert!(obu.header < buf.len(), "header out of bounds");
                    assert!(obu.payload.start <= buf.len(), "payload start out of bounds");
                    assert!(obu.payload.end <= buf.len(), "payload end out of bounds");
                    assert!(obu.payload.start <= obu.payload.end, "invalid range");
                }
            }
        }
    }
}
