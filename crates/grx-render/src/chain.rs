//! Chaîne de repli des décodeurs (logique pure, testable sans GPU).
//!
//! Pour chaque codec on essaie dans l'ordre [`DecoderKind::CHAIN`] : Media Foundation (MFT
//! matériel ; HEVC/AV1 via les extensions du Store) puis FFmpeg D3D11VA (DLL LGPL facultatives).
//! Si tout échoue, [`ChainError`] décrit chaque tentative et les solutions ; le client annonce
//! alors le codec comme indisponible et le host passe en H.264.
//!
//! Phase 2 : voies logicielles de secours ([`DecoderKind::OpenH264`] pour H.264,
//! [`DecoderKind::FfmpegSoftware`] pour HEVC/AV1/4:4:4) après les voies matérielles, et choix de
//! l'utilisateur ([`DecoderPref`] : Auto = matériel puis logiciel, Matériel, Logiciel).

use grx_video::Codec;

/// Voie de décodage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DecoderKind {
    /// MFT Media Foundation piloté en D3D11 (DXVA, zéro copie).
    MediaFoundation,
    /// libavcodec + hwaccel D3D11VA (zéro copie, DLL LGPL chargées dynamiquement).
    FfmpegD3d11va,
    /// OpenH264 de Cisco (CPU, H.264 4:2:0 8 bits), image téléversée sur le GPU.
    OpenH264,
    /// libavcodec sans hwaccel (CPU : H.264/HEVC/AV1, 4:2:0/4:4:4, 8/10 bits).
    FfmpegSoftware,
}

/// Choix de l'utilisateur (`play --decoder`, ligne « Décodeur » de l'overlay).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DecoderPref {
    /// Matériel d'abord, logiciel en secours.
    #[default]
    Auto,
    Hardware,
    Software,
}

impl DecoderPref {
    pub fn parse(s: &str) -> Result<DecoderPref, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(DecoderPref::Auto),
            "hardware" | "materiel" | "matériel" | "hw" => Ok(DecoderPref::Hardware),
            "software" | "logiciel" | "sw" => Ok(DecoderPref::Software),
            o => Err(format!("décodeur « {o} » inconnu (auto, hardware, software)")),
        }
    }

    /// Voies essayées, dans l'ordre.
    pub fn chain(self) -> &'static [DecoderKind] {
        match self {
            DecoderPref::Auto => &DecoderKind::ALL,
            DecoderPref::Hardware => &DecoderKind::CHAIN,
            DecoderPref::Software => &DecoderKind::SOFTWARE_CHAIN,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DecoderPref::Auto => "Auto",
            DecoderPref::Hardware => "Matériel",
            DecoderPref::Software => "Logiciel",
        }
    }
}

impl DecoderKind {
    /// Voies matérielles, dans l'ordre d'essai (comportement phase 1).
    pub const CHAIN: [DecoderKind; 2] = [DecoderKind::MediaFoundation, DecoderKind::FfmpegD3d11va];
    /// Voies logicielles (secours).
    pub const SOFTWARE_CHAIN: [DecoderKind; 2] = [DecoderKind::OpenH264, DecoderKind::FfmpegSoftware];
    /// Toutes les voies : matérielles puis logicielles.
    pub const ALL: [DecoderKind; 4] =
        [DecoderKind::MediaFoundation, DecoderKind::FfmpegD3d11va, DecoderKind::OpenH264, DecoderKind::FfmpegSoftware];

    pub fn is_software(self) -> bool {
        matches!(self, DecoderKind::OpenH264 | DecoderKind::FfmpegSoftware)
    }

    /// Nom court des options (`--disable-decoder mf-hevc`).
    pub fn key(self) -> &'static str {
        match self {
            DecoderKind::MediaFoundation => "mf",
            DecoderKind::FfmpegD3d11va => "ffmpeg",
            DecoderKind::OpenH264 => "openh264",
            DecoderKind::FfmpegSoftware => "ffsw",
        }
    }

    /// Libellé affiché (overlay, journaux).
    pub fn label(self) -> &'static str {
        match self {
            DecoderKind::MediaFoundation => "MF matériel",
            DecoderKind::FfmpegD3d11va => "FFmpeg D3D11VA",
            DecoderKind::OpenH264 => "OpenH264 (logiciel)",
            DecoderKind::FfmpegSoftware => "FFmpeg logiciel",
        }
    }

    /// Identifiant de backend du protocole de négociation (`grx_core::codec_neg::BackendId`).
    pub fn backend_id(self) -> u8 {
        match self {
            DecoderKind::MediaFoundation => 1,
            DecoderKind::FfmpegD3d11va => 2,
            DecoderKind::OpenH264 | DecoderKind::FfmpegSoftware => 3,
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
            let kind_of = |k: &str| DecoderKind::ALL.into_iter().find(|d| d.key() == k);
            let codec_of = |k: &str| [Codec::H264, Codec::Hevc, Codec::Av1].into_iter().find(|c| codec_key(*c) == k);
            let e = match tok.split_once('-') {
                Some((k, c)) => match (kind_of(k), codec_of(c)) {
                    (Some(k), Some(c)) => (Some(k), Some(c)),
                    _ => return Err(format!("« {tok} » : attendu mf|ffmpeg|openh264|ffsw[-h264|-hevc|-av1] ou h264|hevc|av1")),
                },
                None => match (kind_of(&tok), codec_of(&tok)) {
                    (Some(k), _) => (Some(k), None),
                    (_, Some(c)) => (None, Some(c)),
                    _ => return Err(format!("« {tok} » : attendu mf|ffmpeg|openh264|ffsw[-h264|-hevc|-av1] ou h264|hevc|av1")),
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
        let sw = !self.attempts.is_empty() && self.attempts.iter().all(|a| a.kind.is_software());
        write!(f, "aucun décodeur {} {} utilisable sur ce PC", if sw { "logiciel" } else { "matériel" }, codec_label(self.codec))?;
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
    make: impl FnMut(DecoderKind) -> Result<T, String>,
) -> Result<(T, DecoderKind, Vec<Attempt>), ChainError> {
    create_with_pref(codec, disabled, DecoderPref::Hardware, make)
}

/// Identifiant PCI d'Intel (`DXGI_ADAPTER_DESC.VendorId`).
pub const VENDOR_INTEL: u32 = 0x8086;

/// Ordre des voies pour l'adaptateur `vendor_id` : sur un GPU Intel, FFmpeg D3D11VA passe AVANT
/// Media Foundation (le MFT de Microsoft y sature le moteur Video Processing, voie suivie par
/// Moonlight) ; MF reste en repli. Ailleurs (NVIDIA, AMD, inconnu) : ordre inchangé.
pub fn order_for_vendor(kinds: &mut [DecoderKind], vendor_id: u32) {
    if vendor_id != VENDOR_INTEL {
        return;
    }
    let mf = kinds.iter().position(|k| *k == DecoderKind::MediaFoundation);
    let ff = kinds.iter().position(|k| *k == DecoderKind::FfmpegD3d11va);
    if let (Some(m), Some(f)) = (mf, ff) {
        if f > m {
            kinds[m..=f].rotate_right(1);
        }
    }
}

/// Comme [`create_with`], sur les voies de `pref` ([`DecoderPref::chain`]).
pub fn create_with_pref<T>(
    codec: Codec,
    disabled: &DisabledDecoders,
    pref: DecoderPref,
    make: impl FnMut(DecoderKind) -> Result<T, String>,
) -> Result<(T, DecoderKind, Vec<Attempt>), ChainError> {
    create_with_pref_vendor(codec, disabled, pref, 0, make)
}

/// Comme [`create_with_pref`], ordre adapté au fabricant du GPU ([`order_for_vendor`]).
pub fn create_with_pref_vendor<T>(
    codec: Codec,
    disabled: &DisabledDecoders,
    pref: DecoderPref,
    vendor_id: u32,
    mut make: impl FnMut(DecoderKind) -> Result<T, String>,
) -> Result<(T, DecoderKind, Vec<Attempt>), ChainError> {
    let mut attempts = Vec::new();
    let mut order = pref.chain().to_vec();
    order_for_vendor(&mut order, vendor_id);
    for kind in order {
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

#[cfg(test)]
#[path = "chain_pref_tests.rs"]
mod pref_tests;

#[cfg(test)]
#[path = "chain_vendor_tests.rs"]
mod vendor_tests;
