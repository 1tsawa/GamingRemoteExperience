//! Cote CLIENT : conversion d'evenements Windows (messages fenetre, Raw Input) en [`InputEvent`].
//!
//! Tout est pur (pas d'appel systeme, pas de `HWND`) donc testable ; le code de la boucle de
//! messages appelle simplement ces fonctions avec les champs bruts.
//!
//! Deux sources possibles pour la souris :
//! * **Messages fenetre** (`WM_MOUSEMOVE`, ...) via [`from_window_message`] : position absolue
//!   dans la zone client, normalisee 0..=65535 -> mode "bureau/menus".
//! * **Raw Input** (`WM_INPUT`) via [`mouse_from_raw`] / [`crate::keymap::key_from_raw`] : deltas
//!   bruts sans acceleration -> mode "jeu" (capture souris relative).

use crate::event::{InputEvent, MouseButton};
use crate::keymap::key_from_lparam;

// Messages fenetre (winuser.h).
pub const WM_KEYDOWN: u32 = 0x0100;
pub const WM_KEYUP: u32 = 0x0101;
pub const WM_SYSKEYDOWN: u32 = 0x0104;
pub const WM_SYSKEYUP: u32 = 0x0105;
pub const WM_MOUSEMOVE: u32 = 0x0200;
pub const WM_LBUTTONDOWN: u32 = 0x0201;
pub const WM_LBUTTONUP: u32 = 0x0202;
pub const WM_RBUTTONDOWN: u32 = 0x0204;
pub const WM_RBUTTONUP: u32 = 0x0205;
pub const WM_MBUTTONDOWN: u32 = 0x0207;
pub const WM_MBUTTONUP: u32 = 0x0208;
pub const WM_MOUSEWHEEL: u32 = 0x020A;
pub const WM_XBUTTONDOWN: u32 = 0x020B;
pub const WM_XBUTTONUP: u32 = 0x020C;
pub const WM_MOUSEHWHEEL: u32 = 0x020E;

// RAWMOUSE.usFlags
pub const MOUSE_MOVE_ABSOLUTE: u16 = 0x01;
pub const MOUSE_VIRTUAL_DESKTOP: u16 = 0x02;
// RAWMOUSE.usButtonFlags
pub const RI_MOUSE_LEFT_BUTTON_DOWN: u16 = 0x0001;
pub const RI_MOUSE_LEFT_BUTTON_UP: u16 = 0x0002;
pub const RI_MOUSE_RIGHT_BUTTON_DOWN: u16 = 0x0004;
pub const RI_MOUSE_RIGHT_BUTTON_UP: u16 = 0x0008;
pub const RI_MOUSE_MIDDLE_BUTTON_DOWN: u16 = 0x0010;
pub const RI_MOUSE_MIDDLE_BUTTON_UP: u16 = 0x0020;
pub const RI_MOUSE_BUTTON_4_DOWN: u16 = 0x0040;
pub const RI_MOUSE_BUTTON_4_UP: u16 = 0x0080;
pub const RI_MOUSE_BUTTON_5_DOWN: u16 = 0x0100;
pub const RI_MOUSE_BUTTON_5_UP: u16 = 0x0200;
pub const RI_MOUSE_WHEEL: u16 = 0x0400;
pub const RI_MOUSE_HWHEEL: u16 = 0x0800;

/// Normalise une position client (pixels) en 0..=65535. Les coordonnees hors fenetre
/// (capture souris pendant un glisser) sont saturees. Renvoie `None` si la zone est vide.
pub fn normalize_abs(x: i32, y: i32, width: u32, height: u32) -> Option<InputEvent> {
    if width == 0 || height == 0 {
        return None;
    }
    let norm = |v: i32, size: u32| -> u16 {
        // pixel central : (v + 0.5) / size, sur size-1 pas pour que le dernier pixel donne 65535
        let max = (size - 1).max(1) as i64;
        let v = (v as i64).clamp(0, max);
        ((v * 65535 + max / 2) / max) as u16
    };
    Some(InputEvent::MouseAbs { x: norm(x, width), y: norm(y, height) })
}

fn clamp_i16(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// Extrait (x, y) signes du `lParam` d'un message souris (`GET_X_LPARAM`/`GET_Y_LPARAM`).
pub fn point_from_lparam(lparam: isize) -> (i32, i32) {
    let l = lparam as usize as u32;
    ((l & 0xFFFF) as u16 as i16 as i32, ((l >> 16) & 0xFFFF) as u16 as i16 as i32)
}

/// Delta de molette signe depuis le `wParam` de `WM_MOUSEWHEEL`/`WM_MOUSEHWHEEL`
/// (mot haut, `GET_WHEEL_DELTA_WPARAM`).
pub fn wheel_delta_from_wparam(wparam: usize) -> i16 {
    ((wparam >> 16) & 0xFFFF) as u16 as i16
}

/// Convertit un message fenetre en evenement. `client_size` = taille de la zone client en pixels
/// (pour normaliser la position absolue). Renvoie `None` pour les messages non geres ou invalides.
///
/// Gere : `WM_(SYS)KEYDOWN/UP`, `WM_MOUSEMOVE`, boutons G/D/M/X1/X2, `WM_MOUSEWHEEL`, `WM_MOUSEHWHEEL`.
/// Attention : les coordonnees de `WM_MOUSEWHEEL` sont en pixels **ecran**, on les ignore.
pub fn from_window_message(msg: u32, wparam: usize, lparam: isize, client_size: (u32, u32)) -> Option<InputEvent> {
    let button = |button, pressed| Some(InputEvent::MouseButton { button, pressed });
    match msg {
        WM_KEYDOWN | WM_SYSKEYDOWN => key_from_lparam(lparam, true),
        WM_KEYUP | WM_SYSKEYUP => key_from_lparam(lparam, false),
        WM_MOUSEMOVE => {
            let (x, y) = point_from_lparam(lparam);
            normalize_abs(x, y, client_size.0, client_size.1)
        }
        WM_LBUTTONDOWN => button(MouseButton::Left, true),
        WM_LBUTTONUP => button(MouseButton::Left, false),
        WM_RBUTTONDOWN => button(MouseButton::Right, true),
        WM_RBUTTONUP => button(MouseButton::Right, false),
        WM_MBUTTONDOWN => button(MouseButton::Middle, true),
        WM_MBUTTONUP => button(MouseButton::Middle, false),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            // mot haut de wParam : XBUTTON1 = 1, XBUTTON2 = 2
            let which = match (wparam >> 16) & 0xFFFF {
                1 => MouseButton::X1,
                2 => MouseButton::X2,
                _ => return None,
            };
            button(which, msg == WM_XBUTTONDOWN)
        }
        WM_MOUSEWHEEL => Some(InputEvent::Wheel { horizontal: 0, vertical: wheel_delta_from_wparam(wparam) }),
        WM_MOUSEHWHEEL => Some(InputEvent::Wheel { horizontal: wheel_delta_from_wparam(wparam), vertical: 0 }),
        _ => None,
    }
}

/// `RAWMOUSE` -> evenements. `button_data` est signe pour les molettes.
/// Un meme paquet raw peut contenir mouvement + plusieurs boutons : l'ordre produit est
/// mouvement, boutons, molette verticale, molette horizontale.
///
/// * Mouvement relatif : `lLastX/lLastY` saturees en i16 (un delta nul ne produit rien).
/// * Mouvement absolu (tablettes, RDP/VM) : `lLastX/lLastY` sont deja sur 0..=65535.
pub fn mouse_from_raw(flags: u16, button_flags: u16, button_data: u16, last_x: i32, last_y: i32) -> Vec<InputEvent> {
    let mut out = Vec::with_capacity(2);
    if flags & MOUSE_MOVE_ABSOLUTE != 0 {
        out.push(InputEvent::MouseAbs {
            x: last_x.clamp(0, 65535) as u16,
            y: last_y.clamp(0, 65535) as u16,
        });
    } else if last_x != 0 || last_y != 0 {
        out.push(InputEvent::MouseMove { dx: clamp_i16(last_x), dy: clamp_i16(last_y) });
    }
    const BUTTONS: [(u16, MouseButton, bool); 10] = [
        (RI_MOUSE_LEFT_BUTTON_DOWN, MouseButton::Left, true),
        (RI_MOUSE_LEFT_BUTTON_UP, MouseButton::Left, false),
        (RI_MOUSE_RIGHT_BUTTON_DOWN, MouseButton::Right, true),
        (RI_MOUSE_RIGHT_BUTTON_UP, MouseButton::Right, false),
        (RI_MOUSE_MIDDLE_BUTTON_DOWN, MouseButton::Middle, true),
        (RI_MOUSE_MIDDLE_BUTTON_UP, MouseButton::Middle, false),
        (RI_MOUSE_BUTTON_4_DOWN, MouseButton::X1, true),
        (RI_MOUSE_BUTTON_4_UP, MouseButton::X1, false),
        (RI_MOUSE_BUTTON_5_DOWN, MouseButton::X2, true),
        (RI_MOUSE_BUTTON_5_UP, MouseButton::X2, false),
    ];
    for (bit, button, pressed) in BUTTONS {
        if button_flags & bit != 0 {
            out.push(InputEvent::MouseButton { button, pressed });
        }
    }
    let delta = button_data as i16;
    if button_flags & RI_MOUSE_WHEEL != 0 {
        out.push(InputEvent::Wheel { horizontal: 0, vertical: delta });
    }
    if button_flags & RI_MOUSE_HWHEEL != 0 {
        out.push(InputEvent::Wheel { horizontal: delta, vertical: 0 });
    }
    out
}

/// Accumulateur de mouvements relatifs : plusieurs `WM_INPUT` par frame sont fusionnes en un seul
/// `MouseMove` (reduit le debit reseau sans perdre de mouvement). Les deltas depassant i16 sont
/// emis en plusieurs evenements.
#[derive(Debug, Default)]
pub struct MouseAccumulator {
    dx: i64,
    dy: i64,
}

impl MouseAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, dx: i32, dy: i32) {
        self.dx += dx as i64;
        self.dy += dy as i64;
    }

    /// Vide l'accumulateur en evenements `MouseMove` (aucun si le total est nul).
    pub fn drain(&mut self) -> Vec<InputEvent> {
        let mut out = Vec::new();
        while self.dx != 0 || self.dy != 0 {
            let sx = self.dx.clamp(i16::MIN as i64, i16::MAX as i64);
            let sy = self.dy.clamp(i16::MIN as i64, i16::MAX as i64);
            self.dx -= sx;
            self.dy -= sy;
            out.push(InputEvent::MouseMove { dx: sx as i16, dy: sy as i16 });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abs_normalization() {
        assert_eq!(normalize_abs(0, 0, 1920, 1080), Some(InputEvent::MouseAbs { x: 0, y: 0 }));
        assert_eq!(normalize_abs(1919, 1079, 1920, 1080), Some(InputEvent::MouseAbs { x: 65535, y: 65535 }));
        // milieu
        match normalize_abs(960, 540, 1920, 1080) {
            Some(InputEvent::MouseAbs { x, y }) => {
                assert!((x as i32 - 32768).abs() < 40, "{x}");
                assert!((y as i32 - 32768).abs() < 70, "{y}");
            }
            other => panic!("{other:?}"),
        }
        // hors fenetre : sature
        assert_eq!(normalize_abs(-50, 5000, 100, 100), Some(InputEvent::MouseAbs { x: 0, y: 65535 }));
        // zone degeneree
        assert_eq!(normalize_abs(0, 0, 0, 100), None);
        assert_eq!(normalize_abs(0, 0, 100, 0), None);
        // fenetre 1x1 : pas de division par zero
        assert!(normalize_abs(0, 0, 1, 1).is_some());
        // extremes i32 : pas de panic ni overflow
        let _ = normalize_abs(i32::MIN, i32::MAX, u32::MAX, u32::MAX);
    }

    #[test]
    fn abs_is_monotonic() {
        let mut last = 0u16;
        for x in 0..1920 {
            let InputEvent::MouseAbs { x: n, .. } = normalize_abs(x, 0, 1920, 1080).unwrap() else { panic!() };
            assert!(n >= last);
            last = n;
        }
        assert_eq!(last, 65535);
    }

    #[test]
    fn lparam_points_are_signed() {
        assert_eq!(point_from_lparam(((20u32 << 16) | 10) as isize), (10, 20));
        // x = -5, y = -7 (fenetre avec capture souris)
        let l = (((-7i16 as u16 as u32) << 16) | (-5i16 as u16 as u32)) as i32 as isize;
        assert_eq!(point_from_lparam(l), (-5, -7));
        let _ = point_from_lparam(isize::MIN);
    }

    #[test]
    fn wheel_delta_signed() {
        assert_eq!(wheel_delta_from_wparam(120usize << 16), 120);
        assert_eq!(wheel_delta_from_wparam((-120i16 as u16 as usize) << 16), -120);
    }

    #[test]
    fn window_messages() {
        let size = (800, 600);
        let lp = ((0x1Eu32 << 16) | 1) as isize;
        assert_eq!(
            from_window_message(WM_KEYDOWN, 0x41, lp, size),
            Some(InputEvent::Key { scancode: 0x1E, extended: false, pressed: true })
        );
        assert_eq!(
            from_window_message(WM_SYSKEYUP, 0x12, lp, size),
            Some(InputEvent::Key { scancode: 0x1E, extended: false, pressed: false })
        );
        assert_eq!(
            from_window_message(WM_RBUTTONDOWN, 0, 0, size),
            Some(InputEvent::MouseButton { button: MouseButton::Right, pressed: true })
        );
        assert_eq!(
            from_window_message(WM_XBUTTONUP, 2usize << 16, 0, size),
            Some(InputEvent::MouseButton { button: MouseButton::X2, pressed: false })
        );
        assert_eq!(from_window_message(WM_XBUTTONDOWN, 3usize << 16, 0, size), None);
        assert_eq!(
            from_window_message(WM_MOUSEWHEEL, (-120i16 as u16 as usize) << 16, 0, size),
            Some(InputEvent::Wheel { horizontal: 0, vertical: -120 })
        );
        assert_eq!(
            from_window_message(WM_MOUSEHWHEEL, 120usize << 16, 0, size),
            Some(InputEvent::Wheel { horizontal: 120, vertical: 0 })
        );
        assert_eq!(
            from_window_message(WM_MOUSEMOVE, 0, 0, size),
            Some(InputEvent::MouseAbs { x: 0, y: 0 })
        );
        assert_eq!(from_window_message(0x0001, 0, 0, size), None);
        // pas de panic sur entrees arbitraires
        for msg in [0u32, 0x100, 0x200, 0x20A, 0x20B, u32::MAX] {
            for (w, l) in [(0usize, 0isize), (usize::MAX, -1), (1 << 63, isize::MIN)] {
                let _ = from_window_message(msg, w, l, (0, 0));
                let _ = from_window_message(msg, w, l, (u32::MAX, 1));
            }
        }
    }

    #[test]
    fn raw_mouse_relative_and_buttons() {
        assert_eq!(mouse_from_raw(0, 0, 0, 0, 0), vec![]);
        assert_eq!(mouse_from_raw(0, 0, 0, 5, -3), vec![InputEvent::MouseMove { dx: 5, dy: -3 }]);
        assert_eq!(
            mouse_from_raw(0, 0, 0, 100_000, -100_000),
            vec![InputEvent::MouseMove { dx: i16::MAX, dy: i16::MIN }]
        );
        assert_eq!(
            mouse_from_raw(0, RI_MOUSE_LEFT_BUTTON_DOWN | RI_MOUSE_RIGHT_BUTTON_UP, 0, 1, 0),
            vec![
                InputEvent::MouseMove { dx: 1, dy: 0 },
                InputEvent::MouseButton { button: MouseButton::Left, pressed: true },
                InputEvent::MouseButton { button: MouseButton::Right, pressed: false },
            ]
        );
        assert_eq!(
            mouse_from_raw(0, RI_MOUSE_BUTTON_4_DOWN | RI_MOUSE_BUTTON_5_UP, 0, 0, 0),
            vec![
                InputEvent::MouseButton { button: MouseButton::X1, pressed: true },
                InputEvent::MouseButton { button: MouseButton::X2, pressed: false },
            ]
        );
    }

    #[test]
    fn raw_mouse_wheel_and_absolute() {
        assert_eq!(
            mouse_from_raw(0, RI_MOUSE_WHEEL, (-120i16) as u16, 0, 0),
            vec![InputEvent::Wheel { horizontal: 0, vertical: -120 }]
        );
        assert_eq!(
            mouse_from_raw(0, RI_MOUSE_HWHEEL, 120, 0, 0),
            vec![InputEvent::Wheel { horizontal: 120, vertical: 0 }]
        );
        assert_eq!(
            mouse_from_raw(MOUSE_MOVE_ABSOLUTE | MOUSE_VIRTUAL_DESKTOP, 0, 0, 65535, 70000),
            vec![InputEvent::MouseAbs { x: 65535, y: 65535 }]
        );
        assert_eq!(
            mouse_from_raw(MOUSE_MOVE_ABSOLUTE, 0, 0, -1, 0),
            vec![InputEvent::MouseAbs { x: 0, y: 0 }]
        );
    }

    #[test]
    fn accumulator_merges_and_splits() {
        let mut a = MouseAccumulator::new();
        assert!(a.drain().is_empty());
        a.add(3, -1);
        a.add(4, 1);
        assert_eq!(a.drain(), vec![InputEvent::MouseMove { dx: 7, dy: 0 }]);
        assert!(a.drain().is_empty());
        // total hors i16 : plusieurs evenements, somme conservee
        a.add(70_000, 0);
        let evs = a.drain();
        assert!(evs.len() >= 2);
        let sum: i64 = evs
            .iter()
            .map(|e| match e {
                InputEvent::MouseMove { dx, .. } => *dx as i64,
                _ => panic!(),
            })
            .sum();
        assert_eq!(sum, 70_000);
        assert!(a.drain().is_empty());
    }
}
