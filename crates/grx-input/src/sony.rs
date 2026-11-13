//! Manettes Sony cote CLIENT : decodeur des rapports HID d'entree DualShock 4 / DualSense (USB,
//! Bluetooth complet et Bluetooth simplifie) vers [`GamepadState`] (Croix = A, Rond = B, Carre = X,
//! Triangle = Y, PS = Guide, clic du pave tactile = [`xbutton::TOUCHPAD`]) et construction des
//! rapports de sortie de vibration (CRC32 en Bluetooth). Pur, sans dependance, sans panique.
//! Gyroscope, accelerometre, coordonnees du pave tactile, LED et gachettes adaptatives : non geres.

use crate::event::{xbutton, GamepadState};

pub const SONY_VID: u16 = 0x054C;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SonyModel {
    DualShock4,
    DualSense,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SonyLink {
    Usb,
    Bluetooth,
}

/// Retourne le modele de manette correspondant a l'identifiant de produit.
pub fn model_from_pid(pid: u16) -> Option<SonyModel> {
    match pid {
        0x05C4 | 0x09CC | 0x0BA0 => Some(SonyModel::DualShock4),
        0x0CE6 | 0x0DF2 => Some(SonyModel::DualSense),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SonyInput {
    pub lx: u8,
    pub ly: u8,
    pub rx: u8,
    pub ry: u8,
    pub l2: u8,
    pub r2: u8,
    pub hat: u8,
    pub square: bool,
    pub cross: bool,
    pub circle: bool,
    pub triangle: bool,
    pub l1: bool,
    pub r1: bool,
    pub l2_button: bool,
    pub r2_button: bool,
    pub share: bool,
    pub options: bool,
    pub l3: bool,
    pub r3: bool,
    pub ps: bool,
    pub touchpad: bool,
}

/// Decode un rapport d'entree Sony en fonction du modele et du lien.
pub fn parse_input_report(model: SonyModel, report: &[u8]) -> Option<(SonyInput, SonyLink)> {
    let report_id = *report.first()?;
    // (base, disposition DS4 ?, lien)
    let (b, ds4_layout, link) = match (model, report_id) {
        (SonyModel::DualShock4, 0x01) => {
            (1, true, if report.len() >= 64 { SonyLink::Usb } else { SonyLink::Bluetooth })
        }
        (SonyModel::DualShock4, 0x11) => (3, true, SonyLink::Bluetooth),
        (SonyModel::DualSense, 0x01) if report.len() >= 64 => (1, false, SonyLink::Usb),
        // Rapport Bluetooth simplifie (avant lecture de la calibration) : format DS4.
        (SonyModel::DualSense, 0x01) => (1, true, SonyLink::Bluetooth),
        (SonyModel::DualSense, 0x31) => (2, false, SonyLink::Bluetooth),
        _ => return None,
    };
    // Decalages (L2, R2, btn0, btn1, btn2) relatifs a la base.
    let (o_l2, o_r2, o_b0, o_b1, o_b2) = if ds4_layout { (7, 8, 4, 5, 6) } else { (4, 5, 7, 8, 9) };
    let required_len = b + if ds4_layout { 9 } else { 10 };
    if report.len() < required_len {
        return None;
    }
    let at = |o: usize| report.get(b + o).copied();
    let (lx, ly, rx, ry) = (at(0)?, at(1)?, at(2)?, at(3)?);
    let (l2, r2) = (at(o_l2)?, at(o_r2)?);
    let (btn0, btn1, btn2) = (at(o_b0)?, at(o_b1)?, at(o_b2)?);

    let hat = btn0 & 0x0F;
    let hat = if hat > 8 { 8 } else { hat };

    let square = (btn0 & 0x10) != 0;
    let cross = (btn0 & 0x20) != 0;
    let circle = (btn0 & 0x40) != 0;
    let triangle = (btn0 & 0x80) != 0;

    let l1 = (btn1 & 0x01) != 0;
    let r1 = (btn1 & 0x02) != 0;
    let l2_button = (btn1 & 0x04) != 0;
    let r2_button = (btn1 & 0x08) != 0;
    let share = (btn1 & 0x10) != 0;
    let options = (btn1 & 0x20) != 0;
    let l3 = (btn1 & 0x40) != 0;
    let r3 = (btn1 & 0x80) != 0;

    let ps = (btn2 & 0x01) != 0;
    let touchpad = (btn2 & 0x02) != 0;

    Some((
        SonyInput {
            lx,
            ly,
            rx,
            ry,
            l2,
            r2,
            hat,
            square,
            cross,
            circle,
            triangle,
            l1,
            r1,
            l2_button,
            r2_button,
            share,
            options,
            l3,
            r3,
            ps,
            touchpad,
        },
        link,
    ))
}

/// Convertit l'etat Sony en etat Gamepad standard.
pub fn to_gamepad_state(s: &SonyInput, index: u8) -> GamepadState {
    let mut buttons = 0u16;
    if s.cross {
        buttons |= xbutton::A;
    }
    if s.circle {
        buttons |= xbutton::B;
    }
    if s.square {
        buttons |= xbutton::X;
    }
    if s.triangle {
        buttons |= xbutton::Y;
    }
    if s.l1 {
        buttons |= xbutton::LEFT_SHOULDER;
    }
    if s.r1 {
        buttons |= xbutton::RIGHT_SHOULDER;
    }
    if s.share {
        buttons |= xbutton::BACK;
    }
    if s.options {
        buttons |= xbutton::START;
    }
    if s.l3 {
        buttons |= xbutton::LEFT_THUMB;
    }
    if s.r3 {
        buttons |= xbutton::RIGHT_THUMB;
    }
    if s.ps {
        buttons |= xbutton::GUIDE;
    }
    if s.touchpad {
        buttons |= xbutton::TOUCHPAD;
    }

    let hat = s.hat;
    if hat <= 7 {
        match hat {
            0 => buttons |= xbutton::DPAD_UP,
            1 => buttons |= xbutton::DPAD_UP | xbutton::DPAD_RIGHT,
            2 => buttons |= xbutton::DPAD_RIGHT,
            3 => buttons |= xbutton::DPAD_DOWN | xbutton::DPAD_RIGHT,
            4 => buttons |= xbutton::DPAD_DOWN,
            5 => buttons |= xbutton::DPAD_DOWN | xbutton::DPAD_LEFT,
            6 => buttons |= xbutton::DPAD_LEFT,
            7 => buttons |= xbutton::DPAD_UP | xbutton::DPAD_LEFT,
            _ => {}
        }
    }

    GamepadState {
        index,
        buttons,
        left_trigger: s.l2,
        right_trigger: s.r2,
        thumb_lx: axis_x(s.lx),
        thumb_ly: axis_y(s.ly),
        thumb_rx: axis_x(s.rx),
        thumb_ry: axis_y(s.ry),
    }
}

/// Convertit une valeur axe X Sony (0..255, centre 128) en i16 : 0x80 -> 0 exactement (pas de
/// derive au repos), 255 -> 32767, 0 -> -32768.
pub fn axis_x(v: u8) -> i16 {
    ((v as i32 - 128) * 32767 / 127).clamp(-32768, 32767) as i16
}

/// Axe Y Sony (0 = haut) vers XInput (haut positif) : 0x80 -> 0, 0 -> 32767, 255 -> -32767.
pub fn axis_y(v: u8) -> i16 {
    ((128 - v as i32) * 32767 / 127).clamp(-32768, 32767) as i16
}

/// Genere un rapport de rumble pour la manette cible.
pub fn rumble_report(model: SonyModel, link: SonyLink, strong: u8, weak: u8, seq: u8) -> Vec<u8> {
    match (model, link) {
        (SonyModel::DualShock4, SonyLink::Usb) => {
            let mut b = vec![0u8; 32];
            b[0] = 0x05;
            b[1] = 0x01;
            b[4] = weak;
            b[5] = strong;
            b
        }
        (SonyModel::DualShock4, SonyLink::Bluetooth) => {
            let mut b = vec![0u8; 78];
            b[0] = 0x11;
            b[1] = 0xC0;
            b[3] = 0x01;
            b[6] = weak;
            b[7] = strong;
            let mut crc_data = [0u8; 75];
            crc_data[0] = 0xA2;
            crc_data[1..75].copy_from_slice(&b[0..74]);
            let crc = crc32(&crc_data);
            b[74..78].copy_from_slice(&crc.to_le_bytes());
            b
        }
        (SonyModel::DualSense, SonyLink::Usb) => {
            let mut b = vec![0u8; 48];
            b[0] = 0x02;
            b[1] = 0x03;
            b[3] = weak;
            b[4] = strong;
            b
        }
        (SonyModel::DualSense, SonyLink::Bluetooth) => {
            let mut b = vec![0u8; 78];
            b[0] = 0x31;
            b[1] = (seq & 0x0F) << 4;
            b[2] = 0x10;
            b[3] = 0x03;
            b[5] = weak;
            b[6] = strong;
            let mut crc_data = [0u8; 75];
            crc_data[0] = 0xA2;
            crc_data[1..75].copy_from_slice(&b[0..74]);
            let crc = crc32(&crc_data);
            b[74..78].copy_from_slice(&crc.to_le_bytes());
            b
        }
    }
}

/// Calcule le CRC-32 IEEE reflechi sur les donnees fournies.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFFFFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB88320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_from_pid() {
        assert_eq!(model_from_pid(0x05C4), Some(SonyModel::DualShock4));
        assert_eq!(model_from_pid(0x09CC), Some(SonyModel::DualShock4));
        assert_eq!(model_from_pid(0x0BA0), Some(SonyModel::DualShock4));
        assert_eq!(model_from_pid(0x0CE6), Some(SonyModel::DualSense));
        assert_eq!(model_from_pid(0x0DF2), Some(SonyModel::DualSense));
        assert_eq!(model_from_pid(0x1234), None);
    }

    #[test]
    fn test_ds4_usb_decode() {
        let mut report = vec![0u8; 64];
        report[0] = 0x01;
        report[1] = 0;
        report[2] = 255;
        report[3] = 0x80;
        report[4] = 0x80;
        report[5] = 0x02 | 0x20 | 0x80; // hat 2 + croix + triangle
        report[6] = 0x01 | 0x20; // L1 + Options
        report[7] = 0x01 | 0x02; // PS + pave tactile
        report[8] = 200;
        report[9] = 10;
        let res = parse_input_report(SonyModel::DualShock4, &report);
        assert!(res.is_some());
        let (input, link) = res.unwrap();
        assert_eq!(link, SonyLink::Usb);
        assert_eq!(input.lx, 0);
        assert_eq!(input.ly, 255);
        assert_eq!(input.rx, 0x80);
        assert_eq!(input.ry, 0x80);
        assert_eq!(input.hat, 2);
        assert!(input.cross);
        assert!(input.triangle);
        assert!(input.l1);
        assert!(input.options);
        assert!(input.ps);
        assert!(input.touchpad);
        assert_eq!(input.l2, 200);
        assert_eq!(input.r2, 10);
    }

    #[test]
    fn test_ds4_bt_0x11() {
        let mut report = vec![0u8; 12];
        report[0] = 0x11;
        report[3] = 0x80;
        report[4] = 0x80;
        report[5] = 0x80;
        report[6] = 0x80;
        report[7] = 0x00;
        report[8] = 0x00;
        report[9] = 0x00;
        report[10] = 0x00;
        report[11] = 0x00;
        let res = parse_input_report(SonyModel::DualShock4, &report);
        assert!(res.is_some());
        let (_, link) = res.unwrap();
        assert_eq!(link, SonyLink::Bluetooth);
    }

    #[test]
    fn test_ds4_bt_simplified() {
        let mut report = vec![0u8; 10];
        report[0] = 0x01;
        report[1] = 0x80;
        report[2] = 0x80;
        report[3] = 0x80;
        report[4] = 0x80;
        report[5] = 0x00;
        report[6] = 0x00;
        report[7] = 0x00;
        report[8] = 0x00;
        report[9] = 0x00;
        let res = parse_input_report(SonyModel::DualShock4, &report);
        assert!(res.is_some());
        let (_, link) = res.unwrap();
        assert_eq!(link, SonyLink::Bluetooth);
    }

    #[test]
    fn test_ds_usb() {
        let mut report = vec![0u8; 64];
        report[0] = 0x01;
        report[1] = 0x80;
        report[2] = 0x80;
        report[3] = 0x80;
        report[4] = 0x80;
        report[5] = 100;
        report[6] = 50;
        report[7] = 0x00;
        report[8] = 0x00;
        report[9] = 0x00;
        let res = parse_input_report(SonyModel::DualSense, &report);
        assert!(res.is_some());
        let (input, link) = res.unwrap();
        assert_eq!(link, SonyLink::Usb);
        assert_eq!(input.l2, 100);
        assert_eq!(input.r2, 50);
    }

    #[test]
    fn test_ds_bt_0x31() {
        let mut report = vec![0u8; 78];
        report[0] = 0x31;
        report[2] = 0x80;
        report[3] = 0x80;
        report[4] = 0x80;
        report[5] = 0x80;
        report[6] = 100;
        report[7] = 50;
        report[9] = 0x00;
        report[10] = 0x00;
        report[11] = 0x00;
        let res = parse_input_report(SonyModel::DualSense, &report);
        assert!(res.is_some());
        let (input, link) = res.unwrap();
        assert_eq!(link, SonyLink::Bluetooth);
        assert_eq!(input.l2, 100);
        assert_eq!(input.r2, 50);
    }

    #[test]
    fn test_parse_short_and_unknown() {
        assert!(parse_input_report(SonyModel::DualShock4, &[]).is_none());
        assert!(parse_input_report(SonyModel::DualShock4, &[0x00]).is_none());
        assert!(parse_input_report(SonyModel::DualSense, &[0x00]).is_none());
        assert!(parse_input_report(SonyModel::DualShock4, &[0x01]).is_none());
        assert!(parse_input_report(SonyModel::DualSense, &[0x01]).is_none());
    }

    #[test]
    fn test_to_gamepad_state() {
        let s = SonyInput {
            lx: 0x80,
            ly: 0x80,
            rx: 0x80,
            ry: 0x80,
            l2: 0,
            r2: 0,
            hat: 0,
            square: true,
            cross: true,
            circle: true,
            triangle: true,
            l1: true,
            r1: false,
            l2_button: false,
            r2_button: false,
            share: true,
            options: true,
            l3: true,
            r3: true,
            ps: true,
            touchpad: true,
        };
        let g = to_gamepad_state(&s, 0);
        assert_eq!(
            g.buttons,
            xbutton::X
                | xbutton::A
                | xbutton::B
                | xbutton::Y
                | xbutton::LEFT_SHOULDER
                | xbutton::BACK
                | xbutton::START
                | xbutton::LEFT_THUMB
                | xbutton::RIGHT_THUMB
                | xbutton::DPAD_UP
                | xbutton::GUIDE
                | xbutton::TOUCHPAD
        );
        let s1 = SonyInput { hat: 1, ..Default::default() };
        let g1 = to_gamepad_state(&s1, 0);
        assert_eq!(g1.buttons, xbutton::DPAD_UP | xbutton::DPAD_RIGHT);
        let s15 = SonyInput { hat: 15, ..Default::default() };
        let g15 = to_gamepad_state(&s15, 0);
        assert_eq!(g15.buttons, 0);
        assert_eq!(axis_x(0x80), 0);
        assert_eq!(axis_y(0x80), 0);
        assert_eq!(axis_x(255), 32767);
        assert_eq!(axis_y(255), -32767);
        assert_eq!(axis_x(0), -32768);
        assert_eq!(axis_y(0), 32767);
    }

    #[test]
    fn test_crc32() {
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
    }

    #[test]
    fn test_rumble_report() {
        let ds4_usb = rumble_report(SonyModel::DualShock4, SonyLink::Usb, 0x55, 0xAA, 0);
        assert_eq!(ds4_usb.len(), 32);
        assert_eq!(ds4_usb[0], 0x05);
        assert_eq!(ds4_usb[1], 0x01);
        assert_eq!(ds4_usb[4], 0xAA);
        assert_eq!(ds4_usb[5], 0x55);

        let ds4_bt = rumble_report(SonyModel::DualShock4, SonyLink::Bluetooth, 0x55, 0xAA, 0);
        assert_eq!(ds4_bt.len(), 78);
        assert_eq!(ds4_bt[0], 0x11);
        assert_eq!(ds4_bt[1], 0xC0);
        assert_eq!(ds4_bt[3], 0x01);
        assert_eq!(ds4_bt[6], 0xAA);
        assert_eq!(ds4_bt[7], 0x55);
        let mut crc_data = [0u8; 75];
        crc_data[0] = 0xA2;
        crc_data[1..75].copy_from_slice(&ds4_bt[0..74]);
        assert_eq!(crc32(&crc_data), u32::from_le_bytes(ds4_bt[74..78].try_into().unwrap()));

        let ds_usb = rumble_report(SonyModel::DualSense, SonyLink::Usb, 0x55, 0xAA, 0);
        assert_eq!(ds_usb.len(), 48);
        assert_eq!(ds_usb[0], 0x02);
        assert_eq!(ds_usb[1], 0x03);
        assert_eq!(ds_usb[3], 0xAA);
        assert_eq!(ds_usb[4], 0x55);

        let ds_bt = rumble_report(SonyModel::DualSense, SonyLink::Bluetooth, 0x55, 0xAA, 0x05);
        assert_eq!(ds_bt.len(), 78);
        assert_eq!(ds_bt[0], 0x31);
        assert_eq!(ds_bt[1], 0x50);
        assert_eq!(ds_bt[2], 0x10);
        assert_eq!(ds_bt[3], 0x03);
        assert_eq!(ds_bt[5], 0xAA);
        assert_eq!(ds_bt[6], 0x55);
        let mut crc_data = [0u8; 75];
        crc_data[0] = 0xA2;
        crc_data[1..75].copy_from_slice(&ds_bt[0..74]);
        assert_eq!(crc32(&crc_data), u32::from_le_bytes(ds_bt[74..78].try_into().unwrap()));
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    #[test]
    fn fuzz_never_panics() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..100_000 {
            let len = (rng.next() % 100) as usize;
            let mut buf = vec![0u8; len];
            for byte in buf.iter_mut() {
                *byte = rng.next() as u8;
            }
            if !buf.is_empty() && rng.next().is_multiple_of(3) {
                buf[0] = match rng.next() % 3 {
                    0 => 0x01,
                    1 => 0x11,
                    _ => 0x31,
                };
            }
            let _ = parse_input_report(SonyModel::DualShock4, &buf);
            let _ = parse_input_report(SonyModel::DualSense, &buf);
        }
    }
}
