//! Gestion des informations DisplayConfig pour les sorties actives.
//! Associe les noms GDI des sorties aux noms conviviaux des moniteurs et à leur technologie.

#![cfg(windows)]

use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, WIN32_ERROR};
use std::collections::HashMap;
use std::mem::size_of;

/// Informations DisplayConfig d'une source GDI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MonitorName {
    /// Nom convivial du moniteur (`monitorFriendlyDeviceName`), vide si inconnu.
    pub friendly: String,
    /// Sortie indirecte/virtuelle (DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED ou _INDIRECT_VIRTUAL, ou nom d'adaptateur/ moniteur typique d'un écran virtuel ne compte PAS ici : seulement la technologie).
    pub is_virtual: bool,
}

/// Table nom GDI (ex. "\\.\DISPLAY1") -> MonitorName pour les chemins ACTIFS.
/// Ne panique jamais : en cas d'erreur d'API, renvoie une table vide.
pub fn monitor_names() -> HashMap<String, MonitorName> {
    let mut npaths = 0u32;
    let mut nmodes = 0u32;

    // 1. Allocation et requête des chemins actifs
    let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> = Vec::new();
    let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> = Vec::new();
    let mut query_success = false;

    // Tentatives max pour QueryDisplayConfig (gestion de ERROR_INSUFFICIENT_BUFFER)
    for _ in 0..3 {
        // Tailles nécessaires (relues à chaque essai : la configuration a pu changer entre-temps).
        // SAFETY : pointeurs vers des u32 locaux valides.
        if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut npaths, &mut nmodes) } != ERROR_SUCCESS {
            return HashMap::new();
        }
        paths.resize(npaths as usize, DISPLAYCONFIG_PATH_INFO::default());
        modes.resize(nmodes as usize, DISPLAYCONFIG_MODE_INFO::default());

        let err: WIN32_ERROR = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut npaths,
                paths.as_mut_ptr(),
                &mut nmodes,
                modes.as_mut_ptr(),
                None,
            )
        };

        if err == ERROR_SUCCESS {
            query_success = true;
            break;
        }
        if err != ERROR_INSUFFICIENT_BUFFER {
            break;
        }
    }

    if !query_success {
        return HashMap::new();
    }

    // Tronquer aux tailles réelles retournées par l'API
    paths.truncate(npaths as usize);
    modes.truncate(nmodes as usize);

    // 2. Extraction des noms pour chaque chemin
    let mut result = HashMap::new();

    for path in &paths {
        // Nom GDI de la source
        let mut src_name = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        src_name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        src_name.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
        src_name.header.adapterId = path.sourceInfo.adapterId;
        src_name.header.id = path.sourceInfo.id;

        // SAFETY: L'API DisplayConfigGetDeviceInfo initialise correctement la structure passée en paramètre.
        let _ = unsafe { DisplayConfigGetDeviceInfo(&mut src_name.header as *mut _) };
        let gdi_name = utf16z(&src_name.viewGdiDeviceName);

        if gdi_name.is_empty() {
            continue;
        }

        // Nom convivial et technologie de la cible
        let mut tgt_name = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
        tgt_name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
        tgt_name.header.size = size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
        tgt_name.header.adapterId = path.targetInfo.adapterId;
        tgt_name.header.id = path.targetInfo.id;

        // SAFETY: L'API DisplayConfigGetDeviceInfo initialise correctement la structure passée en paramètre.
        let _ = unsafe { DisplayConfigGetDeviceInfo(&mut tgt_name.header as *mut _) };
        let friendly = utf16z(&tgt_name.monitorFriendlyDeviceName);
        let is_virtual = path.targetInfo.outputTechnology == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED
            || path.targetInfo.outputTechnology == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_VIRTUAL;

        // Garder le premier nom convivial non vide pour une source donnée
        // (une source clonée sur plusieurs cibles apparaît dans plusieurs chemins).
        let e: &mut MonitorName = result.entry(gdi_name).or_default();
        if e.friendly.is_empty() {
            e.friendly = friendly;
        }
        e.is_virtual |= is_virtual;
    }

    result
}

/// Convertit un tampon UTF-16 terminé par un caractère nul en `String`.
fn utf16z(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len]).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_utf16z() {
        // Avec caractère nul au milieu
        let buf1: Vec<u16> = "ABC\0xyz".encode_utf16().collect();
        assert_eq!(utf16z(&buf1), "ABC");

        // Sans caractère nul
        let buf2: Vec<u16> = "ABC".encode_utf16().collect();
        assert_eq!(utf16z(&buf2), "ABC");

        // Vide
        let buf3: Vec<u16> = vec![];
        assert_eq!(utf16z(&buf3), "");

        // Espaces à trimmer
        let buf4: Vec<u16> = "  ABC  ".encode_utf16().collect();
        assert_eq!(utf16z(&buf4), "ABC");
    }

    #[test]
    fn test_monitor_names_no_panic_and_valid_keys() {
        let names = monitor_names();
        // Vérifie que la fonction ne panique pas
        // Vérifie que si des entrées existent, les clés commencent par "\\.\\"
        for key in names.keys() {
            assert!(key.starts_with(r"\\.\"), "clé GDI inattendue : {key}");
        }
        // `cargo test -- --nocapture` : liste réelle des écrans capturables.
        for o in crate::list_displays().unwrap_or_default() {
            println!("{o:?}");
        }
    }
}
