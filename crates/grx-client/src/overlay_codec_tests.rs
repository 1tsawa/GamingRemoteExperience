use super::*;
use grx_render::DecoderPref;

#[test]
fn test_rows_order_after_vsync() {
    let mut o = Overlay::new("pc", "Ctrl+Alt+Maj+G");
    o.set_view(1280, 720, 1.0);
    o.open_menu();
    let rows = o.rows();
    let vsync_idx = rows.iter().position(|&r| r == Row::Vsync).unwrap();
    assert_eq!(rows.get(vsync_idx + 1), Some(&Row::Decoder));
    assert_eq!(rows.get(vsync_idx + 2), Some(&Row::Color444));
    assert_eq!(rows.get(vsync_idx + 3), Some(&Row::TenBit));
}

#[test]
fn test_decoder_cycling() {
    let mut o = Overlay::new("pc", "Ctrl+Alt+Maj+G");
    o.set_view(1280, 720, 1.0);
    o.open_menu();
    o.focus = o.row_index(Row::Decoder);

    let (_, actions) = o.on_ev(Ev::Nav(Nav::Right));
    assert_eq!(o.decoder, DecoderPref::Hardware);
    assert!(actions.iter().any(|a| matches!(a, Action::SetCodecPrefs { decoder: DecoderPref::Hardware, yuv444: false, ten_bit: false })));

    let (_, actions) = o.on_ev(Ev::Nav(Nav::Right));
    assert_eq!(o.decoder, DecoderPref::Software);
    assert!(actions.iter().any(|a| matches!(a, Action::SetCodecPrefs { decoder: DecoderPref::Software, yuv444: false, ten_bit: false })));

    let (_, actions) = o.on_ev(Ev::Nav(Nav::Right));
    assert_eq!(o.decoder, DecoderPref::Auto);
    assert!(actions.iter().any(|a| matches!(a, Action::SetCodecPrefs { decoder: DecoderPref::Auto, yuv444: false, ten_bit: false })));

    let (_, actions) = o.on_ev(Ev::Nav(Nav::Left));
    assert_eq!(o.decoder, DecoderPref::Software);
    assert!(actions.iter().any(|a| matches!(a, Action::SetCodecPrefs { decoder: DecoderPref::Software, yuv444: false, ten_bit: false })));
}

#[test]
fn test_color444_and_tenbit_toggles() {
    let mut o = Overlay::new("pc", "Ctrl+Alt+Maj+G");
    o.set_view(1280, 720, 1.0);
    o.open_menu();

    o.focus = o.row_index(Row::Color444);
    let (_, actions) = o.on_ev(Ev::Nav(Nav::Activate));
    assert!(o.yuv444);
    assert!(actions.iter().any(|a| matches!(a, Action::SetCodecPrefs { yuv444: true, .. })));

    let (_, _) = o.on_ev(Ev::Nav(Nav::Left));
    assert!(!o.yuv444);

    o.focus = o.row_index(Row::TenBit);
    let (_, actions) = o.on_ev(Ev::Nav(Nav::Activate));
    assert!(o.ten_bit);
    assert!(actions.iter().any(|a| matches!(a, Action::SetCodecPrefs { ten_bit: true, .. })));

    let (_, _) = o.on_ev(Ev::Nav(Nav::Left));
    assert!(!o.ten_bit);
}

#[test]
fn test_prefs_and_apply_prefs() {
    let mut o = Overlay::new("pc", "Ctrl+Alt+Maj+G");
    o.set_view(1280, 720, 1.0);
    o.open_menu();

    o.decoder = DecoderPref::Software;
    o.yuv444 = true;
    o.ten_bit = true;

    let p = o.prefs();
    assert_eq!(p.decoder, "software");
    assert!(p.yuv444);
    assert!(p.ten_bit);

    let mut o2 = Overlay::new("pc", "Ctrl+Alt+Maj+G");
    o2.set_view(1280, 720, 1.0);
    o2.apply_prefs(&p);
    assert_eq!(o2.decoder, DecoderPref::Software);
    assert!(o2.yuv444);
    assert!(o2.ten_bit);
}

#[test]
fn test_prefs_sanitization_and_default() {
    let bogus = OverlayPrefs { decoder: "bogus".into(), ..Default::default() };
    let sanitized = bogus.sanitized();
    assert_eq!(sanitized.decoder, "auto");

    let default_prefs = OverlayPrefs::default();
    assert_eq!(default_prefs.decoder, "auto");
}

#[test]
fn test_panel_rect_fits_in_view() {
    let mut o = Overlay::new("pc", "Ctrl+Alt+Maj+G");
    o.set_view(1280, 720, 1.0);
    o.open_menu();
    o.show_shortcuts = true;

    let rect = o.panel_rect();
    assert!(rect.y >= 0.0);
    assert!(rect.y + rect.h <= 720.0);

    o.set_view(800, 600, 1.0);
    let rect2 = o.panel_rect();
    assert!(rect2.y >= 0.0);
    assert!(rect2.y + rect2.h <= 600.0);
}

#[test]
fn test_stats_snapshot_contains_encoder_text() {
    let mut o = Overlay::new("pc", "Ctrl+Alt+Maj+G");
    o.set_view(1280, 720, 1.0);
    o.show_stats = true;
    o.stats = Some(StatsSnapshot {
        encoder: "NVIDIA (NVENC)".into(),
        decoder: "MF matériel · x".into(),
        ..Default::default()
    });

    let scene = o.scene().unwrap();
    let has_nvenc = scene.items.iter().any(|item| {
        matches!(item, grx_render::OverlayItem::Text { text, .. } if text.contains("NVENC"))
    });
    assert!(has_nvenc, "Scene should contain text with NVENC");
}
