use super::*;

fn ov() -> Overlay {
    let mut o = Overlay::new("pc", "Ctrl+Alt+Maj+G");
    o.set_view(1280, 720, 1.0);
    o.host = Some(HostInfo { bitrate_cap_kbps: None, fps_cap: None, host_max_bitrate_kbps: 50_000, host_fps: 60 });
    o.monitor_hz = 144;
    o
}

fn screen(name: &str, left: i32, primary: bool) -> DisplayInfo {
    DisplayInfo {
        name: name.to_string(),
        left,
        top: 0,
        width: 2560,
        height: 1440,
        primary,
        is_virtual: false,
    }
}

fn three() -> DisplayList {
    DisplayList {
        active: Some(2),
        displays: vec![
            screen("VG27A", -5120, false),
            screen("VG27A", -2560, false),
            screen("VDD", 0, true),
        ],
    }
}

fn center(r: Rect) -> (f32, f32) {
    (r.x + r.w / 2.0, r.y + r.h / 2.0)
}

#[test]
fn display_row_hidden_with_zero_or_one_screen() {
    let mut o = ov();
    assert_eq!(o.rows().len(), 13);
    assert!(!o.rows().contains(&Row::Display));
    let h0 = o.panel_rect().h;

    o.displays = vec![screen("A", 0, true)];
    assert_eq!(o.rows().len(), 13);
    assert!(!o.rows().contains(&Row::Display));

    o.displays = three().displays;
    assert_eq!(o.rows().len(), 14);
    let rows = o.rows();
    let fps_idx = rows.iter().position(|&r| r == Row::Fps).unwrap();
    let disp_idx = rows.iter().position(|&r| r == Row::Display).unwrap();
    assert_eq!(disp_idx, fps_idx + 1);
    assert!((o.panel_rect().h - h0 - ROW_H).abs() < 0.01);
}

#[test]
fn hotkeys_ctrl_shift_arrows() {
    assert_eq!(classify_hotkey(KeyCode::ArrowLeft, true, true, false), Some(Hot::PrevDisplay));
    assert_eq!(classify_hotkey(KeyCode::ArrowRight, true, true, false), Some(Hot::NextDisplay));
    assert_eq!(classify_hotkey(KeyCode::ArrowLeft, false, true, false), None);
    assert_eq!(classify_hotkey(KeyCode::ArrowLeft, true, false, false), None);
    assert_eq!(classify_hotkey(KeyCode::ArrowLeft, true, true, true), None);
}

#[test]
fn cycle_wraps_and_sends_select() {
    let mut o = ov();
    assert!(o.set_displays(three()).is_empty(), "aucune préférence : rien à redemander");
    assert_eq!(o.on_hotkey(Hot::NextDisplay), vec![Action::SelectDisplay(0)]);
    assert_eq!(o.active_display, Some(0));

    assert_eq!(o.on_hotkey(Hot::PrevDisplay), vec![Action::SelectDisplay(2)]);

    let mut o = ov();
    o.displays = vec![screen("A", 0, true)];
    assert!(o.cycle_display(1).is_empty());
    assert!(o.cycle_display(-1).is_empty());
}

#[test]
fn click_on_display_button_selects_it() {
    let mut o = ov();
    o.open_menu();
    o.set_displays(three());
    let rects = o.display_rects();
    let (_, r1) = rects[1];
    let (x, y) = center(r1);
    let (c, a) = o.on_ev(Ev::Down(x, y));
    assert!(c);
    assert_eq!(a, vec![Action::SelectDisplay(1)]);
    assert_eq!(o.active_display, Some(1));
    assert_eq!(o.preferred_display, Some(display_key(&three().displays[1])));

    let (c, a) = o.on_ev(Ev::Down(x, y));
    assert!(c);
    assert!(a.is_empty());

    let (_, r2) = rects[2];
    let (x2, y2) = center(r2);
    assert_eq!(o.hit_at(x2, y2), Some(Hit::Display(2)));
}

#[test]
fn keyboard_navigation_on_display_row() {
    let mut o = ov();
    o.open_menu();
    o.set_displays(three());
    o.focus = o.row_index(Row::Display);
    let (_, a) = o.on_ev(Ev::Nav(Nav::Right));
    assert_eq!(a, vec![Action::SelectDisplay(0)]);
    assert_eq!(o.active_display, Some(0));

    let (_, a) = o.on_ev(Ev::Nav(Nav::Left));
    assert_eq!(a, vec![Action::SelectDisplay(2)]);
    assert_eq!(o.active_display, Some(2));
}

#[test]
fn remembered_display_is_requested_once_if_present() {
    let mut o = ov();
    let pref_key = display_key(&screen("VG27A", -2560, false));
    o.apply_prefs(&OverlayPrefs { display: Some(pref_key.clone()), ..Default::default() });
    assert_eq!(o.set_displays(three()), vec![Action::SelectDisplay(1)]);
    assert_eq!(o.set_displays(three()), vec![]);

    let mut o = ov();
    let bad_key = display_key(&screen("Bad", 9999, false));
    o.apply_prefs(&OverlayPrefs { display: Some(bad_key.clone()), ..Default::default() });
    assert_eq!(o.set_displays(three()), vec![]);
    assert_eq!(o.prefs().display, Some(bad_key));

    let mut o = ov();
    o.apply_prefs(&OverlayPrefs { display: Some(display_key(&screen("VDD", 0, true))), ..Default::default() });
    o.set_displays(three());
    assert_eq!(o.set_displays(three()), vec![]);
}

#[test]
fn host_announce_corrects_active_and_keeps_focus() {
    let mut o = ov();
    o.open_menu();
    o.set_displays(three());
    o.focus = o.row_index(Row::Disconnect);
    assert_eq!(o.rows()[o.focus], Row::Disconnect);

    o.set_displays(DisplayList { active: Some(0), displays: vec![screen("A", 0, true)] });
    assert_eq!(o.rows()[o.focus], Row::Disconnect);

    let mut o = ov();
    o.open_menu();
    o.set_displays(DisplayList { active: Some(7), displays: three().displays });
    assert_eq!(o.active_display, None);
}

#[test]
fn stats_and_hint_show_current_display() {
    let mut o = ov();
    assert_eq!(o.display_summary(), "-");
    o.set_displays(three());
    assert_eq!(o.display_summary(), "3/3 VDD");

    o.show_stats = true;
    o.stats = Some(StatsSnapshot::default());
    assert!(o.scene().is_some());
}

#[test]
fn prefs_roundtrip_display() {
    let mut o = ov();
    o.open_menu();
    o.set_displays(three());
    o.select_display(1);
    let expected = display_key(&three().displays[1]);
    assert_eq!(o.prefs().display, Some(expected));

    let mut o2 = ov();
    o2.apply_prefs(&o.prefs());
    assert_eq!(o2.preferred_display, o.preferred_display);
}

#[test]
fn hint_fits_and_keeps_resolution_visible() {
    let d = screen("VG27A", -2560, false);
    assert_eq!(display_hint(1, &d), "2: VG27A 2560x1440");
    assert_eq!(display_hint(2, &screen("VDD", 0, true)), "3: VDD 2560x1440*");
    let long = screen("Un moniteur au nom vraiment très long", 0, false);
    let h = display_hint(0, &long);
    assert!(h.chars().count() <= 26 && h.ends_with(" 2560x1440") && h.contains('\u{2026}'), "{h}");
    assert_eq!(display_hint(0, &screen("", 0, false)), "1: Écran 2560x1440");
}
