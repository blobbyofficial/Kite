//! Visual style.
//!
//! Dark, because that is what editors are used to working in and a neutral surround stops the
//! interface biasing how you judge the picture. The greys carry a slight blue bias so they read as
//! chosen rather than defaulted, and the one saturated colour is reserved for the playhead and the
//! current selection — the two things you always need to find.

use egui::{Color32, CornerRadius, Margin, Stroke, Visuals};

pub const BG: Color32 = Color32::from_rgb(0x0D, 0x0F, 0x14);
pub const PANEL: Color32 = Color32::from_rgb(0x15, 0x18, 0x1F);
pub const PANEL_HI: Color32 = Color32::from_rgb(0x1C, 0x20, 0x29);
pub const RAISED: Color32 = Color32::from_rgb(0x24, 0x29, 0x34);
pub const LINE: Color32 = Color32::from_rgb(0x25, 0x2A, 0x35);
pub const LINE_2_OR_DIM: Color32 = Color32::from_rgb(0x39, 0x41, 0x4F);
pub const TEXT: Color32 = Color32::from_rgb(0xDD, 0xE2, 0xEA);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x8B, 0x94, 0xA4);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x63, 0x6B, 0x7A);
pub const ACCENT: Color32 = Color32::from_rgb(0xFF, 0x5F, 0xA5);
pub const ACCENT_DIM: Color32 = Color32::from_rgb(0x6E, 0x25, 0x48);
pub const MEAS: Color32 = Color32::from_rgb(0x4A, 0xCA, 0xDA);

pub const VIDEO_CLIP: Color32 = Color32::from_rgb(0x2E, 0x4A, 0x7C);
pub const VIDEO_CLIP_HI: Color32 = Color32::from_rgb(0x41, 0x66, 0xAA);
pub const AUDIO_CLIP: Color32 = Color32::from_rgb(0x22, 0x5C, 0x4C);
pub const AUDIO_CLIP_HI: Color32 = Color32::from_rgb(0x31, 0x81, 0x6A);
pub const TEXT_CLIP: Color32 = Color32::from_rgb(0x5E, 0x3F, 0x86);
pub const TEXT_CLIP_HI: Color32 = Color32::from_rgb(0x83, 0x59, 0xB4);
pub const WAVE: Color32 = Color32::from_rgb(0x8E, 0xE6, 0xC8);
pub const PLAYHEAD: Color32 = Color32::from_rgb(0xFF, 0x5F, 0xA5);
pub const GOOD: Color32 = Color32::from_rgb(0x4F, 0xC9, 0x8A);
pub const WARN: Color32 = Color32::from_rgb(0xD9, 0xA1, 0x28);
pub const BAD: Color32 = Color32::from_rgb(0xF2, 0x70, 0x5F);

pub const R: u8 = 4;

pub fn apply(ctx: &egui::Context) {
    let mut v = Visuals::dark();
    v.override_text_color = Some(TEXT);
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.extreme_bg_color = BG;
    v.faint_bg_color = PANEL_HI;
    v.window_stroke = Stroke::new(1.0, LINE_2_OR_DIM);

    v.widgets.noninteractive.bg_fill = PANEL;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_DIM);

    v.widgets.inactive.bg_fill = RAISED;
    v.widgets.inactive.weak_bg_fill = RAISED;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, LINE);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.inactive.corner_radius = CornerRadius::same(R);

    v.widgets.hovered.bg_fill = LINE_2_OR_DIM;
    v.widgets.hovered.weak_bg_fill = LINE_2_OR_DIM;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, TEXT_FAINT);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.hovered.corner_radius = CornerRadius::same(R);

    v.widgets.active.bg_fill = ACCENT_DIM;
    v.widgets.active.weak_bg_fill = ACCENT_DIM;
    v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    v.widgets.active.corner_radius = CornerRadius::same(R);

    v.widgets.open.bg_fill = PANEL_HI;
    v.widgets.open.weak_bg_fill = PANEL_HI;

    v.selection.bg_fill = ACCENT_DIM;
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.window_corner_radius = CornerRadius::same(6);
    v.menu_corner_radius = CornerRadius::same(6);
    v.popup_shadow = egui::epaint::Shadow {
        offset: [0, 6],
        blur: 18,
        spread: 0,
        color: Color32::from_black_alpha(120),
    };
    v.window_shadow = v.popup_shadow;
    ctx.set_visuals(v);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 7.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.menu_margin = Margin::same(6);
    style.spacing.slider_width = 128.0;
    style.spacing.interact_size.y = 24.0;
    style.spacing.indent = 14.0;
    style.visuals.slider_trailing_fill = true;
    ctx.set_style(style);
}

pub fn mono(size: f32) -> egui::FontId {
    egui::FontId::monospace(size)
}
pub fn ui_font(size: f32) -> egui::FontId {
    egui::FontId::proportional(size)
}

/// A small uppercase heading used to separate groups inside a panel.
pub fn section(ui: &mut egui::Ui, label: &str) {
    ui.add_space(10.0);
    ui.label(
        egui::RichText::new(label)
            .font(mono(10.0))
            .color(TEXT_FAINT)
            .extra_letter_spacing(1.4),
    );
    ui.add_space(2.0);
}

/// A labelled row: caption on the left at a fixed width, control on the right.
/// egui puts slider labels on the right by default, which reads badly in a column of settings.
pub fn row(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui) -> egui::Response) -> egui::Response {
    let mut resp = None;
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(66.0, 20.0), egui::Sense::hover());
        ui.painter().text(
            egui::pos2(rect.left(), rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            ui_font(11.5),
            TEXT_DIM,
        );
        resp = Some(add(ui));
    });
    resp.expect("row body ran")
}
