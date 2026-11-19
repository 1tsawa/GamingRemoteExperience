use super::*;

fn d(local_visible: Option<bool>, draw_remote: bool, icon: CursorIcon) -> CursorDecision {
    CursorDecision { local_visible, draw_remote, icon }
}

/// Jeu normal : souris sur la vidéo, curseur distant actif, menu fermé, pas de capture.
fn playing() -> CursorInputs {
    CursorInputs { remote_active: true, over_video: true, ..Default::default() }
}

#[test]
fn normal_behaviour_outside_overlay_is_unchanged() {
    assert_eq!(decide(playing()), d(Some(false), true, CursorIcon::Default), "curseur distant à la place du local");
    let bars = CursorInputs { over_video: false, ..playing() };
    assert_eq!(decide(bars), d(Some(true), true, CursorIcon::Default), "bandes noires : curseur local");
    let no_remote = CursorInputs { remote_active: false, ..playing() };
    assert_eq!(decide(no_remote), d(Some(true), true, CursorIcon::Default), "aucun curseur distant reçu");
    let captured = CursorInputs { captured: true, ..playing() };
    assert_eq!(decide(captured), d(None, true, CursorIcon::Default), "capture : géré par input.rs");
}

#[test]
fn hovering_overlay_shows_local_cursor_and_hides_remote() {
    let h = |hover| decide(CursorInputs { hover, ..playing() });
    assert_eq!(h(OverlayHover::Clickable), d(Some(true), false, CursorIcon::Pointer));
    assert_eq!(h(OverlayHover::Move), d(Some(true), false, CursorIcon::Move));
    assert_eq!(h(OverlayHover::Inert), d(Some(true), false, CursorIcon::Default));
}

#[test]
fn open_menu_always_wins_even_from_capture() {
    let m = CursorInputs { menu_open: true, captured: true, ..playing() };
    assert_eq!(decide(m), d(Some(true), false, CursorIcon::Default));
    let m = CursorInputs { menu_open: true, hover: OverlayHover::Clickable, ..playing() };
    assert_eq!(decide(m), d(Some(true), false, CursorIcon::Pointer));
    // Menu ouvert sur les bandes noires, sans curseur distant : toujours le curseur local.
    let m = CursorInputs { menu_open: true, ..Default::default() };
    assert_eq!(decide(m), d(Some(true), false, CursorIcon::Default));
}

#[test]
fn capture_without_menu_ignores_hover() {
    let c = CursorInputs { captured: true, hover: OverlayHover::Clickable, ..playing() };
    assert_eq!(decide(c), d(None, true, CursorIcon::Default));
    assert!(!overlay_owns_pointer(&c));
}

#[test]
fn leaving_overlay_restores_exact_previous_decision() {
    let before = decide(playing());
    assert_ne!(decide(CursorInputs { hover: OverlayHover::Clickable, ..playing() }), before);
    assert_eq!(decide(playing()), before);
    let after_menu = decide(CursorInputs { menu_open: false, ..playing() });
    assert_eq!(after_menu, before);
}

#[test]
fn icons_and_pointer_ownership() {
    assert_eq!(icon_for(OverlayHover::None), CursorIcon::Default);
    assert_eq!(icon_for(OverlayHover::Inert), CursorIcon::Default);
    assert_eq!(icon_for(OverlayHover::Clickable), CursorIcon::Pointer);
    assert_eq!(icon_for(OverlayHover::Move), CursorIcon::Move);
    assert!(!overlay_owns_pointer(&playing()));
    assert!(overlay_owns_pointer(&CursorInputs { hover: OverlayHover::Inert, ..playing() }));
    assert!(overlay_owns_pointer(&CursorInputs { menu_open: true, captured: true, ..Default::default() }));
}
