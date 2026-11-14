//! Choix de l'encodeur (logique pure, testable sans GPU).
//!
//! `run --encoder auto|nvidia|amd|intel|software`. En `auto`, seuls les encodeurs de
//! l'**adaptateur de l'écran capturé** sont candidats (les textures capturées y vivent : un MFT
//! d'un autre GPU exigerait une copie entre adaptateurs), vendeur de l'adaptateur d'abord ; puis
//! l'encodeur logiciel OpenH264 (H.264 4:2:0 8 bits) s'il est installé. Le 4:4:4 passe par l'API
//! NVENC directe (NVIDIA seulement) ; les autres formats par les MFT matériels.

use crate::{Codec, PixelFormat};

/// Vendeur d'un adaptateur / d'un encodeur.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    Other,
    /// Encodeur logiciel (CPU).
    Software,
}

impl Vendor {
    /// Depuis l'identifiant PCI (`DXGI_ADAPTER_DESC1::VendorId`).
    pub fn from_pci(id: u32) -> Vendor {
        match id {
            0x10DE => Vendor::Nvidia,
            0x1002 | 0x1022 => Vendor::Amd,
            0x8086 => Vendor::Intel,
            _ => Vendor::Other,
        }
    }

    /// Depuis l'attribut `MFT_ENUM_HARDWARE_VENDOR_ID_Attribute` (« VEN_10DE ») ou, à défaut,
    /// le nom convivial du MFT.
    pub fn from_mft(vendor_attr: Option<&str>, name: &str) -> Vendor {
        if let Some(id) = vendor_attr
            .and_then(|s| s.to_ascii_uppercase().split("VEN_").nth(1).map(|h| h.chars().take(4).collect::<String>()))
            .and_then(|h| u32::from_str_radix(&h, 16).ok())
        {
            return Vendor::from_pci(id);
        }
        let n = name.to_ascii_lowercase();
        if n.contains("nvidia") {
            Vendor::Nvidia
        } else if n.contains("amd") || n.contains("amf") || n.contains("radeon") {
            Vendor::Amd
        } else if n.contains("intel") || n.contains("quick sync") || n.contains("qsv") {
            Vendor::Intel
        } else {
            Vendor::Other
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Vendor::Nvidia => "NVIDIA",
            Vendor::Amd => "AMD",
            Vendor::Intel => "Intel",
            Vendor::Other => "autre",
            Vendor::Software => "logiciel",
        }
    }
}

/// `run --encoder`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum EncoderPref {
    #[default]
    Auto,
    Nvidia,
    Amd,
    Intel,
    Software,
}

impl EncoderPref {
    pub fn parse(s: &str) -> Result<EncoderPref, String> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "auto" => EncoderPref::Auto,
            "nvidia" | "nvenc" => EncoderPref::Nvidia,
            "amd" | "amf" => EncoderPref::Amd,
            "intel" | "qsv" | "quicksync" => EncoderPref::Intel,
            "software" | "logiciel" | "openh264" => EncoderPref::Software,
            o => return Err(format!("encodeur « {o} » inconnu (auto, nvidia, amd, intel, software)")),
        })
    }

    pub fn vendor(self) -> Option<Vendor> {
        match self {
            EncoderPref::Auto => None,
            EncoderPref::Nvidia => Some(Vendor::Nvidia),
            EncoderPref::Amd => Some(Vendor::Amd),
            EncoderPref::Intel => Some(Vendor::Intel),
            EncoderPref::Software => Some(Vendor::Software),
        }
    }
}

/// Une voie d'encodage à essayer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Route {
    /// MFT matériel n° `i` de la liste énumérée sur l'adaptateur.
    Mf(usize),
    /// API NVENC directe.
    Nvenc,
    /// OpenH264 (CPU).
    OpenH264,
}

/// Voies à essayer, dans l'ordre.
///
/// * `adapter` : vendeur de l'adaptateur de l'écran capturé ;
/// * `mfts` : vendeurs des MFT matériels énumérés sur cet adaptateur pour `codec` ;
/// * `nvenc_444` : NVENC direct sait encoder ce codec en 4:4:4 (et 10 bits si demandé) ;
/// * `openh264` : la DLL OpenH264 de Cisco est installée.
pub fn plan_routes(
    pref: EncoderPref,
    codec: Codec,
    format: PixelFormat,
    adapter: Vendor,
    mfts: &[Vendor],
    nvenc_444: bool,
    openh264: bool,
) -> Vec<Route> {
    let soft_ok = openh264 && codec == Codec::H264 && format == PixelFormat::Yuv420;
    if format.is_444() {
        let nv = nvenc_444 && adapter == Vendor::Nvidia && matches!(pref, EncoderPref::Auto | EncoderPref::Nvidia);
        return if nv { vec![Route::Nvenc] } else { Vec::new() };
    }
    match pref {
        EncoderPref::Software => {
            if soft_ok {
                vec![Route::OpenH264]
            } else {
                Vec::new()
            }
        }
        EncoderPref::Auto => {
            let mut v: Vec<Route> = (0..mfts.len()).filter(|&i| mfts[i] == adapter).map(Route::Mf).collect();
            v.extend((0..mfts.len()).filter(|&i| mfts[i] != adapter).map(Route::Mf));
            if soft_ok {
                v.push(Route::OpenH264);
            }
            v
        }
        p => {
            let want = p.vendor();
            (0..mfts.len()).filter(|&i| Some(mfts[i]) == want).map(Route::Mf).collect()
        }
    }
}

/// Résolution de l'encodeur logiciel selon le nombre de cœurs logiques : la source est réduite
/// (sur le GPU) pour tenir 60 img/s sans saturer le CPU. Dimensions paires, rapport conservé,
/// jamais d'agrandissement.
pub fn soft_size(src_w: u32, src_h: u32, cores: usize) -> (u32, u32) {
    let (max_w, max_h) = match cores {
        c if c >= 12 => (1920, 1080),
        c if c >= 6 => (1600, 900),
        c if c >= 4 => (1280, 720),
        _ => (960, 540),
    };
    if src_w == 0 || src_h == 0 {
        return (2, 2);
    }
    let s = (max_w as f64 / src_w as f64).min(max_h as f64 / src_h as f64).min(1.0);
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    (even(src_w as f64 * s), even(src_h as f64 * s))
}

/// Débit maximal de l'encodeur logiciel (kbit/s) : au-delà, le coût CPU explose sans gain visible
/// à ces résolutions.
pub fn soft_max_kbps(w: u32, h: u32) -> u32 {
    let px = w as u64 * h as u64;
    ((px * 6 / 1000) as u32).clamp(2_000, 15_000)
}

#[cfg(test)]
#[path = "select_tests.rs"]
mod tests;
