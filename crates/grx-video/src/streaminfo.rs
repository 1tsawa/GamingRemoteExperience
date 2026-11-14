//! Lecture minimale des en-têtes de flux (SPS H.264/HEVC, sequence header AV1) : profil,
//! sous-échantillonnage, profondeur. Sert à valider le flux produit par la sonde des encodeurs.

use crate::{nal, obu, Codec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamInfo {
    pub profile: u8,
    /// 0 monochrome, 1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4.
    pub chroma_format_idc: u8,
    pub bit_depth: u8,
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }
    fn bit(&mut self) -> Option<u64> {
        let b = *self.data.get(self.pos / 8)?;
        let v = (b >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(v as u64)
    }
    fn u(&mut self, n: u32) -> Option<u64> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }
    fn flag(&mut self) -> Option<bool> {
        Some(self.bit()? == 1)
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        if self.pos + n > self.data.len() * 8 {
            return None;
        }
        self.pos += n;
        Some(())
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + self.u(zeros)?) as u32)
    }
    /// `uvlc()` AV1.
    fn uvlc(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros >= 32 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + self.u(zeros)?) as u32)
    }
}

/// Retire les octets d'émulation (00 00 03 -> 00 00).
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Première NAL de type `nal_type` (en-tête compris), jusqu'au start code suivant.
fn find_nal(codec: Codec, data: &[u8], nal_type: u8) -> Option<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    for (k, &s) in starts.iter().enumerate() {
        let h = *data.get(s)?;
        let t = match codec {
            Codec::H264 => h & 0x1F,
            _ => (h >> 1) & 0x3F,
        };
        if t != nal_type {
            continue;
        }
        let mut end = starts.get(k + 1).map(|&n| n - 3).unwrap_or(data.len());
        // Start code sur 4 octets : le zéro de tête appartient au suivant.
        while end > s && data[end - 1] == 0 && starts.get(k + 1).is_some() {
            end -= 1;
        }
        return Some(&data[s..end]);
    }
    None
}

fn h264(sps: &[u8]) -> Option<StreamInfo> {
    let rbsp = unescape(sps.get(1..)?);
    let mut r = BitReader::new(&rbsp);
    let profile = r.u(8)? as u8;
    r.skip(16)?;
    r.ue()?;
    let (chroma, depth) = if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        let c = r.ue()?;
        if c == 3 {
            r.skip(1)?;
        }
        (c, r.ue()? + 8)
    } else {
        (1, 8)
    };
    Some(StreamInfo { profile, chroma_format_idc: chroma.min(255) as u8, bit_depth: depth.min(255) as u8 })
}

fn hevc(sps: &[u8]) -> Option<StreamInfo> {
    let rbsp = unescape(sps.get(2..)?);
    let mut r = BitReader::new(&rbsp);
    r.skip(4)?;
    let m = r.u(3)? as usize;
    r.skip(1)?;
    r.skip(3)?;
    let profile = r.u(5)? as u8;
    r.skip(32 + 48)?;
    r.skip(8)?;
    let mut flags = Vec::new();
    for _ in 0..m {
        flags.push((r.flag()?, r.flag()?));
    }
    if m > 0 {
        r.skip(2 * (8 - m))?;
    }
    for (p, l) in flags {
        if p {
            r.skip(88)?;
        }
        if l {
            r.skip(8)?;
        }
    }
    r.ue()?;
    let chroma = r.ue()?;
    if chroma == 3 {
        r.skip(1)?;
    }
    r.ue()?;
    r.ue()?;
    if r.flag()? {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    let depth = r.ue()? + 8;
    Some(StreamInfo { profile, chroma_format_idc: chroma.min(255) as u8, bit_depth: depth.min(255) as u8 })
}

fn av1(seq: &[u8]) -> Option<StreamInfo> {
    let mut r = BitReader::new(seq);
    let profile = r.u(3)? as u8;
    r.skip(1)?;
    let reduced = r.flag()?;
    if reduced {
        r.skip(5)?;
    } else {
        let mut decoder_model = false;
        let mut n = 0;
        if r.flag()? {
            r.skip(64)?;
            if r.flag()? {
                r.uvlc()?;
            }
            decoder_model = r.flag()?;
            if decoder_model {
                n = r.u(5)? as u32 + 1;
                r.skip(32 + 5 + 5)?;
            }
        }
        let initial_display = r.flag()?;
        let ops = r.u(5)? + 1;
        for _ in 0..ops {
            r.skip(12)?;
            if r.u(5)? > 7 {
                r.skip(1)?;
            }
            if decoder_model && r.flag()? {
                r.skip(2 * n as usize + 1)?;
            }
            if initial_display && r.flag()? {
                r.skip(4)?;
            }
        }
    }
    let fw = r.u(4)? as u32 + 1;
    let fh = r.u(4)? as u32 + 1;
    r.u(fw)?;
    r.u(fh)?;
    if !reduced && r.flag()? {
        r.skip(7)?;
    }
    r.skip(3)?;
    if !reduced {
        r.skip(4)?;
        let order_hint = r.flag()?;
        if order_hint {
            r.skip(2)?;
        }
        let force_sct = if r.flag()? { 2 } else { r.u(1)? };
        if force_sct > 0 && !r.flag()? {
            r.skip(1)?;
        }
        if order_hint {
            r.skip(3)?;
        }
    }
    r.skip(3)?;
    let high = r.flag()?;
    let depth = if profile == 2 && high {
        if r.flag()? {
            12
        } else {
            10
        }
    } else if high {
        10
    } else {
        8
    };
    let mono = if profile == 1 { false } else { r.flag()? };
    let chroma = if mono {
        0
    } else {
        match profile {
            0 => 1,
            1 => 3,
            _ => 2,
        }
    };
    Some(StreamInfo { profile, chroma_format_idc: chroma, bit_depth: depth })
}

/// Profil, sous-échantillonnage et profondeur lus dans le premier SPS / sequence header.
pub fn stream_info(codec: Codec, data: &[u8]) -> Option<StreamInfo> {
    match codec {
        Codec::H264 => h264(find_nal(codec, data, 7)?),
        Codec::Hevc => hevc(find_nal(codec, data, 33)?),
        Codec::Av1 => {
            let obus = obu::split_obus(data)?;
            let o = obus.iter().find(|o| o.obu_type == obu::OBU_SEQUENCE_HEADER)?;
            av1(data.get(o.payload.clone())?)
        }
    }
}

fn chroma_label(c: u8) -> &'static str {
    match c {
        0 => "monochrome",
        1 => "4:2:0",
        2 => "4:2:2",
        3 => "4:4:4",
        _ => "inconnu",
    }
}

/// Vérifie qu'un flux (la première image encodée) est plausible et du format attendu.
pub fn check_stream(codec: Codec, want_444: bool, want_10bit: bool, data: &[u8]) -> Result<StreamInfo, String> {
    if data.is_empty() {
        return Err("flux vide".into());
    }
    if codec == Codec::Av1 {
        if !obu::looks_like_av1(data) {
            return Err("flux AV1 invalide (OBU)".into());
        }
    } else if nal::nal_types(codec, data).is_empty() {
        return Err("aucune NAL Annex-B".into());
    }
    if !nal::contains_parameter_sets(codec, data) {
        return Err("jeux de paramètres absents (SPS/PPS/VPS ou sequence header)".into());
    }
    if !nal::contains_idr(codec, data) {
        return Err("pas d'image clé (IDR)".into());
    }
    let info = stream_info(codec, data).ok_or("en-tête de séquence illisible")?;
    let want_c = if want_444 { 3 } else { 1 };
    if info.chroma_format_idc != want_c {
        return Err(format!("sous-échantillonnage {} au lieu de {}", chroma_label(info.chroma_format_idc), chroma_label(want_c)));
    }
    let want_d = if want_10bit { 10 } else { 8 };
    if info.bit_depth != want_d {
        return Err(format!("profondeur {} bits au lieu de {}", info.bit_depth, want_d));
    }
    Ok(info)
}

#[cfg(test)]
#[path = "streaminfo_tests.rs"]
mod tests;
