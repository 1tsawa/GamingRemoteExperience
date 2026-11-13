use super::*;

fn ov() -> Overlay {
    let mut o = Overlay::new("pc", "Ctrl+Alt+Maj+G");
    o.set_view(1280, 720, 1.0);
    o.host = Some(HostInfo { bitrate_cap_kbps: None, fps_cap: None, host_max_bitrate_kbps: 50_000, host_fps: 60 });
    o.monitor_hz = 144;
    o
}

fn inside(r: Rect, w: f32, h: f32) -> bool {
    r.x >= 0.0 && r.y >= 0.0 && r.x + r.w <= w + 0.01 && r.y + r.h <= h + 0.01
}

#[test]
fn menu_drags_by_title_bar() {
    let mut o = ov();
    o.open_menu();
    let p = o.panel_rect();
    let title_pt = (p.x + 60.0, p.y + TITLE_H / 2.0);
    o.on_ev(Ev::Move(title_pt.0, title_pt.1));
    assert!(o.wants_move_cursor());
    let (c, a) = o.on_ev(Ev::Down(title_pt.0, title_pt.1));
    assert!(c);
    assert!(a.is_empty());
    
    let new_pt = (title_pt.0 + 200.0, title_pt.1 + 100.0);
    o.on_ev(Ev::Move(new_pt.0, new_pt.1));
    let p2 = o.panel_rect();
    assert!((p2.x - (p.x + 200.0)).abs() < 0.5);
    assert!((p2.y - (p.y + 100.0)).abs() < 0.5);
    
    let (c, a) = o.on_ev(Ev::Up);
    assert!(c);
    assert!(a.is_empty());
    assert!(!o.is_dragging());
    assert!(o.menu_open);
}

#[test]
fn menu_drag_is_clamped_inside_window() {
    let mut o = ov();
    o.open_menu();
    let p = o.panel_rect();
    let title_pt = (p.x + 60.0, p.y + TITLE_H / 2.0);
    o.on_ev(Ev::Move(title_pt.0, title_pt.1));
    o.on_ev(Ev::Down(title_pt.0, title_pt.1));
    
    o.on_ev(Ev::Move(-5000.0, -5000.0));
    assert!(inside(o.panel_rect(), 1280.0, 720.0));
    assert!((o.panel_rect().x - MARGIN).abs() < 0.5);
    
    o.on_ev(Ev::Move(5000.0, 5000.0));
    assert!(inside(o.panel_rect(), 1280.0, 720.0));
    assert!((o.panel_rect().x + o.panel_rect().w - (1280.0 - MARGIN)).abs() < 0.5);
    
    o.on_ev(Ev::Up);
}

#[test]
fn click_in_rows_does_not_drag() {
    let mut o = ov();
    o.open_menu();
    let r = o.row_rect(row_index(Row::Stats));
    let center = (r.x + r.w / 2.0, r.y + r.h / 2.0);
    o.on_ev(Ev::Move(center.0, center.1));
    o.on_ev(Ev::Down(center.0, center.1));
    o.on_ev(Ev::Up);
    
    let p1 = o.panel_rect();
    o.on_ev(Ev::Move(center.0 + 100.0, center.1));
    let p2 = o.panel_rect();
    assert!((p1.x - p2.x).abs() < 0.5);
    assert!((p1.y - p2.y).abs() < 0.5);
}

#[test]
fn stats_drag_only_when_menu_open() {
    let mut o = ov();
    o.show_stats = true;
    let stats_r = o.stats_rect();
    let center = (stats_r.x + stats_r.w / 2.0, stats_r.y + stats_r.h / 2.0);
    
    o.on_ev(Ev::Move(center.0, center.1));
    let (c, a) = o.on_ev(Ev::Down(center.0, center.1));
    assert!(!c);
    assert!(a.is_empty());
    let p1 = o.stats_rect();
    o.on_ev(Ev::Move(center.0 + 100.0, center.1));
    let p2 = o.stats_rect();
    assert!((p1.x - p2.x).abs() < 0.5);
    o.on_ev(Ev::Up);
    
    o.open_menu();
    o.menu_pos = Preset::BottomRight.frac();
    let stats_r2 = o.stats_rect();
    let center2 = (stats_r2.x + stats_r2.w / 2.0, stats_r2.y + stats_r2.h / 2.0);
    o.on_ev(Ev::Move(center2.0, center2.1));
    let (c, a) = o.on_ev(Ev::Down(center2.0, center2.1));
    assert!(c);
    assert!(a.is_empty());
    
    let p3 = o.stats_rect();
    o.on_ev(Ev::Move(center2.0 + 300.0, center2.1 + 200.0));
    let p4 = o.stats_rect();
    assert!((p4.x - (p3.x + 300.0)).abs() < 0.5);
    assert!((p4.y - (p3.y + 200.0)).abs() < 0.5);
    
    let (c, a) = o.on_ev(Ev::Up);
    assert!(c);
    assert!(a.is_empty());
    assert!(o.menu_open);
}

#[test]
fn float_button_drag_moves_without_opening() {
    let mut o = ov();
    o.on_ev(Ev::Move(640.0, 10.0));
    let fr = o.float_rect();
    let center = (fr.x + fr.w / 2.0, fr.y + fr.h / 2.0);
    o.on_ev(Ev::Move(center.0, center.1));
    let (c, a) = o.on_ev(Ev::Down(center.0, center.1));
    assert!(c);
    assert!(a.is_empty());
    
    let fr1 = o.float_rect();
    o.on_ev(Ev::Move(center.0 + 2.0, center.1));
    let fr2 = o.float_rect();
    assert!((fr1.x - fr2.x).abs() < 0.5);
    assert!((fr1.y - fr2.y).abs() < 0.5);
    
    o.on_ev(Ev::Move(100.0, 600.0));
    let fr3 = o.float_rect();
    assert!((fr1.x - fr3.x).abs() > 0.5);
    assert!(inside(fr3, 1280.0, 720.0));
    
    let (c, a) = o.on_ev(Ev::Up);
    assert!(c);
    assert!(a.is_empty());
    assert!(!o.menu_open);
    
    let fr4 = o.float_rect();
    let center4 = (fr4.x + fr4.w / 2.0, fr4.y + fr4.h / 2.0);
    o.on_ev(Ev::Move(center4.0, center4.1));
    o.on_ev(Ev::Down(center4.0, center4.1));
    let (c, a) = o.on_ev(Ev::Up);
    assert!(c);
    assert_eq!(a, vec![Action::MenuOpened]);
    assert!(o.menu_open);
}

#[test]
fn presets_and_reset_rows() {
    let mut o = ov();
    o.open_menu();
    
    let stats_pos_rects = o.preset_rects(Row::StatsPos);
    let center1 = (stats_pos_rects[1].x + stats_pos_rects[1].w / 2.0, stats_pos_rects[1].y + stats_pos_rects[1].h / 2.0);
    o.on_ev(Ev::Move(center1.0, center1.1));
    o.on_ev(Ev::Down(center1.0, center1.1));
    assert_eq!(o.stats_pos, Preset::TopRight.frac());
    let sr = o.stats_rect();
    assert!((sr.x + sr.w - (1280.0 - MARGIN)).abs() < 0.5);
    o.on_ev(Ev::Up);
    
    let menu_pos_rects = o.preset_rects(Row::MenuPos);
    let center2 = (menu_pos_rects[4].x + menu_pos_rects[4].w / 2.0, menu_pos_rects[4].y + menu_pos_rects[4].h / 2.0);
    o.on_ev(Ev::Move(center2.0, center2.1));
    o.on_ev(Ev::Down(center2.0, center2.1));
    let pr = o.panel_rect();
    assert!((pr.x + pr.w - (1280.0 - MARGIN)).abs() < 0.5);
    assert!((pr.y + pr.h - (720.0 - MARGIN)).abs() < 0.5);
    o.on_ev(Ev::Up);
    
    let reset_rect = o.preset_rects(Row::MenuPos)[5]; // le menu a bougé : rectangles recalculés
    let center3 = (reset_rect.x + reset_rect.w / 2.0, reset_rect.y + reset_rect.h / 2.0);
    o.on_ev(Ev::Move(center3.0, center3.1));
    o.on_ev(Ev::Down(center3.0, center3.1));
    assert_eq!(o.menu_pos, DEFAULT_MENU_POS);
    o.on_ev(Ev::Up);
    
    o.focus = row_index(Row::StatsPos);
    o.on_ev(Ev::Nav(Nav::Right));
    assert_eq!(o.stats_pos, Preset::Center.frac());
}

#[test]
fn positions_are_relative_and_survive_resize() {
    let mut o = ov();
    o.open_menu();
    o.menu_pos = Preset::BottomRight.frac();
    
    o.set_view(1920, 1080, 1.0);
    let p = o.panel_rect();
    assert!((p.x + p.w - (1920.0 - MARGIN)).abs() < 0.5);
    assert!((p.y + p.h - (1080.0 - MARGIN)).abs() < 0.5);
    
    o.set_view(800, 600, 1.0);
    assert!(inside(o.panel_rect(), 800.0, 600.0));
    
    o.set_view(2560, 1440, 2.0);
    let p = o.panel_rect();
    assert!((p.x + p.w - (1280.0 - MARGIN)).abs() < 0.5);
    assert!((p.y + p.h - (720.0 - MARGIN)).abs() < 0.5);
}

#[test]
fn prefs_roundtrip_through_overlay() {
    let mut o = ov();
    o.open_menu();
    o.stats_pos = Preset::TopRight.frac();
    o.button_pos = Preset::BottomLeft.frac();
    o.volume = 0.3;
    o.muted = true;
    o.vsync = true;
    o.show_stats = true;
    o.button_enabled = false;
    o.bitrate_cap_mbps = Some(20);
    o.fps_cap = Some(30);
    
    let p = o.prefs();
    
    let mut o2 = ov();
    o2.apply_prefs(&p);
    assert_eq!(o2.prefs(), p);
    
    assert_eq!(OverlayPrefs::from_json(&p.to_json()), p);
}
