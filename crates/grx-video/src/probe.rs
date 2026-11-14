//! Sonde des encodeurs **par test réel** (logique pure, testable sans GPU).
//!
//! Pour chaque combinaison candidate (voie × codec × format), la sonde de [`crate::probe_adapter`]
//! crée réellement l'encodeur sur l'adaptateur, encode deux petites images et valide le flux
//! ([`crate::streaminfo::check_stream`]). Ce module garde les résultats ([`ProbeResult`]), les
//! installe par adaptateur (LUID) pour [`crate::Encoder::with_device`] et
//! [`crate::encoders_for_adapter`], et fournit les identifiants texte du cache disque (grx-host).

use std::sync::Mutex;

use crate::{Codec, EncoderInfo, EncoderKind, PixelFormat, Vendor};

/// Résultat du test d'une combinaison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    pub kind: EncoderKind,
    /// Nom convivial de l'encodeur (MFT, « NVENC direct », version OpenH264).
    pub name: String,
    pub codec: Codec,
    pub format: PixelFormat,
    pub ok: bool,
    /// Raison de l'échec (vide si `ok`).
    pub reason: String,
    /// Création de l'encodeur + première image (ms).
    pub init_ms: u32,
    /// Encodage de la seconde image (ms).
    pub frame_ms: u32,
    /// Échec peut-être passager (délai dépassé, encodeur occupé) : ne pas le figer dans le cache.
    pub transient: bool,
}

impl ProbeResult {
    pub fn vendor(&self) -> Vendor {
        kind_vendor(self.kind)
    }
}

pub fn kind_vendor(k: EncoderKind) -> Vendor {
    match k {
        EncoderKind::Mf(v) => v,
        EncoderKind::Nvenc => Vendor::Nvidia,
        EncoderKind::OpenH264 => Vendor::Software,
    }
}

/// Verdict de la sonde pour une voie : `None` si la combinaison n'a pas été testée.
pub fn verdict(results: &[ProbeResult], kind: EncoderKind, codec: Codec, format: PixelFormat) -> Option<Result<(), String>> {
    results
        .iter()
        .find(|r| r.kind == kind && r.codec == codec && r.format == format)
        .map(|r| if r.ok { Ok(()) } else { Err(r.reason.clone()) })
}

/// Encodeurs annonçables : seules les combinaisons qui ont passé la sonde, regroupées par
/// (codec, voie) dans l'ordre de la sonde.
pub fn infos_from_results(results: &[ProbeResult]) -> Vec<EncoderInfo> {
    let mut out: Vec<EncoderInfo> = Vec::new();
    for r in results.iter().filter(|r| r.ok) {
        match out.iter_mut().find(|e| e.codec == r.codec && e.kind == r.kind) {
            Some(e) => {
                if !e.formats.contains(&r.format) {
                    e.formats.push(r.format);
                }
            }
            None => out.push(EncoderInfo { codec: r.codec, kind: r.kind, name: r.name.clone(), formats: vec![r.format] }),
        }
    }
    out
}

static INSTALLED: Mutex<Vec<(i64, Vec<ProbeResult>)>> = Mutex::new(Vec::new());

/// Installe les résultats de la sonde de l'adaptateur `luid` (remplace les précédents).
pub fn install(luid: i64, results: Vec<ProbeResult>) {
    let mut g = INSTALLED.lock().unwrap_or_else(|e| e.into_inner());
    g.retain(|(l, _)| *l != luid);
    g.push((luid, results));
}

/// Résultats installés pour l'adaptateur `luid` (`None` : sonde pas encore finie).
pub fn installed(luid: i64) -> Option<Vec<ProbeResult>> {
    INSTALLED.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(l, _)| *l == luid).map(|(_, v)| v.clone())
}

/// Retire les résultats installés pour `luid`.
pub fn uninstall(luid: i64) {
    INSTALLED.lock().unwrap_or_else(|e| e.into_inner()).retain(|(l, _)| *l != luid);
}

// ---- identifiants texte (cache disque, sortie JSON) ----

pub fn kind_id(k: EncoderKind) -> &'static str {
    match k {
        EncoderKind::Mf(Vendor::Nvidia) => "mf-nvidia",
        EncoderKind::Mf(Vendor::Amd) => "mf-amd",
        EncoderKind::Mf(Vendor::Intel) => "mf-intel",
        EncoderKind::Mf(_) => "mf-other",
        EncoderKind::Nvenc => "nvenc",
        EncoderKind::OpenH264 => "openh264",
    }
}

pub fn kind_from_id(s: &str) -> Option<EncoderKind> {
    Some(match s {
        "mf-nvidia" => EncoderKind::Mf(Vendor::Nvidia),
        "mf-amd" => EncoderKind::Mf(Vendor::Amd),
        "mf-intel" => EncoderKind::Mf(Vendor::Intel),
        "mf-other" => EncoderKind::Mf(Vendor::Other),
        "nvenc" => EncoderKind::Nvenc,
        "openh264" => EncoderKind::OpenH264,
        _ => return None,
    })
}

pub fn codec_id(c: Codec) -> &'static str {
    match c {
        Codec::H264 => "h264",
        Codec::Hevc => "hevc",
        Codec::Av1 => "av1",
    }
}

pub fn codec_from_id(s: &str) -> Option<Codec> {
    Some(match s {
        "h264" => Codec::H264,
        "hevc" => Codec::Hevc,
        "av1" => Codec::Av1,
        _ => return None,
    })
}

pub fn codec_label(c: Codec) -> &'static str {
    match c {
        Codec::H264 => "H.264",
        Codec::Hevc => "HEVC",
        Codec::Av1 => "AV1",
    }
}

pub fn format_id(f: PixelFormat) -> &'static str {
    match f {
        PixelFormat::Yuv420 => "420",
        PixelFormat::Yuv420P10 => "420p10",
        PixelFormat::Yuv444 => "444",
        PixelFormat::Yuv444P10 => "444p10",
    }
}

pub fn format_from_id(s: &str) -> Option<PixelFormat> {
    Some(match s {
        "420" => PixelFormat::Yuv420,
        "420p10" => PixelFormat::Yuv420P10,
        "444" => PixelFormat::Yuv444,
        "444p10" => PixelFormat::Yuv444P10,
        _ => return None,
    })
}

#[cfg(test)]
#[path = "probe_tests.rs"]
mod tests;
