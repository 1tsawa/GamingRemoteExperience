//! Conversion VK <-> scancode (set 1 / PS/2) cote client.
//!
//! Principe : on transmet des **scancodes physiques**, pas des VK, pour que la touche
//! envoyee soit celle que le joueur presse physiquement (un joueur AZERTY presse la touche
//! en position "W" du clavier US, le host l'injecte telle quelle et c'est le layout du
//! host qui decide du caractere). Sources de scancodes, par ordre de preference :
//!
//! 1. `lParam` de `WM_KEYDOWN`/`WM_KEYUP` ([`scancode_from_lparam`]) : exact, layout inclus.
//! 2. Raw Input `RAWKEYBOARD` ([`key_from_raw`]) : `MakeCode` + `RI_KEY_E0`.
//! 3. Une valeur winit/`PhysicalKeyExtScancode` ([`scancode_from_extended_u32`]).
//! 4. A defaut un VK seul : [`vk_to_scancode`] (table pure, touches independantes du layout)
//!    puis, sous Windows, [`vk_to_scancode_layout`] (`MapVirtualKeyExW`, layout courant).
//!
//! Limite : la touche Pause (sequence E1 1D 45) n'est pas representable et est ignoree.

use crate::event::{is_valid_scancode, InputEvent};

/// Constantes VK utiles (valeurs `VK_*` de winuser.h), definies ici pour rester pures.
pub mod vk {
    pub const BACK: u16 = 0x08;
    pub const TAB: u16 = 0x09;
    pub const RETURN: u16 = 0x0D;
    pub const SHIFT: u16 = 0x10;
    pub const CONTROL: u16 = 0x11;
    pub const MENU: u16 = 0x12;
    pub const PAUSE: u16 = 0x13;
    pub const CAPITAL: u16 = 0x14;
    pub const ESCAPE: u16 = 0x1B;
    pub const SPACE: u16 = 0x20;
    pub const PRIOR: u16 = 0x21;
    pub const NEXT: u16 = 0x22;
    pub const END: u16 = 0x23;
    pub const HOME: u16 = 0x24;
    pub const LEFT: u16 = 0x25;
    pub const UP: u16 = 0x26;
    pub const RIGHT: u16 = 0x27;
    pub const DOWN: u16 = 0x28;
    pub const SNAPSHOT: u16 = 0x2C;
    pub const INSERT: u16 = 0x2D;
    pub const DELETE: u16 = 0x2E;
    pub const LWIN: u16 = 0x5B;
    pub const RWIN: u16 = 0x5C;
    pub const APPS: u16 = 0x5D;
    pub const NUMPAD0: u16 = 0x60;
    pub const NUMPAD9: u16 = 0x69;
    pub const MULTIPLY: u16 = 0x6A;
    pub const ADD: u16 = 0x6B;
    pub const SUBTRACT: u16 = 0x6D;
    pub const DECIMAL: u16 = 0x6E;
    pub const DIVIDE: u16 = 0x6F;
    pub const F1: u16 = 0x70;
    pub const F10: u16 = 0x79;
    pub const F11: u16 = 0x7A;
    pub const F12: u16 = 0x7B;
    pub const F13: u16 = 0x7C;
    pub const F23: u16 = 0x86;
    pub const F24: u16 = 0x87;
    pub const NUMLOCK: u16 = 0x90;
    pub const SCROLL: u16 = 0x91;
    pub const LSHIFT: u16 = 0xA0;
    pub const RSHIFT: u16 = 0xA1;
    pub const LCONTROL: u16 = 0xA2;
    pub const RCONTROL: u16 = 0xA3;
    pub const LMENU: u16 = 0xA4;
    pub const RMENU: u16 = 0xA5;
    pub const VOLUME_MUTE: u16 = 0xAD;
    pub const VOLUME_DOWN: u16 = 0xAE;
    pub const VOLUME_UP: u16 = 0xAF;
    pub const MEDIA_NEXT_TRACK: u16 = 0xB0;
    pub const MEDIA_PREV_TRACK: u16 = 0xB1;
    pub const MEDIA_STOP: u16 = 0xB2;
    pub const MEDIA_PLAY_PAUSE: u16 = 0xB3;
}

/// Table (VK, scancode, etendu) des touches dont la position physique ne depend pas du layout.
/// Les lettres, chiffres et touches OEM en sont volontairement absents.
const FIXED_TABLE: &[(u16, u8, bool)] = &[
    (vk::ESCAPE, 0x01, false),
    (vk::BACK, 0x0E, false),
    (vk::TAB, 0x0F, false),
    (vk::RETURN, 0x1C, false), // Entree du pave numerique : meme scancode + flag etendu (lParam/raw)
    (vk::SPACE, 0x39, false),
    (vk::CAPITAL, 0x3A, false),
    (vk::SHIFT, 0x2A, false),
    (vk::CONTROL, 0x1D, false),
    (vk::MENU, 0x38, false),
    (vk::LSHIFT, 0x2A, false),
    (vk::RSHIFT, 0x36, false),
    (vk::LCONTROL, 0x1D, false),
    (vk::RCONTROL, 0x1D, true),
    (vk::LMENU, 0x38, false),
    (vk::RMENU, 0x38, true),
    (vk::LWIN, 0x5B, true),
    (vk::RWIN, 0x5C, true),
    (vk::APPS, 0x5D, true),
    (vk::NUMLOCK, 0x45, false),
    (vk::SCROLL, 0x46, false),
    (vk::SNAPSHOT, 0x37, true),
    // bloc de navigation (tous etendus)
    (vk::INSERT, 0x52, true),
    (vk::DELETE, 0x53, true),
    (vk::HOME, 0x47, true),
    (vk::END, 0x4F, true),
    (vk::PRIOR, 0x49, true),
    (vk::NEXT, 0x51, true),
    (vk::LEFT, 0x4B, true),
    (vk::UP, 0x48, true),
    (vk::RIGHT, 0x4D, true),
    (vk::DOWN, 0x50, true),
    // pave numerique
    (vk::NUMPAD0, 0x52, false),
    (vk::NUMPAD0 + 1, 0x4F, false),
    (vk::NUMPAD0 + 2, 0x50, false),
    (vk::NUMPAD0 + 3, 0x51, false),
    (vk::NUMPAD0 + 4, 0x4B, false),
    (vk::NUMPAD0 + 5, 0x4C, false),
    (vk::NUMPAD0 + 6, 0x4D, false),
    (vk::NUMPAD0 + 7, 0x47, false),
    (vk::NUMPAD0 + 8, 0x48, false),
    (vk::NUMPAD9, 0x49, false),
    (vk::MULTIPLY, 0x37, false),
    (vk::ADD, 0x4E, false),
    (vk::SUBTRACT, 0x4A, false),
    (vk::DECIMAL, 0x53, false),
    (vk::DIVIDE, 0x35, true),
    // F1..F10
    (vk::F1, 0x3B, false),
    (vk::F1 + 1, 0x3C, false),
    (vk::F1 + 2, 0x3D, false),
    (vk::F1 + 3, 0x3E, false),
    (vk::F1 + 4, 0x3F, false),
    (vk::F1 + 5, 0x40, false),
    (vk::F1 + 6, 0x41, false),
    (vk::F1 + 7, 0x42, false),
    (vk::F1 + 8, 0x43, false),
    (vk::F10, 0x44, false),
    (vk::F11, 0x57, false),
    (vk::F12, 0x58, false),
    // F13..F24 (0x64..0x6E puis 0x76)
    (vk::F13, 0x64, false),
    (vk::F13 + 1, 0x65, false),
    (vk::F13 + 2, 0x66, false),
    (vk::F13 + 3, 0x67, false),
    (vk::F13 + 4, 0x68, false),
    (vk::F13 + 5, 0x69, false),
    (vk::F13 + 6, 0x6A, false),
    (vk::F13 + 7, 0x6B, false),
    (vk::F13 + 8, 0x6C, false),
    (vk::F13 + 9, 0x6D, false),
    (vk::F23, 0x6E, false),
    (vk::F24, 0x76, false),
    // multimedia (etendus)
    (vk::VOLUME_MUTE, 0x20, true),
    (vk::VOLUME_DOWN, 0x2E, true),
    (vk::VOLUME_UP, 0x30, true),
    (vk::MEDIA_NEXT_TRACK, 0x19, true),
    (vk::MEDIA_PREV_TRACK, 0x10, true),
    (vk::MEDIA_STOP, 0x24, true),
    (vk::MEDIA_PLAY_PAUSE, 0x22, true),
];

/// Table pure VK -> (scancode, etendu) pour les touches independantes du layout.
/// `None` pour les lettres/chiffres/OEM (utiliser [`vk_to_scancode_layout`]) et les VK inconnus.
pub fn vk_to_scancode(vk: u16) -> Option<(u8, bool)> {
    FIXED_TABLE.iter().find(|e| e.0 == vk).map(|e| (e.1, e.2))
}

/// Table inverse (scancode, etendu) -> VK, pour les touches fixes. Utile aux tests et au debug.
/// Pour les doublons (VK generique vs gauche/droite) le VK specifique est prefere.
pub fn scancode_to_vk(scancode: u8, extended: bool) -> Option<u16> {
    FIXED_TABLE
        .iter()
        .filter(|e| e.1 == scancode && e.2 == extended)
        .map(|e| e.0)
        .find(|v| !matches!(*v, vk::SHIFT | vk::CONTROL | vk::MENU))
}

/// Cree un `Key` en validant le scancode.
fn key(scancode: u8, extended: bool, pressed: bool) -> Option<InputEvent> {
    is_valid_scancode(scancode).then_some(InputEvent::Key { scancode, extended, pressed })
}

/// Extrait (scancode, etendu) du `lParam` de `WM_KEYDOWN`/`WM_KEYUP`/`WM_SYSKEY*`
/// (bits 16-23 = scancode, bit 24 = etendu).
pub fn scancode_from_lparam(lparam: isize) -> (u8, bool) {
    let l = lparam as usize as u32;
    (((l >> 16) & 0xFF) as u8, (l >> 24) & 1 != 0)
}

/// `WM_KEYDOWN`/`WM_KEYUP` -> `InputEvent::Key`. `pressed` vient du type de message
/// (le bit 31 de lParam n'est pas fiable pour distinguer down/up selon le message).
/// Renvoie `None` si le scancode est nul/invalide (touches virtuelles injectees par IME, etc.).
pub fn key_from_lparam(lparam: isize, pressed: bool) -> Option<InputEvent> {
    let (sc, ext) = scancode_from_lparam(lparam);
    key(sc, ext, pressed)
}

/// Flags de `RAWKEYBOARD.Flags`.
pub const RI_KEY_BREAK: u16 = 0x01;
pub const RI_KEY_E0: u16 = 0x02;
pub const RI_KEY_E1: u16 = 0x04;

/// `RAWKEYBOARD` -> `InputEvent::Key`. Ignore les "fake keys" (MakeCode 0 ou 0xFF),
/// les sequences E1 (Pause) et les VK 0xFF.
pub fn key_from_raw(make_code: u16, flags: u16, vkey: u16) -> Option<InputEvent> {
    if flags & RI_KEY_E1 != 0 || vkey == 0xFF || make_code == 0 || make_code > 0xFF {
        return None;
    }
    key(make_code as u8, flags & RI_KEY_E0 != 0, flags & RI_KEY_BREAK == 0)
}

/// Valeur de type `PhysicalKeyExtScancode::to_scancode()` de winit (`0xE0xx` = etendu) -> `Key`.
pub fn scancode_from_extended_u32(sc: u32) -> Option<(u8, bool)> {
    match sc {
        0x01..=0xFF => is_valid_scancode(sc as u8).then_some((sc as u8, false)),
        0xE001..=0xE0FF => is_valid_scancode((sc & 0xFF) as u8).then_some(((sc & 0xFF) as u8, true)),
        _ => None,
    }
}

/// Pareil, directement sous forme d'evenement.
pub fn key_from_extended_u32(sc: u32, pressed: bool) -> Option<InputEvent> {
    scancode_from_extended_u32(sc).and_then(|(s, e)| key(s, e, pressed))
}

/// VK -> (scancode, etendu) en tenant compte du layout clavier courant
/// (`MapVirtualKeyExW(MAPVK_VK_TO_VSC_EX)`), avec repli sur la table pure.
#[cfg(windows)]
pub fn vk_to_scancode_layout(vk_code: u16) -> Option<(u8, bool)> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardLayout, MapVirtualKeyExW, MAPVK_VK_TO_VSC_EX};
    if let Some(r) = vk_to_scancode(vk_code) {
        return Some(r);
    }
    // SAFETY: appels Win32 sans pointeur ; GetKeyboardLayout(0) = layout du thread courant.
    let sc = unsafe { MapVirtualKeyExW(vk_code as u32, MAPVK_VK_TO_VSC_EX, Some(GetKeyboardLayout(0))) };
    match sc {
        0 => None,
        s if s & 0xFF00 == 0xE000 => is_valid_scancode((s & 0xFF) as u8).then_some(((s & 0xFF) as u8, true)),
        s if s <= 0xFF => is_valid_scancode(s as u8).then_some((s as u8, false)),
        _ => None, // E1xx (Pause)
    }
}

/// Nom lisible (positions US) d'une touche, pour logs/exemples.
pub fn scancode_name(scancode: u8, extended: bool) -> &'static str {
    if extended {
        return match scancode {
            0x1C => "NumpadEnter",
            0x1D => "RightCtrl",
            0x35 => "Numpad/",
            0x37 => "PrintScreen",
            0x38 => "RightAlt",
            0x47 => "Home",
            0x48 => "ArrowUp",
            0x49 => "PageUp",
            0x4B => "ArrowLeft",
            0x4D => "ArrowRight",
            0x4F => "End",
            0x50 => "ArrowDown",
            0x51 => "PageDown",
            0x52 => "Insert",
            0x53 => "Delete",
            0x5B => "LeftWin",
            0x5C => "RightWin",
            0x5D => "Menu",
            0x10 => "MediaPrev",
            0x19 => "MediaNext",
            0x20 => "VolumeMute",
            0x22 => "MediaPlayPause",
            0x24 => "MediaStop",
            0x2E => "VolumeDown",
            0x30 => "VolumeUp",
            _ => "?",
        };
    }
    const NAMES: [&str; 0x59] = [
        "?", "Esc", "1", "2", "3", "4", "5", "6", "7", "8", "9", "0", "-", "=", "Backspace", "Tab", //
        "Q", "W", "E", "R", "T", "Y", "U", "I", "O", "P", "[", "]", "Enter", "LeftCtrl", "A", "S", //
        "D", "F", "G", "H", "J", "K", "L", ";", "'", "`", "LeftShift", "\\", "Z", "X", "C", "V", //
        "B", "N", "M", ",", ".", "/", "RightShift", "Numpad*", "LeftAlt", "Space", "CapsLock", "F1", //
        "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "NumLock", "ScrollLock", "Numpad7", //
        "Numpad8", "Numpad9", "Numpad-", "Numpad4", "Numpad5", "Numpad6", "Numpad+", "Numpad1", //
        "Numpad2", "Numpad3", "Numpad0", "Numpad.", "SysRq", "?", "ISO<>", "F11", "F12",
    ];
    match scancode {
        0x64..=0x6E => ["F13", "F14", "F15", "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23"]
            [(scancode - 0x64) as usize],
        0x76 => "F24",
        s => NAMES.get(s as usize).copied().unwrap_or("?"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_has_no_duplicate_vk() {
        for (i, a) in FIXED_TABLE.iter().enumerate() {
            for b in &FIXED_TABLE[i + 1..] {
                assert_ne!(a.0, b.0, "VK {:#x} en double", a.0);
            }
            assert!(is_valid_scancode(a.1), "scancode invalide pour VK {:#x}", a.0);
        }
    }

    #[test]
    fn known_vk_mappings() {
        assert_eq!(vk_to_scancode(vk::ESCAPE), Some((0x01, false)));
        assert_eq!(vk_to_scancode(vk::LSHIFT), Some((0x2A, false)));
        assert_eq!(vk_to_scancode(vk::RSHIFT), Some((0x36, false)));
        assert_eq!(vk_to_scancode(vk::RCONTROL), Some((0x1D, true)));
        assert_eq!(vk_to_scancode(vk::RMENU), Some((0x38, true)));
        assert_eq!(vk_to_scancode(vk::LEFT), Some((0x4B, true)));
        assert_eq!(vk_to_scancode(vk::NUMPAD0 + 4), Some((0x4B, false)));
        assert_eq!(vk_to_scancode(vk::F1), Some((0x3B, false)));
        assert_eq!(vk_to_scancode(vk::F12), Some((0x58, false)));
        assert_eq!(vk_to_scancode(vk::F24), Some((0x76, false)));
        assert_eq!(vk_to_scancode(vk::DIVIDE), Some((0x35, true)));
        // lettres / chiffres / inconnus : pas dans la table pure
        assert_eq!(vk_to_scancode(b'A' as u16), None);
        assert_eq!(vk_to_scancode(b'1' as u16), None);
        assert_eq!(vk_to_scancode(0), None);
        assert_eq!(vk_to_scancode(0xFFFF), None);
    }

    #[test]
    fn all_f_keys_are_contiguous_and_distinct() {
        let mut seen = std::collections::HashSet::new();
        for v in vk::F1..=vk::F24 {
            let r = vk_to_scancode(v).unwrap_or_else(|| panic!("F-key VK {v:#x} manquante"));
            assert!(seen.insert(r), "scancode duplique pour VK {v:#x}");
        }
        assert_eq!(seen.len(), 24);
    }

    #[test]
    fn inverse_roundtrip() {
        for &(v, sc, ext) in FIXED_TABLE {
            if matches!(v, vk::SHIFT | vk::CONTROL | vk::MENU) {
                continue;
            }
            // pave numerique : meme scancode que le bloc de navigation mais flag etendu different
            let back = scancode_to_vk(sc, ext).expect("inverse manquant");
            assert_eq!(vk_to_scancode(back), Some((sc, ext)), "VK {v:#x}");
        }
        assert_eq!(scancode_to_vk(0x2A, false), Some(vk::LSHIFT));
        assert_eq!(scancode_to_vk(0x1D, true), Some(vk::RCONTROL));
        assert_eq!(scancode_to_vk(0x1E, false), None); // A : depend du layout
    }

    #[test]
    fn lparam_decoding() {
        // 'A' (QWERTY) : scancode 0x1E, repetition 1, pas etendu
        let lp = (0x1Eisize << 16) | 1;
        assert_eq!(scancode_from_lparam(lp), (0x1E, false));
        assert_eq!(
            key_from_lparam(lp, true),
            Some(InputEvent::Key { scancode: 0x1E, extended: false, pressed: true })
        );
        // fleche gauche relachee : scancode 0x4B, etendu (bit 24), transition (bit 31)
        let lp = ((1u32 << 31) | (1 << 24) | (0x4B << 16) | 1) as i32 as isize;
        assert_eq!(
            key_from_lparam(lp, false),
            Some(InputEvent::Key { scancode: 0x4B, extended: true, pressed: false })
        );
        // scancode 0 / 0xE0 rejetes
        assert_eq!(key_from_lparam(1, true), None);
        assert_eq!(key_from_lparam(0xE0 << 16, true), None);
        // lParam negatif/arbitraire : aucun panic
        for lp in [isize::MIN, isize::MAX, -1, 0] {
            let _ = key_from_lparam(lp, true);
        }
    }

    #[test]
    fn raw_keyboard() {
        assert_eq!(
            key_from_raw(0x1D, RI_KEY_E0, 0xA3),
            Some(InputEvent::Key { scancode: 0x1D, extended: true, pressed: true })
        );
        assert_eq!(
            key_from_raw(0x1E, RI_KEY_BREAK, 0x41),
            Some(InputEvent::Key { scancode: 0x1E, extended: false, pressed: false })
        );
        assert_eq!(key_from_raw(0x2A, RI_KEY_E0, 0xFF), None); // fake shift (NumLock/PrtSc)
        assert_eq!(key_from_raw(0x1D, RI_KEY_E1, vk::PAUSE), None); // Pause
        assert_eq!(key_from_raw(0, 0, 0x41), None);
        assert_eq!(key_from_raw(0x100, 0, 0x41), None);
    }

    #[test]
    fn extended_u32_from_winit() {
        assert_eq!(scancode_from_extended_u32(0x1E), Some((0x1E, false)));
        assert_eq!(scancode_from_extended_u32(0xE04B), Some((0x4B, true)));
        assert_eq!(scancode_from_extended_u32(0), None);
        assert_eq!(scancode_from_extended_u32(0xE000), None);
        assert_eq!(scancode_from_extended_u32(0xE1_1D), None);
        assert_eq!(scancode_from_extended_u32(0x1_0000), None);
        assert_eq!(scancode_from_extended_u32(u32::MAX), None);
    }

    #[test]
    fn names() {
        assert_eq!(scancode_name(0x1E, false), "A");
        assert_eq!(scancode_name(0x4B, true), "ArrowLeft");
        assert_eq!(scancode_name(0x4B, false), "Numpad4");
        assert_eq!(scancode_name(0x76, false), "F24");
        assert_eq!(scancode_name(0x58, false), "F12");
        assert_eq!(scancode_name(0xFF, false), "?");
        // aucun panic sur l'espace complet
        for sc in 0..=255u8 {
            let _ = scancode_name(sc, false);
            let _ = scancode_name(sc, true);
        }
    }

    #[cfg(windows)]
    #[test]
    fn layout_mapping_agrees_with_os_for_fixed_keys() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VK_TO_VSC_EX};
        // La table est la verite pour ces touches ; on verifie que l'OS (independant du layout
        // pour ces VK) est d'accord : fleches, F1..F12, modificateurs gauches, Echap, Espace.
        let keys = [
            vk::ESCAPE, vk::SPACE, vk::RETURN, vk::LSHIFT, vk::RSHIFT, vk::LCONTROL, vk::LMENU,
            vk::LEFT, vk::RIGHT, vk::UP, vk::DOWN, vk::F1, vk::F10, vk::F11, vk::F12,
        ];
        for k in keys {
            let (sc, _ext) = vk_to_scancode(k).unwrap();
            // SAFETY: appel Win32 pur sans pointeur.
            let os = unsafe { MapVirtualKeyW(k as u32, MAPVK_VK_TO_VSC_EX) };
            let os_sc = (os & 0xFF) as u8;
            // On ne compare que le scancode : MapVirtualKey ne remonte pas le flag E0 pour les
            // fleches/navigation (quirk connu), la table pure est la reference pour le flag.
            assert_eq!(os_sc, sc, "VK {k:#x}");
        }
        // lettres : dependent du layout, mais toujours un scancode valide
        for c in b'A'..=b'Z' {
            assert!(vk_to_scancode_layout(c as u16).is_some(), "lettre {}", c as char);
        }
    }
}
