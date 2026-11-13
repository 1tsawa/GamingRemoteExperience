//! `InputEvent` et son format binaire compact (little-endian, auto-delimite).
//!
//! Chaque evenement = 1 octet de tag + charge utile de taille fixe :
//!
//! ```text
//! 0x01 Key         [flags u8][scancode u8]                 flags: bit0 = etendu (E0), bit1 = relache
//! 0x02 MouseMove   [dx i16][dy i16]                        relatif, en pixels bruts
//! 0x03 MouseAbs    [x u16][y u16]                          0..=65535 (0 = bord gauche/haut)
//! 0x04 MouseButton [button u8][pressed u8]                 0=G 1=D 2=milieu 3=X1 4=X2 ; pressed 0/1
//! 0x05 Wheel       [horizontal i16][vertical i16]          unites WHEEL_DELTA (120 = un cran)
//! 0x06 Gamepad     [index u8][buttons u16][lt u8][rt u8][lx i16][ly i16][rx i16][ry i16]
//! 0x07 GamepadConnect    [index u8][kind u8][virt u8]       kind 0..=4 (GamepadKind), virt 0..=1 (VirtualPad)
//! 0x08 GamepadDisconnect [index u8]
//! ```
//!
//! `buttons` suit `XINPUT_GAMEPAD.wButtons` ; le bit 0x0800 (non defini par XInput) porte le clic
//! du pave tactile des manettes Sony ([`xbutton::TOUCHPAD`], ignore par une manette virtuelle Xbox).
//! `GamepadConnect` annonce le type de la manette physique et la manette virtuelle souhaitee ;
//! `GamepadDisconnect` demande au host de debrancher la manette virtuelle de cet index.
//! Un host plus ancien rejette proprement ces deux tags (evenement ignore) et garde une manette
//! Xbox 360 branchee jusqu'a la fin de la session.
//!
//! Le decodeur est strict : tag inconnu, charge tronquee, octets en trop, bits de flags
//! reserves, scancode invalide (0, 0xE0, 0xE1), index de manette > 3, type de manette inconnu
//! ou booleen different de 0/1 sont rejetes. Il ne panique jamais.

use thiserror::Error;

/// Taille maximale d'un evenement encode (la manette).
pub const MAX_ENCODED_LEN: usize = 14;
/// Nombre de manettes supportees (index 0..=3, comme XInput).
pub const MAX_GAMEPADS: usize = 4;
/// Masque des boutons XInput valides (le bit 0x0800 n'est pas defini par XInput).
pub const XINPUT_BUTTONS_MASK: u16 = 0xF7FF;
/// Masque des boutons acceptes sur le fil : XInput + [`xbutton::TOUCHPAD`].
pub const GAMEPAD_BUTTONS_MASK: u16 = XINPUT_BUTTONS_MASK | xbutton::TOUCHPAD;

/// Boutons XInput (valeurs de `XINPUT_GAMEPAD.wButtons`).
pub mod xbutton {
    pub const DPAD_UP: u16 = 0x0001;
    pub const DPAD_DOWN: u16 = 0x0002;
    pub const DPAD_LEFT: u16 = 0x0004;
    pub const DPAD_RIGHT: u16 = 0x0008;
    pub const START: u16 = 0x0010;
    pub const BACK: u16 = 0x0020;
    pub const LEFT_THUMB: u16 = 0x0040;
    pub const RIGHT_THUMB: u16 = 0x0080;
    pub const LEFT_SHOULDER: u16 = 0x0100;
    pub const RIGHT_SHOULDER: u16 = 0x0200;
    pub const GUIDE: u16 = 0x0400;
    /// Extension GRX : clic du pave tactile (DualShock 4 / DualSense). Bit non defini par XInput.
    pub const TOUCHPAD: u16 = 0x0800;
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;
}

/// Bouton de souris.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

impl MouseButton {
    pub const fn to_u8(self) -> u8 {
        match self {
            MouseButton::Left => 0,
            MouseButton::Right => 1,
            MouseButton::Middle => 2,
            MouseButton::X1 => 3,
            MouseButton::X2 => 4,
        }
    }

    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(MouseButton::Left),
            1 => Some(MouseButton::Right),
            2 => Some(MouseButton::Middle),
            3 => Some(MouseButton::X1),
            4 => Some(MouseButton::X2),
            _ => None,
        }
    }
}

/// Etat complet d'une manette (format XInput).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct GamepadState {
    /// Index de la manette, 0..=3.
    pub index: u8,
    /// Masque de boutons XInput (voir [`xbutton`]).
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub thumb_lx: i16,
    pub thumb_ly: i16,
    pub thumb_rx: i16,
    pub thumb_ry: i16,
}

/// Famille de la manette physique cote client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum GamepadKind {
    /// Xbox 360 / One / Series et compatibles XInput.
    #[default]
    Xbox,
    DualShock4,
    DualSense,
    /// Nintendo Switch Pro (et Joy-Con) via la couche generique.
    Switch,
    Generic,
}

impl GamepadKind {
    pub const fn to_u8(self) -> u8 {
        match self {
            GamepadKind::Xbox => 0,
            GamepadKind::DualShock4 => 1,
            GamepadKind::DualSense => 2,
            GamepadKind::Switch => 3,
            GamepadKind::Generic => 4,
        }
    }

    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(GamepadKind::Xbox),
            1 => Some(GamepadKind::DualShock4),
            2 => Some(GamepadKind::DualSense),
            3 => Some(GamepadKind::Switch),
            4 => Some(GamepadKind::Generic),
            _ => None,
        }
    }

    /// Manette Sony (pave tactile, bouton PS).
    pub const fn is_sony(self) -> bool {
        matches!(self, GamepadKind::DualShock4 | GamepadKind::DualSense)
    }

    /// Libelle court pour l'interface.
    pub const fn label(self) -> &'static str {
        match self {
            GamepadKind::Xbox => "Xbox",
            GamepadKind::DualShock4 => "DualShock 4",
            GamepadKind::DualSense => "DualSense",
            GamepadKind::Switch => "Switch",
            GamepadKind::Generic => "generique",
        }
    }
}

/// Manette virtuelle cote host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum VirtualPad {
    /// Xbox 360 (XInput) : compatibilite maximale.
    #[default]
    Xbox360,
    /// DualShock 4 (jeux qui affichent les symboles PlayStation ou exigent une manette Sony).
    DualShock4,
}

impl VirtualPad {
    pub const fn to_u8(self) -> u8 {
        match self {
            VirtualPad::Xbox360 => 0,
            VirtualPad::DualShock4 => 1,
        }
    }

    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(VirtualPad::Xbox360),
            1 => Some(VirtualPad::DualShock4),
            _ => None,
        }
    }
}

/// Evenement d'entree envoye du client vers le host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    /// Touche physique, independante du layout (scancode set 1 / PS/2, comme `KEYEVENTF_SCANCODE`).
    Key { scancode: u8, extended: bool, pressed: bool },
    /// Deplacement relatif de la souris (capture souris relative, jeux).
    MouseMove { dx: i16, dy: i16 },
    /// Position absolue normalisee sur 0..=65535 (menus, bureau).
    MouseAbs { x: u16, y: u16 },
    MouseButton { button: MouseButton, pressed: bool },
    /// Molette, en unites `WHEEL_DELTA` (120 par cran). `vertical > 0` = vers le haut.
    Wheel { horizontal: i16, vertical: i16 },
    Gamepad(GamepadState),
    /// Une manette physique est branchee cote client a cet index (type + manette virtuelle souhaitee).
    GamepadConnect { index: u8, kind: GamepadKind, virt: VirtualPad },
    /// La manette de cet index a ete debranchee cote client.
    GamepadDisconnect { index: u8 },
}

/// Erreur de decodage.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    #[error("donnees vides")]
    Empty,
    #[error("tag d'evenement inconnu : {0:#04x}")]
    UnknownTag(u8),
    #[error("evenement tronque")]
    Truncated,
    #[error("octets superflus apres l'evenement")]
    TrailingBytes,
    #[error("valeur invalide : {0}")]
    InvalidValue(&'static str),
}

const TAG_KEY: u8 = 0x01;
const TAG_MOUSE_MOVE: u8 = 0x02;
const TAG_MOUSE_ABS: u8 = 0x03;
const TAG_MOUSE_BUTTON: u8 = 0x04;
const TAG_WHEEL: u8 = 0x05;
const TAG_GAMEPAD: u8 = 0x06;
const TAG_GAMEPAD_CONNECT: u8 = 0x07;
const TAG_GAMEPAD_DISCONNECT: u8 = 0x08;

const KEY_FLAG_EXTENDED: u8 = 0x01;
const KEY_FLAG_UP: u8 = 0x02;

/// Un scancode est valide s'il n'est ni 0 ni un octet de prefixe (0xE0/0xE1).
pub const fn is_valid_scancode(sc: u8) -> bool {
    sc != 0 && sc != 0xE0 && sc != 0xE1
}

impl InputEvent {
    /// Taille de l'encodage de cet evenement.
    pub const fn encoded_len(&self) -> usize {
        match self {
            InputEvent::Key { .. } => 3,
            InputEvent::MouseMove { .. } | InputEvent::MouseAbs { .. } | InputEvent::Wheel { .. } => 5,
            InputEvent::MouseButton { .. } => 3,
            InputEvent::Gamepad(_) => MAX_ENCODED_LEN,
            InputEvent::GamepadConnect { .. } => 4,
            InputEvent::GamepadDisconnect { .. } => 2,
        }
    }

    /// Encode dans un nouveau `Vec`.
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(self.encoded_len());
        self.encode_into(&mut v);
        v
    }

    /// Ajoute l'encodage a `out` (permet de grouper plusieurs evenements par datagramme).
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match *self {
            InputEvent::Key { scancode, extended, pressed } => {
                let mut flags = 0;
                if extended {
                    flags |= KEY_FLAG_EXTENDED;
                }
                if !pressed {
                    flags |= KEY_FLAG_UP;
                }
                out.extend_from_slice(&[TAG_KEY, flags, scancode]);
            }
            InputEvent::MouseMove { dx, dy } => {
                out.push(TAG_MOUSE_MOVE);
                out.extend_from_slice(&dx.to_le_bytes());
                out.extend_from_slice(&dy.to_le_bytes());
            }
            InputEvent::MouseAbs { x, y } => {
                out.push(TAG_MOUSE_ABS);
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
            }
            InputEvent::MouseButton { button, pressed } => {
                out.extend_from_slice(&[TAG_MOUSE_BUTTON, button.to_u8(), pressed as u8]);
            }
            InputEvent::Wheel { horizontal, vertical } => {
                out.push(TAG_WHEEL);
                out.extend_from_slice(&horizontal.to_le_bytes());
                out.extend_from_slice(&vertical.to_le_bytes());
            }
            InputEvent::Gamepad(g) => {
                out.push(TAG_GAMEPAD);
                out.push(g.index);
                out.extend_from_slice(&g.buttons.to_le_bytes());
                out.push(g.left_trigger);
                out.push(g.right_trigger);
                out.extend_from_slice(&g.thumb_lx.to_le_bytes());
                out.extend_from_slice(&g.thumb_ly.to_le_bytes());
                out.extend_from_slice(&g.thumb_rx.to_le_bytes());
                out.extend_from_slice(&g.thumb_ry.to_le_bytes());
            }
            InputEvent::GamepadConnect { index, kind, virt } => {
                out.extend_from_slice(&[TAG_GAMEPAD_CONNECT, index, kind.to_u8(), virt.to_u8()]);
            }
            InputEvent::GamepadDisconnect { index } => out.extend_from_slice(&[TAG_GAMEPAD_DISCONNECT, index]),
        }
    }

    /// Decode exactement un evenement ; tout octet en trop est une erreur.
    pub fn decode(data: &[u8]) -> Result<InputEvent, DecodeError> {
        let (ev, used) = Self::decode_prefix(data)?;
        if used != data.len() {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(ev)
    }

    /// Decode un evenement au debut de `data` et renvoie aussi le nombre d'octets consommes.
    pub fn decode_prefix(data: &[u8]) -> Result<(InputEvent, usize), DecodeError> {
        let (&tag, body) = data.split_first().ok_or(DecodeError::Empty)?;
        let need = |n: usize| -> Result<&[u8], DecodeError> { body.get(..n).ok_or(DecodeError::Truncated) };
        let i16_at = |b: &[u8], o: usize| i16::from_le_bytes([b[o], b[o + 1]]);
        let u16_at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let boolean = |v: u8| match v {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError::InvalidValue("booleen different de 0/1")),
        };

        match tag {
            TAG_KEY => {
                let b = need(2)?;
                let flags = b[0];
                if flags & !(KEY_FLAG_EXTENDED | KEY_FLAG_UP) != 0 {
                    return Err(DecodeError::InvalidValue("bits de flags reserves"));
                }
                if !is_valid_scancode(b[1]) {
                    return Err(DecodeError::InvalidValue("scancode invalide"));
                }
                Ok((
                    InputEvent::Key {
                        scancode: b[1],
                        extended: flags & KEY_FLAG_EXTENDED != 0,
                        pressed: flags & KEY_FLAG_UP == 0,
                    },
                    3,
                ))
            }
            TAG_MOUSE_MOVE => {
                let b = need(4)?;
                Ok((InputEvent::MouseMove { dx: i16_at(b, 0), dy: i16_at(b, 2) }, 5))
            }
            TAG_MOUSE_ABS => {
                let b = need(4)?;
                Ok((InputEvent::MouseAbs { x: u16_at(b, 0), y: u16_at(b, 2) }, 5))
            }
            TAG_MOUSE_BUTTON => {
                let b = need(2)?;
                let button =
                    MouseButton::from_u8(b[0]).ok_or(DecodeError::InvalidValue("bouton de souris inconnu"))?;
                Ok((InputEvent::MouseButton { button, pressed: boolean(b[1])? }, 3))
            }
            TAG_WHEEL => {
                let b = need(4)?;
                Ok((InputEvent::Wheel { horizontal: i16_at(b, 0), vertical: i16_at(b, 2) }, 5))
            }
            TAG_GAMEPAD => {
                let b = need(MAX_ENCODED_LEN - 1)?;
                if b[0] as usize >= MAX_GAMEPADS {
                    return Err(DecodeError::InvalidValue("index de manette > 3"));
                }
                // Les 16 bits sont definis (XInput + pave tactile) : aucun bouton reserve.
                let buttons = u16_at(b, 1);
                const _: () = assert!(GAMEPAD_BUTTONS_MASK == u16::MAX);
                Ok((
                    InputEvent::Gamepad(GamepadState {
                        index: b[0],
                        buttons,
                        left_trigger: b[3],
                        right_trigger: b[4],
                        thumb_lx: i16_at(b, 5),
                        thumb_ly: i16_at(b, 7),
                        thumb_rx: i16_at(b, 9),
                        thumb_ry: i16_at(b, 11),
                    }),
                    MAX_ENCODED_LEN,
                ))
            }
            TAG_GAMEPAD_CONNECT => {
                let b = need(3)?;
                if b[0] as usize >= MAX_GAMEPADS {
                    return Err(DecodeError::InvalidValue("index de manette > 3"));
                }
                let kind = GamepadKind::from_u8(b[1]).ok_or(DecodeError::InvalidValue("type de manette inconnu"))?;
                let virt =
                    VirtualPad::from_u8(b[2]).ok_or(DecodeError::InvalidValue("manette virtuelle inconnue"))?;
                Ok((InputEvent::GamepadConnect { index: b[0], kind, virt }, 4))
            }
            TAG_GAMEPAD_DISCONNECT => {
                let b = need(1)?;
                if b[0] as usize >= MAX_GAMEPADS {
                    return Err(DecodeError::InvalidValue("index de manette > 3"));
                }
                Ok((InputEvent::GamepadDisconnect { index: b[0] }, 2))
            }
            other => Err(DecodeError::UnknownTag(other)),
        }
    }
}

/// Encode plusieurs evenements dans un seul tampon (ex. un datagramme).
pub fn encode_batch(events: &[InputEvent]) -> Vec<u8> {
    let mut out = Vec::with_capacity(events.iter().map(InputEvent::encoded_len).sum());
    for e in events {
        e.encode_into(&mut out);
    }
    out
}

/// Decode un tampon contenant 0..n evenements concatenes. Tout ou rien : la moindre
/// erreur rejette l'ensemble du lot.
pub fn decode_batch(mut data: &[u8]) -> Result<Vec<InputEvent>, DecodeError> {
    let mut out = Vec::new();
    while !data.is_empty() {
        let (ev, used) = InputEvent::decode_prefix(data)?;
        out.push(ev);
        data = &data[used..];
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Vec<InputEvent> {
        vec![
            InputEvent::Key { scancode: 0x1E, extended: false, pressed: true },
            InputEvent::Key { scancode: 0x4D, extended: true, pressed: false },
            InputEvent::MouseMove { dx: -32768, dy: 32767 },
            InputEvent::MouseMove { dx: 0, dy: 0 },
            InputEvent::MouseAbs { x: 0, y: 65535 },
            InputEvent::MouseButton { button: MouseButton::Left, pressed: true },
            InputEvent::MouseButton { button: MouseButton::X2, pressed: false },
            InputEvent::Wheel { horizontal: -120, vertical: 240 },
            InputEvent::Gamepad(GamepadState {
                index: 3,
                buttons: xbutton::A | xbutton::DPAD_UP | xbutton::Y,
                left_trigger: 255,
                right_trigger: 7,
                thumb_lx: -32768,
                thumb_ly: 32767,
                thumb_rx: 1,
                thumb_ry: -1,
            }),
            InputEvent::Gamepad(GamepadState { index: 0, buttons: xbutton::TOUCHPAD | xbutton::GUIDE, ..Default::default() }),
            InputEvent::GamepadConnect { index: 0, kind: GamepadKind::Xbox, virt: VirtualPad::Xbox360 },
            InputEvent::GamepadConnect { index: 3, kind: GamepadKind::DualSense, virt: VirtualPad::DualShock4 },
            InputEvent::GamepadConnect { index: 2, kind: GamepadKind::Generic, virt: VirtualPad::Xbox360 },
            InputEvent::GamepadDisconnect { index: 1 },
        ]
    }

    #[test]
    fn roundtrip_all_variants() {
        for e in samples() {
            let bytes = e.encode();
            assert_eq!(bytes.len(), e.encoded_len(), "{e:?}");
            assert!(bytes.len() <= MAX_ENCODED_LEN);
            assert_eq!(InputEvent::decode(&bytes), Ok(e));
        }
    }

    #[test]
    fn known_wire_bytes() {
        let k = InputEvent::Key { scancode: 0x1D, extended: true, pressed: false };
        assert_eq!(k.encode(), [0x01, 0x03, 0x1D]);
        let m = InputEvent::MouseMove { dx: 1, dy: -2 };
        assert_eq!(m.encode(), [0x02, 1, 0, 0xFE, 0xFF]);
    }

    #[test]
    fn batch_roundtrip() {
        let s = samples();
        let bytes = encode_batch(&s);
        assert_eq!(decode_batch(&bytes).unwrap(), s);
        assert_eq!(decode_batch(&[]).unwrap(), vec![]);
        // un lot dont le dernier evenement est tronque est rejete en entier
        assert!(decode_batch(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(InputEvent::decode(&[]), Err(DecodeError::Empty));
        assert_eq!(InputEvent::decode(&[0x00]), Err(DecodeError::UnknownTag(0)));
        assert_eq!(InputEvent::decode(&[0xFF, 1, 2]), Err(DecodeError::UnknownTag(0xFF)));
        assert_eq!(InputEvent::decode(&[0x01, 0x00]), Err(DecodeError::Truncated));
        assert_eq!(InputEvent::decode(&[0x02, 1, 2, 3]), Err(DecodeError::Truncated));
        assert_eq!(InputEvent::decode(&[0x01, 0x00, 0x1E, 0x00]), Err(DecodeError::TrailingBytes));
        assert!(matches!(InputEvent::decode(&[0x01, 0x04, 0x1E]), Err(DecodeError::InvalidValue(_))));
        for sc in [0x00u8, 0xE0, 0xE1] {
            assert!(matches!(InputEvent::decode(&[0x01, 0x00, sc]), Err(DecodeError::InvalidValue(_))));
        }
        assert!(matches!(InputEvent::decode(&[0x04, 5, 1]), Err(DecodeError::InvalidValue(_))));
        assert!(matches!(InputEvent::decode(&[0x04, 0, 2]), Err(DecodeError::InvalidValue(_))));
    }

    #[test]
    fn rejects_bad_gamepad() {
        let good = InputEvent::Gamepad(GamepadState { index: 0, ..Default::default() }).encode();
        assert!(InputEvent::decode(&good).is_ok());
        let mut bad_index = good.clone();
        bad_index[1] = 4;
        assert!(matches!(InputEvent::decode(&bad_index), Err(DecodeError::InvalidValue(_))));
        // bit 11 = pave tactile (extension GRX) : accepte
        let mut touch = good.clone();
        touch[3] = 0x08;
        assert!(InputEvent::decode(&touch).is_ok());
        assert_eq!(InputEvent::decode(&good[..good.len() - 1]), Err(DecodeError::Truncated));
    }

    #[test]
    fn gamepad_connect_wire_format_and_rejections() {
        let c = InputEvent::GamepadConnect { index: 2, kind: GamepadKind::DualShock4, virt: VirtualPad::DualShock4 };
        assert_eq!(c.encode(), [0x07, 2, 1, 1]);
        assert_eq!(InputEvent::GamepadDisconnect { index: 3 }.encode(), [0x08, 3]);
        assert!(matches!(InputEvent::decode(&[0x07, 4, 0, 0]), Err(DecodeError::InvalidValue(_))));
        assert!(matches!(InputEvent::decode(&[0x07, 0, 5, 0]), Err(DecodeError::InvalidValue(_))));
        assert!(matches!(InputEvent::decode(&[0x07, 0, 0, 2]), Err(DecodeError::InvalidValue(_))));
        assert_eq!(InputEvent::decode(&[0x07, 0, 0]), Err(DecodeError::Truncated));
        assert_eq!(InputEvent::decode(&[0x07, 0, 0, 0, 0]), Err(DecodeError::TrailingBytes));
        assert!(matches!(InputEvent::decode(&[0x08, 4]), Err(DecodeError::InvalidValue(_))));
        assert_eq!(InputEvent::decode(&[0x08]), Err(DecodeError::Truncated));
        for v in 0..=255u8 {
            assert_eq!(GamepadKind::from_u8(v).map(GamepadKind::to_u8), (v <= 4).then_some(v));
            assert_eq!(VirtualPad::from_u8(v).map(VirtualPad::to_u8), (v <= 1).then_some(v));
        }
    }

    /// Les formats existants (tags 0x01..=0x06) sont inchanges : octets figes.
    #[test]
    fn legacy_gamepad_bytes_unchanged() {
        let g = InputEvent::Gamepad(GamepadState {
            index: 1,
            buttons: 0x1001,
            left_trigger: 2,
            right_trigger: 3,
            thumb_lx: 4,
            thumb_ly: -1,
            thumb_rx: 0,
            thumb_ry: 0,
        });
        assert_eq!(g.encode(), [0x06, 1, 0x01, 0x10, 2, 3, 4, 0, 0xFF, 0xFF, 0, 0, 0, 0]);
    }

    /// Petit PRNG deterministe (xorshift64*), pas de dependance externe.
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
    fn fuzz_never_panics_and_valid_results_reencode_identically() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut accepted = 0u32;
        for _ in 0..200_000 {
            let len = (rng.next() % 20) as usize;
            let mut buf: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            // biaise le premier octet vers les tags valides pour exercer les chemins
            if !buf.is_empty() && !rng.next().is_multiple_of(4) {
                buf[0] = (rng.next() % 10) as u8;
            }
            if let Ok(ev) = InputEvent::decode(&buf) {
                accepted += 1;
                assert_eq!(ev.encode(), buf, "decodage non canonique pour {buf:?}");
            }
            let _ = decode_batch(&buf);
            let _ = InputEvent::decode_prefix(&buf);
        }
        assert!(accepted > 1000, "le fuzz doit atteindre des cas valides ({accepted})");
    }

    #[test]
    fn exhaustive_short_inputs() {
        for a in 0..=255u8 {
            let _ = InputEvent::decode(&[a]);
            for b in 0..=255u8 {
                let _ = InputEvent::decode(&[a, b]);
            }
        }
    }
}
