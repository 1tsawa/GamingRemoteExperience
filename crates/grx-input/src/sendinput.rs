//! Injection clavier/souris via `SendInput` (Windows).
//!
//! * Clavier : `KEYEVENTF_SCANCODE` (+ `KEYEVENTF_EXTENDEDKEY`, `KEYEVENTF_KEYUP`), `wVk = 0` :
//!   la touche physique est rejouee telle quelle, le layout du host decide du caractere.
//! * Souris : `MOUSEEVENTF_MOVE` (relatif ; Windows applique la vitesse/acceleration du pointeur
//!   du host), `MOUSEEVENTF_ABSOLUTE` (0..=65535 sur l'ecran principal, ou le bureau virtuel
//!   complet avec `virtual_desktop`), boutons, `WHEEL`/`HWHEEL`, `XDOWN`/`XUP`.
//!
//! Limites Windows : UIPI interdit d'injecter vers une fenetre de niveau d'integrite superieur
//! (le host doit etre eleve pour piloter un programme administrateur) ; le bureau securise
//! (UAC, Ctrl+Alt+Suppr) est inaccessible depuis une session utilisateur ; depuis un service
//! (session 0) `SendInput` n'atteint pas le bureau interactif.
//!
//! Chaque evenement porte `dwExtraInfo == INJECTED_MAGIC` pour pouvoir etre reconnu (hooks).

use std::mem::size_of;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP,
    MOUSE_EVENT_FLAGS, MOUSEINPUT, VIRTUAL_KEY,
};

use crate::event::MouseButton;
use crate::injector::{InjectError, InputSink};

/// Marqueur place dans `dwExtraInfo` des evenements injectes par GRX ("GRX\0").
pub const INJECTED_MAGIC: usize = 0x4752_5800;

#[derive(Debug, Clone, Default)]
pub struct SendInputSink {
    /// Mapper `MouseAbs` sur tout le bureau virtuel (multi-ecrans) plutot que l'ecran principal.
    pub virtual_desktop: bool,
    /// Ecran vise par `MouseAbs` (ecran capture + bureau virtuel, voir [`crate::absmap`]) : les
    /// coordonnees normalisees dans l'image sont converties en coordonnees VIRTUALDESK. Prioritaire
    /// sur `virtual_desktop` quand une cible est connue.
    pub abs_target: Option<std::sync::Arc<crate::absmap::AbsTarget>>,
}

impl SendInputSink {
    pub fn new() -> Self {
        Self::default()
    }

    fn send(&self, inputs: &[INPUT]) -> Result<(), InjectError> {
        if inputs.is_empty() {
            return Ok(());
        }
        // SAFETY: `inputs` est un slice valide de structures INPUT initialisees ; cbSize = size_of::<INPUT>().
        let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
        if sent as usize != inputs.len() {
            let err = windows::core::Error::from_thread();
            return Err(InjectError::SendInput(format!("{sent}/{} evenements injectes : {err}", inputs.len())));
        }
        Ok(())
    }
}

fn kbd(scancode: u8, extended: bool, pressed: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scancode as u16,
                dwFlags: flags as KEYBD_EVENT_FLAGS,
                time: 0,
                dwExtraInfo: INJECTED_MAGIC,
            },
        },
    }
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: INJECTED_MAGIC },
        },
    }
}

impl InputSink for SendInputSink {
    fn key(&mut self, scancode: u8, extended: bool, pressed: bool) -> Result<(), InjectError> {
        self.send(&[kbd(scancode, extended, pressed)])
    }

    fn mouse_move_rel(&mut self, dx: i32, dy: i32) -> Result<(), InjectError> {
        self.send(&[mouse(dx, dy, 0, MOUSEEVENTF_MOVE)])
    }

    fn mouse_move_abs(&mut self, x: u16, y: u16) -> Result<(), InjectError> {
        if let Some((out, vdesk)) = self.abs_target.as_ref().and_then(|t| t.get()) {
            if let Some((nx, ny)) = crate::absmap::map_abs(x, y, &out, &vdesk) {
                let flags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
                return self.send(&[mouse(nx as i32, ny as i32, 0, flags)]);
            }
        }
        let mut flags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE;
        if self.virtual_desktop {
            flags |= MOUSEEVENTF_VIRTUALDESK;
        }
        self.send(&[mouse(x as i32, y as i32, 0, flags)])
    }

    fn mouse_button(&mut self, button: MouseButton, pressed: bool) -> Result<(), InjectError> {
        let (flags, data) = match (button, pressed) {
            (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (MouseButton::X1, true) => (MOUSEEVENTF_XDOWN, 1), // XBUTTON1
            (MouseButton::X1, false) => (MOUSEEVENTF_XUP, 1),
            (MouseButton::X2, true) => (MOUSEEVENTF_XDOWN, 2), // XBUTTON2
            (MouseButton::X2, false) => (MOUSEEVENTF_XUP, 2),
        };
        self.send(&[mouse(0, 0, data, flags)])
    }

    fn wheel(&mut self, horizontal: i16, vertical: i16) -> Result<(), InjectError> {
        let mut inputs = Vec::with_capacity(2);
        // mouseData est un DWORD qui porte un delta signe : conversion i32 -> u32 par reinterpretation.
        if vertical != 0 {
            inputs.push(mouse(0, 0, vertical as i32 as u32, MOUSEEVENTF_WHEEL));
        }
        if horizontal != 0 {
            inputs.push(mouse(0, 0, horizontal as i32 as u32, MOUSEEVENTF_HWHEEL));
        }
        self.send(&inputs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_scancode_keyboard_input() {
        let i = kbd(0x4D, true, false);
        // SAFETY: on vient de construire la variante clavier.
        let ki = unsafe { i.Anonymous.ki };
        assert_eq!(i.r#type, INPUT_KEYBOARD);
        assert_eq!(ki.wVk, VIRTUAL_KEY(0));
        assert_eq!(ki.wScan, 0x4D);
        assert_eq!(ki.dwExtraInfo, INJECTED_MAGIC);
        assert!(ki.dwFlags.contains(KEYEVENTF_SCANCODE));
        assert!(ki.dwFlags.contains(KEYEVENTF_EXTENDEDKEY));
        assert!(ki.dwFlags.contains(KEYEVENTF_KEYUP));

        let down = unsafe { kbd(0x1E, false, true).Anonymous.ki };
        assert!(down.dwFlags.contains(KEYEVENTF_SCANCODE));
        assert!(!down.dwFlags.contains(KEYEVENTF_EXTENDEDKEY));
        assert!(!down.dwFlags.contains(KEYEVENTF_KEYUP));
    }

    #[test]
    fn negative_wheel_is_sign_preserved() {
        let m = unsafe { mouse(0, 0, (-120i16) as i32 as u32, MOUSEEVENTF_WHEEL).Anonymous.mi };
        assert_eq!(m.mouseData as i32, -120);
    }

    // ---- Tests d'injection REELS, volontairement inoffensifs ------------------------------
    // Aucun ne peut taper dans une fenetre ni deplacer le curseur :
    //  * mouvement RELATIF de (0, 0) : le curseur ne bouge pas ;
    //  * molette a 0 : l'injecteur ne l'emet meme pas (verifie via Outcome::Ignored ailleurs) ;
    //  * F24 (scancode 0x76) : touche inexistante sur les claviers, ne fait rien dans un
    //    programme normal ; envoyee puis relachee dans la foulee (test #[ignore], explicite).

    #[test]
    fn real_sendinput_zero_mouse_move_succeeds() {
        let mut sink = SendInputSink::new();
        sink.mouse_move_rel(0, 0).expect("SendInput(MOVE 0,0) doit reussir");
    }

    #[test]
    #[ignore = "injecte reellement F24 (down+up) ; inoffensif mais a lancer volontairement"]
    fn real_sendinput_f24_down_up() {
        let mut sink = SendInputSink::new();
        sink.key(0x76, false, true).unwrap();
        sink.key(0x76, false, false).unwrap();
    }
}
