//! Survol de l'overlay : ce qui est sous la souris locale (curseur local/distant, icône) et ce qui
//! ne doit pas partir au host.
use super::*;
use grx_render::winit::event::DeviceId;

fn ov() -> Overlay {
    let mut o = Overlay::new("pc", "Ctrl+Alt+Maj+G");
    o.set_view(1280, 720, 1.0);
    o.host = Some(HostInfo { bitrate_cap_kbps: None, fps_cap: None, host_max_bitrate_kbps: 50_000, host_fps: 60 });
    o.monitor_hz = 144;
    o
}

fn center(r: Rect) -> (f32, f32) {
    (r.x + r.w / 2.0, r.y + r.h / 2.0)
}

#[test]
fn float_button_hover_is_clickable_with_generous_zone() {
    let mut o = ov();
    o.on_ev(Ev::Move(640.0, 400.0));
    assert_eq!(o.hover(), OverlayHover::None, "loin de tout : comportement normal");
    let r = o.float_rect();
    let (cx, cy) = center(r);
    assert!(o.on_ev(Ev::Move(cx, cy)).0, "mouvement sur le bouton : pas pour le host");
    assert_eq!(o.hover(), OverlayHover::Clickable);
    // Juste à côté du dessin, dans la marge cliquable.
    let (x, y) = (r.x - FLOAT_HIT_PAD + 1.0, cy);
    assert!(o.on_ev(Ev::Move(x, y)).0);
    assert_eq!(o.hover(), OverlayHover::Clickable);
    assert!(o.on_ev(Ev::Down(x, y)).0, "clic dans la marge : consommé");
    assert_eq!(o.on_ev(Ev::Up), (true, vec![Action::MenuOpened]));
    // Au-delà de la marge : plus le bouton, et le mouvement repart au host.
    let mut o = ov();
    let (x, y) = (r.x - FLOAT_HIT_PAD - 2.0, cy);
    assert!(!o.on_ev(Ev::Move(x, y)).0);
    assert_eq!(o.hover(), OverlayHover::None);
    assert_eq!(o.on_ev(Ev::Down(x, y)), (false, vec![]));
}

#[test]
fn wheel_over_float_button_is_not_sent_to_host() {
    let mut o = ov();
    let (cx, cy) = center(o.float_rect());
    o.on_ev(Ev::Move(cx, cy));
    assert_eq!(o.on_ev(Ev::Wheel(1.0)), (true, vec![]));
    o.on_ev(Ev::Move(640.0, 400.0));
    assert_eq!(o.on_ev(Ev::Wheel(1.0)), (false, vec![]));
}

#[test]
fn no_float_hover_in_capture_or_while_a_game_button_is_held() {
    let mut o = ov();
    let (cx, cy) = center(o.float_rect());
    o.capture = true;
    assert!(!o.on_ev(Ev::Move(cx, cy)).0);
    assert_eq!(o.hover(), OverlayHover::None);
    o.capture = false;
    o.host_drag = true;
    assert!(!o.on_ev(Ev::Move(cx, cy)).0, "glisser dans le jeu : le host garde la souris");
    assert_eq!(o.hover(), OverlayHover::None);
    assert_eq!(o.on_ev(Ev::Down(cx, cy)), (false, vec![]));
}

#[test]
fn menu_hover_kinds() {
    let mut o = ov();
    o.open_menu();
    let p = o.panel_rect();
    o.on_ev(Ev::Move(p.x + 60.0, p.y + TITLE_H / 2.0));
    assert_eq!(o.hover(), OverlayHover::Move, "barre de titre : poignée");
    o.on_ev(Ev::Move(center(o.close_rect()).0, center(o.close_rect()).1));
    assert_eq!(o.hover(), OverlayHover::Clickable);
    let (x, y) = center(o.row_rect(o.row_index(Row::Fullscreen)));
    o.on_ev(Ev::Move(x, y));
    assert_eq!(o.hover(), OverlayHover::Clickable);
    o.on_ev(Ev::Move(p.x + p.w / 2.0, p.y + p.h - 3.0));
    assert_eq!(o.hover(), OverlayHover::Inert, "fond du panneau");
    o.on_ev(Ev::Move(5.0, 700.0));
    assert_eq!(o.hover(), OverlayHover::None, "hors du menu (clic = fermeture)");
    // Ligne désactivée (entrées coupées) : pas de main.
    o.input_enabled = false;
    let (x, y) = center(o.row_rect(o.row_index(Row::Capture)));
    o.on_ev(Ev::Move(x, y));
    assert_eq!(o.hover(), OverlayHover::Inert);
}

#[test]
fn stats_panel_is_a_move_handle_only_with_menu_open() {
    let mut o = ov();
    o.show_stats = true;
    let (x, y) = center(o.stats_rect());
    o.on_ev(Ev::Move(x, y));
    assert_eq!(o.hover(), OverlayHover::None, "menu fermé : panneau non interactif");
    o.open_menu();
    o.on_ev(Ev::Move(x, y));
    // Le panneau des stats peut être sous le menu selon les positions : seulement s'il est dégagé.
    if !o.panel_rect().contains(x, y) {
        assert_eq!(o.hover(), OverlayHover::Move);
    }
}

#[test]
fn place_cursor_on_menu_targets_panel_center_in_physical_pixels() {
    let mut o = ov();
    o.set_view(2560, 1440, 2.0);
    o.open_menu();
    let p = o.panel_rect();
    let (px, py) = o.place_cursor_on_menu();
    assert!((px - ((p.x + p.w / 2.0) * 2.0) as f64).abs() < 0.5);
    assert!((py - ((p.y + p.h / 2.0) * 2.0) as f64).abs() < 0.5);
    assert_ne!(o.hover(), OverlayHover::None, "le curseur est sur le menu");
}

fn mouse(state: ElementState, button: WinButton) -> WindowEvent {
    WindowEvent::MouseInput { device_id: DeviceId::dummy(), state, button }
}

#[test]
fn right_click_on_float_button_is_kept_but_releases_always_pass() {
    let mut o = ov();
    let mut inp = OverlayInput::default();
    let (cx, cy) = center(o.float_rect());
    o.on_ev(Ev::Move(cx, cy));
    assert!(inp.window_event(&mut o, &mouse(ElementState::Pressed, WinButton::Right)).consumed);
    // Bouton enfoncé dans le jeu puis relâché au-dessus du bouton flottant : le host doit le recevoir.
    let mut o = ov();
    let mut inp = OverlayInput::default();
    o.on_ev(Ev::Move(640.0, 400.0));
    assert!(!inp.window_event(&mut o, &mouse(ElementState::Pressed, WinButton::Right)).consumed);
    assert!(o.host_drag, "bouton tenu côté host");
    o.on_ev(Ev::Move(cx, cy));
    assert_eq!(o.hover(), OverlayHover::None, "pas de bouton flottant pendant le glisser");
    assert!(!inp.window_event(&mut o, &mouse(ElementState::Released, WinButton::Right)).consumed);
    assert!(!o.host_drag);
}
