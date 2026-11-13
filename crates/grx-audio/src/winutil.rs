//! Utilitaires WASAPI communs a la capture et a la lecture.

use wasapi::{SampleType, WaveFormat};

/// Priorite temps reel "Pro Audio" (MMCSS) pour le thread courant ; sans effet si indisponible.
pub fn boost_thread() {
    use windows::core::w;
    use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
    let mut idx = 0u32;
    let _ = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut idx) };
}

/// Format d'echantillon d'un flux WASAPI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    F32,
    I16,
    I24,
    I32,
}

pub fn kind_of(fmt: &WaveFormat) -> Option<Kind> {
    let bits = fmt.get_bitspersample();
    let st = fmt.get_subformat().ok()?;
    match (st, bits) {
        (SampleType::Float, 32) => Some(Kind::F32),
        (SampleType::Int, 16) => Some(Kind::I16),
        (SampleType::Int, 24) => Some(Kind::I24),
        (SampleType::Int, 32) => Some(Kind::I32),
        _ => None,
    }
}

impl Kind {
    pub fn bytes(self) -> usize {
        match self {
            Kind::F32 | Kind::I32 => 4,
            Kind::I16 => 2,
            Kind::I24 => 3,
        }
    }

    fn read(self, b: &[u8]) -> f32 {
        match self {
            Kind::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            Kind::I16 => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
            Kind::I24 => (((b[0] as i32) << 8 | (b[1] as i32) << 16 | (b[2] as i32) << 24) >> 8) as f32 / 8_388_608.0,
            Kind::I32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
        }
    }

    fn write(self, v: f32, out: &mut Vec<u8>) {
        match self {
            Kind::F32 => out.extend_from_slice(&v.to_le_bytes()),
            Kind::I16 => out.extend_from_slice(&((v * 32768.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes()),
            Kind::I24 => {
                let x = (v * 8_388_608.0).round().clamp(-8_388_608.0, 8_388_607.0) as i32;
                out.extend_from_slice(&x.to_le_bytes()[..3]);
            }
            Kind::I32 => out.extend_from_slice(&((v as f64 * 2_147_483_648.0).round().clamp(-2_147_483_648.0, 2_147_483_647.0) as i32).to_le_bytes()),
        }
    }
}

/// Octets d'un flux WASAPI (`channels` canaux entrelaces) -> stereo f32 entrelace.
/// mono : duplique ; >2 canaux : on garde avant gauche/droite.
pub fn to_stereo_f32(bytes: &[u8], kind: Kind, channels: usize, out: &mut Vec<f32>) {
    let sb = kind.bytes();
    let frame = sb * channels;
    for f in bytes.chunks_exact(frame) {
        let l = kind.read(&f[..sb]);
        let r = if channels >= 2 { kind.read(&f[sb..2 * sb]) } else { l };
        out.push(l);
        out.push(r);
    }
}

/// Stereo f32 entrelace -> octets pour un flux a `channels` canaux (les canaux >2 restent a zero ;
/// mono : moyenne).
pub fn from_stereo_f32(stereo: &[f32], kind: Kind, channels: usize, volume: f32, out: &mut Vec<u8>) {
    for f in stereo.chunks_exact(2) {
        let (l, r) = (f[0] * volume, f[1] * volume);
        if channels == 1 {
            kind.write(0.5 * (l + r), out);
            continue;
        }
        kind.write(l, out);
        kind.write(r, out);
        for _ in 2..channels {
            kind.write(0.0, out);
        }
    }
}
