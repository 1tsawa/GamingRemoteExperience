//! Mappage des coordonnées absolues de la souris entre l'image capturée et le bureau virtuel Windows.
//!
//! La souris du client envoie (x, y) normalisés dans 0..=65535 sur l'image de l'écran capturé.
//! Le host doit convertir ces coordonnées en valeurs normalisées 0..=65535 pour `SendInput`
//! avec `MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`.
//! Windows applique ensuite : `pixel = vstart + floor(n * vlen / 65536)`.

/// Rectangle en pixels du bureau virtuel (gauche/haut peuvent être négatifs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct DeskRect {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

impl DeskRect {
    pub const fn new(left: i32, top: i32, width: u32, height: u32) -> Self {
        Self { left, top, width, height }
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub fn right(&self) -> i64 {
        self.left as i64 + self.width as i64
    }

    pub fn bottom(&self) -> i64 {
        self.top as i64 + self.height as i64
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        let r = self.right();
        let b = self.bottom();
        self.left <= x && (x as i64) < r && self.top <= y && (y as i64) < b
    }

    /// Plus petit rectangle englobant (union). Si l'un est vide, renvoie l'autre.
    pub fn union(&self, o: &DeskRect) -> DeskRect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        let l = self.left.min(o.left);
        let t = self.top.min(o.top);
        let r = self.right().max(o.right());
        let b = self.bottom().max(o.bottom());
        Self {
            left: l,
            top: t,
            width: (r - l as i64) as u32,
            height: (b - t as i64) as u32,
        }
    }
}

/// Rectangle englobant d'une liste (bureau virtuel = union des écrans actifs) ; None si la liste n'a aucun rectangle non vide.
pub fn bounding(rects: &[DeskRect]) -> Option<DeskRect> {
    let mut acc: Option<DeskRect> = None;
    for r in rects {
        if r.is_empty() {
            continue;
        }
        acc = Some(match acc {
            Some(a) => a.union(r),
            None => *r,
        });
    }
    acc
}

/// Pixel du bureau virtuel visé par (x, y) normalisés dans `out` :
/// px = out.left + round(x * (out.width - 1) / 65535), idem en y ;
/// résultat toujours dans out (calcul en i64/u64, pas de débordement).
pub fn pixel_in_output(x: u16, y: u16, out: &DeskRect) -> (i32, i32) {
    if out.is_empty() {
        return (out.left, out.top);
    }
    let w = out.width as i64;
    let h = out.height as i64;
    // Arrondi entier : (num + div/2) / div
    let px = out.left + ((x as i64 * (w - 1) + 32767) / 65535) as i32;
    let py = out.top + ((y as i64 * (h - 1) + 32767) / 65535) as i32;
    (px, py)
}

/// Coordonnée normalisée VIRTUALDESK qui fait tomber Windows exactement sur le pixel `p` (centre du pixel) :
/// n = ((p - vstart) * 65536 + 32768) / vlen, borné à 0..=65535 (p hors du bureau : borné).
/// Une dimension virtuelle nulle donne 0.
pub fn to_virtual_norm(p: i32, vstart: i32, vlen: u32) -> u16 {
    if vlen == 0 {
        return 0;
    }
    let num = (p as i64 - vstart as i64) * 65536 + 32768;
    (num / vlen as i64).clamp(0, 65535) as u16
}

/// Combinaison : (x, y) normalisés dans l'écran `out` -> (nx, ny) normalisés VIRTUALDESK dans `vdesk`.
/// Si `out` ou `vdesk` est vide : None.
pub fn map_abs(x: u16, y: u16, out: &DeskRect, vdesk: &DeskRect) -> Option<(u16, u16)> {
    if out.is_empty() || vdesk.is_empty() {
        return None;
    }
    let (px, py) = pixel_in_output(x, y, out);
    let nx = to_virtual_norm(px, vdesk.left, vdesk.width);
    let ny = to_virtual_norm(py, vdesk.top, vdesk.height);
    Some((nx, ny))
}

/// Inverse de la conversion Windows, utile aux tests : pixel obtenu par Windows pour un normalisé n :
/// vstart + floor(n * vlen / 65536).
pub fn windows_pixel(n: u16, vstart: i32, vlen: u32) -> i32 {
    vstart + ((n as u64 * vlen as u64) / 65536) as i32
}

/// Cible partagée de la souris absolue : (écran capturé, bureau virtuel), écrite par le thread
/// vidéo à chaque bascule, lue par le thread d'injection. `None` : comportement historique
/// (coordonnées sur l'écran principal).
#[derive(Debug, Default)]
pub struct AbsTarget(std::sync::Mutex<Option<(DeskRect, DeskRect)>>);

impl AbsTarget {
    pub fn set(&self, v: Option<(DeskRect, DeskRect)>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = v;
    }

    pub fn get(&self) -> Option<(DeskRect, DeskRect)> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 2560;
    const H: u32 = 1440;
    const MAIN: DeskRect = DeskRect::new(0, 0, W, H);

    fn configs() -> Vec<Vec<DeskRect>> {
        vec![
            vec![MAIN, DeskRect::new(2560, 0, W, H)],
            vec![MAIN, DeskRect::new(-2560, 0, W, H)],
            vec![MAIN, DeskRect::new(0, -1440, W, H)],
            vec![MAIN, DeskRect::new(-2560, -300, W, H)],
            vec![MAIN, DeskRect::new(2560, 200, 1920, 1080)],
        ]
    }

    #[test]
    fn test_bounding() {
        for cfg in configs() {
            let b = bounding(&cfg).expect("bounding non vide");
            let expected_left = cfg.iter().map(|r| r.left).min().unwrap();
            let expected_top = cfg.iter().map(|r| r.top).min().unwrap();
            let expected_right = cfg.iter().map(|r| r.right()).max().unwrap();
            let expected_bottom = cfg.iter().map(|r| r.bottom()).max().unwrap();
            assert_eq!(b.left, expected_left);
            assert_eq!(b.top, expected_top);
            assert_eq!(b.width, (expected_right - expected_left as i64) as u32);
            assert_eq!(b.height, (expected_bottom - expected_top as i64) as u32);
        }
        assert_eq!(bounding(&[]), None);
    }

    #[test]
    fn test_corners_and_center() {
        for cfg in configs() {
            for screen in &cfg {
                if screen.is_empty() {
                    continue;
                }
                let vdesk = bounding(&cfg).unwrap();
                let check_corner = |x: u16, y: u16, exp_px: i32, exp_py: i32| {
                    let (nx, ny) = map_abs(x, y, screen, &vdesk).unwrap();
                    let (got_px, got_py) = (
                        windows_pixel(nx, vdesk.left, vdesk.width),
                        windows_pixel(ny, vdesk.top, vdesk.height),
                    );
                    assert_eq!(got_px, exp_px, "x={x}, y={y}");
                    assert_eq!(got_py, exp_py, "x={x}, y={y}");
                };
                // haut-gauche
                check_corner(0, 0, screen.left, screen.top);
                // bas-droite
                check_corner(
                    65535,
                    65535,
                    screen.left + screen.width as i32 - 1,
                    screen.top + screen.height as i32 - 1,
                );
                // centre
                let cx = screen.left + (screen.width as i32) / 2;
                let cy = screen.top + (screen.height as i32) / 2;
                let (nx, ny) = map_abs(32768, 32768, screen, &vdesk).unwrap();
                let (got_px, got_py) = (
                    windows_pixel(nx, vdesk.left, vdesk.width),
                    windows_pixel(ny, vdesk.top, vdesk.height),
                );
                assert!((got_px - cx).abs() <= 1, "centre x : got {got_px}, exp {cx}");
                assert!((got_py - cy).abs() <= 1, "centre y : got {got_py}, exp {cy}");
            }
        }
    }

    #[test]
    fn test_pixel_always_in_target() {
        for cfg in configs() {
            let vdesk = bounding(&cfg).unwrap();
            for screen in &cfg {
                if screen.is_empty() {
                    continue;
                }
                for x in (0..=65535).step_by(257) {
                    for y in (0..=65535).step_by(257) {
                        let (nx, ny) = map_abs(x, y, screen, &vdesk).unwrap();
                        let px = windows_pixel(nx, vdesk.left, vdesk.width);
                        let py = windows_pixel(ny, vdesk.top, vdesk.height);
                        assert!(
                            screen.contains(px, py),
                            "({x}, {y}) -> ({px}, {py}) hors {screen:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_to_virtual_norm_roundtrip() {
        for cfg in configs() {
        let vdesk = bounding(&cfg).unwrap();
        // test pixels dans vdesk
        for p in (vdesk.left..vdesk.right() as i32).step_by(7) {
            let n = to_virtual_norm(p, vdesk.left, vdesk.width);
            assert_eq!(windows_pixel(n, vdesk.left, vdesk.width), p);
        }
        for p in (vdesk.top..vdesk.bottom() as i32).step_by(7) {
            let n = to_virtual_norm(p, vdesk.top, vdesk.height);
            assert_eq!(windows_pixel(n, vdesk.top, vdesk.height), p);
        }
        }
        let vdesk = bounding(&configs()[0]).unwrap();
        // bornage hors bureau
        assert_eq!(to_virtual_norm(vdesk.left - 100, vdesk.left, vdesk.width), 0);
        assert_eq!(to_virtual_norm(vdesk.right() as i32 + 100, vdesk.left, vdesk.width), 65535);
        // vlen 0
        assert_eq!(to_virtual_norm(0, 0, 0), 0);
    }

    #[test]
    fn test_map_abs_empty() {
        assert!(map_abs(0, 0, &DeskRect::default(), &MAIN).is_none());
        assert!(map_abs(0, 0, &MAIN, &DeskRect::default()).is_none());
    }

    #[test]
    fn test_deskrect_methods() {
        let r = DeskRect::new(-100, -50, 200, 100);
        assert!(!r.is_empty());
        assert_eq!(r.right(), 100);
        assert_eq!(r.bottom(), 50);
        assert!(r.contains(-100, -50));
        assert!(r.contains(99, 49));
        assert!(!r.contains(100, 50));
        assert!(!r.contains(-101, -50));

        let u = r.union(&DeskRect::new(-50, 0, 100, 50));
        assert_eq!(u, r); // contenu

        let u2 = r.union(&DeskRect::new(100, 50, 100, 50));
        assert_eq!(u2, DeskRect::new(-100, -50, 300, 150));

        let empty = DeskRect::default();
        assert!(empty.is_empty());
        assert_eq!(empty.union(&r), r);
        assert_eq!(r.union(&empty), r);
    }
}
