//! Chaîne de repli des décodeurs (logique pure, testable sans GPU).
//!
//! Pour chaque codec on essaie dans l'ordre [`DecoderKind::CHAIN`] : Media Foundation (MFT
//! matériel ; HEVC/AV1 via les extensions du Store) puis FFmpeg D3D11VA (DLL LGPL facultatives).
//! Si tout échoue, [`ChainError`] décrit chaque tentative et les solutions ; le client annonce
//! alors le codec comme indisponible et le host passe en H.264.
//!
//! Point d'extension (phase 2) : ajouter un variant (décodeur logiciel…) à [`DecoderKind`] et à
//! [`DecoderKind::CHAIN`], et son identifiant de protocole dans [`DecoderKind::backend_id`].

use grx_video::Codec;

/// Voie de décodage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DecoderKind {
    /// MFT Media Foundation piloté en D3D11 (DXVA, zéro copie).
    MediaFoundation,
    /// libavcodec + hwaccel D3D11VA (zéro copie, DLL LGPL chargées dynamiquement).
    FfmpegD3d11va,
}

impl DecoderKind {
    /// Ordre d'essai.
    pub const CHAIN: [DecoderKind; 2] = [DecoderKind::MediaFoundation, DecoderKind::FfmpegD3d11va];

    /// Nom court des options (`--disable-decoder mf-hevc`).
    pub fn key(self) -> &'static str {
        match self {
            DecoderKind::MediaFoundation => "mf",
            DecoderKind::FfmpegD3d11va => "ffmpeg",
        }
    }

    /// Libellé affiché (overlay, journaux).
    pub fn label(self) -> &'static str {
        match self {
            DecoderKind::MediaFoundation => "MF matériel",
            DecoderKind::FfmpegD3d11va => "FFmpeg D3D11VA",
        }
    }

    /// Identifiant de backend du protocole de négociation (`grx_core::codec_neg::BackendId`).
    pub fn backend_id(self) -> u8 {
        match self {
            DecoderKind::MediaFoundation => 1,
            DecoderKind::FfmpegD3d11va => 2,
        }
    }
}

/// Nom court d'un codec (`h264`, `hevc`, `av1`).
pub fn codec_key(c: Codec) -> &'static str {
    match c {
        Codec::H264 => "h264",
        Codec::Hevc => "hevc",
        Codec::Av1 => "av1",
    }
}

fn codec_label(c: Codec) -> &'static str {
    match c {
        Codec::H264 => "H.264",
        Codec::Hevc => "HEVC",
        Codec::Av1 => "AV1",
    }
}

/// Voies désactivées (option cachée de test `--disable-decoder`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct DisabledDecoders {
    entries: Vec<(Option<DecoderKind>, Option<Codec>)>,
}

impl DisabledDecoders {
    /// Liste séparée par des virgules : `mf-hevc`, `ffmpeg` (toutes les voies FFmpeg), `av1`
    /// (toutes les voies de ce codec), `ffmpeg-h264`… Chaîne vide ou `none` : rien.
    pub fn parse(s: &str) -> Result<DisabledDecoders, String> {
        let mut entries = Vec::new();
        for tok in s.split(',').map(|t| t.trim().to_ascii_lowercase()).filter(|t| !t.is_empty() && t != "none") {
            let kind_of = |k: &str| DecoderKind::CHAIN.into_iter().find(|d| d.key() == k);
            let codec_of = |k: &str| [Codec::H264, Codec::Hevc, Codec::Av1].into_iter().find(|c| codec_key(*c) == k);
            let e = match tok.split_once('-') {
                Some((k, c)) => match (kind_of(k), codec_of(c)) {
                    (Some(k), Some(c)) => (Some(k), Some(c)),
                    _ => return Err(format!("« {tok} » : attendu mf|ffmpeg[-h264|-hevc|-av1] ou h264|hevc|av1")),
                },
                None => match (kind_of(&tok), codec_of(&tok)) {
                    (Some(k), _) => (Some(k), None),
                    (_, Some(c)) => (None, Some(c)),
                    _ => return Err(format!("« {tok} » : attendu mf|ffmpeg[-h264|-hevc|-av1] ou h264|hevc|av1")),
                },
            };
            entries.push(e);
        }
        Ok(DisabledDecoders { entries })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn is_disabled(&self, kind: DecoderKind, codec: Codec) -> bool {
        self.entries.iter().any(|(k, c)| k.is_none_or(|k| k == kind) && c.is_none_or(|c| c == codec))
    }
}

/// Une tentative ratée.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub kind: DecoderKind,
    pub error: String,
}

/// Aucune voie n'a pu créer de décodeur pour ce codec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainError {
    pub codec: Codec,
    pub attempts: Vec<Attempt>,
}

impl ChainError {
    /// Causes des échecs, par voie (« MF matériel : … ; FFmpeg D3D11VA : … »).
    pub fn causes(&self) -> String {
        self.attempts.iter().map(|a| format!("{} : {}", a.kind.label(), a.error)).collect::<Vec<_>>().join(" ; ")
    }

    /// Solutions à proposer à l'utilisateur.
    pub fn advice(&self) -> String {
        let mut v = Vec::new();
        match self.codec {
            Codec::Hevc => v.push(
                "installez l'extension gratuite « HEVC Video Extensions from Device Manufacturer » (Microsoft Store)".to_string(),
            ),
            Codec::Av1 => v.push("installez l'extension gratuite « AV1 Video Extension » (Microsoft Store)".to_string()),
            Codec::H264 => v.push("mettez à jour le pilote graphique".to_string()),
        }
        if self.codec != Codec::H264 {
            v.push("ou placez les DLL FFmpeg LGPL (avcodec-62, avutil-60, swresample-6) à côté de grx-client.exe".into());
            v.push("ou laissez la négociation automatique choisir H.264 (`play --codec auto`, host en `--codec auto`)".into());
        }
        v.join(" ; ")
    }
}

impl std::fmt::Display for ChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "aucun décodeur matériel {} utilisable sur ce PC", codec_label(self.codec))?;
        if !self.attempts.is_empty() {
            let d: Vec<String> = self.attempts.iter().map(|a| format!("{} : {}", a.kind.label(), a.error)).collect();
            write!(f, " ({})", d.join(" ; "))?;
        }
        write!(f, ". Solutions : {}.", self.advice())
    }
}

impl std::error::Error for ChainError {}

/// Essaie chaque voie de [`DecoderKind::CHAIN`] (sauf les désactivées) avec `make` ; renvoie le
/// premier décodeur créé, sa voie et les échecs précédents.
pub fn create_with<T>(
    codec: Codec,
    disabled: &DisabledDecoders,
    mut make: impl FnMut(DecoderKind) -> Result<T, String>,
) -> Result<(T, DecoderKind, Vec<Attempt>), ChainError> {
    let mut attempts = Vec::new();
    for kind in DecoderKind::CHAIN {
        if disabled.is_disabled(kind, codec) {
            attempts.push(Attempt { kind, error: "désactivé (--disable-decoder)".into() });
            continue;
        }
        match make(kind) {
            Ok(t) => return Ok((t, kind, attempts)),
            Err(error) => attempts.push(Attempt { kind, error }),
        }
    }
    Err(ChainError { codec, attempts })
}

#[cfg(test)]
#[path = "chain_tests.rs"]
mod tests;
