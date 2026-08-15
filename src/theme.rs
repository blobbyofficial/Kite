//! Visual style. Dark by default because that is what editors are used to working in, and because
//! a neutral surround stops the UI biasing how you judge the picture.

use egui::{Color32, CornerRadius, Stroke, Visuals};

pub const BG: Color32 = Color32::from_rgb(0x11, 0x13, 0x18);
pub const PANEL: Color32 = Color32::from_rgb(0x17, 0x1A, 0x21);
pub const PANEL_HI: Color32 = Color32::from_rgb(0x1E, 0x22, 0x2B);
pub const LINE: Color32 = Color32::from_rgb(0x2A, 0x2F, 0x3A);
pub const TEXT: Color32 = Color32::from_rgb(0xD8, 0xDD, 0xE6);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x8A, 0x93, 0xA3);
pub const ACCENT: Color32 = Color32::from_rgb(0xFF, 0x5F, 0xA5);
pub const ACCENT_DIM: Color32 = Color32::from_rgb(0x8E, 0x2F, 0x5C);
pub const VIDEO_CLIP: Color32 = Color32::from_rgb(0x30, 0x4A, 0x7A);
pub const VIDEO_CLIP_HI: Color32 = Color32::from_rgb(0x40, 0x63, 0xA2);
pub const AUDIO_CLIP: Color32 = Color32::from_rgb(0x27, 0x5E, 0x4E);
pub const AUDIO_CLIP_HI: Color32 = Color32::from_rgb(0x34, 0x7D, 0x68);
pub const TEXT_CLIP: Color32 = Color32::from_rgb(0x6A, 0x45, 0x8A);
pub const TEXT_CLIP_HI: Color32 = Color32::from_rgb(0x8B, 0x5C, 0xB2);
pub const WAVE: Color32 = Color32::from_rgb(0x8E, 0xE6, 0xC8);
pub const PLAYHEAD: Color32 = Color32::from_rgb(0xFF, 0x5F, 0xA5);
pub const GOOD: Color32 = Color32::from_rgb(0x4F, 0xC9, 0x8A);
pub const WARN: Color32 = Color32::from_rgb(0xD9, 0xA1, 0x28);
pub const BAD: Color32 = Color32::from_rgb(0xF2, 0x70, 0x5F);

pub fn apply(ctx: &egui::Context) {
    let mut v = Visuals::dark();
    v.override_text_color = Some(TEXT);
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.extreme_bg_color = BG;
    v.faint_bg_color = PANEL_HI;
    v.widgets.noninteractive.bg_fill = PANEL;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    v.widgets.inactive.bg_fill = PANEL_HI;
    v.widgets.inactive.weak_bg_fill = PANEL_HI;
    v.widgets.hovered.bg_fill = LINE;
    v.widgets.hovered.weak_bg_fill = LINE;
    v.widgets.active.bg_fill = ACCENT_DIM;
    v.widgets.active.weak_bg_fill = ACCENT_DIM;
    v.selection.bg_fill = ACCENT_DIM;
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.window_corner_radius = CornerRadius::same(4);
    v.menu_corner_radius = CornerRadius::same(4);
    ctx.set_visuals(v);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(6.0, 5.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    style.spacing.slider_width = 140.0;
    style.spacing.interact_size.y = 22.0;
    ctx.set_style(style);
}

pub fn mono(size: f32) -> egui::FontId {
    egui::FontId::monospace(size)
}
pub fn ui_font(size: f32) -> egui::FontId {
    egui::FontId::proportional(size)
}
