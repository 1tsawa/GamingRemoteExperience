//! Table de correspondance : etat de manette au format XInput -> rapport d'entree DualShock 4 (format ViGEm DS4_REPORT).

use crate::event::{xbutton, GamepadState};

/// Rapport DS4 (memes champs que vigem_client::DS4Report).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ds4Report {
    pub thumb_lx: u8,
    pub thumb_ly: u8,
    pub thumb_rx: u8,
    pub thumb_ry: u8,
    pub buttons: u16,
    pub special: u8,
    pub trigger_l: u8,
    pub trigger_r: u8,
}

impl Default for Ds4Report {
    fn default() -> Self {
        Self {
            thumb_lx: 0x80,
            thumb_ly: 0x80,
            thumb_rx: 0x80,
            thumb_ry: 0x80,
            buttons: 0x0008,
            special: 0,
            trigger_l: 0,
            trigger_r: 0,
        }
    }
}

pub mod ds4 {
    pub const SQUARE: u16 = 1 << 4;
    pub const CROSS: u16 = 1 << 5;
    pub const CIRCLE: u16 = 1 << 6;
    pub const TRIANGLE: u16 = 1 << 7;
    pub const L1: u16 = 1 << 8;
    pub const R1: u16 = 1 << 9;
    pub const L2: u16 = 1 << 10;
    pub const R2: u16 = 1 << 11;
    pub const SHARE: u16 = 1 << 12;
    pub const OPTIONS: u16 = 1 << 13;
    pub const L3: u16 = 1 << 14;
    pub const R3: u16 = 1 << 15;
    pub const SPECIAL_PS: u8 = 1;
    pub const SPECIAL_TOUCHPAD: u8 = 2;
}

/// Seuil au-dela duquel une gachette analogique enfonce aussi le bouton numerique L2/R2.
pub const TRIGGER_BUTTON_THRESHOLD: u8 = 30;

/// Conversion axe X : -32768 -> 0, 0 -> 128, 32767 -> 255.
pub fn stick_x_to_u8(v: i16) -> u8 {
    ((v as i32 + 32768) >> 8) as u8
}

/// Conversion axe Y : inverse de X (DS4 : 0 = haut ; XInput : haut positif).
pub fn stick_y_to_u8(v: i16) -> u8 {
    255 - stick_x_to_u8(v)
}

/// Croix directionnelle XInput -> chapeau DS4 : 0=N, 1=NE, 2=E, 3=SE, 4=S, 5=SO, 6=O, 7=NO, 8=neutre.
/// Directions opposees simultanees (haut+bas ou gauche+droite) : s'annulent sur cet axe.
pub fn dpad_to_hat(buttons: u16) -> u8 {
    let up = buttons & xbutton::DPAD_UP != 0;
    let down = buttons & xbutton::DPAD_DOWN != 0;
    let left = buttons & xbutton::DPAD_LEFT != 0;
    let right = buttons & xbutton::DPAD_RIGHT != 0;

    let u = up && !down;
    let d = down && !up;
    let l = left && !right;
    let r = right && !left;

    if !u && !d && !l && !r {
        return 8;
    }
    if u && l {
        return 7;
    }
    if u && r {
        return 1;
    }
    if d && l {
        return 5;
    }
    if d && r {
        return 3;
    }
    if u {
        return 0;
    }
    if d {
        return 4;
    }
    if l {
        return 6;
    }
    if r {
        return 2;
    }
    8
}

/// Conversion complete d'un etat XInput vers un rapport DS4.
pub fn to_ds4(s: &GamepadState) -> Ds4Report {
    // Sticks et gachettes analogiques
    let mut report = Ds4Report {
        thumb_lx: stick_x_to_u8(s.thumb_lx),
        thumb_ly: stick_y_to_u8(s.thumb_ly),
        thumb_rx: stick_x_to_u8(s.thumb_rx),
        thumb_ry: stick_y_to_u8(s.thumb_ry),
        trigger_l: s.left_trigger,
        trigger_r: s.right_trigger,
        ..Default::default()
    };

    // Boutons numeriques
    let mut btns: u16 = 0;
    btns |= dpad_to_hat(s.buttons) as u16; // bits 0-3

    if s.buttons & xbutton::A != 0 {
        btns |= ds4::CROSS;
    }
    if s.buttons & xbutton::B != 0 {
        btns |= ds4::CIRCLE;
    }
    if s.buttons & xbutton::X != 0 {
        btns |= ds4::SQUARE;
    }
    if s.buttons & xbutton::Y != 0 {
        btns |= ds4::TRIANGLE;
    }
    if s.buttons & xbutton::LEFT_SHOULDER != 0 {
        btns |= ds4::L1;
    }
    if s.buttons & xbutton::RIGHT_SHOULDER != 0 {
        btns |= ds4::R1;
    }
    if s.buttons & xbutton::BACK != 0 {
        btns |= ds4::SHARE;
    }
    if s.buttons & xbutton::START != 0 {
        btns |= ds4::OPTIONS;
    }
    if s.buttons & xbutton::LEFT_THUMB != 0 {
        btns |= ds4::L3;
    }
    if s.buttons & xbutton::RIGHT_THUMB != 0 {
        btns |= ds4::R3;
    }

    // Gachettes analogiques -> boutons numeriques
    if s.left_trigger > TRIGGER_BUTTON_THRESHOLD {
        btns |= ds4::L2;
    }
    if s.right_trigger > TRIGGER_BUTTON_THRESHOLD {
        btns |= ds4::R2;
    }

    report.buttons = btns;

    // Boutons speciaux
    let mut special: u8 = 0;
    if s.buttons & xbutton::GUIDE != 0 {
        special |= ds4::SPECIAL_PS;
    }
    if s.buttons & xbutton::TOUCHPAD != 0 {
        special |= ds4::SPECIAL_TOUCHPAD;
    }
    report.special = special;

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::xbutton;

    #[test]
    fn neutral_state() {
        let s = GamepadState::default();
        let r = to_ds4(&s);
        assert_eq!(r.thumb_lx, 128);
        assert_eq!(r.thumb_ly, 127);
        assert_eq!(r.thumb_rx, 128);
        assert_eq!(r.thumb_ry, 127);
        assert_eq!(r.buttons, 0x0008);
        assert_eq!(r.special, 0);
        assert_eq!(r.trigger_l, 0);
        assert_eq!(r.trigger_r, 0);
    }

    #[test]
    fn each_button_alone() {
        let base = GamepadState {
            thumb_lx: 0,
            thumb_ly: 0,
            thumb_rx: 0,
            thumb_ry: 0,
            left_trigger: 0,
            right_trigger: 0,
            ..Default::default()
        };
        let cases = [
            (xbutton::DPAD_UP, 0),
            (xbutton::DPAD_DOWN, 4),
            (xbutton::DPAD_LEFT, 6),
            (xbutton::DPAD_RIGHT, 2),
            (xbutton::START, ds4::OPTIONS),
            (xbutton::BACK, ds4::SHARE),
            (xbutton::LEFT_THUMB, ds4::L3),
            (xbutton::RIGHT_THUMB, ds4::R3),
            (xbutton::LEFT_SHOULDER, ds4::L1),
            (xbutton::RIGHT_SHOULDER, ds4::R1),
            (xbutton::A, ds4::CROSS),
            (xbutton::B, ds4::CIRCLE),
            (xbutton::X, ds4::SQUARE),
            (xbutton::Y, ds4::TRIANGLE),
        ];
        for (btn, expected_ds4) in cases {
            let s = GamepadState { buttons: btn, ..base };
            let r = to_ds4(&s);
            // hors croix directionnelle, le chapeau reste neutre (8)
            let expected = if btn & 0x000F != 0 { expected_ds4 } else { expected_ds4 | 8 };
            assert_eq!(r.buttons, expected, "failed for button {btn:#06x}");
        }
    }

    #[test]
    fn dpad_directions() {
        let base = GamepadState { buttons: 0, ..Default::default() };
        let cases = [
            (0, 8), // neutre
            (xbutton::DPAD_UP, 0),
            (xbutton::DPAD_UP | xbutton::DPAD_RIGHT, 1),
            (xbutton::DPAD_RIGHT, 2),
            (xbutton::DPAD_RIGHT | xbutton::DPAD_DOWN, 3),
            (xbutton::DPAD_DOWN, 4),
            (xbutton::DPAD_DOWN | xbutton::DPAD_LEFT, 5),
            (xbutton::DPAD_LEFT, 6),
            (xbutton::DPAD_LEFT | xbutton::DPAD_UP, 7),
            // Opposites cancel
            (xbutton::DPAD_UP | xbutton::DPAD_DOWN, 8),
            (xbutton::DPAD_LEFT | xbutton::DPAD_RIGHT, 8),
            (xbutton::DPAD_UP | xbutton::DPAD_DOWN | xbutton::DPAD_LEFT, 6),
        ];
        for (btn, expected_hat) in cases {
            let s = GamepadState { buttons: btn, ..base };
            let r = to_ds4(&s);
            assert_eq!(r.buttons & 0x0F, expected_hat, "dpad {btn:#06x} -> hat {expected_hat}");
        }
    }

    #[test]
    fn stick_extremes() {
        let base = GamepadState { buttons: 0, left_trigger: 0, right_trigger: 0, ..Default::default() };
        // (valeur, attendu en X, attendu en Y pour la meme valeur)
        let cases = [
            (-32768i16, 0u8, 255u8), // min : x -> 0, y -> 255 (bas)
            (32767, 255, 0),         // max : x -> 255, y -> 0 (haut)
            (0, 128, 127),           // centre
        ];
        for (lx, exp_lx, exp_ly) in cases {
            let s = GamepadState { thumb_lx: lx, thumb_ly: lx, ..base };
            let r = to_ds4(&s);
            assert_eq!(r.thumb_lx, exp_lx, "lx {lx}");
            assert_eq!(r.thumb_ly, exp_ly, "ly {lx}");
        }
        // Test rx/ry
        for (rx, exp_rx, exp_ry) in cases {
            let s = GamepadState { thumb_rx: rx, thumb_ry: rx, ..base };
            let r = to_ds4(&s);
            assert_eq!(r.thumb_rx, exp_rx, "rx {rx}");
            assert_eq!(r.thumb_ry, exp_ry, "ry {rx}");
        }
    }

    #[test]
    fn trigger_threshold() {
        let base =
            GamepadState { buttons: 0, thumb_lx: 0, thumb_ly: 0, thumb_rx: 0, thumb_ry: 0, ..Default::default() };
        let s30 = GamepadState { left_trigger: 30, right_trigger: 30, ..base };
        let r30 = to_ds4(&s30);
        assert_eq!(r30.trigger_l, 30);
        assert_eq!(r30.trigger_r, 30);
        assert_eq!(r30.buttons & (ds4::L2 | ds4::R2), 0);

        let s31 = GamepadState { left_trigger: 31, right_trigger: 31, ..base };
        let r31 = to_ds4(&s31);
        assert_eq!(r31.trigger_l, 31);
        assert_eq!(r31.trigger_r, 31);
        assert_eq!(r31.buttons & (ds4::L2 | ds4::R2), ds4::L2 | ds4::R2);
    }

    #[test]
    fn special_buttons() {
        let base = GamepadState { buttons: 0, left_trigger: 0, right_trigger: 0, ..Default::default() };
        let s_guide = GamepadState { buttons: xbutton::GUIDE, ..base };
        let r_guide = to_ds4(&s_guide);
        assert_eq!(r_guide.special, ds4::SPECIAL_PS);

        let s_touch = GamepadState { buttons: xbutton::TOUCHPAD, ..base };
        let r_touch = to_ds4(&s_touch);
        assert_eq!(r_touch.special, ds4::SPECIAL_TOUCHPAD);

        let s_both = GamepadState { buttons: xbutton::GUIDE | xbutton::TOUCHPAD, ..base };
        let r_both = to_ds4(&s_both);
        assert_eq!(r_both.special, ds4::SPECIAL_PS | ds4::SPECIAL_TOUCHPAD);
    }

    #[test]
    fn exhaustive_buttons_no_panic() {
        let base = GamepadState {
            thumb_lx: 0,
            thumb_ly: 0,
            thumb_rx: 0,
            thumb_ry: 0,
            left_trigger: 0,
            right_trigger: 0,
            ..Default::default()
        };
        for b in 0..=0xFFFF {
            let s = GamepadState { buttons: b, ..base };
            let _ = to_ds4(&s);
        }
    }
}
