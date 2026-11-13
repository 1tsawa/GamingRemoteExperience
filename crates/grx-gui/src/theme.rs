//! Thème sombre, polices et petits composants visuels.

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, RichText, Stroke, TextStyle,
};

pub const BG: Color32 = Color32::from_rgb(20, 22, 27);
pub const CARD: Color32 = Color32::from_rgb(30, 33, 40);
pub const CARD_BORDER: Color32 = Color32::from_rgb(52, 57, 68);
pub const FIELD: Color32 = Color32::from_rgb(16, 18, 22);
pub const ACCENT: Color32 = Color32::from_rgb(64, 132, 240);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(86, 150, 252);
pub const OK: Color32 = Color32::from_rgb(96, 204, 128);
pub const WARN: Color32 = Color32::from_rgb(244, 186, 70);
pub const ERR: Color32 = Color32::from_rgb(244, 112, 112);
pub const MUTED: Color32 = Color32::from_rgb(150, 158, 174);
pub const TEXT: Color32 = Color32::from_rgb(226, 230, 238);

pub fn setup(ctx: &egui::Context) {
    // Segoe UI (police système de Windows) pour un texte net avec accents ; repli sur la police intégrée.
    let mut fonts = FontDefinitions::default();
    for (key, file, front) in [("Segoe UI", "segoeui.ttf", true)] {
        if let Ok(bytes) = std::fs::read(std::path::Path::new(r"C:\Windows\Fonts").join(file)) {
            fonts.font_data.insert(key.to_string(), std::sync::Arc::new(FontData::from_owned(bytes)));
            let fam = fonts.families.entry(FontFamily::Proportional).or_default();
            if front {
                fam.insert(0, key.to_string());
            } else {
                fam.push(key.to_string());
            }
        }
    }
    if let Ok(bytes) = std::fs::read(r"C:\Windows\Fonts\consola.ttf") {
        fonts.font_data.insert("Consolas".into(), std::sync::Arc::new(FontData::from_owned(bytes)));
        fonts.families.entry(FontFamily::Monospace).or_default().insert(0, "Consolas".into());
    }
    ctx.set_fonts(fonts);

    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Heading, FontId::new(19.0, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(15.0, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(15.0, FontFamily::Proportional)),
            (TextStyle::Small, FontId::new(13.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(14.5, FontFamily::Monospace)),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(10.0, 9.0);
        style.spacing.button_padding = egui::vec2(14.0, 6.0);
        style.spacing.interact_size.y = 28.0;
        style.spacing.text_edit_width = 280.0;

        let v = &mut style.visuals;
        v.panel_fill = BG;
        v.window_fill = CARD;
        v.extreme_bg_color = FIELD;
        v.faint_bg_color = Color32::from_rgb(36, 40, 48);
        v.override_text_color = Some(TEXT);
        v.hyperlink_color = ACCENT_HOVER;
        v.selection.bg_fill = Color32::from_rgb(40, 84, 160);
        v.selection.stroke = Stroke::new(1.0, TEXT);
        let r = CornerRadius::same(7);
        for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open]
        {
            w.corner_radius = r;
        }
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, CARD_BORDER);
        v.widgets.inactive.bg_fill = Color32::from_rgb(48, 53, 64);
        v.widgets.inactive.weak_bg_fill = Color32::from_rgb(48, 53, 64);
        v.widgets.hovered.bg_fill = Color32::from_rgb(62, 69, 84);
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(62, 69, 84);
        v.widgets.active.bg_fill = Color32::from_rgb(74, 82, 100);
        v.widgets.active.weak_bg_fill = Color32::from_rgb(74, 82, 100);
        v.window_corner_radius = CornerRadius::same(12);
        v.window_stroke = Stroke::new(1.0, CARD_BORDER);
    });
}

/// Carte avec titre.
pub fn card<R>(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, CARD_BORDER))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).heading().strong());
            ui.add_space(2.0);
            add(ui)
        })
        .inner
}

pub fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).color(MUTED)
}

pub fn small_muted(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).color(MUTED).small()
}

pub fn colored(text: impl Into<String>, color: Color32) -> RichText {
    RichText::new(text.into()).color(color)
}

/// Bouton principal (rempli, couleur d'accent).
pub fn primary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).color(Color32::WHITE).strong()).fill(ACCENT)
}

pub fn danger_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).color(Color32::from_rgb(255, 214, 214)))
        .fill(Color32::from_rgb(112, 40, 46))
}

/// Pastille d'état.
pub fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.5, color);
}
