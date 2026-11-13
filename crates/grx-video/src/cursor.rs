//! Curseur du host lu depuis DXGI Desktop Duplication (position, visibilité, forme), SANS
//! l'incruster dans l'image : il est envoyé à part au client, qui le dessine par-dessus la vidéo.
//!
//! DXGI livre la forme dans trois formats (`DXGI_OUTDUPL_POINTER_SHAPE_TYPE`) ; on les convertit
//! en deux calques RGBA de même taille :
//! * `rgba` : couleur + alpha **non prémultiplié** (mélange alpha classique) ;
//! * `xor` (optionnel) : pixels qui **inversent** l'écran. Le client les dessine avec un mélange
//!   `src*(1-dst) + dst*(1-src)`, ce qui vaut exactement un XOR pour des canaux à 0 ou 255
//!   (cas réel : curseur texte « I » monochrome, viseurs) et une approximation douce ailleurs.
//!
//! | Type | Pixel source | `rgba` | `xor` |
//! |---|---|---|---|
//! | MONOCHROME (1 bpp, hauteur ×2 : masque AND puis XOR) | AND=0 XOR=0 | noir opaque | - |
//! | | AND=0 XOR=1 | blanc opaque | - |
//! | | AND=1 XOR=0 | transparent | - |
//! | | AND=1 XOR=1 | transparent | blanc (inversion) |
//! | COLOR (BGRA 32 bpp, alpha) | B,G,R,A | R,G,B,A | - |
//! | MASKED_COLOR (BGRA, octet A = masque) | A=0x00 | R,G,B opaque | - |
//! | | A=0xFF, RGB=0 | transparent | - |
//! | | A=0xFF, RGB≠0 | transparent | R,G,B (XOR de cette couleur) |
//!
//! Une forme plus grande que [`MAX_CURSOR_DIM`] est réduite (plus proche voisin, hotspot
//! recalculé) : le protocole borne la taille.

use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Côté maximal d'une forme de curseur transmise (pixels).
pub const MAX_CURSOR_DIM: u32 = 256;

pub const SHAPE_MONOCHROME: u32 = 1;
pub const SHAPE_COLOR: u32 = 2;
pub const SHAPE_MASKED_COLOR: u32 = 4;

/// Forme de curseur convertie. `rgba` et `xor` font `width * height * 4` octets (ordre R,G,B,A).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    /// Couleur, alpha non prémultiplié.
    pub rgba: Vec<u8>,
    /// Calque d'inversion (alpha = 255 là où le pixel inverse l'écran), `None` s'il est vide.
    pub xor: Option<Vec<u8>>,
}

/// Convertit le tampon de `IDXGIOutputDuplication::GetFramePointerShape`.
///
/// `shape_type`, `width`, `height` (pour MONOCHROME : hauteur des DEUX masques), `pitch`,
/// `hot_x/hot_y` viennent de `DXGI_OUTDUPL_POINTER_SHAPE_INFO`. Renvoie `None` si les
/// dimensions sont nulles, le type inconnu ou le tampon trop court (jamais de panique).
pub fn convert_pointer_shape(
    shape_type: u32,
    width: u32,
    height: u32,
    pitch: u32,
    hot_x: u32,
    hot_y: u32,
    buf: &[u8],
) -> Option<CursorImage> {
    if width == 0 || height == 0 || width > 4096 || height > 8192 {
        return None;
    }
    let pitch = pitch as usize;
    let (w, h) = (width as usize, height as usize);
    let mut rgba;
    let mut xor = vec![0u8; 0];
    let mut any_xor = false;
    let out_h;
    match shape_type {
        SHAPE_MONOCHROME => {
            if h < 2 || pitch < w.div_ceil(8) {
                return None;
            }
            out_h = h / 2;
            if buf.len() < pitch.checked_mul(out_h * 2)? {
                return None;
            }
            rgba = vec![0u8; w * out_h * 4];
            xor = vec![0u8; w * out_h * 4];
            for y in 0..out_h {
                for x in 0..w {
                    let bit = 0x80u8 >> (x % 8);
                    let and = buf[y * pitch + x / 8] & bit != 0;
                    let xr = buf[(y + out_h) * pitch + x / 8] & bit != 0;
                    let o = (y * w + x) * 4;
                    match (and, xr) {
                        (false, false) => rgba[o..o + 4].copy_from_slice(&[0, 0, 0, 255]),
                        (false, true) => rgba[o..o + 4].copy_from_slice(&[255, 255, 255, 255]),
                        (true, false) => {}
                        (true, true) => {
                            xor[o..o + 4].copy_from_slice(&[255, 255, 255, 255]);
                            any_xor = true;
                        }
                    }
                }
            }
        }
        SHAPE_COLOR | SHAPE_MASKED_COLOR => {
            if pitch < w * 4 {
                return None;
            }
            out_h = h;
            if buf.len() < pitch.checked_mul(h - 1)?.checked_add(w * 4)? {
                return None;
            }
            rgba = vec![0u8; w * h * 4];
            if shape_type == SHAPE_MASKED_COLOR {
                xor = vec![0u8; w * h * 4];
            }
            for y in 0..h {
                for x in 0..w {
                    let s = y * pitch + x * 4;
                    let (b, g, r, a) = (buf[s], buf[s + 1], buf[s + 2], buf[s + 3]);
                    let o = (y * w + x) * 4;
                    if shape_type == SHAPE_COLOR {
                        rgba[o..o + 4].copy_from_slice(&[r, g, b, a]);
                    } else if a == 0 {
                        rgba[o..o + 4].copy_from_slice(&[r, g, b, 255]);
                    } else if r | g | b != 0 {
                        // Masque à 0xFF (toute autre valeur est traitée pareil) : XOR avec l'écran.
                        xor[o..o + 4].copy_from_slice(&[r, g, b, 255]);
                        any_xor = true;
                    }
                }
            }
        }
        _ => return None,
    }
    let img = CursorImage {
        width,
        height: out_h as u32,
        hot_x: hot_x.min(width - 1),
        hot_y: hot_y.min(out_h as u32 - 1),
        rgba,
        xor: if any_xor { Some(xor) } else { None },
    };
    Some(downscale_to_fit(img, MAX_CURSOR_DIM))
}

/// Réduit (plus proche voisin) une forme dont un côté dépasse `max`, en conservant le ratio.
pub fn downscale_to_fit(img: CursorImage, max: u32) -> CursorImage {
    if img.width <= max && img.height <= max {
        return img;
    }
    let scale = (max as f64 / img.width as f64).min(max as f64 / img.height as f64);
    let nw = ((img.width as f64 * scale).floor() as u32).clamp(1, max);
    let nh = ((img.height as f64 * scale).floor() as u32).clamp(1, max);
    let sample = |src: &[u8]| {
        let mut out = vec![0u8; (nw * nh * 4) as usize];
        for y in 0..nh {
            let sy = ((y as u64 * img.height as u64) / nh as u64) as u32;
            for x in 0..nw {
                let sx = ((x as u64 * img.width as u64) / nw as u64) as u32;
                let s = ((sy * img.width + sx) * 4) as usize;
                let o = ((y * nw + x) * 4) as usize;
                out[o..o + 4].copy_from_slice(&src[s..s + 4]);
            }
        }
        out
    };
    CursorImage {
        width: nw,
        height: nh,
        hot_x: ((img.hot_x as u64 * nw as u64) / img.width as u64).min(nw as u64 - 1) as u32,
        hot_y: ((img.hot_y as u64 * nh as u64) / img.height as u64).min(nh as u64 - 1) as u32,
        rgba: sample(&img.rgba),
        xor: img.xor.as_deref().map(sample),
    }
}

/// État courant du curseur du bureau capturé.
#[derive(Clone, Debug)]
pub struct CursorState {
    /// Incrémenté à chaque changement (position, visibilité, forme, taille du bureau).
    pub serial: u64,
    /// Incrémenté à chaque nouvelle forme.
    pub shape_serial: u64,
    pub visible: bool,
    /// Coin supérieur gauche de la forme, en pixels du bureau capturé (peut être négatif).
    pub x: i32,
    pub y: i32,
    /// Taille du bureau capturé (référence des coordonnées).
    pub frame_w: u32,
    pub frame_h: u32,
    pub shape: Option<Arc<CursorImage>>,
    /// Instant de la dernière mise à jour lue dans DXGI.
    pub updated_at: Instant,
}

impl CursorState {
    /// Position du point chaud (là où « pointe » le curseur), en pixels du bureau.
    pub fn hotspot_pos(&self) -> (i32, i32) {
        let (hx, hy) = self.shape.as_ref().map_or((0, 0), |s| (s.hot_x as i32, s.hot_y as i32));
        (self.x.saturating_add(hx), self.y.saturating_add(hy))
    }
}

/// Point de rendez-vous thread-safe entre le thread de capture (écrivain) et la boucle réseau
/// (lectrice, « dernier état gagnant »).
pub struct CursorTracker {
    state: Mutex<CursorState>,
}

impl Default for CursorTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorTracker {
    pub fn new() -> Self {
        CursorTracker {
            state: Mutex::new(CursorState {
                serial: 0,
                shape_serial: 0,
                visible: false,
                x: 0,
                y: 0,
                frame_w: 0,
                frame_h: 0,
                shape: None,
                updated_at: Instant::now(),
            }),
        }
    }

    /// Copie de l'état courant (la forme est partagée par `Arc`).
    pub fn snapshot(&self) -> CursorState {
        self.state.lock().unwrap().clone()
    }

    /// Numéro de série courant (lecture bon marché pour savoir si quelque chose a changé).
    pub fn serial(&self) -> u64 {
        self.state.lock().unwrap().serial
    }

    /// Mise à jour depuis une image DXGI : `pos` = `(visible, x, y)` si `LastMouseUpdateTime != 0`,
    /// `shape` si une nouvelle forme a été lue.
    pub fn update(&self, pos: Option<(bool, i32, i32)>, shape: Option<CursorImage>, frame_w: u32, frame_h: u32) {
        let mut s = self.state.lock().unwrap();
        let mut changed = false;
        if let Some((v, x, y)) = pos {
            if (v, x, y) != (s.visible, s.x, s.y) {
                s.visible = v;
                s.x = x;
                s.y = y;
                changed = true;
            }
        }
        if let Some(img) = shape {
            if s.shape.as_deref() != Some(&img) {
                s.shape = Some(Arc::new(img));
                s.shape_serial += 1;
                changed = true;
            }
        }
        if (frame_w, frame_h) != (s.frame_w, s.frame_h) {
            s.frame_w = frame_w;
            s.frame_h = frame_h;
            changed = true;
        }
        if changed {
            s.serial += 1;
            s.updated_at = Instant::now();
        }
    }
}

#[cfg(test)]
#[path = "cursor_tests.rs"]
mod tests;

/// Curseur système maintenant : `(affiché, x, y)` en pixels du bureau virtuel (physiques si le
/// processus est « per-monitor DPI aware », comme `grx-host run`). `None` si l'appel échoue
/// (bureau sécurisé…). Sert à repositionner le curseur distant juste après une bascule d'écran.
pub fn cursor_screen_pos() -> Option<(bool, i32, i32)> {
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorInfo, CURSORINFO, CURSOR_SHOWING};
    let mut ci = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
    // SAFETY : structure initialisée avec sa taille, pointeur valide le temps de l'appel.
    unsafe { GetCursorInfo(&mut ci) }.ok()?;
    Some((ci.flags.0 & CURSOR_SHOWING.0 != 0, ci.ptScreenPos.x, ci.ptScreenPos.y))
}
