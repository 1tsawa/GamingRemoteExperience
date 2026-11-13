//! Module de placement de panneaux d'overlay en DIP (Device Independent Pixels).
//!
//! Fournit des fonctions pures pour calculer les positions haut-gauche de panneaux
//! dans une fenêtre, en respectant une marge et une fraction d'espace libre.
//! Aucune dépendance externe requise.

/// Marge par défaut appliquée aux bords de la fenêtre.
pub const MARGIN: f32 = 8.0;

/// Fraction relative de l'espace libre (axe X et Y).
///
/// Chaque composante appartient à `[0,1]` : `0` correspond au bord gauche/haut
/// moins la marge, `1` au bord droit/bas.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frac {
    pub fx: f32,
    pub fy: f32,
}

impl Frac {
    /// Crée une nouvelle fraction relative.
    pub const fn new(fx: f32, fy: f32) -> Self {
        Self { fx, fy }
    }

    /// Sanitise la fraction : remplace les valeurs non finies par `0.5` et borne
    /// chaque composante à `[0.0, 1.0]`.
    pub fn sanitized(self) -> Self {
        let sanitize = |v: f32| {
            if !v.is_finite() {
                0.5
            } else {
                v.clamp(0.0, 1.0)
            }
        };
        Self {
            fx: sanitize(self.fx),
            fy: sanitize(self.fy),
        }
    }
}

/// Calcule la position haut-gauche d'un panneau en DIP.
///
/// `frac` est d'abord sanitisé. Les valeurs de `view` et `size` non finies ou
/// négatives sont traitées comme `0.0`.
pub fn place(frac: Frac, size: (f32, f32), view: (f32, f32), margin: f32) -> (f32, f32) {
    let frac = frac.sanitized();
    let (vw, vh) = view;
    let (sw, sh) = size;
    (place_axis(vw, sw, margin, frac.fx), place_axis(vh, sh, margin, frac.fy))
}

fn place_axis(view: f32, size: f32, margin: f32, frac: f32) -> f32 {
    let view = if view.is_finite() && view >= 0.0 { view } else { 0.0 };
    let size = if size.is_finite() && size >= 0.0 { size } else { 0.0 };
    let free = view - size - 2.0 * margin;
    if free <= 0.0 {
        if view >= size {
            ((view - size) / 2.0).max(0.0)
        } else {
            0.0
        }
    } else {
        margin + frac * free
    }
}

/// Calcule la fraction relative correspondant à une position donnée.
///
/// Inverse de [`place`]. Le résultat est automatiquement sanitisé.
pub fn frac_of(pos: (f32, f32), size: (f32, f32), view: (f32, f32), margin: f32) -> Frac {
    let (vw, vh) = view;
    let (sw, sh) = size;
    let fx = frac_axis(vw, sw, margin, pos.0);
    let fy = frac_axis(vh, sh, margin, pos.1);
    Frac { fx, fy }.sanitized()
}

fn frac_axis(view: f32, size: f32, margin: f32, pos: f32) -> f32 {
    let view = if view.is_finite() && view >= 0.0 { view } else { 0.0 };
    let size = if size.is_finite() && size >= 0.0 { size } else { 0.0 };
    let free = view - size - 2.0 * margin;
    if free <= 0.0 {
        0.5
    } else {
        ((pos - margin) / free).clamp(0.0, 1.0)
    }
}

/// Ramène une position hors écran vers une position valide et entièrement visible.
///
/// Équivalent à `place(frac_of(pos, size, view, margin), size, view, margin)`.
#[cfg_attr(not(test), allow(dead_code))] // utilisé par les tests (invariant « toujours visible »)
pub fn clamp_pos(pos: (f32, f32), size: (f32, f32), view: (f32, f32), margin: f32) -> (f32, f32) {
    place(frac_of(pos, size, view, margin), size, view, margin)
}

/// Préréglages de positionnement standard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    TopLeft,
    TopRight,
    Center,
    BottomLeft,
    BottomRight,
}

impl Preset {
    /// Liste ordonnée des préréglages.
    pub const ALL: [Preset; 5] = [
        Preset::TopLeft,
        Preset::TopRight,
        Preset::Center,
        Preset::BottomLeft,
        Preset::BottomRight,
    ];

    /// Retourne la fraction relative associée au préréglage.
    pub fn frac(self) -> Frac {
        match self {
            Preset::TopLeft => Frac::new(0.0, 0.0),
            Preset::TopRight => Frac::new(1.0, 0.0),
            Preset::Center => Frac::new(0.5, 0.5),
            Preset::BottomLeft => Frac::new(0.0, 1.0),
            Preset::BottomRight => Frac::new(1.0, 1.0),
        }
    }

    /// Libellé descriptif en français.
    pub fn label(self) -> &'static str {
        match self {
            Preset::TopLeft => "Haut-gauche",
            Preset::TopRight => "Haut-droite",
            Preset::Center => "Centre",
            Preset::BottomLeft => "Bas-gauche",
            Preset::BottomRight => "Bas-droite",
        }
    }

    /// Glyphe Unicode associé au préréglage.
    pub fn glyph(self) -> &'static str {
        match self {
            Preset::TopLeft => "\u{2196}",
            Preset::TopRight => "\u{2197}",
            Preset::Center => "\u{25A3}",
            Preset::BottomLeft => "\u{2199}",
            Preset::BottomRight => "\u{2198}",
        }
    }

    /// Retourne le préréglage dont la fraction est à moins de `0.01` sur chaque axe.
    pub fn matching(frac: Frac) -> Option<Preset> {
        let f = frac.sanitized();
        for p in &Self::ALL {
            let pf = p.frac();
            if (f.fx - pf.fx).abs() < 0.01 && (f.fy - pf.fy).abs() < 0.01 {
                return Some(*p);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn place_presets_1280x720_460x500() {
        let view = (1280.0, 720.0);
        let size = (460.0, 500.0);
        let m = MARGIN;
        assert_eq!(place(Preset::TopLeft.frac(), size, view, m), (8.0, 8.0));
        assert_eq!(place(Preset::TopRight.frac(), size, view, m), (812.0, 8.0));
        assert_eq!(place(Preset::Center.frac(), size, view, m), (410.0, 110.0));
        assert_eq!(place(Preset::BottomLeft.frac(), size, view, m), (8.0, 212.0));
        assert_eq!(place(Preset::BottomRight.frac(), size, view, m), (812.0, 212.0));
    }

    #[test]
    fn roundtrip_frac_of_place() {
        let view = (1280.0, 720.0);
        let size = (460.0, 500.0);
        let m = MARGIN;
        for p in Preset::ALL {
            let f = p.frac();
            let pos = place(f, size, view, m);
            let f2 = frac_of(pos, size, view, m);
            assert!((f.fx - f2.fx).abs() < 1e-5, "fx mismatch for {:?}", p);
            assert!((f.fy - f2.fy).abs() < 1e-5, "fy mismatch for {:?}", p);
        }
    }

    #[test]
    fn frac_consistent_across_sizes() {
        let size = (460.0, 500.0);
        let m = MARGIN;
        let f = Preset::TopRight.frac();
        let pos1 = place(f, size, (1280.0, 720.0), m);
        let pos2 = place(f, size, (1920.0, 1080.0), m);
        let f1 = frac_of(pos1, size, (1280.0, 720.0), m);
        let f2 = frac_of(pos2, size, (1920.0, 1080.0), m);
        assert!((f1.fx - f2.fx).abs() < 1e-5, "TR should stick to right across sizes");
        assert!((f1.fy - f2.fy).abs() < 1e-5);
    }

    #[test]
    fn panel_larger_than_view_gives_zero() {
        let view = (100.0, 100.0);
        let size = (200.0, 200.0);
        let m = MARGIN;
        let pos = place(Frac::new(0.0, 0.0), size, view, m);
        assert_eq!(pos.0, 0.0);
        assert_eq!(pos.1, 0.0);
    }

    #[test]
    fn clamp_pos_keeps_fully_visible() {
        let view = (1280.0, 720.0);
        let size = (460.0, 500.0);
        let m = MARGIN;
        // Position hors écran négative
        let p1 = clamp_pos((-10.0, -10.0), size, view, m);
        assert!(p1.0 >= m, "x should be >= margin");
        assert!(p1.0 + size.0 <= view.0 - m, "x+w should be <= view-m");
        // Position hors écran positive
        let p2 = clamp_pos((1300.0, 800.0), size, view, m);
        assert!(p2.0 >= m);
        assert!(p2.0 + size.0 <= view.0 - m);
    }

    #[test]
    fn nan_sanitized() {
        let f = Frac::new(f32::NAN, f32::INFINITY);
        let s = f.sanitized();
        assert_eq!(s.fx, 0.5);
        assert_eq!(s.fy, 0.5);
    }

    #[test]
    fn preset_matching() {
        assert_eq!(Preset::matching(Frac::new(0.0, 0.0)), Some(Preset::TopLeft));
        assert_eq!(Preset::matching(Frac::new(0.005, 0.005)), Some(Preset::TopLeft));
        assert_eq!(Preset::matching(Frac::new(0.99, 0.99)), Some(Preset::BottomRight));
        assert_eq!(Preset::matching(Frac::new(0.5, 0.5)), Some(Preset::Center));
        assert_eq!(Preset::matching(Frac::new(0.3, 0.3)), None);
    }
}
