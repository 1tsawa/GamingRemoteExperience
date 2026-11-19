//! Quel curseur montrer : le curseur Windows LOCAL ou le curseur DISTANT dessiné dans la vidéo ?
//!
//! L'overlay (bouton flottant, menu, panneaux) fait son hit-test sur la position locale de la
//! souris. Le curseur distant, lui, arrive du host avec du retard, peut être mis à l'échelle,
//! figé ou caché par le jeu : viser l'overlay avec lui revient à deviner. Donc dès que la souris
//! est sur un élément interactif de l'overlay (ou que le menu est ouvert), on montre le curseur
//! local et on ne dessine plus le curseur distant ; ailleurs, comportement normal (curseur local
//! masqué au-dessus de la vidéo quand le curseur distant est actif).
//!
//! Logique pure, sans winit ni Windows (testée dans `overlay_cursor_tests.rs`).

/// Ce qu'il y a sous la souris côté overlay (voir `Overlay::hover`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverlayHover {
    /// Pas d'élément de l'overlay sous la souris.
    #[default]
    None,
    /// Élément de l'overlay non cliquable (fond du menu, ligne désactivée).
    Inert,
    /// Élément cliquable (bouton flottant, ligne, curseur de réglage, bouton…).
    Clickable,
    /// Poignée de déplacement (titre du menu, panneau des stats) ou déplacement en cours.
    Move,
}

/// Icône du curseur local.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorIcon {
    #[default]
    Default,
    /// Main (élément cliquable).
    Pointer,
    Move,
}

/// Entrées de la décision.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CursorInputs {
    pub menu_open: bool,
    pub hover: OverlayHover,
    /// Capture souris relative active (curseur local masqué et confiné par `input.rs`).
    pub captured: bool,
    /// Un curseur distant a déjà été reçu (il est dessiné à la place du curseur local).
    pub remote_active: bool,
    /// La souris locale est au-dessus de l'image vidéo (pas sur les bandes noires).
    pub over_video: bool,
}

/// Décision appliquée par `LocalCursorHider`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorDecision {
    /// Visibilité du curseur local ; `None` = ne pas y toucher (mode capture, géré par `input.rs`).
    pub local_visible: Option<bool>,
    /// Dessiner le curseur distant dans la vidéo.
    pub draw_remote: bool,
    pub icon: CursorIcon,
}

/// L'overlay a-t-il la main sur la souris (menu ouvert, ou survol d'un de ses éléments hors
/// capture) ? Dans ce cas les mouvements et clics ne partent pas au host.
pub fn overlay_owns_pointer(i: &CursorInputs) -> bool {
    i.menu_open || (!i.captured && i.hover != OverlayHover::None)
}

/// Icône du curseur local pour ce survol.
pub fn icon_for(hover: OverlayHover) -> CursorIcon {
    match hover {
        OverlayHover::Clickable => CursorIcon::Pointer,
        OverlayHover::Move => CursorIcon::Move,
        OverlayHover::None | OverlayHover::Inert => CursorIcon::Default,
    }
}

/// Curseur local visible ? curseur distant dessiné ? quelle icône ?
pub fn decide(i: CursorInputs) -> CursorDecision {
    if overlay_owns_pointer(&i) {
        return CursorDecision { local_visible: Some(true), draw_remote: false, icon: icon_for(i.hover) };
    }
    CursorDecision {
        // Comportement normal : en capture, `input.rs` gère ; sinon curseur local masqué au-dessus
        // de la vidéo dès qu'un curseur distant le remplace (visible sur les bandes noires).
        local_visible: (!i.captured).then_some(!(i.remote_active && i.over_video)),
        draw_remote: true,
        icon: CursorIcon::Default,
    }
}

#[cfg(test)]
#[path = "overlay_cursor_tests.rs"]
mod tests;
